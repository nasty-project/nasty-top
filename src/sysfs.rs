//! bcachefs discovery and sysfs reading.

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
    /// Time spent doing IO in milliseconds (from /proc/diskstats field 13).
    pub diskstats_io_ms: u64,
    /// Completed read ops (from /proc/diskstats).
    pub diskstats_reads: u64,
    /// Completed write ops (from /proc/diskstats).
    pub diskstats_writes: u64,
    /// Milliseconds spent servicing reads and writes.
    pub diskstats_read_ms: u64,
    pub diskstats_write_ms: u64,
    /// Instantaneous requests currently in the block layer.
    pub diskstats_in_flight: u64,
    /// Weighted milliseconds doing I/O, used to derive average queue depth.
    pub diskstats_weighted_io_ms: u64,
    /// Whether this snapshot contained a complete parseable diskstats row.
    pub diskstats_valid: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DiskStats {
    reads: u64,
    writes: u64,
    read_ms: u64,
    write_ms: u64,
    in_flight: u64,
    io_ms: u64,
    weighted_io_ms: u64,
    valid: bool,
}

/// Full time_stats entry from JSON.
#[derive(Debug, Clone, Default)]
pub struct TimeStatFull {
    pub name: String,
    pub count: u64,
    pub dur_max_ns: u64,
    pub dur_mean_ns: u64,
    pub dur_recent_ns: u64,
}

/// Snapshot of all metrics for one filesystem at one point in time.
#[derive(Debug, Clone, Default)]
pub struct FsSnapshot {
    pub counters: HashMap<String, u64>,
    /// Key latencies from time_stats "recent" column.
    pub recent_data_read_us: f64,
    pub recent_data_write_us: f64,
    pub recent_btree_read_us: f64,
    pub btree_read_count: u64,
    /// Blocked stats: (name, cumulative_count, recent_mean_us).
    pub blocked_stats: Vec<(String, u64, f64)>,
    /// All time_stats from JSON: full detail per operation.
    pub all_time_stats: Vec<TimeStatFull>,
    pub devices: Vec<DeviceInfo>,
    pub space_total: u64,
    pub space_used: u64,
    pub options: HashMap<String, String>,
    pub background: Vec<(String, String)>,
    /// CPU iowait jiffies (from /proc/stat).
    pub cpu_iowait: u64,
    /// Total CPU jiffies (for computing iowait %).
    pub cpu_total: u64,
    /// Journal fill: (dirty, total) entries.
    pub journal_fill: (u64, u64),
    /// Journal watermark level.
    pub journal_watermark: String,
    /// Host RAM from `/proc/meminfo`.
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    pub kernel_reclaimable_bytes: u64,
    /// Kernel-reported btree-node main buffers for this filesystem. Included
    /// node states vary by module version; this is not all bcachefs memory.
    pub btree_cache_size_bytes: Option<u64>,
}

/// Discover mounted bcachefs filesystems.
/// Scans /sys/fs/bcachefs/ for UUIDs, then matches to mount points from /proc/mounts.
/// Deduplicates by UUID, keeping the first mount (original, not bind mounts).
/// Uses filesystem label for the name if set, otherwise the mount point basename.
pub fn discover() -> Vec<BcachefsFs> {
    // Build a map of mount source -> mount point from /proc/mounts. Legacy
    // multi-device sources register every member so the sysfs lookup can
    // match any live device path.
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let source_to_mount = bcachefs_mounts(&mounts);

    let mut result = Vec::new();

    // Scan /sys/fs/bcachefs/ — each entry is a UUID
    let sysfs_base = Path::new("/sys/fs/bcachefs");
    let entries = match std::fs::read_dir(sysfs_base) {
        Ok(e) => e,
        Err(_) => return result,
    };

    for entry in entries.flatten() {
        let uuid = entry.file_name().to_string_lossy().to_string();
        let sysfs = entry.path();
        if !sysfs.is_dir() {
            continue;
        }

        // Find mount point from the UUID source used by newer bcachefs, or
        // fall back to matching the filesystem's member devices.
        let mount_point = find_mount_for_uuid(&uuid, &sysfs, &source_to_mount).unwrap_or_default();

        // Read fs label from sysfs label file if available
        let label = std::fs::read_to_string(sysfs.join("options/label"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "(none)");

        let fs_name = label.unwrap_or_else(|| {
            Path::new(&mount_point)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| uuid.clone())
        });

        result.push(BcachefsFs {
            uuid,
            mount_point,
            fs_name,
            sysfs,
        });
    }
    result
}

