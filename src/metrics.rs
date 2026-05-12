//! Rate computation — diff consecutive snapshots to get per-second rates.

use crate::sysfs::{FsSnapshot, ProcessIo};
use std::collections::HashMap;

/// Computed rates between two snapshots.
#[derive(Debug, Clone, Default)]
pub struct Rates {
    /// Per-device IO rates.
    pub devices: Vec<DeviceRate>,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceRate {
    pub name: String,
    pub label: Option<String>,
    pub read_bytes_sec: f64,
    pub write_bytes_sec: f64,
    /// Per-category rates (sb, journal, btree, user, etc.) in bytes/sec.
    pub read_by_type: HashMap<String, f64>,
    pub write_by_type: HashMap<String, f64>,
    /// Recent EWMA latency — only meaningful when there's actual IO.
    pub read_latency_ns: u64,
    pub write_latency_ns: u64,
    pub read_active: bool,
    pub write_active: bool,
    pub read_iops: f64,
    pub write_iops: f64,
    pub util_pct: f64,
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
        let buf = self.series.entry(key.to_string()).or_default();
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

        let diff_map = |curr: &HashMap<String, u64>, prev: &HashMap<String, u64>| -> HashMap<String, f64> {
            curr.iter()
                .map(|(k, &v)| {
                    let delta = v.saturating_sub(*prev.get(k).unwrap_or(&0));
                    (k.clone(), delta as f64 / dt)
                })
                .filter(|(_, rate)| *rate > 0.0)
                .collect()
        };

        devices.push(DeviceRate {
            name: curr_dev.name.clone(),
            label: curr_dev.label.clone(),
            read_bytes_sec: read_delta as f64 / dt,
            write_bytes_sec: write_delta as f64 / dt,
            read_active: read_delta > 0,
            write_active: write_delta > 0,
            read_iops: curr_dev.diskstats_reads.saturating_sub(prev_dev.diskstats_reads) as f64 / dt,
            write_iops: curr_dev.diskstats_writes.saturating_sub(prev_dev.diskstats_writes) as f64 / dt,
            util_pct: {
                let io_ms_delta = curr_dev.diskstats_io_ms.saturating_sub(prev_dev.diskstats_io_ms) as f64;
                (io_ms_delta / (dt * 1000.0) * 100.0).min(100.0)
            },
            read_by_type: diff_map(&curr_dev.io_read_by_type, &prev_dev.io_read_by_type),
            write_by_type: diff_map(&curr_dev.io_write_by_type, &prev_dev.io_write_by_type),
            read_latency_ns: curr_dev.io_latency_read_ns,
            write_latency_ns: curr_dev.io_latency_write_ns,
            io_errors: curr_dev.io_errors,
        });
    }

    Rates { devices }
}

#[derive(Debug, Clone)]
pub struct ProcessRate {
    pub pid: u32,
    pub name: String,
    pub read_bytes_sec: f64,
    pub write_bytes_sec: f64,
    /// Cumulative total IO since process start.
    pub total_read: u64,
    pub total_write: u64,
}

/// Compute per-process I/O rates. Merges with `previous_rates` so recently-seen
/// processes stay visible (with zero rates) instead of disappearing immediately.
/// Active processes sort to the top, idle ones sink to the bottom.
pub fn compute_process_rates(
    prev: &[ProcessIo],
    curr: &[ProcessIo],
    dt: f64,
    top_n: usize,
    previous_rates: &[ProcessRate],
) -> Vec<ProcessRate> {
    if dt <= 0.0 {
        return previous_rates.to_vec();
    }

    let prev_map: HashMap<u32, &ProcessIo> = prev.iter().map(|p| (p.pid, p)).collect();
    let curr_pids: std::collections::HashSet<u32> = curr.iter().map(|c| c.pid).collect();

    // Compute current rates
    let mut by_pid: HashMap<u32, ProcessRate> = HashMap::new();
    for c in curr {
        if let Some(p) = prev_map.get(&c.pid) {
            let rd = c.read_bytes.saturating_sub(p.read_bytes) as f64 / dt;
            let wd = c.write_bytes.saturating_sub(p.write_bytes) as f64 / dt;
            by_pid.insert(c.pid, ProcessRate {
                pid: c.pid,
                name: c.name.clone(),
                read_bytes_sec: rd,
                write_bytes_sec: wd,
                total_read: c.read_bytes,
                total_write: c.write_bytes,
            });
        }
    }

    // Carry forward previously-seen processes that still exist (with zero rates if idle)
    for prev_rate in previous_rates {
        if curr_pids.contains(&prev_rate.pid) && !by_pid.contains_key(&prev_rate.pid) {
            by_pid.insert(prev_rate.pid, ProcessRate {
                pid: prev_rate.pid,
                name: prev_rate.name.clone(),
                read_bytes_sec: 0.0,
                write_bytes_sec: 0.0,
                total_read: prev_rate.total_read,
                total_write: prev_rate.total_write,
            });
        }
    }

    // Sort: active first (by total desc), then idle alphabetically
    let mut rates: Vec<ProcessRate> = by_pid.into_values().collect();
    rates.sort_by(|a, b| {
        let ta = a.read_bytes_sec + a.write_bytes_sec;
        let tb = b.read_bytes_sec + b.write_bytes_sec;
        let both_idle = ta == 0.0 && tb == 0.0;
        if both_idle {
            a.name.cmp(&b.name)
        } else {
            tb.partial_cmp(&ta).unwrap_or(std::cmp::Ordering::Equal)
        }
    });
    rates.truncate(top_n);
    rates
}
