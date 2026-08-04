//! Application state and tick logic.

use crate::metrics::{self, History, ProcessRate, Rates};
use crate::sysfs::{self, BcachefsFs, FsSnapshot, ProcessIo};
use crate::tuning::TuningState;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct StallEvent {
    pub time: std::time::Instant,
    pub device: String,
    pub direction: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct TimeStatView {
    pub name: String,
    pub count_delta: u64,
    pub count_total: u64,
    pub mean_ns: u64,
    pub recent_ns: u64,
    pub max_ns: u64,
    pub is_blocked: bool,
}

/// Which panel has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Metrics,
    Tuning,
}

pub struct App {
    pub fs: BcachefsFs,
    pub current: FsSnapshot,
    pub previous: Option<FsSnapshot>,
    pub rates: Option<Rates>,
    pub history: History,
    pub tuning: TuningState,
    pub focus: Focus,
    pub last_tick: Instant,
    pub status_msg: Option<String>,
    pub status_msg_since: Option<Instant>,
    pub status_msg_rendered: bool,
    pub should_quit: bool,
    /// All discovered bcachefs filesystems.
    pub all_fs: Vec<sysfs::BcachefsFs>,
    /// Index of the currently active filesystem in all_fs.
    pub fs_index: usize,
    pub show_options: bool,
    pub show_processes: bool,
    pub show_blocked: bool,
    pub prev_proc_io: Vec<ProcessIo>,
    pub process_rates: Vec<ProcessRate>,
    /// Recent stall events (newest first, capped at 10).
    pub stall_events: Vec<StallEvent>,
    /// Current advisor hint (informational only).
    pub proposal: Option<crate::advisor::Proposal>,
    /// When the current hint first appeared — used to enforce a minimum
    /// display time so triggers that fire for a single tick stay visible
    /// long enough to read.
    pub proposal_first_shown: Option<Instant>,
    /// Temporarily dismissed: (option_name, dismissed_at).
    pub dismissed_temp: Vec<(String, std::time::Instant)>,
    /// Permanently dismissed option names ("don't ask again").
    pub dismissed_permanent: std::collections::HashSet<String>,
    /// Blocked stats delta per tick: (name, delta_count, recent_mean_us).
    pub blocked_deltas: Vec<(String, u64, f64)>,
    /// Counter deltas per tick: (name, delta, cumulative).
    pub counter_deltas: Vec<(String, u64, u64)>,
    /// Actual seconds between the snapshots used for the current deltas.
    pub sample_interval: f64,
    /// Time stats with delta count per tick.
    pub time_stats_view: Vec<TimeStatView>,
    /// CPU iowait % (computed from delta between ticks).
    pub iowait_pct: f64,
    pub show_counters: bool,
    pub show_help: bool,
    /// Scroll offset for counter/time stats/process views.
    pub view_scroll: usize,
    /// Selected row in the synchronized device tables.
    pub device_scroll: usize,
    /// Show the most pressured devices first instead of sysfs order.
    pub sort_devices_by_pressure: bool,
    /// Per-device io_errors value when first observed in this session.
    /// Used to color the Err cell: matching baseline = historic (dim),
    /// growing = active (red).
    pub initial_errors: std::collections::HashMap<String, u64>,
}

impl App {
    pub fn new(all_fs: Vec<BcachefsFs>, fs_index: usize) -> Self {
        let fs = all_fs[fs_index].clone();
        let snap = sysfs::snapshot(&fs);
        let tuning = TuningState::new(&snap.options);
        let initial_errors = snap
            .devices
            .iter()
            .map(|d| (d.name.clone(), d.io_errors))
            .collect();
        Self {
            fs,
            current: snap,
            previous: None,
            rates: None,
            history: History::new(120), // 2 minutes at 1s tick
            tuning,
            focus: Focus::Metrics,
            last_tick: Instant::now(),
            status_msg: None,
            status_msg_since: None,
            status_msg_rendered: false,
            should_quit: false,
            all_fs,
            fs_index,
            show_options: false,
            show_processes: false,
            show_blocked: false,
            prev_proc_io: sysfs::read_all_process_io(),
            process_rates: Vec::new(),
            stall_events: Vec::new(),
            proposal: None,
            proposal_first_shown: None,
            blocked_deltas: Vec::new(),
            counter_deltas: Vec::new(),
            sample_interval: 0.0,
            time_stats_view: Vec::new(),
            iowait_pct: 0.0,
            show_counters: false,
            show_help: false,
            view_scroll: 0,
            device_scroll: 0,
            sort_devices_by_pressure: false,
            dismissed_temp: Vec::new(),
            dismissed_permanent: std::collections::HashSet::new(),
            initial_errors,
        }
    }

