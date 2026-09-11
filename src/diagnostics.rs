//! Stateful advisor evidence. All time is supplied by the caller so sampling
//! gaps, expiry and recovery can be tested without sleeping.

use crate::sysfs::{DeviceInfo, FsSnapshot};
use crate::targets::Severity;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

pub const WINDOW: Duration = Duration::from_secs(60);
pub const IO_STALL_AFTER: Duration = Duration::from_secs(30);
const MAX_RECENT_FINDINGS: usize = 128;
const SLOW_JOURNAL_NS: u64 = 200_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Observed,
    PossibleCause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Active,
    Resolved,
    Stale,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub id: String,
    pub severity: Severity,
    pub confidence: Confidence,
    pub summary: String,
    pub evidence: Vec<String>,
    pub action: String,
    pub first_seen: Instant,
    pub last_seen: Instant,
    pub status_since: Instant,
    pub status: Status,
}

struct Observation {
    severity: Severity,
    confidence: Confidence,
    summary: String,
    evidence: Vec<String>,
    action: String,
}

impl Observation {
    fn observed(summary: String, evidence: Vec<String>, action: &str) -> Self {
        Self {
            severity: Severity::Warning,
            confidence: Confidence::Observed,
            summary,
            evidence,
            action: action.into(),
        }
    }
}

struct Bucket {
    start: Instant,
    end: Instant,
    events: u64,
    recent_ns: Option<u64>,
}

#[derive(Default)]
struct CounterWindow {
    previous: Option<(Instant, u64)>,
    buckets: VecDeque<Bucket>,
}

#[derive(Clone, Copy)]
struct CounterEvidence {
    events: u64,
    seconds: f64,
    recent_ns: Option<u64>,
}

impl CounterEvidence {
    fn slow(self) -> bool {
        self.events > 0 && self.recent_ns.is_some_and(|n| n >= SLOW_JOURNAL_NS)
    }
    fn describe(self, name: &str) -> String {
        let latency = self.recent_ns.map_or(String::new(), |ns| {
            format!("; latest active EWMA {:.1}ms", ns as f64 / 1e6)
        });
        format!(
            "{name}: {} events over {:.1}s ({:.2}/s){latency}",
            self.events,
            self.seconds,
            self.events as f64 / self.seconds
        )
    }
}

