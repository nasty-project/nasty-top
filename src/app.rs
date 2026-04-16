//! Application state and tick logic.

use crate::metrics::{self, History, Rates};
use crate::sysfs::{self, BcachefsFs, FsSnapshot};
use crate::tuning::TuningState;
use std::time::Instant;

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

        // Average latency across devices
        let n = rates.devices.len().max(1) as f64;
        let avg_read_lat: f64 = rates
            .devices
            .iter()
            .map(|d| d.read_latency_ns as f64 / 1000.0) // ns → µs
            .sum::<f64>()
            / n;
        let avg_write_lat: f64 = rates
            .devices
            .iter()
            .map(|d| d.write_latency_ns as f64 / 1000.0)
            .sum::<f64>()
            / n;
        self.history.push("avg_read_latency_us", avg_read_lat);
        self.history.push("avg_write_latency_us", avg_write_lat);

        // Refresh tuning option names if they changed (shouldn't normally)
        self.tuning.refresh_names(&new_snap.options);

        self.previous = Some(std::mem::replace(&mut self.current, new_snap));
        self.rates = Some(rates);

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