    /// Called every tick — refresh metrics and compute rates.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        self.sample_interval = dt;

        let new_snap = sysfs::snapshot(&self.fs);

        // Baseline error counts for any newly-appearing devices so we
        // don't flag pre-existing errors on a device added mid-session.
        for d in &new_snap.devices {
            self.initial_errors
                .entry(d.name.clone())
                .or_insert(d.io_errors);
        }

        // Compute rates from previous snapshot
        let rates = metrics::compute_rates(&self.current, &new_snap, dt);

        // Aggregate IO across all devices for sparklines
        let total_read: f64 = rates.devices.iter().map(|d| d.read_bytes_sec).sum();
        let total_write: f64 = rates.devices.iter().map(|d| d.write_bytes_sec).sum();
        self.history.push("io_read_bytes_sec", total_read);
        self.history.push("io_write_bytes_sec", total_write);

        // CPU iowait %
        let iowait_delta = new_snap.cpu_iowait.saturating_sub(self.current.cpu_iowait) as f64;
        let cpu_total_delta = new_snap.cpu_total.saturating_sub(self.current.cpu_total) as f64;
        self.iowait_pct = if cpu_total_delta > 0.0 {
            iowait_delta / cpu_total_delta * 100.0
        } else {
            0.0
        };

        // IOPS totals for sparkline titles
        let total_read_iops: f64 = rates.devices.iter().map(|d| d.read_iops).sum();
        let total_write_iops: f64 = rates.devices.iter().map(|d| d.write_iops).sum();
        self.history.push("io_read_iops", total_read_iops);
        self.history.push("io_write_iops", total_write_iops);

        // Latency sparkline: use time_stats "recent" mean, but only when
        // there's actual IO — the EWMA doesn't decay when idle.
        let has_reads = rates.devices.iter().any(|d| d.read_active);
        let has_writes = rates.devices.iter().any(|d| d.write_active);
        self.history.push(
            "avg_read_latency_us",
            if has_reads {
                new_snap.recent_data_read_us
            } else {
                0.0
            },
        );
        self.history.push(
            "avg_write_latency_us",
            if has_writes {
                new_snap.recent_data_write_us
            } else {
                0.0
            },
        );

        // Refresh tuning option names if they changed (shouldn't normally)
        self.tuning.refresh_names(&new_snap.options);

        // Stall detection using time_stats "recent" latency + journal pressure.
        if self.previous.is_some() {
            // Data read latency spike (>200ms recent mean, only when reads active)
            if has_reads && new_snap.recent_data_read_us > 200_000.0 {
                self.stall_events.insert(
                    0,
                    StallEvent {
                        time: now,
                        device: "fs".into(),
                        direction: "read",
                        detail: format!(
                            "data_read recent mean {}",
                            format_duration_us(new_snap.recent_data_read_us)
                        ),
                    },
                );
            }

            // Data write latency spike (>200ms recent mean, only when writes active)
            if has_writes && new_snap.recent_data_write_us > 200_000.0 {
                self.stall_events.insert(
                    0,
                    StallEvent {
                        time: now,
                        device: "fs".into(),
                        direction: "write",
                        detail: format!(
                            "data_write recent mean {}",
                            format_duration_us(new_snap.recent_data_write_us)
                        ),
                    },
                );
            }

            // The recent mean is an EWMA and stays high while idle. Only emit
            // a new event when btree reads actually occurred this tick.
            if btree_read_stalled(&self.current, &new_snap) {
                self.stall_events.insert(
                    0,
                    StallEvent {
                        time: now,
                        device: "fs".into(),
                        direction: "read",
                        detail: format!(
                            "btree_read recent mean {}",
                            format_duration_us(new_snap.recent_btree_read_us)
                        ),
                    },
                );
            }

            // Journal pressure: fill rate increasing rapidly
            let (prev_dirty, _) = self.current.journal_fill; // current is still old here
            let (curr_dirty, curr_total) = new_snap.journal_fill;
            let curr_pct = if curr_total > 0 {
                curr_dirty as f64 / curr_total as f64 * 100.0
            } else {
                0.0
            };
            if curr_dirty > prev_dirty + 1000 && curr_pct > 70.0 {
                self.stall_events.insert(
                    0,
                    StallEvent {
                        time: now,
                        device: "journal".into(),
                        direction: "write",
                        detail: format!("filling rapidly: {:.0}%", curr_pct),
                    },
                );
            }

            self.stall_events.truncate(10);
        }
        // Expire stalls older than 60s
        self.stall_events
            .retain(|e| e.time.elapsed().as_secs() < 60);