impl CounterWindow {
    fn observe(
        &mut self,
        now: Instant,
        count: u64,
        recent_ns: Option<u64>,
        max_gap: Duration,
    ) -> Option<CounterEvidence> {
        let previous = self.previous.replace((now, count));
        let (start, old) = previous?;
        let Some(dt) = now
            .checked_duration_since(start)
            .filter(|dt| !dt.is_zero() && *dt <= max_gap)
        else {
            self.buckets.clear();
            return None;
        };
        let Some(events) = count.checked_sub(old) else {
            self.buckets.clear();
            return None;
        };
        let recent_ns = (events > 0).then_some(recent_ns).flatten();
        // Coalesce sub-second samples. There are at most 61 one-second buckets
        // in the window, even with a very short refresh interval.
        if let Some(last) = self.buckets.back_mut()
            && last.end.duration_since(last.start) < Duration::from_secs(1)
            && now.duration_since(last.start) <= WINDOW
        {
            last.end = now;
            last.events = last.events.saturating_add(events);
            if events > 0 {
                last.recent_ns = recent_ns;
            }
        } else {
            self.buckets.push_back(Bucket {
                start,
                end: now,
                events,
                recent_ns,
            });
        }
        while self
            .buckets
            .front()
            .is_some_and(|b| now.duration_since(b.start) > WINDOW)
        {
            self.buckets.pop_front();
        }
        let seconds = self.buckets.front().map_or(dt.as_secs_f64(), |b| {
            now.duration_since(b.start).as_secs_f64()
        });
        Some(CounterEvidence {
            events: self
                .buckets
                .iter()
                .fold(0u64, |n, b| n.saturating_add(b.events)),
            seconds,
            recent_ns: self
                .buckets
                .iter()
                .rev()
                .find(|b| b.events > 0)
                .and_then(|b| b.recent_ns),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysfs::{JournalState, TimeStatFull};
    use crate::targets::MemberAllocation;

    #[test]
    fn headroom_uses_fresh_allocator_samples_and_missing_space_makes_the_finding_stale() {
        const GIB: u64 = 1 << 30;
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        snap.target_configs = vec![crate::targets::TargetConfig {
            role: "metadata",
            target: "ssd".into(),
            device_name: None,
        }];
        snap.options.insert("metadata_replicas".into(), "1".into());
        snap.devices[0].label = Some("ssd.1".into());
        snap.devices[0].allocation.capacity_bytes = Some(500 * GIB);
        snap.devices[0].allocation.durability = Some(1);
        snap.devices[0].allocation.data_allowed = Some(vec!["btree".into()]);
        snap.copygc.needs_gc.insert("nvme1n1".into(), true);
        snap.copygc.running = Some(true);
        for secs in (0..=120).step_by(2) {
            snap.allocation_sampled_at = Some(origin + Duration::from_secs(secs / 10 * 10));
            snap.devices[0].allocation.free_bytes = Some((200 - secs / 10 * 2) * GIB);
            step(&mut engine, &mut snap, origin, secs);
        }
        let trend = &engine.headroom_trends["metadata:ssd"];
        assert_eq!(trend.samples, 13);
        assert_eq!(trend.seconds, 120.0);
        assert_eq!(
            engine.findings["headroom:metadata:ssd"].status,
            Status::Active
        );
        snap.options.insert("metadata_replicas".into(), "2".into());
        step(&mut engine, &mut snap, origin, 122);
        assert_eq!(engine.headroom_trends["metadata:ssd"].samples, 1);
        assert_eq!(
            engine.findings["headroom:metadata:ssd"].status,
            Status::Stale
        );
        snap.devices[0].allocation.free_bytes = None;
        step(&mut engine, &mut snap, origin, 124);
        assert_eq!(
            engine.findings["headroom:metadata:ssd"].status,
            Status::Stale
        );
        assert!(!engine.headroom_trends.contains_key("metadata:ssd"));
        snap.devices[0].allocation.free_bytes = Some(173 * GIB);
        snap.allocation_sampled_at = Some(origin + Duration::from_secs(126));
        step(&mut engine, &mut snap, origin, 126);
        assert_eq!(engine.headroom_trends["metadata:ssd"].samples, 1);
        assert_eq!(
            engine.findings["headroom:metadata:ssd"].status,
            Status::Stale
        );
    }

    #[test]
    fn peer_latency_finding_resolves_on_recovery_and_suspends_when_backing_peers_overlap() {
        use crate::metrics::{DeviceRate, Rates};
        use crate::topology::{BlockTopology, MediaKind};
        let origin = Instant::now();
        let mut snap = snapshot();
        let device = snap.devices[0].clone();
        snap.devices = (0..3)
            .map(|i| DeviceInfo {
                name: format!("dm-{i}"),
                member_uuid: Some(format!("peer-{i}")),
                label: Some(format!("ssd.nvme.{i}")),
                topology: BlockTopology {
                    media: Some(MediaKind::Nvme),
                    leaves: vec![format!("nvme{i}n1")],
                },
                ..device.clone()
            })
            .collect();
        let mut rates = Rates {
            devices: snap
                .devices
                .iter()
                .enumerate()
                .map(|(i, d)| DeviceRate {
                    name: d.name.clone(),
                    member_key: d.identity(),
                    read_iops: 10.0,
                    read_completed: 20,
                    read_await_ms: if i == 0 { 100.0 } else { 5.0 },
                    avg_queue_depth: 0.5,
                    sample_seconds: 2.0,
                    diskstats_interval_valid: true,
                    ..Default::default()
                })
                .collect(),
        };
        let mut engine = Diagnostics::default();
        for secs in (0..=30).step_by(2) {
            let now = origin + Duration::from_secs(secs);
            snap.diskstats_sampled_at = Some(now);
            snap.collection_started_at = Some(now);
            for d in &mut snap.devices {
                d.diskstats_reads = secs * 10;
            }
            engine.update_with_rates("fs-1", &snap, Some(&rates), now, Duration::from_secs(2));
        }
        assert_eq!(
            engine.findings["device:peer-0:peer-read"].status,
            Status::Active
        );
        rates.devices[0].read_await_ms = 5.0;
        let now = origin + Duration::from_secs(32);
        snap.diskstats_sampled_at = Some(now);
        snap.collection_started_at = Some(now);
        engine.update_with_rates("fs-1", &snap, Some(&rates), now, Duration::from_secs(2));
        assert_eq!(
            engine.findings["device:peer-0:peer-read"].status,
            Status::Resolved
        );
        snap.devices[1].topology.leaves = snap.devices[0].topology.leaves.clone();
        let now = origin + Duration::from_secs(34);
        snap.diskstats_sampled_at = Some(now);
        snap.collection_started_at = Some(now);
        engine.update_with_rates("fs-1", &snap, Some(&rates), now, Duration::from_secs(2));
        assert_eq!(
            engine.findings["device:peer-0:peer-read"].status,
            Status::Stale
        );
    }

    fn snapshot() -> FsSnapshot {
        FsSnapshot {
            devices: vec![DeviceInfo {
                index: 1,
                name: "nvme1n1".into(),
                member_uuid: Some("member-1".into()),
                diskstats_valid: true,
                diskstats_in_flight: 1,
                diskstats_discards: Some(0),
                diskstats_flushes: Some(0),
                error_counts: Some(HashMap::from([("read".into(), 100), ("write".into(), 0)])),
                allocation: MemberAllocation {
                    state: Some("rw".into()),
                    online: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            }],
            blocked_stats: [
                "journal_low_on_space",
                "journal_max_in_flight",
                "journal_max_open",
                "write_buffer_full",
                "allocate",
            ]
            .into_iter()
            .map(|n| (n.into(), 0, 0.0))
            .collect(),
            all_time_stats: [
                "journal_flush_write",
                "journal_noflush_write",
                "journal_flush_seq",
                "journal_pin_flush_btree",
                "journal_pin_flush_key_cache",
            ]
            .into_iter()
            .map(|n| TimeStatFull {
                name: n.into(),
                recent_valid: true,
                ..Default::default()
            })
            .collect(),
            journal: JournalState {
                entries: Some((1, 100)),
                seq: Some(10),
                seq_ondisk: Some(10),
            },
            ..Default::default()
        }
    }

    fn step(engine: &mut Diagnostics, snap: &mut FsSnapshot, origin: Instant, secs: u64) {
        let now = origin + Duration::from_secs(secs);
        snap.diskstats_sampled_at = Some(now);
        snap.collection_started_at = Some(now);
        engine.update("fs-1", snap, now, Duration::from_secs(2));
    }

    fn block(snap: &mut FsSnapshot, name: &str, count: u64) {
        snap.blocked_stats
            .iter_mut()
            .find(|(n, _, _)| n == name)
            .unwrap()
            .1 = count;
    }

    fn operation(snap: &mut FsSnapshot, name: &str, count: u64, ns: u64) {
        let stat = snap
            .all_time_stats
            .iter_mut()
            .find(|s| s.name == name)
            .unwrap();
        stat.count = count;
        stat.dur_recent_ns = ns;
    }

    #[test]
    fn io_stall_requires_elapsed_time_and_resolves_on_progress() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        for secs in (0..30).step_by(2) {
            snap.devices[0].diskstats_weighted_io_ms = secs * 1000;
            step(&mut engine, &mut snap, origin, secs);
            assert!(
                !engine
                    .findings
                    .contains_key("device:member-1:no-completions")
            );
        }
        step(&mut engine, &mut snap, origin, 30);
        let finding = &engine.findings["device:member-1:no-completions"];
        assert_eq!(finding.status, Status::Active);
        assert!(finding.summary.contains("30s"));
        assert!(finding.evidence[0].contains("16 consecutive"));
        snap.devices[0].diskstats_reads += 1;
        step(&mut engine, &mut snap, origin, 32);
        assert_eq!(
            engine.findings["device:member-1:no-completions"].status,
            Status::Resolved
        );
        snap.devices[0].diskstats_in_flight = 0;
        for secs in (34..=92).step_by(2) {
            step(&mut engine, &mut snap, origin, secs);
        }
        assert!(
            !engine
                .findings
                .contains_key("device:member-1:no-completions")
        );
    }

    #[test]
    fn discard_or_flush_completions_prevent_false_io_stalls() {
        for flush in [false, true] {
            let origin = Instant::now();
            let mut engine = Diagnostics::default();
            let mut snap = snapshot();
            for secs in (0..=60).step_by(2) {
                if flush {
                    snap.devices[0].diskstats_flushes = Some(secs);
                } else {
                    snap.devices[0].diskstats_discards = Some(secs);
                }
                step(&mut engine, &mut snap, origin, secs);
            }
            assert!(engine.findings.is_empty());
        }
    }

    #[test]
    fn partial_completion_visibility_is_explicit_and_a_change_rebaselines() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        snap.devices[0].diskstats_flushes = None;
        for secs in (0..=30).step_by(2) {
            step(&mut engine, &mut snap, origin, secs);
        }
        assert!(
            engine.findings["device:member-1:no-completions"].evidence[1]
                .contains("flush accounting unavailable")
        );
        snap.devices[0].diskstats_flushes = Some(0);
        step(&mut engine, &mut snap, origin, 32);
        assert_eq!(
            engine.findings["device:member-1:no-completions"].status,
            Status::Stale
        );
    }

    #[test]
    fn a_gap_or_missing_sample_does_not_extend_io_stall_evidence() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        for secs in (0..=30).step_by(2) {
            step(&mut engine, &mut snap, origin, secs);
        }
        snap.devices[0].diskstats_valid = false;
        step(&mut engine, &mut snap, origin, 32);
        assert_eq!(
            engine.findings["device:member-1:no-completions"].status,
            Status::Stale
        );
        snap.devices[0].diskstats_valid = true;
        step(&mut engine, &mut snap, origin, 34);
        step(&mut engine, &mut snap, origin, 60); // gap exceeds configured cadence
        step(&mut engine, &mut snap, origin, 62);
        assert_ne!(
            engine.findings["device:member-1:no-completions"].status,
            Status::Active
        );
    }

    #[test]
    fn slow_collection_does_not_seed_a_falsely_recent_filesystem_baseline() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        snap.collection_started_at = Some(origin);
        snap.diskstats_sampled_at = Some(origin + Duration::from_secs(10));
        engine.update(
            "fs-1",
            &snap,
            origin + Duration::from_secs(10),
            Duration::from_secs(2),
        );
        snap.devices[0]
            .error_counts
            .as_mut()
            .unwrap()
            .insert("read".into(), 104);
        step(&mut engine, &mut snap, origin, 12);
        assert!(!engine.findings.contains_key("device:member-1:errors"));
        snap.devices[0]
            .error_counts
            .as_mut()
            .unwrap()
            .insert("read".into(), 105);
        step(&mut engine, &mut snap, origin, 14);
        assert!(
            engine.findings["device:member-1:errors"].evidence[0].contains("1 events over 2.0s")
        );
    }

