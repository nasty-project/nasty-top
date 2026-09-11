//! bcachefs discovery and sysfs reading.

use crate::targets::{
    CopyGcStatus, MemberAllocation, ReconcileStatus, ReconcileWork, TargetConfig,
};
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
    pub member_uuid: Option<String>,
    pub label: Option<String>,
    pub allocation: MemberAllocation,
    pub io_latency_read_ns: u64,
    pub io_latency_write_ns: u64,
    pub io_done_read: u64,
    pub io_done_write: u64,
    /// Per-category breakdown: sb, journal, btree, user, etc.
    pub io_read_by_type: HashMap<String, u64>,
    pub io_write_by_type: HashMap<String, u64>,
    pub io_errors: u64,
    /// Missing/malformed counters are not zero; each named counter is optional.
    pub error_counts: Option<HashMap<String, u64>>,
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
    pub diskstats_discards: Option<u64>,
    /// Not tracked for partitions, even when diskstats contains a zero column.
    pub diskstats_flushes: Option<u64>,
}

impl DeviceInfo {
    pub fn identity(&self) -> String {
        self.member_uuid
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.index, self.name))
    }
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
    discards: Option<u64>,
    flushes: Option<u64>,
}

/// Full time_stats entry from JSON.
#[derive(Debug, Clone, Default)]
pub struct TimeStatFull {
    pub name: String,
    pub count: u64,
    pub dur_max_ns: u64,
    pub dur_mean_ns: u64,
    pub dur_recent_ns: u64,
    pub recent_valid: bool,
}

