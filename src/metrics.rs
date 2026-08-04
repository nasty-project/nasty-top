//! Rate computation — diff consecutive snapshots to get per-second rates.

use crate::sysfs::{DeviceInfo, FsSnapshot, ProcessIo};
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
    /// Requests currently in progress at the block layer.
    pub queue_depth: u64,
    /// Time-averaged queue depth over this sample interval.
    pub avg_queue_depth: f64,
    /// Average block-layer completion time per request during this interval.
    pub read_await_ms: f64,
    pub write_await_ms: f64,
    diskstats_interval_valid: bool,
    pub io_errors: u64,
}

impl DeviceRate {
    pub fn total_iops(&self) -> f64 {
        self.read_iops + self.write_iops
    }

    pub fn max_await_ms(&self) -> f64 {
        self.read_await_ms.max(self.write_await_ms)
    }

    pub fn pressure_outlier(&self, median_await_ms: f64, median_queue: f64) -> bool {
        if self.total_iops() == 0.0
            && self.queue_depth == 0
            && self.avg_queue_depth == 0.0
            && self.util_pct == 0.0
        {
            return false;
        }
        let await_limit = (median_await_ms * 3.0).max(20.0);
        let queue_limit = (median_queue * 3.0).max(2.0);
        self.max_await_ms() > await_limit
            || self.avg_queue_depth > queue_limit
            || (self.util_pct >= 95.0 && self.max_await_ms() >= 10.0)
    }

    pub fn pressure_score(&self, median_await_ms: f64, median_queue: f64) -> f64 {
        let outlier = if self.pressure_outlier(median_await_ms, median_queue) {
            10_000.0
        } else {
            0.0
        };
        outlier
            + self.avg_queue_depth * 100.0
            + self.queue_depth as f64 * 10.0
            + self.max_await_ms()
            + self.util_pct / 100.0
    }
}

pub fn device_pressure_medians(devices: &[DeviceRate]) -> (f64, f64) {
    let median = |mut values: Vec<f64>| {
        if values.is_empty() {
            return 0.0;
        }
        values.sort_by(f64::total_cmp);
        values[(values.len() - 1) / 2]
    };
    (
        median(
            devices
                .iter()
                .filter(|device| device.total_iops() > 0.0)
                .map(DeviceRate::max_await_ms)
                .collect(),
        ),
        median(
            devices
                .iter()
                .filter(|device| {
                    device.diskstats_interval_valid
                        && (device.total_iops() > 0.0
                            || device.queue_depth > 0
                            || device.avg_queue_depth > 0.0
                            || device.util_pct > 0.0)
                })
                .map(|device| device.avg_queue_depth)
                .collect(),
        ),
    )
}

