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
    pub should_quit: bool,
    pub show_options: bool,
    pub show_processes: bool,
    pub show_blocked: bool,
    pub prev_proc_io: Vec<ProcessIo>,
    pub process_rates: Vec<ProcessRate>,
    /// Recent stall events (newest first, capped at 10).
    pub stall_events: Vec<StallEvent>,
    /// Current tuning proposal from the advisor.
    pub proposal: Option<crate::advisor::Proposal>,
    /// Temporarily dismissed: (option_name, dismissed_at).
    pub dismissed_temp: Vec<(String, std::time::Instant)>,
    /// Permanently dismissed option names ("don't ask again").
    pub dismissed_permanent: std::collections::HashSet<String>,
    /// Blocked stats delta per tick: (name, delta_count, recent_mean_us).
    pub blocked_deltas: Vec<(String, u64, f64)>,
    pub verbose_devices: bool,
    /// Next marker slot to fill (rotates 0-8).
    pub next_marker_slot: usize,
}

impl App {
    pub fn new(fs: BcachefsFs) -> Self {
        let snap = sysfs::snapshot(&fs);
        let tuning = TuningState::new(&snap.options);
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
            should_quit: false,
            show_options: false,
            show_processes: false,
            show_blocked: false,
            prev_proc_io: sysfs::read_all_process_io(),
            process_rates: Vec::new(),
            stall_events: Vec::new(),
            proposal: None,
            blocked_deltas: Vec::new(),
            dismissed_temp: Vec::new(),
            dismissed_permanent: std::collections::HashSet::new(),
            verbose_devices: false,
            next_marker_slot: 0,
        }
    }

    /// Called every tick — refresh metrics and compute rates.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;

        let new_snap = sysfs::snapshot(&self.fs);

        // Compute rates from previous snapshot
        let rates = metrics::compute_rates(&self.current, &new_snap, dt);

        // Aggregate IO across all devices for sparklines
        let total_read: f64 = rates.devices.iter().map(|d| d.read_bytes_sec).sum();
        let total_write: f64 = rates.devices.iter().map(|d| d.write_bytes_sec).sum();
        self.history.push("io_read_bytes_sec", total_read);
        self.history.push("io_write_bytes_sec", total_write);

        // Latency sparkline: use time_stats "recent" mean, but only when
        // there's actual IO — the EWMA doesn't decay when idle.
        let has_reads = rates.devices.iter().any(|d| d.read_active);
        let has_writes = rates.devices.iter().any(|d| d.write_active);
        self.history.push("avg_read_latency_us", if has_reads { new_snap.recent_data_read_us } else { 0.0 });
        self.history.push("avg_write_latency_us", if has_writes { new_snap.recent_data_write_us } else { 0.0 });

        // Refresh tuning option names if they changed (shouldn't normally)
        self.tuning.refresh_names(&new_snap.options);

        // Stall detection using time_stats "recent" latency + journal pressure.
        if self.previous.is_some() {
            // Data read latency spike (>200ms recent mean, only when reads active)
            if has_reads && new_snap.recent_data_read_us > 200_000.0 {
                self.stall_events.insert(0, StallEvent {
                    time: now,
                    device: "fs".into(),
                    direction: "read",
                    detail: format!("data_read recent mean {}", format_duration_us(new_snap.recent_data_read_us)),
                });
            }

            // Data write latency spike (>200ms recent mean, only when writes active)
            if has_writes && new_snap.recent_data_write_us > 200_000.0 {
                self.stall_events.insert(0, StallEvent {
                    time: now,
                    device: "fs".into(),
                    direction: "write",
                    detail: format!("data_write recent mean {}", format_duration_us(new_snap.recent_data_write_us)),
                });
            }

            // Btree read latency spike (>50ms is bad for metadata)
            if new_snap.recent_btree_read_us > 50_000.0 {
                self.stall_events.insert(0, StallEvent {
                    time: now,
                    device: "fs".into(),
                    direction: "read",
                    detail: format!("btree_read recent mean {}", format_duration_us(new_snap.recent_btree_read_us)),
                });
            }

            // Journal pressure: fill rate increasing rapidly
            let (prev_dirty, _) = self.current.journal_fill; // current is still old here
            let (curr_dirty, curr_total) = new_snap.journal_fill;
            let curr_pct = if curr_total > 0 { curr_dirty as f64 / curr_total as f64 * 100.0 } else { 0.0 };
            if curr_dirty > prev_dirty + 1000 && curr_pct > 70.0 {
                self.stall_events.insert(0, StallEvent {
                    time: now,
                    device: "journal".into(),
                    direction: "write",
                    detail: format!("filling rapidly: {:.0}%", curr_pct),
                });
            }

            self.stall_events.truncate(10);
        }
        // Expire stalls older than 60s
        self.stall_events.retain(|e| e.time.elapsed().as_secs() < 60);

        // Compute blocked stat deltas (before new_snap moves)
        self.blocked_deltas = new_snap.blocked_stats.iter().map(|(name, count, recent_us)| {
            let prev_count = self.current.blocked_stats.iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, c, _)| *c)
                .unwrap_or(*count);
            let delta = count.saturating_sub(prev_count);
            (name.clone(), delta, *recent_us)
        }).collect();
        self.blocked_deltas.sort_by(|a, b| {
            let a_active = a.1 > 0;
            let b_active = b.1 > 0;
            match (a_active, b_active) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => b.1.cmp(&a.1).then(a.0.cmp(&b.0)),
            }
        });

        self.previous = Some(std::mem::replace(&mut self.current, new_snap));
        self.rates = Some(rates);

        // Run advisor
        self.proposal = crate::advisor::evaluate(self);

        // Process I/O
        if self.show_processes {
            let curr_proc = sysfs::read_all_process_io();
            self.process_rates = metrics::compute_process_rates(
                &self.prev_proc_io, &curr_proc, dt, 20, &self.process_rates,
            );
            self.prev_proc_io = curr_proc;
        }

        // Clear status message after a few ticks
        if self.status_msg.is_some() {
            self.status_msg = None;
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Metrics => Focus::Tuning,
            Focus::Tuning => Focus::Metrics,
        };
    }

    pub fn save_marker(&mut self) {
        let slot = self.next_marker_slot;
        self.tuning.save_marker(slot, &self.current.options);
        self.status_msg = Some(format!("Saved marker {} with current options", slot + 1));
        self.next_marker_slot = (self.next_marker_slot + 1) % 9;
    }

    pub fn restore_marker(&mut self, slot: usize) {
        match self.tuning.restore_marker(slot, &self.fs) {
            Ok(()) => {
                self.status_msg = Some(format!("Restored marker {}", slot + 1));
            }
            Err(e) => {
                self.status_msg = Some(format!("Restore failed: {e}"));
            }
        }
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
    pub fn toggle_reconcile(&mut self) {
        let current = self.current.options.get("reconcile_enabled")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);
        let new_val = if current { "0" } else { "1" };
        match sysfs::write_option(&self.fs, "reconcile_enabled", new_val) {
            Ok(()) => self.status_msg = Some(format!("reconcile_enabled = {new_val}")),
            Err(e) => self.status_msg = Some(format!("Failed: {e}")),
        }
    }

    pub fn apply_proposal(&mut self) {
        let proposal = match self.proposal.take() {
            Some(p) => p,
            None => return,
        };
        match sysfs::write_option(&self.fs, &proposal.option, &proposal.value) {
            Ok(()) => {
                self.status_msg = Some(format!("Applied: {} = {}", proposal.option, proposal.value));
            }
            Err(e) => {
                self.status_msg = Some(format!("Failed: {e}"));
            }
        }
    }

    pub fn dismiss_proposal(&mut self) {
        if let Some(ref p) = self.proposal {
            self.dismissed_temp.push((p.option.clone(), std::time::Instant::now()));
        }
        self.proposal = None;
        self.status_msg = Some("Dismissed for 2 minutes".into());
    }

    pub fn dismiss_permanent(&mut self) {
        if let Some(ref p) = self.proposal {
            self.dismissed_permanent.insert(p.option.clone());
            self.status_msg = Some(format!("Won't suggest {} again (press C to clear)", p.option));
        }
        self.proposal = None;
    }

    pub fn clear_dismissals(&mut self) {
        let count = self.dismissed_permanent.len();
        self.dismissed_permanent.clear();
        self.dismissed_temp.clear();
        self.status_msg = Some(format!("Cleared {} permanent dismissals", count));
    }

    pub fn is_dismissed(&self, option: &str) -> bool {
        if self.dismissed_permanent.contains(option) {
            return true;
        }
        self.dismissed_temp.iter().any(|(name, when)| {
            name == option && when.elapsed().as_secs() < 120
        })
    }

    pub fn handle_enter(&mut self) {
        if !matches!(self.focus, Focus::Tuning) {
            return;
        }
        if self.tuning.editing {
            match self.tuning.commit_edit(&self.fs) {
                Ok(val) => {
                    let name = self.tuning.selected_name().unwrap_or("?").to_string();
                    self.status_msg = Some(format!("Set {name} = {val}"));
                }
                Err(e) => {
                    self.status_msg = Some(format!("Error: {e}"));
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