#[derive(Debug, Clone, Default)]
pub struct JournalState {
    pub entries: Option<(u64, u64)>,
    pub seq: Option<u64>,
    pub seq_ondisk: Option<u64>,
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
    pub journal: JournalState,
    pub diskstats_sampled_at: Option<std::time::Instant>,
    pub collection_started_at: Option<std::time::Instant>,
    /// Host RAM from `/proc/meminfo`.
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    pub kernel_reclaimable_bytes: u64,
    /// Kernel-reported btree-node main buffers for this filesystem. Included
    /// node states vary by module version; this is not all bcachefs memory.
    pub btree_cache_size_bytes: Option<u64>,
    /// Accounted on-disk btree sectors converted to bytes, already replicated.
    pub btree_disk_bytes: Option<u64>,
    pub target_configs: Vec<TargetConfig>,
    pub reconcile: ReconcileStatus,
    pub copygc: CopyGcStatus,
    /// Slow allocator tables are refreshed at most every ten seconds unless
    /// membership or runtime options change. Never reused across filesystems.
    pub allocation_sampled_at: Option<std::time::Instant>,
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
    fn collects_member_capacity_and_refreshes_cached_usage_on_state_change() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(std::env::temp_dir().join(
            format!("nasty-top-allocation-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()),
        ));
        for dir in [
            "options",
            "internal",
            "dev-31",
            "backing",
            "time_stats",
            "time_stats_json",
        ] {
            std::fs::create_dir_all(fixture.0.join(dir)).unwrap();
        }
        for (name, value) in [
            ("options/metadata_target", "ssd.nvme"),
            ("options/metadata_replicas", "3"),
            ("options/reconcile_enabled", "0"),
            ("dev-31/dev", "nvme0n1p3"),
            ("dev-31/uuid", "test-member-uuid"),
            ("dev-31/io_errors", include_str!("fixtures/io-errors.txt")),
            ("dev-31/label", "ssd.nvme.31"),
            ("dev-31/state", "[rw] ro failed spare"),
            ("dev-31/durability", "1"),
            ("dev-31/data_allowed", "journal,btree,user"),
            ("dev-31/bucket_size", "1.00M"),
            ("dev-31/nbuckets", "1000"),
            ("backing/size", "99999999999"),
            (
                "time_stats/journal_flush_write",
                "count: 4\nduration of events\n  mean: 2 s 100 ms\ntime between events\n  mean: 2 s 5 s\n",
            ),
            (
                "time_stats_json/journal_flush_seq",
                "{\"count\":0,\"duration_ewma_ns\":{\"mean\":0}}",
            ),
            (
                "dev-31/alloc_debug",
                include_str!("fixtures/member-alloc-debug.txt"),
            ),
            (
                "internal/alloc_debug",
                include_str!("fixtures/fs-alloc-debug.txt"),
            ),
        ] {
            std::fs::write(fixture.0.join(name), value).unwrap();
        }
        symlink("../backing", fixture.0.join("dev-31/block")).unwrap();
        let fs = BcachefsFs {
            uuid: "fixture".into(),
            mount_point: "/".into(),
            fs_name: "fixture".into(),
            sysfs: fixture.0.clone(),
        };
        let first = snapshot(&fs);
        assert_eq!(first.devices[0].identity(), "test-member-uuid");
        assert_eq!(first.devices[0].io_errors, 115); // lifetime, not lifetime + since-reset
        let flush = first
            .all_time_stats
            .iter()
            .find(|s| s.name == "journal_flush_write")
            .unwrap();
        assert_eq!(flush.dur_recent_ns, 100_000_000);
        assert!(flush.recent_valid);
        assert!(
            first
                .all_time_stats
                .iter()
                .any(|s| s.name == "journal_flush_seq" && s.count == 0)
        );
        assert_eq!(first.devices[0].allocation.state.as_deref(), Some("rw"));
        assert_eq!(first.devices[0].allocation.online, Some(true));
        assert_eq!(first.devices[0].allocation.capacity_bytes, Some(1000 << 20)); // member, not entire backing disk
        assert_eq!(first.devices[0].allocation.free_bytes, Some(100 << 20));
        assert_eq!(first.btree_disk_bytes, Some(7_821_903_360 * 512));
        std::fs::write(fixture.0.join("dev-31/nbuckets"), "2000").unwrap();
        std::fs::write(fixture.0.join("internal/alloc_debug"), "btree 1\n").unwrap();
        let cached = snapshot_after(&fs, Some(&first));
        assert_eq!(
            cached.devices[0].allocation.capacity_bytes,
            Some(1000 << 20)
        );
        assert_eq!(cached.btree_disk_bytes, first.btree_disk_bytes);
        std::fs::write(fixture.0.join("dev-31/state"), "rw [ro] failed spare").unwrap();
        let fresh = snapshot_after(&fs, Some(&cached));
        assert_eq!(fresh.devices[0].allocation.state.as_deref(), Some("ro"));
        assert_eq!(fresh.devices[0].allocation.capacity_bytes, Some(2000 << 20));
        assert_eq!(fresh.btree_disk_bytes, Some(512));
        std::fs::remove_file(fixture.0.join("dev-31/alloc_debug")).unwrap();
        let mut expired = fresh;
        expired.allocation_sampled_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(11));
        let missing = snapshot_after(&fs, Some(&expired));
        assert_eq!(missing.devices[0].allocation.free_bytes, None); // never reuse stale success after a failed refresh
    }

    #[test]
    fn reconcile_keeps_metadata_backlog_and_does_not_treat_parent_wait_as_idle() {
        let status = parse_reconcile_status(include_str!("fixtures/reconcile-status.txt"));
        assert_eq!(status.state.as_deref(), Some("working"));
        assert_eq!(
            status.metadata_pending("target"),
            parse_human_bytes("1.16T")
        );
        assert_eq!(status.metadata_pending("replicas"), Some(0));
        assert!(
            status
                .summary()
                .contains("target:data=124.00 GiB meta=1.16 TiB")
        );
        assert_eq!(status.scan_pending, Some(0));
    }

    #[test]
    fn reconcile_handles_metadata_only_reordered_and_partial_columns() {
        let status = parse_reconcile_status(
            "Scan pending: 2\nmetadata data\ntarget: 1.16T 0\nprocessing 12.5%\n",
        );
        assert_eq!(
            status.metadata_pending("target"),
            parse_human_bytes("1.16T")
        );
        assert!(status.summary().contains("working 12.5%"));
        assert!(status.summary().contains("scans:2"));
        assert!(status.summary().contains("target:data=0 B meta=1.16 TiB"));
        let partial = parse_reconcile_status("data metadata\ntarget: 124G\n");
        assert_eq!(partial.metadata_pending("target"), None);
        assert!(partial.summary().contains("meta=?"));
        for content in [
            "",
            "permission denied",
            "WARNING: no kernel support\nplease alert upstream",
        ] {
            assert_eq!(parse_reconcile_status(content).summary(), "n/a");
        }
    }

    #[test]
    fn gc_pressure_uses_only_signed_device_waits_not_global_wait() {
        let status = parse_copygc_status(include_str!("fixtures/copy-gc-wait.txt"));
        assert_eq!(status.running, Some(true));
        assert_eq!(status.needs_gc.get("nvme1n1p3"), Some(&true));
        assert_eq!(status.needs_gc.get("nvme2n1"), Some(&false));
        assert!(!status.needs_gc.contains_key("Currently waiting for"));
        assert!(!status.needs_gc.contains_key("unreported"));
        let truncated = parse_copygc_status(
            "running: invalid\nCurrently calculated wait:\n dev-2: 0\n dev-3: -\n",
        );
        assert_eq!(truncated.running, None);
        assert_eq!(truncated.needs_gc.get("dev-2"), Some(&true));
        assert!(!truncated.needs_gc.contains_key("dev-3"));
    }

    #[test]
    fn allocator_units_distinguish_free_buckets_live_sectors_and_replica_footprint() {
        assert_eq!(
            parse_btree_disk_bytes(include_str!("fixtures/fs-alloc-debug.txt")),
            Some(7_821_903_360 * 512)
        );
        assert_eq!(parse_btree_disk_bytes("btree invalid\n"), None);
        assert_eq!(parse_btree_disk_bytes("btree 18446744073709551615\n"), None);
        let mut allocation = MemberAllocation::default();
        parse_member_usage(
            include_str!("fixtures/member-alloc-debug.txt"),
            Some(1 << 20),
            &mut allocation,
        );
        assert_eq!(allocation.free_bytes, Some(100 << 20));
        assert_eq!(allocation.btree_bytes, Some(900_000 * 512));
        assert_eq!(
            allocation.fragmented_bytes,
            Some((124_000 + 109_600 + 12_400) * 512)
        );
        let mut unknown = MemberAllocation::default();
        parse_member_usage(
            "buckets sectors fragmented\nfree 100 0 0\nbtree 500 900000 124000\n",
            None,
            &mut unknown,
        );
        assert_eq!(unknown.free_bytes, None);
        assert_eq!(unknown.btree_bytes, Some(900_000 * 512));
        assert_eq!(unknown.fragmented_bytes, None); // table did not finish
    }

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
    fn error_parser_preserves_categories_without_double_counting_reset_section() {
        let counts = parse_io_errors(include_str!("fixtures/io-errors.txt")).unwrap();
        assert_eq!(counts["read"], 100);
        assert_eq!(counts["write"], 12);
        assert_eq!(counts["checksum"], 1);
        assert_eq!(counts["flush"], 2);
        assert_eq!(counts.len(), 4);
        let legacy = parse_io_errors("read 5\nwrite 2\ncsum 1\n").unwrap();
        assert_eq!(legacy["checksum"], 1);
        assert!(!legacy.contains_key("flush"));
        for invalid in [
            "",
            "IO errors since filesystem creation\n",
            "read: bad\n",
            "read: 3\nwrite: bad\n",
        ] {
            assert!(parse_io_errors(invalid).is_none(), "accepted {invalid}");
        }
    }

    #[test]
    fn journal_and_time_stats_keep_unknown_distinct_from_zero() {
        let (journal, watermark) = parse_journal_state(
            "dirty journal entries: 0/100\nseq: 19\nseq_ondisk: 18\nwatermark: stripe\n",
        );
        assert_eq!(journal.entries, Some((0, 100)));
        assert_eq!(journal.seq, Some(19));
        assert_eq!(journal.seq_ondisk, Some(18));
        assert_eq!(watermark, "stripe");
        for invalid in [
            "",
            "dirty journal entries: ?/100\n",
            "dirty journal entries: 1/0\n",
            "dirty journal entries: 101/100\n",
        ] {
            assert_eq!(parse_journal_state(invalid).0.entries, None);
        }
        assert!(parse_time_stat_text("test", "count: ?\n").is_none());
        let count_only = parse_time_stat_text("test", "count: 0\n").unwrap();
        assert_eq!(count_only.count, 0);
        assert!(!count_only.recent_valid);
        for mean in ["NaN ms", "-2 ms", "12 unexpected"] {
            let stat = parse_time_stat_text(
                "test",
                &format!("count: 1\nduration of events\nmean: 4 ms {mean}\n"),
            )
            .unwrap();
            assert!(!stat.recent_valid);
        }
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
                discards: Some(0),
                flushes: None,
            }
        );
        assert_eq!(parse_diskstats_for(diskstats, "sdb"), DiskStats::default());
        let extended = parse_diskstats_for(
            "8 0 sda 100 0 0 400 50 0 0 600 3 700 900 7 0 0 1 8 2",
            "sda",
        );
        assert_eq!(extended.discards, Some(7));
        assert_eq!(extended.flushes, Some(8));
        let old = parse_diskstats_for("8 0 sda 100 0 0 400 50 0 0 600 3 700 900", "sda");
        assert!(old.valid);
        assert_eq!(old.discards, None);
        assert_eq!(old.flushes, None);
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
    snapshot_after(fs, None)
}