    #[test]
    fn device_replacement_and_disk_counter_reset_break_continuity() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        snap.devices[0].diskstats_reads = 10;
        for secs in (0..=30).step_by(2) {
            step(&mut engine, &mut snap, origin, secs);
        }
        snap.devices[0].diskstats_reads = 0;
        step(&mut engine, &mut snap, origin, 32);
        assert_eq!(
            engine.findings["device:member-1:no-completions"].status,
            Status::Stale
        );
        snap.devices[0].member_uuid = Some("replacement".into());
        step(&mut engine, &mut snap, origin, 34);
        assert!(
            !engine
                .findings
                .contains_key("device:replacement:no-completions")
        );
        assert_eq!(engine.progress.len(), 1);
    }

    #[test]
    fn journal_classification_distinguishes_space_completion_and_reclaim() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        step(&mut engine, &mut snap, origin, 0);
        block(&mut snap, "journal_max_in_flight", 3);
        operation(&mut snap, "journal_flush_write", 2, 840_000_000);
        step(&mut engine, &mut snap, origin, 2);
        assert!(
            engine.findings["journal:pressure"]
                .summary
                .contains("completion")
        );
        assert_eq!(
            engine.findings["journal:pressure"].confidence,
            Confidence::PossibleCause
        );
        assert!(
            engine.findings["journal:pressure"]
                .evidence
                .iter()
                .any(|s| s.contains("840.0ms"))
        );
        operation(&mut snap, "journal_pin_flush_btree", 1, 1_000_000_000);
        step(&mut engine, &mut snap, origin, 4);
        assert!(
            engine.findings["journal:pressure"]
                .summary
                .contains("reclaim")
        );
        block(&mut snap, "journal_low_on_space", 1);
        step(&mut engine, &mut snap, origin, 6);
        assert!(
            engine.findings["journal:pressure"]
                .summary
                .contains("space")
        );
        assert_eq!(
            engine.findings["journal:pressure"].confidence,
            Confidence::Observed
        );
    }

    #[test]
    fn missing_journal_counters_are_not_zero_event_evidence() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        snap.blocked_stats
            .retain(|(name, _, _)| name != "journal_low_on_space");
        step(&mut engine, &mut snap, origin, 0);
        block(&mut snap, "journal_max_open", 2);
        step(&mut engine, &mut snap, origin, 2);
        let finding = &engine.findings["journal:pressure"];
        assert_eq!(finding.summary, "Journal-pipeline pressure");
        assert!(
            finding
                .evidence
                .iter()
                .any(|s| s == "journal_low_on_space: unavailable or establishing baseline")
        );
        snap.blocked_stats.clear();
        step(&mut engine, &mut snap, origin, 4);
        assert_eq!(engine.findings["journal:pressure"].status, Status::Stale);
    }

    #[test]
    fn idle_ewma_does_not_trigger_and_recent_journal_evidence_expires() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        snap.devices[0].diskstats_in_flight = 0;
        operation(&mut snap, "journal_flush_write", 100, 1_000_000_000);
        step(&mut engine, &mut snap, origin, 0);
        step(&mut engine, &mut snap, origin, 2);
        assert!(engine.findings.is_empty());
        block(&mut snap, "journal_max_open", 1);
        operation(&mut snap, "journal_flush_write", 101, 1_000_000_000);
        step(&mut engine, &mut snap, origin, 4);
        for secs in (6..=66).step_by(2) {
            step(&mut engine, &mut snap, origin, secs);
        }
        assert_eq!(engine.findings["journal:pressure"].status, Status::Resolved);
    }

    #[test]
    fn occupancy_and_write_buffer_pressure_are_not_disk_space_diagnoses() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        snap.journal.entries = Some((90, 100));
        step(&mut engine, &mut snap, origin, 0);
        assert!(!engine.findings.contains_key("journal:pressure"));
        assert!(engine.findings["journal:entry-occupancy"].evidence[0].contains("not on-disk"));
        block(&mut snap, "write_buffer_full", 2);
        step(&mut engine, &mut snap, origin, 2);
        assert!(
            engine.findings["journal:write-buffer"]
                .action
                .contains("not an established remedy")
        );
    }

    #[test]
    fn historic_errors_are_baselines_and_new_errors_expire_or_go_stale() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        snap.devices[0].diskstats_in_flight = 0;
        step(&mut engine, &mut snap, origin, 0);
        step(&mut engine, &mut snap, origin, 2);
        assert!(engine.findings.is_empty());
        snap.devices[0]
            .error_counts
            .as_mut()
            .unwrap()
            .insert("read".into(), 103);
        step(&mut engine, &mut snap, origin, 4);
        assert!(engine.findings["device:member-1:errors"].evidence[0].contains("3 events"));
        let counts = snap.devices[0].error_counts.take();
        step(&mut engine, &mut snap, origin, 6);
        assert_eq!(
            engine.findings["device:member-1:errors"].status,
            Status::Stale
        );
        snap.devices[0].error_counts = counts;
        step(&mut engine, &mut snap, origin, 8);
        step(&mut engine, &mut snap, origin, 10);
        assert_eq!(
            engine.findings["device:member-1:errors"].status,
            Status::Resolved
        );
    }

    #[test]
    fn counter_resets_and_missing_error_categories_do_not_create_new_errors() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        step(&mut engine, &mut snap, origin, 0);
        snap.devices[0]
            .error_counts
            .as_mut()
            .unwrap()
            .insert("read".into(), 104);
        step(&mut engine, &mut snap, origin, 2);
        snap.devices[0]
            .error_counts
            .as_mut()
            .unwrap()
            .remove("read");
        step(&mut engine, &mut snap, origin, 4);
        assert_eq!(
            engine.findings["device:member-1:errors"].status,
            Status::Stale
        );
        snap.devices[0]
            .error_counts
            .as_mut()
            .unwrap()
            .insert("read".into(), 1);
        step(&mut engine, &mut snap, origin, 6);
        step(&mut engine, &mut snap, origin, 8);
        assert_eq!(
            engine.findings["device:member-1:errors"].status,
            Status::Resolved
        );
    }

    #[test]
    fn member_transitions_persist_until_recovery_and_baseline_existing_ro() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        step(&mut engine, &mut snap, origin, 0);
        snap.devices[0].allocation.state = Some("ro".into());
        step(&mut engine, &mut snap, origin, 2);
        step(&mut engine, &mut snap, origin, 4);
        assert_eq!(
            engine.findings["device:member-1:state"].status,
            Status::Active
        );
        assert_eq!(
            engine.findings["device:member-1:state"].severity,
            Severity::Critical
        );
        snap.devices[0].allocation.state = Some("rw".into());
        step(&mut engine, &mut snap, origin, 6);
        assert_eq!(
            engine.findings["device:member-1:state"].status,
            Status::Resolved
        );
        assert!(
            engine.findings["device:member-1:state"]
                .evidence
                .iter()
                .any(|s| s.starts_with("Recovery observed:"))
        );
        snap.devices[0].allocation.state = Some("ro".into());
        engine.update(
            "other-fs",
            &snap,
            origin + Duration::from_secs(8),
            Duration::from_secs(2),
        );
        assert!(engine.findings.is_empty());
    }

    #[test]
    fn restored_state_data_does_not_resolve_a_member_that_is_still_read_only() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        step(&mut engine, &mut snap, origin, 0);
        snap.devices[0].allocation.state = Some("ro".into());
        step(&mut engine, &mut snap, origin, 2);
        snap.devices[0].allocation.state = None;
        step(&mut engine, &mut snap, origin, 4);
        assert_eq!(
            engine.findings["device:member-1:state"].status,
            Status::Stale
        );
        snap.devices[0].allocation.state = Some("ro".into());
        step(&mut engine, &mut snap, origin, 6);
        step(&mut engine, &mut snap, origin, 8);
        assert_eq!(
            engine.findings["device:member-1:state"].status,
            Status::Active
        );
        assert_eq!(
            engine.findings["device:member-1:state"].first_seen,
            origin + Duration::from_secs(6)
        );
    }

    #[test]
    fn error_clustering_reports_members_without_claiming_a_shared_failure() {
        let origin = Instant::now();
        let mut engine = Diagnostics::default();
        let mut snap = snapshot();
        let mut second = snap.devices[0].clone();
        second.member_uuid = Some("member-2".into());
        second.name = "nvme2n1".into();
        snap.devices.push(second);
        step(&mut engine, &mut snap, origin, 0);
        for d in &mut snap.devices {
            d.error_counts.as_mut().unwrap().insert("read".into(), 101);
        }
        step(&mut engine, &mut snap, origin, 2);
        assert_eq!(
            engine.findings["devices:correlated-errors"].confidence,
            Confidence::PossibleCause
        );
        assert!(engine.findings["devices:correlated-errors"].evidence[0].contains("nvme2n1"));
        assert!(
            engine.findings["devices:correlated-errors"].evidence[1]
                .contains("does not establish simultaneous")
        );
    }

    #[test]
    fn windows_are_time_based_bounded_and_reset_when_counters_decrease() {
        let origin = Instant::now();
        for millis in [10, 500, 2000, 10000] {
            let mut window = CounterWindow::default();
            let mut last = None;
            for tick in 0..=(120_000 / millis) {
                last = window.observe(
                    origin + Duration::from_millis(tick * millis),
                    tick * millis,
                    None,
                    Duration::from_secs(30),
                );
                assert!(window.buckets.len() <= 61);
            }
            let last = last.unwrap();
            assert!(last.seconds <= 60.0);
            assert_eq!(last.events as f64 / last.seconds, 1000.0);
            assert!(
                window
                    .observe(
                        origin + Duration::from_secs(122),
                        0,
                        None,
                        Duration::from_secs(30)
                    )
                    .is_none()
            );
            assert!(window.buckets.is_empty());
        }
    }

    #[test]
    fn a_long_valid_interval_keeps_its_events_when_the_previous_bucket_expires() {
        let origin = Instant::now();
        let mut window = CounterWindow::default();
        window.observe(origin, 0, None, WINDOW);
        window.observe(origin + Duration::from_millis(500), 0, None, WINDOW);
        let evidence = window
            .observe(origin + Duration::from_millis(60_500), 1, None, WINDOW)
            .unwrap();
        assert_eq!(evidence.events, 1);
        assert_eq!(evidence.seconds, 60.0);
    }
}

