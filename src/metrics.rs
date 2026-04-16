//! Rate computation — diff consecutive snapshots to get per-second rates.

use crate::sysfs::FsSnapshot;
use std::collections::HashMap;

/// Computed rates between two snapshots.
#[derive(Debug, Clone, Default)]
pub struct Rates {
    /// Counter rates (value/sec).
    pub counters: HashMap<String, f64>,
    /// Per-device IO rates.
    pub devices: Vec<DeviceRate>,
    /// Interval in seconds between snapshots.
    pub interval_secs: f64,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceRate {
    pub name: String,
    pub label: Option<String>,
    pub read_bytes_sec: f64,
    pub write_bytes_sec: f64,
    pub read_latency_ns: u64,
    pub write_latency_ns: u64,
    pub io_errors: u64,
}

/// History ring buffer for sparklines.
pub struct History {
    /// Per-metric ring buffer of values (newest at the end).
    pub series: HashMap<String, Vec<f64>>,
    pub capacity: usize,
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self {
            series: HashMap::new(),
            capacity,
        }
    }

    pub fn push(&mut self, key: &str, value: f64) {
        let buf = self.series.entry(key.to_string()).or_insert_with(Vec::new);
        if buf.len() >= self.capacity {
            buf.remove(0);
        }
        buf.push(value);
    }

    pub fn get(&self, key: &str) -> &[f64] {
        self.series.get(key).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

/// Compute per-second rates from two snapshots taken `dt` seconds apart.
pub fn compute_rates(prev: &FsSnapshot, curr: &FsSnapshot, dt: f64) -> Rates {
    if dt <= 0.0 {
        return Rates::default();
    }

    let mut counters = HashMap::new();
    for (key, &curr_val) in &curr.counters {
        if let Some(&prev_val) = prev.counters.get(key) {
            let delta = curr_val.saturating_sub(prev_val);
            counters.insert(key.clone(), delta as f64 / dt);
        }
    }

    let mut devices = Vec::new();
    for curr_dev in &curr.devices {
        let prev_dev = prev
            .devices
            .iter()
            .find(|d| d.index == curr_dev.index)
            .cloned()
            .unwrap_or_default();

        let read_delta = curr_dev.io_done_read.saturating_sub(prev_dev.io_done_read);
        let write_delta = curr_dev.io_done_write.saturating_sub(prev_dev.io_done_write);

        devices.push(DeviceRate {
            name: curr_dev.name.clone(),
            label: curr_dev.label.clone(),
            // io_done is in sectors (512 bytes) typically — convert to bytes
            read_bytes_sec: (read_delta as f64 * 512.0) / dt,
            write_bytes_sec: (write_delta as f64 * 512.0) / dt,
            read_latency_ns: curr_dev.io_latency_read_ns,
            write_latency_ns: curr_dev.io_latency_write_ns,
            io_errors: curr_dev.io_errors,
        });
    }

    Rates {
        counters,
        devices,
        interval_secs: dt,
    }
}