pub fn snapshot_after(fs: &BcachefsFs, previous: Option<&FsSnapshot>) -> FsSnapshot {
    let collection_started_at = Some(std::time::Instant::now());
    let (iowait, cpu_total) = read_cpu_iowait();
    let (journal, journal_watermark) = read_journal_state(&fs.sysfs);
    let (memory_total_bytes, memory_available_bytes, kernel_reclaimable_bytes) = read_memory_info();

    let (space_total, space_used) = read_fs_space(&fs.mount_point);
    let options = read_options(&fs.sysfs);
    let diskstats = std::fs::read_to_string("/proc/diskstats").unwrap_or_default();
    let diskstats_sampled_at = Some(std::time::Instant::now());
    let mut devices = read_devices(&fs.sysfs, &diskstats);
    let reuse_allocation = previous.filter(|previous| {
        previous.options == options
            && previous
                .allocation_sampled_at
                .is_some_and(|time| time.elapsed().as_secs() < 10)
            && previous.devices.len() == devices.len()
            && devices.iter().all(|d| {
                previous.devices.iter().any(|p| {
                    p.index == d.index
                        && p.name == d.name
                        && p.member_uuid == d.member_uuid
                        && p.label == d.label
                        && p.allocation.state == d.allocation.state
                        && p.allocation.online == d.allocation.online
                        && p.allocation.durability == d.allocation.durability
                        && p.allocation.data_allowed == d.allocation.data_allowed
                })
            })
    });
    for device in &mut devices {
        if let Some(previous) = reuse_allocation {
            device.allocation = previous
                .devices
                .iter()
                .find(|d| d.index == device.index)
                .unwrap()
                .allocation
                .clone();
        } else {
            let path = fs.sysfs.join(format!("dev-{}", device.index));
            read_member_usage(&path, &mut device.allocation);
        }
    }
    let copygc = read_file_string(&fs.sysfs.join("internal/copy_gc_wait"))
        .or_else(|| read_file_string(&fs.sysfs.join("internal/copygc_status")))
        .map(|s| parse_copygc_status(&s))
        .unwrap_or_default();
    let reconcile = if options.get("reconcile_enabled").is_some_and(|v| v == "1") {
        read_reconcile_status(&fs.mount_point)
    } else {
        ReconcileStatus {
            state: options
                .get("reconcile_enabled")
                .filter(|v| *v == "0")
                .map(|_| "off".into()),
            ..Default::default()
        }
    };
    let target_configs = ["metadata", "foreground", "background", "promote"]
        .into_iter()
        .filter_map(|role| {
            let target = options.get(&format!("{role}_target"))?.clone();
            let device_name = if target.starts_with('/') {
                std::fs::canonicalize(&target)
                    .ok()
                    .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            } else {
                None
            };
            Some(TargetConfig {
                role,
                target,
                device_name,
            })
        })
        .collect();
    let background = read_background(&fs.sysfs, &reconcile, &copygc);

    FsSnapshot {
        counters: read_counters(&fs.sysfs),
        recent_data_read_us: read_recent_mean_us(&fs.sysfs, "data_read"),
        recent_data_write_us: read_recent_mean_us(&fs.sysfs, "data_write"),
        recent_btree_read_us: read_recent_mean_us(&fs.sysfs, "btree_node_read"),
        btree_read_count: read_time_stat_count(&fs.sysfs, "btree_node_read"),
        blocked_stats: read_blocked_stats(&fs.sysfs),
        all_time_stats: read_all_time_stats_json(&fs.sysfs),
        devices,
        space_total,
        space_used,
        options,
        background,
        cpu_iowait: iowait,
        cpu_total,
        journal_fill: journal.entries.unwrap_or((0, 0)),
        journal_watermark,
        journal,
        diskstats_sampled_at,
        collection_started_at,
        memory_total_bytes,
        memory_available_bytes,
        kernel_reclaimable_bytes,
        btree_cache_size_bytes: read_file_string(&fs.sysfs.join("btree_cache_size"))
            .and_then(|value| parse_human_bytes(&value)),
        btree_disk_bytes: match reuse_allocation {
            Some(previous) => previous.btree_disk_bytes,
            None => read_file_string(&fs.sysfs.join("internal/alloc_debug"))
                .and_then(|s| parse_btree_disk_bytes(&s)),
        },
        allocation_sampled_at: reuse_allocation
            .and_then(|s| s.allocation_sampled_at)
            .or_else(|| Some(std::time::Instant::now())),
        target_configs,
        reconcile,
        copygc,
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

fn parse_btree_disk_bytes(content: &str) -> Option<u64> {
    content.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == "btree").then_some(())?;
        fields.next()?.parse::<u64>().ok()?.checked_mul(512)
    })
}