struct Progress {
    at: Instant,
    counts: [Option<u64>; 4],
    queue: u64,
    since: Option<Instant>,
    samples: usize,
    times: [u64; 4],
}

#[derive(Default)]
pub struct Diagnostics {
    filesystem: String,
    counters: HashMap<String, CounterWindow>,
    progress: HashMap<String, Progress>,
    member_states: HashMap<String, (String, bool)>,
    error_shapes: HashMap<String, HashSet<String>>,
    last_update: Option<Instant>,
    pub findings: BTreeMap<String, Diagnostic>,
    seen: HashSet<String>,
    latency_history: HashMap<String, crate::trends::LatencyHistory>,
    headroom_history: HashMap<String, crate::trends::HeadroomHistory>,
    pub peer_contexts: HashMap<String, crate::peers::PeerContext>,
    pub headroom_trends: BTreeMap<String, crate::trends::HeadroomSummary>,
}

impl Diagnostics {
    fn counter(
        &mut self,
        key: &str,
        now: Instant,
        count: Option<u64>,
        recent_ns: Option<u64>,
        gap: Duration,
    ) -> Option<CounterEvidence> {
        match count {
            Some(count) => self
                .counters
                .entry(key.into())
                .or_default()
                .observe(now, count, recent_ns, gap),
            None => {
                self.counters.remove(key);
                None
            }
        }
    }

