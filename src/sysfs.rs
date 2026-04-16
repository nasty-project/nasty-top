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
    pub devices: Vec<DeviceInfo>,
    pub space_total: u64,
    pub space_used: u64,
    pub options: HashMap<String, String>,
    pub background: HashMap<String, String>,
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
    snap.devices = read_devices(&fs.sysfs);
    snap.options = read_options(&fs.sysfs);
    snap.background = read_background(&fs.sysfs);

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
            if let Ok(val) = content.trim().parse::<u64>() {
                map.insert(name, val);
            }
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

        let (io_read, io_write) = read_io_done(&dev_path);
        let io_errors = read_io_errors(&dev_path);

        devices.push(DeviceInfo {
            index,
            name: dev_name,
            label,
            io_latency_read_ns: read_lat,
            io_latency_write_ns: write_lat,
            io_done_read: io_read,
            io_done_write: io_write,
            io_errors,
        });
    }
    devices.sort_by_key(|d| d.index);
    devices
}

fn read_latency_ns(dev_path: &Path, direction: &str) -> u64 {
    let path = dev_path.join(format!("io_latency_{direction}"));
    let content = std::fs::read_to_string(path).unwrap_or_default();
    // Format: "quantiles (ns):\n  mean: 12345\n" or just a number
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("mean:").or_else(|| trimmed.strip_prefix("avg:")) {
            return val.trim().split_whitespace().next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    content.trim().parse().unwrap_or(0)
}

fn read_io_done(dev_path: &Path) -> (u64, u64) {
    let path = dev_path.join("io_done");
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut read_total = 0u64;
    let mut write_total = 0u64;
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let val: u64 = parts.last().and_then(|v| v.parse().ok()).unwrap_or(0);
            if line.contains("read") {
                read_total += val;
            } else if line.contains("write") {
                write_total += val;
            }
        }
    }
    (read_total, write_total)
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

fn read_background(sysfs: &Path) -> HashMap<String, String> {
    let dir = sysfs.join("internal");
    let mut map = HashMap::new();
    for name in ["rebalance_status", "copygc_status", "journal_debug"] {
        let path = dir.join(name);
        if let Ok(val) = std::fs::read_to_string(&path) {
            map.insert(name.to_string(), val.trim().to_string());
        }
    }
    // Also try top-level trigger files
    for name in [
        "rebalance_enabled",
        "copygc_enabled",
        "gc_gens_pos",
    ] {
        let path = sysfs.join("options").join(name);
        if let Ok(val) = std::fs::read_to_string(&path) {
            map.insert(name.to_string(), val.trim().to_string());
        }
    }
    map
}

pub fn read_file_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Write a value to a sysfs option file. Returns Ok(()) on success.
pub fn write_option(fs: &BcachefsFs, option: &str, value: &str) -> Result<(), String> {
    let path = fs.sysfs.join("options").join(option);
    std::fs::write(&path, value).map_err(|e| format!("Failed to write {option}: {e}"))
}