        // Compute blocked stat deltas (before new_snap moves)
        self.blocked_deltas = new_snap
            .blocked_stats
            .iter()
            .map(|(name, count, recent_us)| {
                let prev_count = self
                    .current
                    .blocked_stats
                    .iter()
                    .find(|(n, _, _)| n == name)
                    .map(|(_, c, _)| *c)
                    .unwrap_or(*count);
                let delta = count.saturating_sub(prev_count);
                (name.clone(), delta, *recent_us)
            })
            .collect();
        self.blocked_deltas.sort_by(|a, b| {
            let a_active = a.1 > 0;
            let b_active = b.1 > 0;
            match (a_active, b_active) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => b.1.cmp(&a.1).then(a.0.cmp(&b.0)),
            }
        });

        // Compute time_stats view with deltas
        self.time_stats_view = new_snap
            .all_time_stats
            .iter()
            .map(|ts| {
                let prev_count = self
                    .current
                    .all_time_stats
                    .iter()
                    .find(|p| p.name == ts.name)
                    .map(|p| p.count)
                    .unwrap_or(ts.count);
                let delta = ts.count.saturating_sub(prev_count);
                TimeStatView {
                    name: ts.name.clone(),
                    count_delta: delta,
                    count_total: ts.count,
                    mean_ns: ts.dur_mean_ns,
                    recent_ns: ts.dur_recent_ns,
                    max_ns: ts.dur_max_ns,
                    is_blocked: ts.name.starts_with("blocked_"),
                }
            })
            .collect();
        // Sort: active first by delta desc, blocked first within active, then alphabetical
        self.time_stats_view.sort_by(|a, b| {
            match (a.count_delta > 0, b.count_delta > 0) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ if a.count_delta != b.count_delta => b.count_delta.cmp(&a.count_delta),
                _ => {
                    // Within same delta, blocked entries first
                    match (a.is_blocked, b.is_blocked) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a.name.cmp(&b.name),
                    }
                }
            }
        });

        // Compute counter deltas
        self.counter_deltas = new_snap
            .counters
            .iter()
            .map(|(name, &val)| {
                let prev_val = self.current.counters.get(name).copied().unwrap_or(val);
                let delta = val.saturating_sub(prev_val);
                (name.clone(), delta, val)
            })
            .collect();
        // Sort: active first by delta desc, then alphabetical
        self.counter_deltas
            .sort_by(|a, b| match (a.1 > 0, b.1 > 0) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ if a.1 != b.1 => b.1.cmp(&a.1),
                _ => a.0.cmp(&b.0),
            });

        self.previous = Some(std::mem::replace(&mut self.current, new_snap));
        self.rates = Some(rates);
        self.device_scroll = self.device_scroll.min(
            self.rates
                .as_ref()
                .map(|rates| rates.devices.len().saturating_sub(1))
                .unwrap_or(0),
        );

        // Run advisor. Hints persist for at least MIN_HINT_DISPLAY even if
        // the trigger condition stops firing, so single-tick triggers don't
        // flash by faster than the user can read.
        const MIN_HINT_DISPLAY: std::time::Duration = std::time::Duration::from_secs(15);
        let new_proposal = crate::advisor::evaluate(self);
        match (self.proposal.as_ref(), new_proposal) {
            (Some(curr), Some(new)) if curr.option == new.option => {
                self.proposal = Some(new);
            }
            (_, Some(new)) => {
                self.proposal = Some(new);
                self.proposal_first_shown = Some(now);
            }
            (Some(_), None) => {
                let elapsed = self
                    .proposal_first_shown
                    .map(|t| t.elapsed())
                    .unwrap_or(MIN_HINT_DISPLAY);
                if elapsed >= MIN_HINT_DISPLAY {
                    self.proposal = None;
                    self.proposal_first_shown = None;
                }
            }
            (None, None) => {}
        }

        // Process I/O
        if self.show_processes {
            let curr_proc = sysfs::read_all_process_io();
            self.process_rates = metrics::compute_process_rates(
                &self.prev_proc_io,
                &curr_proc,
                dt,
                20,
                &self.process_rates,
            );
            self.prev_proc_io = curr_proc;
        }

        if self.status_msg_since.is_some_and(|shown| {
            self.status_msg_rendered && shown.elapsed() >= std::time::Duration::from_secs(4)
        }) {
            self.clear_status();
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = toggled_focus(self.show_options, self.focus);
    }

    pub fn toggle_options(&mut self) {
        self.show_options = !self.show_options;
        if !self.show_options {
            self.focus = Focus::Metrics;
            self.tuning.cancel_edit();
        }
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_msg = Some(message.into());
        self.status_msg_since = Some(Instant::now());
        self.status_msg_rendered = false;
    }

    pub fn clear_status(&mut self) {
        self.status_msg = None;
        self.status_msg_since = None;
        self.status_msg_rendered = false;
    }

    pub fn scroll_devices_up(&mut self) {
        self.device_scroll = self.device_scroll.saturating_sub(1);
    }

    pub fn scroll_devices_down(&mut self) {
        let last = self
            .rates
            .as_ref()
            .map(|rates| rates.devices.len().saturating_sub(1))
            .unwrap_or(0);
        self.device_scroll = (self.device_scroll + 1).min(last);
    }

    pub fn toggle_device_sort(&mut self) {
        self.sort_devices_by_pressure = !self.sort_devices_by_pressure;
        self.device_scroll = 0;
        self.set_status(if self.sort_devices_by_pressure {
            "Devices sorted by pressure"
        } else {
            "Devices sorted by filesystem order"
        });
    }
}