    fn record(&mut self, id: &str, observation: Option<Observation>, valid: bool, now: Instant) {
        self.seen.insert(id.into());
        if let Some(o) = observation {
            let first_seen = self
                .findings
                .get(id)
                .filter(|d| d.status == Status::Active)
                .map_or(now, |d| d.first_seen);
            self.findings.insert(
                id.into(),
                Diagnostic {
                    id: id.into(),
                    severity: o.severity,
                    confidence: o.confidence,
                    summary: o.summary,
                    evidence: o.evidence,
                    action: o.action,
                    first_seen,
                    last_seen: now,
                    status_since: first_seen,
                    status: Status::Active,
                },
            );
        } else if let Some(d) = self.findings.get_mut(id) {
            let status = if valid {
                Status::Resolved
            } else {
                Status::Stale
            };
            if status != d.status {
                d.status_since = now;
            }
            d.status = status;
        }
    }

    /// `expected_interval` is the configured interval, not a delayed tick's
    /// actual duration. A gap must never count as continuous observation.
    pub fn update(
        &mut self,
        fs: &str,
        snap: &FsSnapshot,
        now: Instant,
        expected_interval: Duration,
    ) {
        self.update_with_rates(fs, snap, None, now, expected_interval);
    }

    pub fn update_with_rates(
        &mut self,
        fs: &str,
        snap: &FsSnapshot,
        rates: Option<&crate::metrics::Rates>,
        now: Instant,
        expected_interval: Duration,
    ) {
        if self.filesystem != fs {
            *self = Self {
                filesystem: fs.into(),
                ..Default::default()
            };
        }
        self.seen.clear();
        let gap = expected_interval
            .saturating_mul(3)
            .max(Duration::from_secs(5))
            .min(WINDOW);
        if self.last_update.is_some_and(|old| {
            now.checked_duration_since(old)
                .is_none_or(|dt| dt.is_zero() || dt > gap)
        }) {
            self.counters.clear();
            self.progress.clear();
            self.member_states.clear();
            self.error_shapes.clear();
            self.latency_history.clear();
            self.headroom_history.clear();
            for d in self.findings.values_mut() {
                d.status = Status::Stale;
                d.status_since = now;
            }
        }
        self.last_update = Some(now);
        let sampled_at = snap.diskstats_sampled_at;
        let filesystem_fresh = snap
            .collection_started_at
            .is_some_and(|at| at <= now && now.duration_since(at) <= gap);
        let mut members = HashSet::new();
        let mut error_members = Vec::new();
        let mut errors_valid = filesystem_fresh && !snap.devices.is_empty();
        for d in &snap.devices {
            let key = d.identity();
            members.insert(key.clone());
            if let Some(at) = sampled_at {
                self.io_progress(d, &key, at, now, gap);
            } else {
                self.progress.remove(&key);
            }
            if filesystem_fresh {
                self.member_state(d, &key, now);
                let (has_errors, valid) = self.device_errors(d, &key, now, gap);
                errors_valid &= valid;
                if has_errors {
                    error_members.push(d.name.clone());
                }
            }
        }
        self.progress.retain(|key, _| members.contains(key));
        self.member_states.retain(|key, _| members.contains(key));
        self.error_shapes.retain(|key, _| members.contains(key));
        let grouped = (error_members.len() >= 2).then(|| {
            let mut o = Observation::observed(format!("Errors increased on {} members in the evidence window", error_members.len()),
                vec![error_members.join(", "), "Counter increases occurred within the last 60s of valid observations; this does not establish simultaneous failures.".into()],
                "Inspect shared power, cables, backplanes and controllers alongside individual device health.");
            o.confidence = Confidence::PossibleCause;
            o
        });
        self.record("devices:correlated-errors", grouped, errors_valid, now);
        if filesystem_fresh {
            self.journal(snap, now, gap);
            self.target_headroom(snap, now, gap);
        } else {
            self.member_states.clear();
            self.error_shapes.clear();
            self.headroom_history.clear();
            self.headroom_trends.clear();
        }
        if filesystem_fresh
            && snap
                .diskstats_sampled_at
                .is_some_and(|at| at <= now && now.duration_since(at) <= gap)
        {
            if let Some(rates) = rates {
                self.peer_latency(snap, rates, now, gap);
            } else {
                self.peer_contexts.clear();
                self.latency_history.clear();
            }
        } else {
            self.peer_contexts.clear();
            self.latency_history.clear();
        }
        // Missing members/counters lose continuity rather than inheriting old
        // baselines when they reappear. Retain diagnostic history for 60s.
        self.counters
            .retain(|_, w| w.previous.is_some_and(|(at, _)| at == now));
        for (id, d) in &mut self.findings {
            if !self.seen.contains(id) && d.status != Status::Stale {
                d.status = Status::Stale;
                d.status_since = now;
            }
        }
        self.findings.retain(|_, d| {
            d.status == Status::Active || now.saturating_duration_since(d.last_seen) <= WINDOW
        });
        let mut recent: Vec<_> = self
            .findings
            .values()
            .filter(|d| d.status != Status::Active)
            .map(|d| (d.last_seen, d.id.clone()))
            .collect();
        recent.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        for (_, id) in recent.into_iter().skip(MAX_RECENT_FINDINGS) {
            self.findings.remove(&id);
        }
    }