fn valid_diskstats_interval(previous: &DeviceInfo, current: &DeviceInfo) -> bool {
    previous.diskstats_valid
        && current.diskstats_valid
        && current.diskstats_reads >= previous.diskstats_reads
        && current.diskstats_writes >= previous.diskstats_writes
        && current.diskstats_read_ms >= previous.diskstats_read_ms
        && current.diskstats_write_ms >= previous.diskstats_write_ms
        && current.diskstats_io_ms >= previous.diskstats_io_ms
        && current.diskstats_weighted_io_ms >= previous.diskstats_weighted_io_ms
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
        let previous = prev
            .devices
            .iter()
            .find(|device| device.index == curr_dev.index && device.name == curr_dev.name);
        // A newly added or re-numbered device has no interval baseline. Use
        // its current counters as the baseline so lifetime totals do not
        // appear as one enormous sample.
        let prev_dev = previous.unwrap_or(curr_dev);
        let diskstats_valid = previous.is_some_and(|prev| valid_diskstats_interval(prev, curr_dev));

        let read_delta = curr_dev.io_done_read.saturating_sub(prev_dev.io_done_read);
        let write_delta = curr_dev
            .io_done_write
            .saturating_sub(prev_dev.io_done_write);
        let read_ios = if diskstats_valid {
            curr_dev.diskstats_reads - prev_dev.diskstats_reads
        } else {
            0
        };
        let write_ios = if diskstats_valid {
            curr_dev.diskstats_writes - prev_dev.diskstats_writes
        } else {
            0
        };

        let diff_map =
            |curr: &HashMap<String, u64>, prev: &HashMap<String, u64>| -> HashMap<String, f64> {
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
            read_iops: read_ios as f64 / dt,
            write_iops: write_ios as f64 / dt,
            util_pct: {
                let io_ms_delta = if diskstats_valid {
                    curr_dev.diskstats_io_ms - prev_dev.diskstats_io_ms
                } else {
                    0
                } as f64;
                (io_ms_delta / (dt * 1000.0) * 100.0).min(100.0)
            },
            queue_depth: if curr_dev.diskstats_valid {
                curr_dev.diskstats_in_flight
            } else {
                0
            },
            avg_queue_depth: if diskstats_valid {
                (curr_dev.diskstats_weighted_io_ms - prev_dev.diskstats_weighted_io_ms) as f64
                    / (dt * 1000.0)
            } else {
                0.0
            },
            read_await_ms: if read_ios > 0 {
                (curr_dev.diskstats_read_ms - prev_dev.diskstats_read_ms) as f64 / read_ios as f64
            } else {
                0.0
            },
            write_await_ms: if write_ios > 0 {
                (curr_dev.diskstats_write_ms - prev_dev.diskstats_write_ms) as f64
                    / write_ios as f64
            } else {
                0.0
            },
            diskstats_interval_valid: diskstats_valid,
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
            by_pid.insert(
                c.pid,
                ProcessRate {
                    pid: c.pid,
                    name: c.name.clone(),
                    read_bytes_sec: rd,
                    write_bytes_sec: wd,
                    total_read: c.read_bytes,
                    total_write: c.write_bytes,
                },
            );
        }
    }

    // Carry forward previously-seen processes that still exist (with zero rates if idle)
    for prev_rate in previous_rates {
        if curr_pids.contains(&prev_rate.pid) && !by_pid.contains_key(&prev_rate.pid) {
            by_pid.insert(
                prev_rate.pid,
                ProcessRate {
                    pid: prev_rate.pid,
                    name: prev_rate.name.clone(),
                    read_bytes_sec: 0.0,
                    write_bytes_sec: 0.0,
                    total_read: prev_rate.total_read,
                    total_write: prev_rate.total_write,
                },
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysfs::DeviceInfo;

    #[test]
    fn computes_queue_depth_and_block_await() {
        let previous = FsSnapshot {
            devices: vec![DeviceInfo {
                index: 1,
                name: "sda".into(),
                diskstats_reads: 100,
                diskstats_writes: 50,
                diskstats_read_ms: 1_000,
                diskstats_write_ms: 2_000,
                diskstats_io_ms: 5_000,
                diskstats_weighted_io_ms: 8_000,
                diskstats_valid: true,
                ..DeviceInfo::default()
            }],
            ..FsSnapshot::default()
        };
        let current = FsSnapshot {
            devices: vec![DeviceInfo {
                index: 1,
                name: "sda".into(),
                diskstats_reads: 110,
                diskstats_writes: 54,
                diskstats_read_ms: 1_050,
                diskstats_write_ms: 2_080,
                diskstats_io_ms: 6_000,
                diskstats_weighted_io_ms: 8_300,
                diskstats_in_flight: 7,
                diskstats_valid: true,
                ..DeviceInfo::default()
            }],
            ..FsSnapshot::default()
        };

        let device = &compute_rates(&previous, &current, 2.0).devices[0];
        assert_eq!(device.read_iops, 5.0);
        assert_eq!(device.write_iops, 2.0);
        assert_eq!(device.read_await_ms, 5.0);
        assert_eq!(device.write_await_ms, 20.0);
        assert_eq!(device.queue_depth, 7);
        assert_eq!(device.avg_queue_depth, 0.15);
        assert_eq!(device.util_pct, 50.0);
    }

    #[test]
    fn pressure_detection_is_relative_but_has_absolute_floors() {
        let normal = DeviceRate {
            read_iops: 100.0,
            read_await_ms: 8.0,
            avg_queue_depth: 0.5,
            diskstats_interval_valid: true,
            ..DeviceRate::default()
        };
        let slow = DeviceRate {
            read_iops: 20.0,
            read_await_ms: 80.0,
            avg_queue_depth: 4.0,
            diskstats_interval_valid: true,
            ..DeviceRate::default()
        };
        let devices = vec![normal.clone(), normal, slow.clone()];
        let (median_await, median_queue) = device_pressure_medians(&devices);
        assert_eq!((median_await, median_queue), (8.0, 0.5));
        assert!(slow.pressure_outlier(median_await, median_queue));
        assert!(!devices[0].pressure_outlier(median_await, median_queue));
    }

    #[test]
    fn two_device_pool_uses_lower_median_for_outlier_detection() {
        let fast = DeviceRate {
            read_iops: 100.0,
            read_await_ms: 5.0,
            ..DeviceRate::default()
        };
        let slow = DeviceRate {
            read_iops: 100.0,
            read_await_ms: 100.0,
            ..DeviceRate::default()
        };
        let devices = vec![fast, slow.clone()];
        let (median_await, median_queue) = device_pressure_medians(&devices);
        assert_eq!(median_await, 5.0);
        assert!(slow.pressure_outlier(median_await, median_queue));
    }

    #[test]
    fn devices_without_completions_do_not_skew_await_median() {
        let devices = vec![
            DeviceRate {
                queue_depth: 4,
                diskstats_interval_valid: true,
                ..DeviceRate::default()
            },
            DeviceRate {
                queue_depth: 3,
                diskstats_interval_valid: true,
                ..DeviceRate::default()
            },
            DeviceRate {
                read_iops: 10.0,
                read_await_ms: 8.0,
                diskstats_interval_valid: true,
                ..DeviceRate::default()
            },
        ];

        assert_eq!(device_pressure_medians(&devices).0, 8.0);
    }

    #[test]
    fn missing_or_reset_diskstats_baseline_suppresses_interval_rates() {
        let current_device = DeviceInfo {
            index: 1,
            name: "sda".into(),
            diskstats_reads: 10_000,
            diskstats_read_ms: 20_000,
            diskstats_io_ms: 30_000,
            diskstats_weighted_io_ms: 40_000,
            diskstats_in_flight: 2,
            diskstats_valid: true,
            ..DeviceInfo::default()
        };
        let current = FsSnapshot {
            devices: vec![current_device.clone()],
            ..FsSnapshot::default()
        };

        let new_device = &compute_rates(&FsSnapshot::default(), &current, 1.0).devices[0];
        assert_eq!(new_device.read_iops, 0.0);
        assert_eq!(new_device.avg_queue_depth, 0.0);
        assert_eq!(new_device.queue_depth, 2);

        let previous = FsSnapshot {
            devices: vec![DeviceInfo {
                diskstats_reads: 20_000,
                diskstats_read_ms: 30_000,
                diskstats_io_ms: 40_000,
                diskstats_weighted_io_ms: 50_000,
                ..current_device
            }],
            ..FsSnapshot::default()
        };
        let reset = &compute_rates(&previous, &current, 1.0).devices[0];
        assert_eq!(reset.read_iops, 0.0);
        assert_eq!(reset.util_pct, 0.0);
        assert_eq!(reset.avg_queue_depth, 0.0);
    }
}