fn bcachefs_mounts(mounts: &str) -> HashMap<String, String> {
    let mut source_to_mount: HashMap<String, String> = HashMap::new();
    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 || parts[2] != "bcachefs" {
            continue;
        }
        // For multi-device, parts[0] is "dev1:dev2:..." — register every
        // member so a UUID's sysfs lookup can match any of them. First
        // mount wins per device — bind mounts appear later in /proc/mounts.
        for dev in parts[0].split(':') {
            source_to_mount
                .entry(dev.to_string())
                .or_insert_with(|| parts[1].to_string());
        }
    }
    source_to_mount
}

/// Find a mount point for a bcachefs UUID from the source in /proc/mounts.
/// Newer bcachefs versions expose multi-device filesystems as
/// `/dev/disk/by-uuid/<uuid>`; older versions expose their member devices.
///
/// bcachefs's sysfs entries for member devices are named `dev-N` where N
/// is the internal device index assigned at format / `device add` time —
/// NOT a contiguous range starting from 0. After device removal or any
/// add/remove cycling the live set can be `dev-2 dev-3 dev-4 dev-5 dev-6`
/// (issue #11): the previous implementation probed `dev-0..64` and
/// `break`ed on the first missing entry, so on any FS without `dev-0` it
/// returned no match → empty mount_point → 0/0 capacity in the top bar
/// and no fallback could recover it (every code path that reads the FS
/// is keyed on the mount point).
///
/// Enumerate the actual `dev-*` entries via `read_dir` so the lookup is
/// correct regardless of how bcachefs numbered the devices.
fn find_mount_for_uuid(
    uuid: &str,
    sysfs: &Path,
    source_to_mount: &HashMap<String, String>,
) -> Option<String> {
    let uuid_source = format!("/dev/disk/by-uuid/{uuid}");
    if let Some(mount_point) = source_to_mount.get(&uuid_source) {
        return Some(mount_point.clone());
    }

    let entries = std::fs::read_dir(sysfs).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("dev-") {
            continue;
        }
        let dev_n = entry.path();
        if !dev_n.is_dir() {
            continue;
        }
        if let Some(dev_name) = read_dev_name(&dev_n) {
            let dev_path = format!("/dev/{dev_name}");
            if let Some(mp) = source_to_mount.get(&dev_path) {
                return Some(mp.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn finds_by_uuid_mount_without_member_device_matching() {
        let mounts = bcachefs_mounts(
            "/dev/disk/by-uuid/6ecff1be-9388-482d-a9fd-f1ff6e29a823 /fs/first bcachefs rw 0 0\n",
        );

        assert_eq!(
            find_mount_for_uuid(
                "6ecff1be-9388-482d-a9fd-f1ff6e29a823",
                Path::new("/nonexistent"),
                &mounts,
            ),
            Some("/fs/first".to_string())
        );
    }

    #[test]
    fn registers_each_legacy_multi_device_source() {
        let mounts = bcachefs_mounts("/dev/sda:/dev/sdb /fs/pool bcachefs rw 0 0\n");

        assert_eq!(mounts.get("/dev/sda").map(String::as_str), Some("/fs/pool"));
        assert_eq!(mounts.get("/dev/sdb").map(String::as_str), Some("/fs/pool"));
    }

    #[test]
    fn falls_back_to_sparse_member_device_entries() {
        let root = std::env::temp_dir().join(format!(
            "nasty-top-sysfs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dev_dir = root.join("dev-7");
        std::fs::create_dir_all(&dev_dir).unwrap();
        symlink("../../devices/virtual/block/sdb", dev_dir.join("block")).unwrap();

        let mounts = bcachefs_mounts("/dev/sda:/dev/sdb /fs/pool bcachefs rw 0 0\n");
        assert_eq!(
            find_mount_for_uuid("example", &root, &mounts),
            Some("/fs/pool".to_string())
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeps_first_mount_for_duplicate_sources() {
        let mounts = bcachefs_mounts(
            "/dev/disk/by-uuid/example /fs/original bcachefs rw 0 0\n\
             /dev/disk/by-uuid/example /fs/bind bcachefs rw 0 0\n",
        );

        assert_eq!(
            mounts.get("/dev/disk/by-uuid/example").map(String::as_str),
            Some("/fs/original")
        );
    }

    #[test]
    fn parses_btree_cache_human_bytes() {
        assert_eq!(parse_human_bytes("4096"), Some(4096));
        assert_eq!(parse_human_bytes("256k"), Some(256 * 1024));
        assert_eq!(parse_human_bytes("1.5M"), Some(1_572_864));
        assert_eq!(parse_human_bytes("unknown"), None);
    }

    #[test]
    fn parses_host_memory_values_as_bytes() {
        let meminfo =
            "MemTotal:       32768 kB\nMemAvailable:   12288 kB\nKReclaimable:    2048 kB\n";
        assert_eq!(
            parse_memory_info(meminfo),
            (32 * 1024 * 1024, 12 * 1024 * 1024, 2 * 1024 * 1024)
        );
    }

    #[test]
    fn parses_diskstats_request_and_queue_fields() {
        let diskstats = "   8       0 sda 100 5 2000 400 50 2 1000 600 3 700 900 0 0 0 0\n";
        assert_eq!(
            parse_diskstats_for(diskstats, "sda"),
            DiskStats {
                reads: 100,
                writes: 50,
                read_ms: 400,
                write_ms: 600,
                in_flight: 3,
                io_ms: 700,
                weighted_io_ms: 900,
                valid: true,
            }
        );
        assert_eq!(parse_diskstats_for(diskstats, "sdb"), DiskStats::default());
        assert_eq!(
            parse_diskstats_for(
                "8 0 sda invalid 5 2000 400 50 2 1000 600 3 700 900\n",
                "sda"
            ),
            DiskStats::default()
        );
    }
}

/// Read the block device name (e.g. "nvme0n1p1") from a bcachefs sysfs dev-N directory.
fn read_dev_name(dev_dir: &Path) -> Option<String> {
    // The "block" symlink points to the block device in sysfs
    std::fs::read_link(dev_dir.join("block"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
}

/// Read all metrics for a filesystem.
pub fn snapshot(fs: &BcachefsFs) -> FsSnapshot {
    let (iowait, cpu_total) = read_cpu_iowait();
    let (journal_fill, journal_watermark) = read_journal_fill(&fs.sysfs);
    let (memory_total_bytes, memory_available_bytes, kernel_reclaimable_bytes) = read_memory_info();

    let (space_total, space_used) = read_fs_space(&fs.mount_point);

    FsSnapshot {
        counters: read_counters(&fs.sysfs),
        recent_data_read_us: read_recent_mean_us(&fs.sysfs, "data_read"),
        recent_data_write_us: read_recent_mean_us(&fs.sysfs, "data_write"),
        recent_btree_read_us: read_recent_mean_us(&fs.sysfs, "btree_node_read"),
        btree_read_count: read_time_stat_count(&fs.sysfs, "btree_node_read"),
        blocked_stats: read_blocked_stats(&fs.sysfs),
        all_time_stats: read_all_time_stats_json(&fs.sysfs),
        devices: read_devices(&fs.sysfs),
        space_total,
        space_used,
        options: read_options(&fs.sysfs),
        background: read_background(&fs.sysfs, &fs.mount_point),
        cpu_iowait: iowait,
        cpu_total,
        journal_fill,
        journal_watermark,
        memory_total_bytes,
        memory_available_bytes,
        kernel_reclaimable_bytes,
        btree_cache_size_bytes: read_file_string(&fs.sysfs.join("btree_cache_size"))
            .and_then(|value| parse_human_bytes(&value)),
    }
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
                content
                    .lines()
                    .find(|l| l.contains("since mount"))
                    .and_then(|l| l.split(':').next_back())
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0)
            });
            map.insert(name, val);
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
        let diskstats = read_diskstats_for(&dev_name);

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
            diskstats_io_ms: diskstats.io_ms,
            diskstats_reads: diskstats.reads,
            diskstats_writes: diskstats.writes,
            diskstats_read_ms: diskstats.read_ms,
            diskstats_write_ms: diskstats.write_ms,
            diskstats_in_flight: diskstats.in_flight,
            diskstats_weighted_io_ms: diskstats.weighted_io_ms,
            diskstats_valid: diskstats.valid,
        });
    }
    // Sort by (label, natural device name) so labeled groups stay together
    // and sd[a-z]+ devices order as sda < sdz < sdaa rather than lexically.
    devices.sort_by(|a, b| {
        a.label
            .cmp(&b.label)
            .then_with(|| natural_key(&a.name).cmp(&natural_key(&b.name)))
    });
    devices
}

/// Tokenize a name into runs of letters and digits for natural ordering.
/// Letter runs compare by (length, lex) so "sda" < "sdz" < "sdaa".
/// Digit runs compare numerically so "nvme0n1" < "nvme10n1".
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum NatToken {
    Letters(usize, String),
    Number(u64),
}

fn natural_key(s: &str) -> Vec<NatToken> {
    let bytes = s.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let is_digit = bytes[i].is_ascii_digit();
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_digit() == is_digit {
            j += 1;
        }
        let slice = &s[i..j];
        tokens.push(if is_digit {
            NatToken::Number(slice.parse().unwrap_or(0))
        } else {
            NatToken::Letters(slice.len(), slice.to_string())
        });
        i = j;
    }
    tokens
}

/// Read per-device recent (EWMA) latency from io_latency_stats_{direction}_json.
/// Falls back to the cumulative io_latency_{direction} if JSON isn't available.
fn read_latency_ns(dev_path: &Path, direction: &str) -> u64 {
    // Prefer the EWMA from the JSON stats — this is actual recent latency
    let json_path = dev_path.join(format!("io_latency_stats_{direction}_json"));
    if let Ok(content) = std::fs::read_to_string(&json_path)
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(ewma) = json["duration_ewma_ns"]["mean"].as_u64()
    {
        return ewma;
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

    // Reconcile — check if enabled first
    let reconcile_enabled = std::fs::read_to_string(opts.join("reconcile_enabled"))
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    if reconcile_enabled {
        result.push(("reconcile".to_string(), read_reconcile_status(mount_point)));
    } else {
        result.push(("reconcile".to_string(), "off".into()));
    }

    // Only show background ops that actually have a sysfs toggle
    for prefix in ["rebalance", "copygc"] {
        let enabled_path = opts.join(format!("{prefix}_enabled"));

        // Skip if the option doesn't exist on this kernel
        if !enabled_path.exists() {
            continue;
        }

        let enabled = std::fs::read_to_string(&enabled_path)
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

        if !enabled {
            result.push((prefix.to_string(), "off".into()));
            continue;
        }

        // Try multiple status file names (varies by kernel version)
        let status_names = [format!("{prefix}_status"), "copy_gc_wait".to_string()];
        let mut status = String::new();
        for name in &status_names {
            let path = dir.join(name);
            if let Ok(content) = std::fs::read_to_string(&path) {
                let running = content
                    .lines()
                    .find(|l| l.trim().starts_with("running:"))
                    .and_then(|l| l.split(':').next_back())
                    .map(|v| v.trim() == "1")
                    .unwrap_or(false);

                status = if running {
                    "working".into()
                } else {
                    "idle".into()
                };
                break;
            }
        }
        if status.is_empty() {
            status = "enabled".into();
        }

        result.push((prefix.to_string(), status));
    }
    result
}

pub fn read_file_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_human_bytes(value: &str) -> Option<u64> {
    let value = value.trim();
    if let Ok(bytes) = value.parse() {
        return Some(bytes);
    }

    let (number, multiplier) = match value.chars().last()? {
        'k' | 'K' => (&value[..value.len() - 1], 1024u64),
        'M' => (&value[..value.len() - 1], 1024u64.pow(2)),
        'G' => (&value[..value.len() - 1], 1024u64.pow(3)),
        'T' => (&value[..value.len() - 1], 1024u64.pow(4)),
        _ => return None,
    };
    number
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite() && *number >= 0.0)
        .map(|number| (number * multiplier as f64) as u64)
}

fn parse_memory_info(content: &str) -> (u64, u64, u64) {
    let mut total = 0;
    let mut available = 0;
    let mut reclaimable = 0;
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let key = fields.next().unwrap_or_default();
        let bytes = fields
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_mul(1024);
        match key {
            "MemTotal:" => total = bytes,
            "MemAvailable:" => available = bytes,
            "KReclaimable:" => reclaimable = bytes,
            _ => {}
        }
    }
    (total, available, reclaimable)
}

fn read_memory_info() -> (u64, u64, u64) {
    parse_memory_info(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default())
}

fn parse_diskstats_for(content: &str, dev_name: &str) -> DiskStats {
    // Fields after major/minor/name follow Documentation/admin-guide/iostats.rst.
    // We need through field 11 (weighted milliseconds doing I/O).
    let Some(fields) = content
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields.len() >= 14 && fields[2] == dev_name)
    else {
        return DiskStats::default();
    };

    let parsed = [3, 6, 7, 10, 11, 12, 13]
        .map(|index| fields[index].parse::<u64>())
        .into_iter()
        .collect::<Result<Vec<_>, _>>();
    let Ok(values) = parsed else {
        return DiskStats::default();
    };
    DiskStats {
        reads: values[0],
        read_ms: values[1],
        writes: values[2],
        write_ms: values[3],
        in_flight: values[4],
        io_ms: values[5],
        weighted_io_ms: values[6],
        valid: true,
    }
}

