//! bcachefs discovery and sysfs reading.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A discovered bcachefs filesystem.
#[derive(Debug, Clone)]
pub struct BcachefsFs {
    pub uuid: String,
    pub mount_point: String,
    pub fs_name: String,
    pub sysfs: PathBuf,
}

/// Per-device info from sysfs.
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub index: u32,
    pub name: String,
    pub label: Option<String>,
    pub io_latency_read_ns: u64,
    pub io_latency_write_ns: u64,
    pub io_done_read: u64,
    pub io_done_write: u64,
    /// Per-category breakdown: sb, journal, btree, user, etc.
    pub io_read_by_type: HashMap<String, u64>,
    pub io_write_by_type: HashMap<String, u64>,
    pub io_errors: u64,
}

/// Time stat entry from time_stats_json.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TimeStat {
    #[serde(default)]
    pub count: u64,
    #[serde(default, alias = "average_duration")]
    pub mean_ns: u64,
    #[serde(default, alias = "min")]
    pub min_ns: u64,
    #[serde(default, alias = "max")]
    pub max_ns: u64,
}

/// Snapshot of all metrics for one filesystem at one point in time.
#[derive(Debug, Clone, Default)]
pub struct FsSnapshot {
    pub counters: HashMap<String, u64>,
    pub time_stats: HashMap<String, TimeStat>,
    /// Key latencies from time_stats "recent" column.
    pub recent_data_read_us: f64,
    pub recent_data_write_us: f64,
    pub recent_btree_read_us: f64,
    /// Blocked stats: (name, cumulative_count, recent_mean_us).
    pub blocked_stats: Vec<(String, u64, f64)>,
    /// Compression: (compressed_bytes, uncompressed_bytes).
    pub compression: (u64, u64),
    pub devices: Vec<DeviceInfo>,
    pub space_total: u64,
    pub space_used: u64,
    pub options: HashMap<String, String>,
    pub background: Vec<(String, String)>,
    /// Journal fill: (dirty, total) entries.
    pub journal_fill: (u64, u64),
    /// Journal watermark level.
    pub journal_watermark: String,
}

/// Discover mounted bcachefs filesystems from /proc/mounts.
pub fn discover() -> Vec<BcachefsFs> {
    let content = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 || parts[2] != "bcachefs" {
            continue;
        }
        let mount_point = parts[1];
        // Skip kubelet bind mounts — prefer /fs/ canonical mounts
        if !mount_point.starts_with("/fs/") {
            continue;
        }
        let first_dev = parts[0].split(':').next().unwrap_or("");
        let uuid = read_blkid_uuid(first_dev).unwrap_or_default();
        if uuid.is_empty() || !seen.insert(uuid.clone()) {
            continue;
        }
        let fs_name = Path::new(mount_point)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let sysfs = PathBuf::from(format!("/sys/fs/bcachefs/{uuid}"));
        if sysfs.is_dir() {
            result.push(BcachefsFs {
                uuid,
                mount_point: mount_point.to_string(),
                fs_name,
                sysfs,
            });
        }
    }
    result
}

fn read_blkid_uuid(device: &str) -> Option<String> {
    let output = std::process::Command::new("blkid")
        .args(["-s", "UUID", "-o", "value", device])
        .output()
        .ok()?;
    let uuid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uuid.is_empty() { None } else { Some(uuid) }
}

/// Read all metrics for a filesystem.
pub fn snapshot(fs: &BcachefsFs) -> FsSnapshot {
    let mut snap = FsSnapshot::default();
    snap.counters = read_counters(&fs.sysfs);
    snap.time_stats = read_time_stats(&fs.sysfs);

    // Extract key recent latencies
    snap.recent_data_read_us = read_recent_mean_us(&fs.sysfs, "data_read");
    snap.recent_data_write_us = read_recent_mean_us(&fs.sysfs, "data_write");
    snap.recent_btree_read_us = read_recent_mean_us(&fs.sysfs, "btree_node_read");
    snap.blocked_stats = read_blocked_stats(&fs.sysfs);
    snap.compression = read_compression_stats(&fs.sysfs);
    snap.devices = read_devices(&fs.sysfs);
    snap.options = read_options(&fs.sysfs);
    snap.background = read_background(&fs.sysfs, &fs.mount_point);
    let (jf, jw) = read_journal_fill(&fs.sysfs);
    snap.journal_fill = jf;
    snap.journal_watermark = jw;

    // Space via statvfs
    if let Ok(stat) = nix::sys::statvfs::statvfs(fs.mount_point.as_str()) {
        snap.space_total = stat.blocks() as u64 * stat.fragment_size() as u64;
        snap.space_used = snap.space_total - stat.blocks_available() as u64 * stat.fragment_size() as u64;
    }

    snap
}