    fn peer_latency(
        &mut self,
        snap: &FsSnapshot,
        rates: &crate::metrics::Rates,
        now: Instant,
        gap: Duration,
    ) {
        let contexts = crate::peers::compare(snap, rates, gap);
        let at = snap.diskstats_sampled_at.unwrap();
        let mut keys = HashSet::new();
        for (key, context) in &contexts {
            let device = snap.devices.iter().find(|d| d.identity() == *key).unwrap();
            for (direction, sample) in [
                ("read", context.read.as_ref()),
                ("write", context.write.as_ref()),
            ] {
                let id = format!("device:{key}:peer-{direction}");
                keys.insert(id.clone());
                let persisted = self.latency_history.entry(id.clone()).or_default().observe(
                    &context.signature,
                    sample,
                    at,
                    gap,
                );
                let valid = sample.is_some_and(|s| !s.outlier) || persisted.is_some();
                let observation = persisted.zip(sample).map(|(p, s)| {
                    let mut evidence = vec![format!("{}; peers: {}", context.description, s.peers.join(", ")),
                        format!("Read/write direction matched; AQ {:.2} versus peer median AQ {:.2}. Similar request rates and I/O mix; {} valid observations.", s.queue, s.median_queue, p.samples),
                        p.baseline_ms.map_or("Own earlier comparable/non-outlier baseline: unavailable.".into(), |ms| format!("Own earlier comparable/non-outlier baseline: {ms:.1}ms (request weighted, recent 5 minutes)."))];
                    if self.findings.get(&format!("device:{key}:errors")).is_some_and(|d| d.status == Status::Active) {
                        evidence.push("Device error counters also increased in the evidence window.".into());
                    }
                    Observation::observed(format!("{}: {direction} await {:.1}ms vs peers {:.1}ms for {:.0}s", device.name, s.await_ms, s.median_ms, p.seconds), evidence,
                        "Inspect the member and its workload. Comparable-peer latency is an outlier signal, not proof of hardware failure; request sizes/locality and other users of a backing disk may differ.")
                });
                self.record(&id, observation, valid, now);
            }
        }
        self.latency_history.retain(|key, _| keys.contains(key));
        self.peer_contexts = contexts;
    }

    fn target_headroom(&mut self, snap: &FsSnapshot, now: Instant, gap: Duration) {
        use crate::targets::format_optional_bytes as bytes;
        let mut summaries = BTreeMap::new();
        for report in crate::targets::analyze(snap) {
            let key = format!("{}:{}", report.role, report.target);
            let id = format!("headroom:{key}");
            let Some((signature, point)) = crate::trends::headroom_input(
                snap,
                &report,
                now,
                crate::trends::ALLOC_INTERVAL + gap,
            ) else {
                self.headroom_history.remove(&key);
                self.record(&id, None, false, now);
                continue;
            };
            let summary = self
                .headroom_history
                .entry(key.clone())
                .or_default()
                .observe(&signature, point, crate::trends::ALLOC_INTERVAL + gap);
            let observation = summary.alert.then(|| Observation::observed(
                format!("{} target {}: free-bucket headroom is declining", report.role, report.target),
                vec![format!("{} → {} over {:.0}s ({} fresh allocator samples).", bytes(Some(summary.first_free)), bytes(Some(summary.last_free)), summary.seconds, summary.samples),
                     format!("GC pressure in {}/{} samples ({} unknown); copygc running in {} samples.", summary.pressure_samples, summary.samples, summary.unknown_pressure, summary.gc_running_samples),
                     format!("Placement backlog: {} → {} (where applicable).", bytes(summary.first_backlog), bytes(summary.last_backlog))],
                "Inspect per-member allocation, GC and target placement. Free buckets are not guaranteed allocatable space; this measured trend is not a time-until-full prediction."));
            self.record(
                &id,
                observation,
                summary.seconds >= crate::trends::HEADROOM_MIN.as_secs_f64(),
                now,
            );
            summaries.insert(key, summary);
        }
        self.headroom_history
            .retain(|key, _| summaries.contains_key(key));
        self.headroom_trends = summaries;
    }