/// Read per-device request, timing, utilization, and queue statistics.
fn read_diskstats_for(dev_name: &str) -> DiskStats {
    let content = std::fs::read_to_string("/proc/diskstats").unwrap_or_default();
    parse_diskstats_for(&content, dev_name)
}

/// Read CPU iowait from /proc/stat. Returns (iowait_jiffies, total_jiffies).
fn read_cpu_iowait() -> (u64, u64) {
    let content = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    if let Some(line) = content.lines().find(|l| l.starts_with("cpu ")) {
        let fields: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|v| v.parse().ok())
            .collect();
        // fields: user, nice, system, idle, iowait, irq, softirq, steal...
        if fields.len() >= 5 {
            let iowait = fields[4];
            let total: u64 = fields.iter().sum();
            return (iowait, total);
        }
    }
    (0, 0)
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

fn read_time_stat_count(sysfs: &Path, stat_name: &str) -> u64 {
    let json_path = sysfs.join("time_stats_json").join(stat_name);
    if let Ok(content) = std::fs::read_to_string(json_path)
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(count) = json["count"].as_u64()
    {
        return count;
    }

    let text_path = sysfs.join("time_stats").join(stat_name);
    std::fs::read_to_string(text_path)
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("count:")
                .and_then(|value| value.trim().parse().ok())
        })
        .unwrap_or(0)
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