fn read_counters(sysfs: &Path) -> HashMap<String, u64> {
    let dir = sysfs.join("counters");
    read_dir_u64_files(&dir)
}

fn read_dir_u64_files(dir: &Path) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return map,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            // Try plain number first, then "since mount: N" format
            let val = content.trim().parse::<u64>().unwrap_or_else(|_| {
                content.lines()
                    .find(|l| l.contains("since mount"))
                    .and_then(|l| l.split(':').last())
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0)
            });
            map.insert(name, val);
        }
    }
    map
}

fn read_time_stats(sysfs: &Path) -> HashMap<String, TimeStat> {
    let dir = sysfs.join("time_stats");
    let mut map = HashMap::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return map,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            // time_stats files have a key: value format per line
            let mut stat = TimeStat::default();
            for line in content.lines() {
                let parts: Vec<&str> = line.splitn(2, ':').collect();
                if parts.len() != 2 {
                    continue;
                }
                let key = parts[0].trim();
                let val_str = parts[1].trim().split_whitespace().next().unwrap_or("0");
                let val: u64 = val_str.parse().unwrap_or(0);
                match key {
                    "count" => stat.count = val,
                    "rate" => {} // skip
                    _ if key.contains("mean") || key.contains("average") || key.contains("avg") => {
                        stat.mean_ns = val;
                    }
                    _ if key.contains("min") => stat.min_ns = val,
                    _ if key.contains("max") => stat.max_ns = val,
                    _ => {}
                }
            }
            map.insert(name, stat);
        }
    }
    map
}

fn read_devices(sysfs: &Path) -> Vec<DeviceInfo> {
    let mut devices = Vec::new();
    let entries = match std::fs::read_dir(sysfs) {
        Ok(e) => e,
        Err(_) => return devices,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("dev-") {
            continue;
        }
        let dev_path = entry.path();
        let index: u32 = name
            .strip_prefix("dev-")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let label = read_file_string(&dev_path.join("label"));
        let dev_name = read_file_string(&dev_path.join("dev"))
            .or_else(|| {
                // Resolve block device name from sysfs
                std::fs::read_link(dev_path.join("block"))
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            })
            .unwrap_or_else(|| format!("dev-{index}"));

        let read_lat = read_latency_ns(&dev_path, "read");
        let write_lat = read_latency_ns(&dev_path, "write");

        let (io_read, io_write, io_read_by_type, io_write_by_type) = read_io_done(&dev_path);
        let io_errors = read_io_errors(&dev_path);

        devices.push(DeviceInfo {
            index,
            name: dev_name,
            label,
            io_latency_read_ns: read_lat,
            io_latency_write_ns: write_lat,
            io_done_read: io_read,
            io_done_write: io_write,
            io_read_by_type,
            io_write_by_type,
            io_errors,
        });
    }
    devices.sort_by_key(|d| d.index);
    devices
}

/// Read per-device recent (EWMA) latency from io_latency_stats_{direction}_json.
/// Falls back to the cumulative io_latency_{direction} if JSON isn't available.
fn read_latency_ns(dev_path: &Path, direction: &str) -> u64 {
    // Prefer the EWMA from the JSON stats — this is actual recent latency
    let json_path = dev_path.join(format!("io_latency_stats_{direction}_json"));
    if let Ok(content) = std::fs::read_to_string(&json_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(ewma) = json["duration_ewma_ns"]["mean"].as_u64() {
                return ewma;
            }
        }
    }
    // Fallback: cumulative mean (not great but better than nothing)
    let path = dev_path.join(format!("io_latency_{direction}"));
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(0)
}

/// io_done is JSON: {"read": {"sb": N, "user": N, ...}, "write": {...}}
/// Values are bytes. Returns (total_read, total_write, read_by_type, write_by_type).
fn read_io_done(dev_path: &Path) -> (u64, u64, HashMap<String, u64>, HashMap<String, u64>) {
    let path = dev_path.join("io_done");
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();

    let parse_obj = |obj: &serde_json::Value| -> HashMap<String, u64> {
        obj.as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_u64().map(|val| (k.clone(), val)))
                    .collect()
            })
            .unwrap_or_default()
    };

    let read_map = parse_obj(&json["read"]);
    let write_map = parse_obj(&json["write"]);
    let read_total: u64 = read_map.values().sum();
    let write_total: u64 = write_map.values().sum();

    (read_total, write_total, read_map, write_map)
}