fn parse_copygc_status(content: &str) -> CopyGcStatus {
    let mut status = CopyGcStatus::default();
    let mut in_devices = false;
    for line in content.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("running:") {
            status.running = match value.trim() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            };
        }
        if line == "Currently calculated wait:" {
            in_devices = true;
            continue;
        }
        if in_devices {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if let Some(magnitude) = parse_human_bytes(value.strip_prefix('-').unwrap_or(value)) {
                status
                    .needs_gc
                    .insert(name.trim().into(), value.starts_with('-') || magnitude == 0);
            }
        }
    }
    status
}

/// dev-N/alloc_debug has a raw buckets/sectors/fragmented table. Free space is
/// free *buckets* times bucket size, not its (usually zero) live-sectors column.
fn parse_member_usage(content: &str, bucket_bytes: Option<u64>, allocation: &mut MemberAllocation) {
    let mut in_table = false;
    let mut fragmented = Some(0u64);
    for line in content.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields == ["buckets", "sectors", "fragmented"] {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if fields.first() == Some(&"capacity") {
            allocation.fragmented_bytes = fragmented;
            break;
        }
        if fields.is_empty() {
            continue;
        }
        if fields.len() != 4 {
            break; // Different format or truncated table: no total fragment count.
        }
        let buckets = fields[1].parse::<u64>().ok();
        let sectors = fields[2]
            .parse::<u64>()
            .ok()
            .and_then(|s| s.checked_mul(512));
        let frag = fields[3]
            .parse::<u64>()
            .ok()
            .and_then(|s| s.checked_mul(512));
        fragmented = fragmented
            .zip(frag)
            .and_then(|(total, value)| total.checked_add(value));
        match fields[0] {
            "free" => {
                allocation.free_bytes = buckets
                    .zip(bucket_bytes)
                    .and_then(|(n, size)| n.checked_mul(size))
            }
            "btree" => allocation.btree_bytes = sectors,
            _ => {}
        }
    }
}