/// Read all time_stats from JSON files.
fn read_all_time_stats_json(sysfs: &Path) -> Vec<TimeStatFull> {
    let dir = sysfs.join("time_stats_json");
    let mut result = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return result,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let json: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let count = json["count"].as_u64().unwrap_or(0);
        if count == 0 {
            continue; // skip entries with zero count
        }
        result.push(TimeStatFull {
            name,
            count,
            dur_max_ns: json["duration_ns"]["max"].as_u64().unwrap_or(0),
            dur_mean_ns: json["duration_ns"]["mean"].as_u64().unwrap_or(0),
            dur_recent_ns: json["duration_ewma_ns"]["mean"].as_u64().unwrap_or(0),
        });
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
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
                count = trimmed
                    .split_whitespace()
                    .nth(1)
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

/// Skip the CONFIG_RUST warning + continuation line that bcachefs CLI
/// prints when the running kernel lacks `CONFIG_RUST`. Most builds put
/// it on stderr (so capturing only stdout is enough), but it's been
/// seen on stdout on at least some builds — defensive filter so the
/// warning can never silently corrupt our parsers.
fn skip_bcachefs_warning(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("WARNING:") || t.starts_with("please alert")
}

/// Read `(total_bytes, used_bytes)` for a mounted bcachefs filesystem.
///
/// Fast path is `statvfs`. Some kernels' bcachefs statvfs implementation
/// returns 0 blocks for multi-device filesystems (issue #12), so on a
/// zero reading we fall back to parsing `bcachefs fs usage` — slower
/// (spawns a process) but authoritative whenever the CLI works.
fn read_fs_space(mount_point: &str) -> (u64, u64) {
    // fsblkcnt_t / c_ulong width varies across targets; the casts are
    // intentionally kept (clippy sees them as redundant on the build host).
    #[allow(clippy::unnecessary_cast)]
    if let Ok(stat) = nix::sys::statvfs::statvfs(mount_point) {
        let total = stat.blocks() as u64 * stat.fragment_size() as u64;
        if total > 0 {
            let avail = stat.blocks_available() as u64 * stat.fragment_size() as u64;
            return (total, total.saturating_sub(avail));
        }
    }
    bcachefs_fs_usage_space(mount_point)
}

/// Fallback for `read_fs_space` when statvfs reports zero. Runs
/// `bcachefs fs usage <mount>` (no `-h` → raw bytes) and pulls the
/// top-level `Size:` / `Used:` lines.
fn bcachefs_fs_usage_space(mount_point: &str) -> (u64, u64) {
    let output = match std::process::Command::new("bcachefs")
        .args(["fs", "usage", mount_point])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return (0, 0),
    };
    let mut total: u64 = 0;
    let mut used: u64 = 0;
    for line in output.lines() {
        if skip_bcachefs_warning(line) {
            continue;
        }
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("Size:") {
            total = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("Used:") {
            used = rest.trim().parse().unwrap_or(0);
        }
    }
    (total, used)
}

/// Parse `bcachefs reconcile status <mount>` into a one-line summary.
fn read_reconcile_status(mount_point: &str) -> String {
    let output = match std::process::Command::new("bcachefs")
        .args(["reconcile", "status", mount_point])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !skip_bcachefs_warning(l))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(_) => return "n/a".into(),
    };

    // Check scan pending
    let scan_pending: u64 = output
        .lines()
        .find(|l| l.contains("Scan pending"))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

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
    let progress = output
        .lines()
        .find(|l| l.contains('%'))
        .and_then(|l| {
            l.split('%')
                .next()
                .and_then(|s| s.split_whitespace().last())
                .map(|s| format!(" {s}%"))
        })
        .unwrap_or_default();

    // Collect non-zero pending categories
    let mut pending_categories: Vec<String> = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() >= 2 && parts[0].ends_with(':') {
            let name = parts[0].trim_end_matches(':');
            if [
                "replicas",
                "checksum",
                "erasure_code",
                "compression",
                "target",
                "pending",
                "stripes",
            ]
            .contains(&name)
            {
                let has_nonzero = parts[1..].iter().any(|v| *v != "0");
                if has_nonzero {
                    pending_categories.push(format!("{name}:{}", parts[1]));
                }
            }
        }
    }

    if state == "processing" {
        if pending_categories.is_empty() {
            format!("working{progress}")
        } else {
            format!("working{progress} — {}", pending_categories.join(" "))
        }
    } else if scan_pending > 0 || !pending_categories.is_empty() {
        format!("idle — pending: {}", pending_categories.join(" "))
    } else {
        "idle".into()
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
        result.push(ProcessIo {
            pid,
            name: comm,
            read_bytes,
            write_bytes,
        });
    }
    result
}

/// Write a value to a sysfs option file. Returns Ok(()) on success.
pub fn write_option(fs: &BcachefsFs, option: &str, value: &str) -> Result<(), String> {
    let path = fs.sysfs.join("options").join(option);
    std::fs::write(&path, value).map_err(|e| format!("Failed to write {option}: {e}"))
}