fn read_io_errors(dev_path: &Path) -> u64 {
    let path = dev_path.join("io_errors");
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut total = 0u64;
    for line in content.lines() {
        if let Some(val) = line.split_whitespace().last() {
            total += val.parse::<u64>().unwrap_or(0);
        }
    }
    total
}

fn read_options(sysfs: &Path) -> HashMap<String, String> {
    let dir = sysfs.join("options");
    let mut map = HashMap::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return map,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Ok(val) = std::fs::read_to_string(entry.path()) {
            map.insert(name, val.trim().to_string());
        }
    }
    map
}

fn read_background(sysfs: &Path, mount_point: &str) -> Vec<(String, String)> {
    let dir = sysfs.join("internal");
    let opts = sysfs.join("options");
    // Fixed order for stable rendering
    let mut result = Vec::new();

    // Reconcile status (replication health)
    result.push(("reconcile".to_string(), read_reconcile_status(mount_point)));

    // Only show background ops that actually have a sysfs toggle
    for prefix in ["rebalance", "copygc"] {
        let enabled_path = opts.join(format!("{prefix}_enabled"));
        let status_path = dir.join(format!("{prefix}_status"));

        // Skip if the option doesn't exist on this kernel
        if !enabled_path.exists() {
            continue;
        }

        let enabled = std::fs::read_to_string(&enabled_path)
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

        let status = std::fs::read_to_string(&status_path)
            .unwrap_or_else(|_| "n/a".into())
            .trim()
            .lines()
            .next()
            .unwrap_or("n/a")
            .to_string();

        let display = if enabled {
            format!("on — {status}")
        } else {
            "off".into()
        };

        result.push((prefix.to_string(), display));
    }
    result
}

pub fn read_file_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse the "recent" mean from a time_stats file.
/// Format: "  mean:    12 ms    762 us" — we want the second value.
fn read_recent_mean_us(sysfs: &Path, stat_name: &str) -> f64 {
    let path = sysfs.join("time_stats").join(stat_name);
    let content = std::fs::read_to_string(path).unwrap_or_default();

    // Find the "mean:" line under "duration of events"
    let mut in_duration = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("duration of events") {
            in_duration = true;
            continue;
        }
        if trimmed.starts_with("time between events") {
            break;
        }
        if in_duration && trimmed.starts_with("mean:") {
            // "mean:    12 ms    762 us"
            // Split by whitespace, take last two tokens (value + unit) as "recent"
            let tokens: Vec<&str> = trimmed.split_whitespace().collect();
            // tokens: ["mean:", "12", "ms", "762", "us"]
            // Recent is the last value+unit pair
            if tokens.len() >= 4 {
                let val: f64 = tokens[tokens.len() - 2].parse().unwrap_or(0.0);
                let unit = tokens[tokens.len() - 1];
                return to_microseconds(val, unit);
            }
        }
    }
    0.0
}

fn to_microseconds(val: f64, unit: &str) -> f64 {
    match unit {
        "ns" => val / 1000.0,
        "us" => val,
        "ms" => val * 1000.0,
        "s" => val * 1_000_000.0,
        "m" => val * 60_000_000.0,
        "h" => val * 3_600_000_000.0,
        _ => val,
    }
}

/// Read all blocked_* time stats: returns (name, count, recent_mean_us).
fn read_blocked_stats(sysfs: &Path) -> Vec<(String, u64, f64)> {
    let dir = sysfs.join("time_stats");
    let mut result = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return result,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("blocked_") {
            continue;
        }
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut count = 0u64;
        let mut recent_mean_us = 0.0f64;
        let mut in_duration = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("count:") {
                count = trimmed.split_whitespace().nth(1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
            }
            if trimmed.starts_with("duration of events") {
                in_duration = true;
                continue;
            }
            if trimmed.starts_with("time between events") {
                in_duration = false;
            }
            if in_duration && trimmed.starts_with("mean:") {
                let tokens: Vec<&str> = trimmed.split_whitespace().collect();
                if tokens.len() >= 4 {
                    let val: f64 = tokens[tokens.len() - 2].parse().unwrap_or(0.0);
                    let unit = tokens[tokens.len() - 1];
                    recent_mean_us = to_microseconds(val, unit);
                }
            }
        }
        let short_name = name.strip_prefix("blocked_").unwrap_or(&name).to_string();
        result.push((short_name, count, recent_mean_us));
    }
    // Sort: non-zero counts first (by count desc), then alphabetical
    result.sort_by(|a, b| {
        let a_active = a.1 > 0;
        let b_active = b.1 > 0;
        match (a_active, b_active) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => b.1.cmp(&a.1).then(a.0.cmp(&b.0)),
        }
    });
    result
}