fn read_member_usage(path: &Path, allocation: &mut MemberAllocation) {
    let bucket_bytes = read_file_string(&path.join("bucket_size"))
        .and_then(|s| parse_human_bytes(&s))
        .filter(|n| *n > 0);
    let nbuckets = read_file_string(&path.join("nbuckets")).and_then(|s| s.parse::<u64>().ok());
    allocation.capacity_bytes = nbuckets
        .zip(bucket_bytes)
        .and_then(|(n, size)| n.checked_mul(size));
    if let Some(content) = read_file_string(&path.join("alloc_debug")) {
        parse_member_usage(&content, bucket_bytes, allocation);
    }
}

fn read_member_allocation(path: &Path) -> MemberAllocation {
    let state = read_file_string(&path.join("state")).and_then(|s| {
        let value = s
            .split_whitespace()
            .find_map(|v| v.strip_prefix('[')?.strip_suffix(']'))
            .unwrap_or(&s);
        ["rw", "ro", "failed", "spare"]
            .contains(&value)
            .then(|| value.to_string())
    });
    let data_allowed = read_file_string(&path.join("data_allowed")).and_then(|s| {
        if s == "none" || s == "(none)" {
            return Some(Vec::new());
        }
        let types: Vec<String> = s
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        types
            .iter()
            .all(|t| ["journal", "btree", "user"].contains(&t.as_str()))
            .then_some(types)
    });
    MemberAllocation {
        state,
        data_allowed,
        durability: read_file_string(&path.join("durability")).and_then(|s| s.parse().ok()),
        online: match std::fs::read_link(path.join("block")) {
            Ok(_) => Some(path.join("block").exists()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
            Err(_) => None,
        },
        ..Default::default()
    }
}

fn read_devices(sysfs: &Path, diskstats_content: &str) -> Vec<DeviceInfo> {
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
        let error_counts =
            read_file_string(&dev_path.join("io_errors")).and_then(|s| parse_io_errors(&s));
        let io_errors = error_counts
            .as_ref()
            .map(|counts| {
                counts
                    .values()
                    .fold(0u64, |total, n| total.saturating_add(*n))
            })
            .unwrap_or(0);
        let mut diskstats = parse_diskstats_for(diskstats_content, &dev_name);
        if dev_path.join("block/partition").exists() {
            diskstats.flushes = None;
        }

        devices.push(DeviceInfo {
            index,
            name: dev_name,
            member_uuid: read_file_string(&dev_path.join("uuid")),
            label,
            allocation: read_member_allocation(&dev_path),
            io_latency_read_ns: read_lat,
            io_latency_write_ns: write_lat,
            io_done_read: io_read,
            io_done_write: io_write,
            io_read_by_type,
            io_write_by_type,
            io_errors,
            error_counts,
            diskstats_io_ms: diskstats.io_ms,
            diskstats_reads: diskstats.reads,
            diskstats_writes: diskstats.writes,
            diskstats_read_ms: diskstats.read_ms,
            diskstats_write_ms: diskstats.write_ms,
            diskstats_in_flight: diskstats.in_flight,
            diskstats_weighted_io_ms: diskstats.weighted_io_ms,
            diskstats_valid: diskstats.valid,
            diskstats_discards: diskstats.discards,
            diskstats_flushes: diskstats.flushes,
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

fn parse_io_errors(content: &str) -> Option<HashMap<String, u64>> {
    let mut counts = HashMap::new();
    let mut since_reset = false;
    for line in content.lines() {
        let line = line.trim();
        if line == "IO errors since filesystem creation" {
            // Current upstream prints lifetime and since-reset copies. Use
            // only lifetime counts, never add/overwrite them with the latter.
            counts.insert("flush".into(), 0);
            continue;
        }
        if line.starts_with("IO errors since ") {
            since_reset = true;
            continue;
        }
        if line.starts_with("Flush errors (device not honoring FUA/flush):") {
            counts.insert(
                "flush".into(),
                line.split_whitespace().last()?.parse().ok()?,
            );
            continue;
        }
        if since_reset || line.is_empty() {
            continue;
        }
        let mut fields: Vec<_> = line.split_whitespace().collect();
        let value = fields.pop()?.parse::<u64>().ok()?;
        if fields.is_empty() {
            continue;
        }
        let name = fields.join(" ").trim_end_matches(':').to_lowercase();
        let name = match name.as_str() {
            "csum" => "checksum".into(),
            _ => name,
        };
        counts.insert(name, value);
    }
    counts.keys().any(|key| key != "flush").then_some(counts)
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

fn read_background(
    sysfs: &Path,
    reconcile: &ReconcileStatus,
    copygc: &CopyGcStatus,
) -> Vec<(String, String)> {
    let dir = sysfs.join("internal");
    let opts = sysfs.join("options");
    // Fixed order for stable rendering
    let mut result = Vec::new();

    result.push(("reconcile".to_string(), reconcile.summary()));

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

        if prefix == "copygc" {
            let status = match copygc.running {
                Some(true) => "working",
                Some(false) => "idle",
                None => "enabled (status unknown)",
            };
            result.push((prefix.into(), status.into()));
            continue;
        }

        // Try multiple status file names (varies by kernel version)
        let status_names = [format!("{prefix}_status")];
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
        'P' => (&value[..value.len() - 1], 1024u64.pow(5)),
        _ => return None,
    };
    number
        .parse::<f64>()
        .ok()
        .filter(|number| {
            number.is_finite() && *number >= 0.0 && *number * (multiplier as f64) < u64::MAX as f64
        })
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
        discards: fields.get(14).and_then(|s| s.parse().ok()),
        flushes: fields.get(18).and_then(|s| s.parse().ok()),
    }
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

fn parse_time_stat_text(name: &str, content: &str) -> Option<TimeStatFull> {
    let count = content
        .lines()
        .find_map(|l| l.trim().strip_prefix("count:")?.trim().parse().ok())?;
    let mut stat = TimeStatFull {
        name: name.into(),
        count,
        ..Default::default()
    };
    let ns = |value: &str, unit: &str| -> Option<u64> {
        let value: f64 = value.parse().ok()?;
        let scale = match unit {
            "ns" => 1.0,
            "us" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => return None,
        };
        let n = value * scale;
        (n.is_finite() && n >= 0.0 && n < u64::MAX as f64).then_some(n as u64)
    };
    let mut in_duration = false;
    for line in content.lines() {
        let line = line.trim();
        if line == "duration of events" {
            in_duration = true;
        }
        if line == "time between events" {
            break;
        }
        if !in_duration {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 3 && fields[0] == "max:" {
            stat.dur_max_ns = ns(fields[1], fields[2]).unwrap_or(0);
        }
        if fields.len() >= 3 && fields[0] == "mean:" {
            stat.dur_mean_ns = ns(fields[1], fields[2]).unwrap_or(0);
            if fields.len() == 5
                && let Some(recent) = ns(fields[3], fields[4])
            {
                stat.dur_recent_ns = recent;
                stat.recent_valid = true;
            }
        }
    }
    Some(stat)
}

/// Read all time_stats from JSON files, with text fallbacks for journal diagnostics.
fn read_all_time_stats_json(sysfs: &Path) -> Vec<TimeStatFull> {
    let dir = sysfs.join("time_stats_json");
    let mut result = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let json: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(count) = json["count"].as_u64() else {
            continue;
        };
        result.push(TimeStatFull {
            name,
            count,
            dur_max_ns: json["duration_ns"]["max"].as_u64().unwrap_or(0),
            dur_mean_ns: json["duration_ns"]["mean"].as_u64().unwrap_or(0),
            dur_recent_ns: json["duration_ewma_ns"]["mean"].as_u64().unwrap_or(0),
            recent_valid: json["duration_ewma_ns"]["mean"].as_u64().is_some(),
        });
    }
    // Older modules expose text only. Keep explicit zero counts so absence of
    // events is distinguishable from an unavailable stat.
    for name in [
        "journal_flush_write",
        "journal_noflush_write",
        "journal_flush_seq",
        "journal_pin_flush_btree",
        "journal_pin_flush_key_cache",
    ] {
        if !result.iter().any(|s| s.name == name)
            && let Some(content) = read_file_string(&sysfs.join("time_stats").join(name))
            && let Some(stat) = parse_time_stat_text(name, &content)
        {
            result.push(stat);
        }
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
        let mut count = None;
        let mut recent_mean_us = 0.0f64;
        let mut in_duration = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("count:") {
                count = trimmed
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok());
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
        if let Some(count) = count {
            result.push((short_name, count, recent_mean_us));
        }
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

/// Dirty-entry occupancy is not on-disk journal-space utilization.
fn read_journal_state(sysfs: &Path) -> (JournalState, String) {
    let path = sysfs.join("internal").join("journal_debug");
    let content = std::fs::read_to_string(path).unwrap_or_default();
    parse_journal_state(&content)
}

fn parse_journal_state(content: &str) -> (JournalState, String) {
    let mut journal = JournalState::default();
    let mut watermark = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("dirty journal entries:") {
            // Format: "187/32768"
            let val = val.trim();
            let parts: Vec<&str> = val.split('/').collect();
            if parts.len() == 2
                && let (Ok(dirty), Ok(total)) = (
                    parts[0].trim().parse::<u64>(),
                    parts[1].trim().parse::<u64>(),
                )
                && total > 0
                && dirty <= total
            {
                journal.entries = Some((dirty, total));
            }
        } else if let Some(val) = trimmed.strip_prefix("watermark:") {
            watermark = val.trim().to_string();
        } else if let Some(val) = trimmed.strip_prefix("seq:") {
            journal.seq = val.trim().parse().ok();
        } else if let Some(val) = trimmed.strip_prefix("seq_ondisk:") {
            journal.seq_ondisk = val.trim().parse().ok();
        }
    }
    (journal, watermark)
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

/// Retain both data and metadata work; formatting belongs to the status view.
fn read_reconcile_status(mount_point: &str) -> ReconcileStatus {
    let output = match std::process::Command::new("bcachefs")
        .args(["reconcile", "status", mount_point])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !skip_bcachefs_warning(l))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return ReconcileStatus::default(),
    };
    parse_reconcile_status(&output)
}

fn parse_reconcile_status(output: &str) -> ReconcileStatus {
    let mut status = ReconcileStatus::default();
    let mut columns = None;
    for line in output.lines() {
        if skip_bcachefs_warning(line) {
            continue;
        }
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("Scan pending:") {
            status.scan_pending = value.trim().parse().ok();
        }
        if trimmed.starts_with("processing ") || trimmed.starts_with("running") {
            status.state = Some("working".into());
        } else if trimmed.starts_with("waiting") || trimmed == "idle" {
            status.state = Some("idle".into());
        }
        if let Some((prefix, _)) = trimmed.split_once('%')
            && let Some(value) = prefix.split_whitespace().last()
            && value
                .parse::<f64>()
                .is_ok_and(|n| n.is_finite() && (0.0..=100.0).contains(&n))
        {
            status.progress = Some(format!("{value}%"));
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if let (Some(data), Some(metadata)) = (
            parts.iter().position(|s| *s == "data"),
            parts.iter().position(|s| *s == "metadata"),
        ) {
            columns = Some((data + 1, metadata + 1));
            continue;
        }
        let Some(name) = parts.first().and_then(|s| s.strip_suffix(':')) else {
            continue;
        };
        if [
            "replicas",
            "checksum",
            "erasure_code",
            "compression",
            "target",
            "high_priority",
            "pending",
            "stripes",
        ]
        .contains(&name)
        {
            let (data_bytes, metadata_bytes) = columns.map_or((None, None), |(data, metadata)| {
                (
                    parts.get(data).and_then(|s| parse_human_bytes(s)),
                    parts.get(metadata).and_then(|s| parse_human_bytes(s)),
                )
            });
            status.work.push(ReconcileWork {
                category: name.into(),
                data_bytes,
                metadata_bytes,
            });
        }
    }
    // An output containing just the recognized table is still useful. An
    // empty/failed/unknown command must not masquerade as a healthy idle FS.
    if status.state.is_none() && (status.scan_pending.is_some() || !status.work.is_empty()) {
        status.state = Some("status unknown".into());
    }
    status
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