    fn io_progress(&mut self, d: &DeviceInfo, key: &str, at: Instant, now: Instant, gap: Duration) {
        let id = format!("device:{key}:no-completions");
        if !d.diskstats_valid
            || at > now
            || now.saturating_duration_since(at) > gap
            || d.allocation.online == Some(false)
        {
            self.progress.remove(key);
            self.record(&id, None, false, now);
            return;
        }
        let counts = [
            Some(d.diskstats_reads),
            Some(d.diskstats_writes),
            d.diskstats_discards,
            d.diskstats_flushes,
        ];
        let times = [
            d.diskstats_read_ms,
            d.diskstats_write_ms,
            d.diskstats_io_ms,
            d.diskstats_weighted_io_ms,
        ];
        let mut p = Progress {
            at,
            counts,
            queue: d.diskstats_in_flight,
            since: None,
            samples: 1,
            times,
        };
        let mut valid = false;
        if let Some(old) = self.progress.get(key) {
            valid = at
                .checked_duration_since(old.at)
                .is_some_and(|dt| !dt.is_zero() && dt <= gap)
                && times.iter().zip(old.times).all(|(n, old)| *n >= old)
                && counts
                    .iter()
                    .zip(old.counts)
                    .all(|(n, old)| match (n, old) {
                        (Some(n), Some(old)) => *n >= old,
                        (None, None) => true,
                        _ => false,
                    });
            if valid && p.queue > 0 && old.queue > 0 && counts == old.counts {
                p.since = Some(old.since.unwrap_or(old.at));
                p.samples = old.samples.saturating_add(1);
            }
        }
        let stalled_for = p.since.map(|start| at.duration_since(start));
        let observation = stalled_for.filter(|dt| *dt >= IO_STALL_AFTER && p.samples >= 3).map(|dt| {
            Observation::observed(format!("{}: outstanding I/O with no observed completions for {:.0}s", d.name, dt.as_secs_f64()),
                vec![format!("Queue depth {}; {} consecutive valid observations. Threshold: {}s.", p.queue, p.samples, IO_STALL_AFTER.as_secs()),
                     format!("Read/write completions tracked; discard accounting {}; flush accounting {} (flushes are not tracked for partitions).", if counts[2].is_some() { "available" } else { "unavailable" }, if counts[3].is_some() { "available" } else { "unavailable" })],
                "Inspect device timeouts, controller resets and kernel logs. Samples show no observed completions, not the age or identity of a particular request.")
        });
        self.progress.insert(key.into(), p);
        self.record(&id, observation, valid || d.diskstats_in_flight == 0, now);
    }

    fn member_state(&mut self, d: &DeviceInfo, key: &str, now: Instant) {
        let id = format!("device:{key}:state");
        let Some(current) = d.allocation.state.clone().zip(d.allocation.online) else {
            self.member_states.remove(key);
            self.record(&id, None, false, now);
            return;
        };
        let previous = self.member_states.insert(key.into(), current.clone());
        let bad = current.0 != "rw" || !current.1;
        let continuing = self
            .findings
            .get(&id)
            .filter(|f| f.status != Status::Resolved);
        let changed = previous.as_ref().is_some_and(|old| *old != current);
        let known_incident = continuing.is_some();
        let observation = (bad && (changed || known_incident)).then(|| {
            let transition = previous.as_ref().filter(|_| changed).map_or_else(
                || continuing.and_then(|f| f.evidence.first().cloned()).unwrap_or_default(),
                |old| format!("Observed transition: {} / online={} → {} / online={}", old.0, old.1, current.0, current.1));
            let summary = format!("{}: member {} / online={}", d.name, current.0, current.1);
            let mut o = Observation::observed(summary, vec![transition, format!("Member identity {key}; label {}. Current state {} / online={}.", d.label.as_deref().unwrap_or("?"), current.0, current.1)],
                "Check why the member changed state and inspect device/kernel errors. A state transition can be administrative; it does not prove hardware failure.");
            o.severity = Severity::Critical;
            o
        });
        self.record(&id, observation, previous.is_some() || known_incident, now);
        if !bad
            && let Some(finding) = self.findings.get_mut(&id)
            && finding.status == Status::Resolved
            && !finding
                .evidence
                .iter()
                .any(|s| s.starts_with("Recovery observed:"))
        {
            finding.evidence.push(format!(
                "Recovery observed: {} / online={}",
                current.0, current.1
            ));
        }
    }

    fn device_errors(
        &mut self,
        d: &DeviceInfo,
        key: &str,
        now: Instant,
        gap: Duration,
    ) -> (bool, bool) {
        let id = format!("device:{key}:errors");
        let Some(counts) = &d.error_counts else {
            self.error_shapes.remove(key);
            self.record(&id, None, false, now);
            return (false, false);
        };
        let mut evidence = Vec::new();
        let shape: HashSet<_> = counts.keys().cloned().collect();
        let mut valid = !counts.is_empty()
            && self
                .error_shapes
                .insert(key.into(), shape.clone())
                .is_some_and(|old| old == shape);
        for (name, count) in counts {
            let window = self.counter(
                &format!("errors:{key}:{name}"),
                now,
                Some(*count),
                None,
                gap,
            );
            valid &= window.is_some();
            if let Some(w) = window.filter(|w| w.events > 0) {
                evidence.push(w.describe(name));
            }
        }
        evidence.sort();
        let has_errors = !evidence.is_empty();
        let observation = has_errors.then(|| Observation::observed(format!("{}: device error counters increased", d.name), evidence,
            "Inspect this device and its storage path. Counts are new observed bcachefs errors, not SMART totals or proof that data was lost."));
        self.record(&id, observation, valid, now);
        (has_errors, valid)
    }