/// Read compression stats from compression_stats sysfs file.
/// Returns (compressed_bytes, uncompressed_bytes).
fn read_compression_stats(sysfs: &Path) -> (u64, u64) {
    let path = sysfs.join("compression_stats");
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut compressed = 0u64;
    let mut uncompressed = 0u64;
    for line in content.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        // Format: "zstd    54.5G    58.1G    490k"
        //         "incompressible  224G  224G  212k"
        if tokens.len() >= 3 && tokens[0] != "typetype" {
            compressed += parse_size(tokens[1]);
            uncompressed += parse_size(tokens[2]);
        }
    }
    (compressed, uncompressed)
}

fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    if s == "0" { return 0; }
    let (num_str, mult) = if let Some(n) = s.strip_suffix('k') {
        (n, 1_000u64)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1_000_000)
    } else if let Some(n) = s.strip_suffix('G') {
        (n, 1_000_000_000)
    } else if let Some(n) = s.strip_suffix('T') {
        (n, 1_000_000_000_000)
    } else {
        (s, 1)
    };
    let val: f64 = num_str.parse().unwrap_or(0.0);
    (val * mult as f64) as u64
}

/// Read journal fill from internal/journal_debug.
fn read_journal_fill(sysfs: &Path) -> ((u64, u64), String) {
    let path = sysfs.join("internal").join("journal_debug");
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut dirty = 0u64;
    let mut total = 1u64;
    let mut watermark = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("dirty journal entries:") {
            // Format: "187/32768"
            let val = val.trim();
            let parts: Vec<&str> = val.split('/').collect();
            if parts.len() == 2 {
                dirty = parts[0].trim().parse().unwrap_or(0);
                total = parts[1].trim().parse().unwrap_or(1).max(1);
            }
        } else if let Some(val) = trimmed.strip_prefix("watermark:") {
            watermark = val.trim().to_string();
        }
    }
    ((dirty, total), watermark)
}

/// Parse `bcachefs reconcile status <mount>` into a one-line summary.
fn read_reconcile_status(mount_point: &str) -> String {
    let output = match std::process::Command::new("bcachefs")
        .args(["reconcile", "status", mount_point])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return "n/a".into(),
    };

    // Check scan pending
    let scan_pending: u64 = output
        .lines()
        .find(|l| l.contains("Scan pending"))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Sum all non-zero "pending:" row values
    let pending_work: u64 = output
        .lines()
        .filter(|l| l.trim().starts_with("pending:"))
        .flat_map(|l| l.split_whitespace().skip(1))
        .filter_map(|v| v.parse::<u64>().ok())
        .sum();

    // Detect state from the output
    let state = if output.contains("processing") {
        "processing"
    } else if output.contains("running") {
        "running"
    } else if output.contains("waiting") {
        "idle"
    } else {
        "unrecognized"
    };

    // Extract progress percentage if processing
    let progress = output.lines()
        .find(|l| l.contains('%'))
        .and_then(|l| {
            l.split('%').next()
                .and_then(|s| s.split_whitespace().last())
                .map(|s| format!(" {s}%"))
        })
        .unwrap_or_default();

    if scan_pending == 0 && pending_work == 0 {
        format!("{state}{progress} — healthy")
    } else {
        format!("{state}{progress} — scan:{scan_pending} pending:{pending_work}")
    }
}

/// Per-process I/O snapshot from /proc/<pid>/io.
#[derive(Debug, Clone, Default)]
pub struct ProcessIo {
    pub pid: u32,
    pub name: String,
    pub read_bytes: u64,
    pub write_bytes: u64,
}

/// Read I/O stats for all processes.
pub fn read_all_process_io() -> Vec<ProcessIo> {
    let mut result = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return result,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let pid: u32 = match name_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let io_path = format!("/proc/{pid}/io");
        let content = match std::fs::read_to_string(&io_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut read_bytes = 0u64;
        let mut write_bytes = 0u64;
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("read_bytes: ") {
                read_bytes = val.trim().parse().unwrap_or(0);
            } else if let Some(val) = line.strip_prefix("write_bytes: ") {
                write_bytes = val.trim().parse().unwrap_or(0);
            }
        }
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            .to_string();
        result.push(ProcessIo { pid, name: comm, read_bytes, write_bytes });
    }
    result
}

/// Write a value to a sysfs option file. Returns Ok(()) on success.
pub fn write_option(fs: &BcachefsFs, option: &str, value: &str) -> Result<(), String> {
    let path = fs.sysfs.join("options").join(option);
    std::fs::write(&path, value).map_err(|e| format!("Failed to write {option}: {e}"))
}