fn format_duration_us(us: f64) -> String {
    if us >= 1_000_000.0 {
        format!("{:.1}s", us / 1_000_000.0)
    } else if us >= 1_000.0 {
        format!("{:.1}ms", us / 1_000.0)
    } else {
        format!("{:.0}µs", us)
    }
}

impl App {
    pub fn switch_fs(&mut self) -> bool {
        if self.all_fs.len() <= 1 {
            self.set_status("Only one filesystem mounted");
            return false;
        }
        self.fs_index = (self.fs_index + 1) % self.all_fs.len();
        self.fs = self.all_fs[self.fs_index].clone();
        let snap = sysfs::snapshot(&self.fs);
        self.tuning = TuningState::new(&snap.options);
        self.initial_errors = snap
            .devices
            .iter()
            .map(|d| (d.name.clone(), d.io_errors))
            .collect();
        self.current = snap;
        self.previous = None;
        self.rates = None;
        self.last_tick = Instant::now();
        self.history = History::new(120);
        self.stall_events.clear();
        self.blocked_deltas.clear();
        self.counter_deltas.clear();
        self.time_stats_view.clear();
        self.sample_interval = 0.0;
        self.process_rates.clear();
        self.prev_proc_io = sysfs::read_all_process_io();
        self.iowait_pct = 0.0;
        self.view_scroll = 0;
        self.device_scroll = 0;
        self.proposal = None;
        self.proposal_first_shown = None;
        self.set_status(format!(
            "Switched to: {} ({}/{})",
            self.fs.fs_name,
            self.fs_index + 1,
            self.all_fs.len()
        ));
        true
    }