    fn journal(&mut self, snap: &FsSnapshot, now: Instant, gap: Duration) {
        let mut blocked = HashMap::new();
        for name in [
            "journal_low_on_space",
            "journal_max_in_flight",
            "journal_max_open",
            "write_buffer_full",
            "allocate",
        ] {
            let count = snap
                .blocked_stats
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, count, _)| *count);
            blocked.insert(
                name,
                self.counter(&format!("blocked:{name}"), now, count, None, gap),
            );
        }
        let mut operations = HashMap::new();
        for name in [
            "journal_flush_write",
            "journal_noflush_write",
            "journal_flush_seq",
            "journal_pin_flush_btree",
            "journal_pin_flush_key_cache",
        ] {
            let stat = snap.all_time_stats.iter().find(|s| s.name == name);
            operations.insert(
                name,
                self.counter(
                    name,
                    now,
                    stat.map(|s| s.count),
                    stat.filter(|s| s.recent_valid).map(|s| s.dur_recent_ns),
                    gap,
                ),
            );
        }
        let space = blocked["journal_low_on_space"];
        let flight = blocked["journal_max_in_flight"];
        let open = blocked["journal_max_open"];
        let events = |w: Option<CounterEvidence>| w.is_some_and(|w| w.events > 0);
        let slow = |name| operations[name].is_some_and(CounterEvidence::slow);
        let mut evidence: Vec<_> = [
            "journal_low_on_space",
            "journal_max_in_flight",
            "journal_max_open",
        ]
        .into_iter()
        .map(|name| {
            blocked[name].map_or(
                format!("{name}: unavailable or establishing baseline"),
                |w| w.describe(name),
            )
        })
        .collect();
        for name in [
            "journal_flush_write",
            "journal_noflush_write",
            "journal_flush_seq",
            "journal_pin_flush_btree",
            "journal_pin_flush_key_cache",
        ] {
            if let Some(w) = operations[name].filter(|w| w.events > 0) {
                evidence.push(w.describe(name));
            }
        }
        evidence.push(match snap.journal.seq.zip(snap.journal.seq_ondisk) {
            Some((seq, disk)) => format!("Current journal seq {seq}; on-disk seq {disk}. A snapshot difference alone is not a stall."),
            None => "Journal sequence information unavailable.".into(),
        });
        if let Some(w) = self.counter(
            "journal_seq_ondisk",
            now,
            snap.journal.seq_ondisk,
            None,
            gap,
        ) {
            evidence.push(w.describe("on-disk journal sequence advance"));
        }
        evidence.push(format!(
            "Journal watermark: {}",
            if snap.journal_watermark.is_empty() {
                "unknown"
            } else {
                &snap.journal_watermark
            }
        ));
        let classification = if events(space) {
            Some((
                "Journal-space pressure",
                Confidence::Observed,
                "Inspect journal reclaim and allocation headroom. Entry occupancy alone is not disk journal utilization.",
            ))
        } else if (events(flight) || events(open))
            && (slow("journal_pin_flush_btree") || slow("journal_pin_flush_key_cache"))
        {
            Some((
                "Possible metadata-reclaim bottleneck affecting the journal",
                Confidence::PossibleCause,
                "Inspect btree/key-cache flushing and metadata-device latency. These events coincide; the dependency is not proven.",
            ))
        } else if events(flight)
            && (slow("journal_flush_write")
                || slow("journal_noflush_write")
                || slow("journal_flush_seq"))
        {
            Some((
                "Possible journal-completion bottleneck",
                Confidence::PossibleCause,
                "Inspect outstanding journal work and per-device flush latency. Enlarging the journal is not justified by in-flight blocking alone.",
            ))
        } else if events(open) || events(flight) {
            Some((
                "Journal-pipeline pressure",
                Confidence::Observed,
                "Inspect outstanding journal work and its dependencies. Space exhaustion and the underlying cause require additional evidence.",
            ))
        } else {
            None
        };
        let observation = classification.map(|(summary, confidence, action)| {
            let mut o = Observation::observed(summary.into(), evidence, action);
            o.confidence = confidence;
            o
        });
        self.record(
            "journal:pressure",
            observation,
            space.is_some() && flight.is_some() && open.is_some(),
            now,
        );
        let buffer = blocked["write_buffer_full"];
        self.record("journal:write-buffer", buffer.filter(|w| w.events > 0).map(|w| Observation::observed("Write-buffer pressure".into(), vec![w.describe("write_buffer_full")],
            "Inspect btree write-buffer flushing and metadata-device pressure; more journal flushes are not an established remedy.")), buffer.is_some(), now);
        let allocate = blocked["allocate"];
        self.record("allocator:pressure", allocate.filter(|w| w.events > 0).map(|w| Observation::observed("Allocator blocking observed".into(), vec![w.describe("allocate")],
            "Inspect member free buckets, reserves and GC pressure in Targets [v]. Increasing the reserve cannot create physical capacity.")), allocate.is_some(), now);
        // Report what this counter measures, never call it disk space used.
        self.record("journal:entry-occupancy", snap.journal.entries.filter(|(dirty, total)| *total > 0 && *dirty as f64 / *total as f64 > 0.8).map(|(dirty, total)| {
            Observation::observed("High journal dirty-entry occupancy".into(), vec![format!("{dirty}/{total} dirty entries ({:.1}%). This is not on-disk journal-space usage.", dirty as f64 / total as f64 * 100.0)],
                "Use journal blocking and reclaim evidence to determine whether this occupancy is limiting progress.")
        }), snap.journal.entries.is_some(), now);
    }
}