    pub fn toggle_option(&mut self, name: &str) {
        let current = self
            .current
            .options
            .get(name)
            .map(|v| v.trim() == "1")
            .unwrap_or(false);
        let new_val = if current { "0" } else { "1" };
        match sysfs::write_option(&self.fs, name, new_val) {
            Ok(()) => self.set_status(format!("{name} = {new_val}")),
            Err(e) => self.set_status(format!("Failed: {e}")),
        }
    }

    pub fn dismiss_proposal(&mut self) {
        if let Some(ref p) = self.proposal {
            self.dismissed_temp
                .push((p.option.clone(), std::time::Instant::now()));
        }
        self.proposal = None;
        self.proposal_first_shown = None;
        self.set_status("Muted for 2 minutes");
    }

    pub fn dismiss_permanent(&mut self) {
        if let Some(ref p) = self.proposal {
            self.dismissed_permanent.insert(p.option.clone());
            self.set_status(format!(
                "Won't hint about {} again (press C to clear)",
                p.option
            ));
        }
        self.proposal = None;
        self.proposal_first_shown = None;
    }

    pub fn clear_dismissals(&mut self) {
        let count = self.dismissed_permanent.len();
        self.dismissed_permanent.clear();
        self.dismissed_temp.clear();
        self.set_status(format!("Cleared {} permanent dismissals", count));
    }

    pub fn is_dismissed(&self, option: &str) -> bool {
        if self.dismissed_permanent.contains(option) {
            return true;
        }
        self.dismissed_temp
            .iter()
            .any(|(name, when)| name == option && when.elapsed().as_secs() < 120)
    }

    pub fn handle_enter(&mut self) {
        if !self.show_options || !matches!(self.focus, Focus::Tuning) {
            return;
        }
        if self.tuning.editing {
            match self.tuning.commit_edit(&self.fs) {
                Ok(val) => {
                    let name = self.tuning.selected_name().unwrap_or("?").to_string();
                    self.set_status(format!("Set {name} = {val}"));
                }
                Err(e) => {
                    self.set_status(format!("Error: {e}"));
                }
            }
        } else if let Some(name) = self.tuning.selected_name() {
            let current = self
                .current
                .options
                .get(name)
                .map(|s| s.as_str())
                .unwrap_or("");
            self.tuning.start_edit(current);
        }
    }
}

fn toggled_focus(show_options: bool, focus: Focus) -> Focus {
    if !show_options {
        return Focus::Metrics;
    }
    match focus {
        Focus::Metrics => Focus::Tuning,
        Focus::Tuning => Focus::Metrics,
    }
}

fn btree_read_stalled(previous: &FsSnapshot, current: &FsSnapshot) -> bool {
    current.btree_read_count > previous.btree_read_count && current.recent_btree_read_us > 50_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_btree_reads(count: u64, recent_us: f64) -> FsSnapshot {
        FsSnapshot {
            btree_read_count: count,
            recent_btree_read_us: recent_us,
            ..FsSnapshot::default()
        }
    }

    #[test]
    fn hidden_options_panel_cannot_receive_focus() {
        assert_eq!(toggled_focus(false, Focus::Metrics), Focus::Metrics);
        assert_eq!(toggled_focus(false, Focus::Tuning), Focus::Metrics);
        assert_eq!(toggled_focus(true, Focus::Metrics), Focus::Tuning);
        assert_eq!(toggled_focus(true, Focus::Tuning), Focus::Metrics);
    }

    #[test]
    fn btree_stall_requires_new_reads_and_high_latency() {
        let previous = snapshot_with_btree_reads(10, 0.0);
        let stale_latency = snapshot_with_btree_reads(10, 75_000.0);
        let low_latency = snapshot_with_btree_reads(11, 50_000.0);
        let stalled = snapshot_with_btree_reads(11, 50_001.0);

        assert!(!btree_read_stalled(&previous, &stale_latency));
        assert!(!btree_read_stalled(&previous, &low_latency));
        assert!(btree_read_stalled(&previous, &stalled));
    }
}
