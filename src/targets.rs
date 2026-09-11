//! Target capacity and allocator-pressure diagnostics. These describe observed
//! constraints, not an exact simulation of bcachefs's best-effort allocator.

use crate::sysfs::{DeviceInfo, FsSnapshot};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct MemberAllocation {
    pub capacity_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub btree_bytes: Option<u64>,
    pub fragmented_bytes: Option<u64>,
    pub state: Option<String>,
    pub durability: Option<u64>,
    pub data_allowed: Option<Vec<String>>,
    pub online: Option<bool>,
}

impl MemberAllocation {
    fn eligible(&self, data_type: &str) -> Option<bool> {
        if self.online == Some(false) || self.state.as_deref().is_some_and(|s| s != "rw") {
            return Some(false);
        }
        if self.durability? == 0 {
            return Some(false);
        }
        Some(
            self.online?
                && self.state.as_deref()? == "rw"
                && self.data_allowed.as_ref()?.iter().any(|t| t == data_type),
        )
    }
}

#[derive(Debug, Clone)]
pub struct TargetConfig {
    pub role: &'static str,
    pub target: String,
    /// Resolved block name for a /dev path (including by-id aliases).
    pub device_name: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CopyGcStatus {
    pub running: Option<bool>,
    /// The sign of the kernel's calculated wait, not a free-space quantity.
    /// Missing devices remain unknown, including on truncated sysfs output.
    pub needs_gc: HashMap<String, bool>,
    /// Exact displayed kernel metric; its sign is used, not a free-byte value.
    pub calculated_wait: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct ReconcileWork {
    pub category: String,
    pub data_bytes: Option<u64>,
    pub metadata_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct ReconcileStatus {
    pub state: Option<String>,
    pub progress: Option<String>,
    pub scan_pending: Option<u64>,
    pub work: Vec<ReconcileWork>,
}

impl ReconcileStatus {
    pub fn metadata_pending(&self, category: &str) -> Option<u64> {
        self.work
            .iter()
            .find(|w| w.category == category)?
            .metadata_bytes
    }

    pub fn summary(&self) -> String {
        let Some(state) = &self.state else {
            return "n/a".into();
        };
        let mut parts = vec![format!(
            "{state}{}",
            self.progress
                .as_ref()
                .map_or(String::new(), |p| format!(" {p}"))
        )];
        if let Some(scans) = self.scan_pending.filter(|n| *n > 0) {
            parts.push(format!("scans:{scans}"));
        }
        for work in &self.work {
            if work.data_bytes.is_some_and(|n| n > 0) || work.metadata_bytes.is_some_and(|n| n > 0)
            {
                parts.push(format!(
                    "{}:data={} meta={}",
                    work.category,
                    format_optional_bytes(work.data_bytes),
                    format_optional_bytes(work.metadata_bytes),
                ));
            }
        }
        parts.join(" — ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Critical,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub id: String,
    pub severity: Severity,
    pub summary: String,
    pub detail: String,
    pub criteria: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TargetReport {
    pub role: &'static str,
    pub target: String,
    pub members: Vec<String>,
    pub eligible_members: Option<usize>,
    pub eligible_devices: Vec<String>,
    /// Raw member capacity: an upper bound, before journals/reserves/other data.
    pub capacity_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
}

pub fn format_optional_bytes(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "?".into();
    };
    for (unit, power) in [("TiB", 4), ("GiB", 3), ("MiB", 2), ("KiB", 1)] {
        let divisor = 1024u64.pow(power);
        if bytes >= divisor {
            return format!("{:.2} {unit}", bytes as f64 / divisor as f64);
        }
    }
    format!("{bytes} B")
}

fn sum_known(mut values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    values.try_fold(0u64, |sum, value| sum.checked_add(value?))
}

pub(crate) fn members_for<'a>(
    config: &TargetConfig,
    devices: &'a [DeviceInfo],
) -> Option<Vec<&'a DeviceInfo>> {
    if devices.is_empty() {
        return None;
    }
    if let Some(name) = &config.device_name {
        return Some(devices.iter().filter(|d| &d.name == name).collect());
    }
    if config.target.starts_with('/') {
        return None; // Unresolved device alias, not an empty label group.
    }
    if let Some(device) = devices.iter().find(|d| d.name == config.target) {
        return Some(vec![device]);
    }
    // A missing label is not evidence that a member is outside the target.
    devices
        .iter()
        .map(|d| d.label.as_ref())
        .collect::<Option<Vec<_>>>()?;
    let prefix = format!("{}.", config.target);
    Some(
        devices
            .iter()
            .filter(|d| {
                d.label
                    .as_deref()
                    .is_some_and(|label| label == config.target || label.starts_with(&prefix))
            })
            .collect(),
    )
}

pub fn analyze(snapshot: &FsSnapshot) -> Vec<TargetReport> {
    snapshot.target_configs.iter().map(|config| {
        let mut report = TargetReport {
            role: config.role,
            target: config.target.clone(),
            members: Vec::new(),
            eligible_members: None,
            eligible_devices: Vec::new(),
            capacity_bytes: None,
            free_bytes: None,
            findings: Vec::new(),
            notes: Vec::new(),
        };
        if config.role == "metadata" {
            report.notes.push(format!("Reported on-disk btree footprint: {}; requested replicas: {}; metadata target backlog: {}.",
                format_optional_bytes(snapshot.btree_disk_bytes), snapshot.options.get("metadata_replicas").map_or("?", String::as_str),
                format_optional_bytes(snapshot.reconcile.metadata_pending("target"))));
        }
        if matches!(config.target.as_str(), "none" | "(none)" | "") {
            report.notes.push("No explicit target configured.".into());
            return report;
        }
        let Some(members) = members_for(config, &snapshot.devices) else {
            report.notes.push("Target membership unknown: missing labels or unresolved device path.".into());
            return report;
        };
        report.members = members.iter().map(|d| d.name.clone()).collect();
        let data_type = if config.role == "metadata" { "btree" } else { "user" };
        let eligibility: Option<Vec<bool>> = members.iter().map(|d| d.allocation.eligible(data_type)).collect();
        let Some(eligibility) = eligibility else {
            report.notes.push("Allocation eligibility unknown: member state or data_allowed unavailable.".into());
            gc_findings(snapshot, &members, &mut report);
            return report;
        };
        let eligible: Vec<_> = members.iter().zip(&eligibility)
            .filter_map(|(d, eligible)| eligible.then_some(*d)).collect();
        report.eligible_members = Some(eligible.len());
        report.eligible_devices = eligible.iter().map(|d| d.name.clone()).collect();
        report.capacity_bytes = sum_known(eligible.iter().map(|d| d.allocation.capacity_bytes));
        report.free_bytes = sum_known(eligible.iter().map(|d| d.allocation.free_bytes));
        if eligible.len() != members.len() {
            let excluded = members.iter().zip(&eligibility)
                .filter_map(|(d, eligible)| (!eligible).then_some(d.name.as_str())).collect::<Vec<_>>();
            report.notes.push(format!("Excluded from new {data_type} allocation: {} (offline, non-rw, disallowed or durability=0).", excluded.join(", ")));
        }
        if report.capacity_bytes.is_none() || report.free_bytes.is_none() {
            report.notes.push("Some capacity/free-bucket metrics unavailable; ? means unknown, not zero.".into());
        }
        if eligible.is_empty() {
            report.findings.push(Finding {
                id: format!("{}:{}:no-members", config.role, config.target),
                severity: Severity::Critical,
                summary: format!("{} target {} has no eligible writable members", config.role, config.target),
                detail: "Check member state, labels, durability and data_allowed. Best-effort placement may spill outside the target.".into(),
                criteria: format!("target membership known AND eligible_count == 0; eligible = online AND state=rw AND durability>0 AND data_allowed contains {data_type}"),
                evidence: eligibility_evidence(&members, data_type),
            });
        }
        if config.role == "metadata" {
            metadata_findings(snapshot, &eligible, &mut report);
        }
        gc_findings(snapshot, &members, &mut report);
        report
    }).collect()
}

fn gc_findings(snapshot: &FsSnapshot, members: &[&DeviceInfo], report: &mut TargetReport) {
    for device in members {
        let allocation = &device.allocation;
        if snapshot.copygc.needs_gc.get(&device.name) == Some(&true) {
            report.findings.push(Finding {
                    id: format!("{}:{}:gc:{}", report.role, report.target, device.index),
                    severity: Severity::Warning,
                    summary: format!("{} target {}: {} needs GC", report.role, report.target, device.name),
                    detail: format!("Kernel calculated wait is non-positive; copygc {}. Free buckets: {}; fragmented: {}. Inspect this member's allocator headroom; GC pressure can delay reconcile. This is not proof of a stalled worker.",
                        match snapshot.copygc.running { Some(true) => "is running", Some(false) => "is idle", None => "state unknown" },
                        format_optional_bytes(allocation.free_bytes), format_optional_bytes(allocation.fragmented_bytes)),
                    criteria: "member belongs to this target AND kernel calculated copygc wait <= 0".into(),
                    evidence: vec![format!("target={}, member={} (dev-{})", report.target, device.name, device.index),
                        format!("internal/copy_gc_wait calculated wait = {} (kernel metric, not free bytes); needs_gc=true", snapshot.copygc.calculated_wait.get(&device.name).map_or("raw value unavailable", String::as_str)),
                        format!("Context only: copygc.running={:?}, free buckets={}, fragmented={}", snapshot.copygc.running, format_optional_bytes(allocation.free_bytes), format_optional_bytes(allocation.fragmented_bytes))],
                });
        }
    }
}

fn eligibility_evidence(members: &[&DeviceInfo], data_type: &str) -> Vec<String> {
    let mut evidence = vec![format!(
        "Resolved target members: {}; eligible_count={}",
        members.len(),
        members
            .iter()
            .filter(|d| d.allocation.eligible(data_type) == Some(true))
            .count()
    )];
    evidence.extend(members.iter().map(|d| {
        format!(
            "{}: online={}, state={}, durability={}, data_allowed={} -> eligible({data_type})={}",
            d.name,
            option_text(d.allocation.online),
            d.allocation.state.as_deref().unwrap_or("unknown"),
            option_text(d.allocation.durability),
            d.allocation
                .data_allowed
                .as_ref()
                .map_or("unknown".into(), |types| types.join(",")),
            option_text(d.allocation.eligible(data_type))
        )
    }));
    evidence
}

fn option_text<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or("unknown".into(), |value| value.to_string())
}

fn metadata_findings(snapshot: &FsSnapshot, members: &[&DeviceInfo], report: &mut TargetReport) {
    let replicas = snapshot
        .options
        .get("metadata_replicas")
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|r| *r > 0);
    let ordinary_durability = members.iter().all(|d| d.allocation.durability == Some(1));
    let physical = snapshot.btree_disk_bytes;
    if !ordinary_durability {
        report.notes.push("Replica-layout estimate unavailable for unknown/non-unit durability. Failure-domain placement is not modeled.".into());
    }
    if let Some(replicas) = replicas
        && ordinary_durability
        && !members.is_empty()
        && (members.len() as u64) < replicas
    {
        report.findings.push(Finding {
            id: format!("metadata:{}:replicas", report.target),
            severity: Severity::Critical,
            summary: format!("Metadata target {} has {} eligible members for {replicas} replicas", report.target, members.len()),
            detail: "All eligible members have durability=1. The requested copies need more distinct writable members in this target; check placement and membership.".into(),
            criteria: "all eligible members have durability=1 AND 0 < eligible_count < metadata_replicas".into(),
            evidence: {
                let mut values = eligibility_evidence(members, "btree");
                values.push(format!("eligible_count={} < metadata_replicas={replicas}", members.len()));
                values
            },
        });
    }
    let (Some(physical), Some(capacity)) = (physical, report.capacity_bytes) else {
        return;
    };
    if physical == 0 || members.is_empty() {
        return;
    }
    if physical > capacity {
        report.findings.push(Finding {
            id: format!("metadata:{}:capacity", report.target),
            severity: Severity::Warning,
            summary: format!("Metadata target {}: footprint {} exceeds capacity {}", report.target, format_optional_bytes(Some(physical)), format_optional_bytes(Some(capacity))),
            detail: format!("Reported current physical metadata exceeds raw eligible capacity by {} before journals, reserves and other data. Expand/select a larger healthy SSD target. Replica changes or compaction can change the footprint; targets permit spillover.", format_optional_bytes(Some(physical - capacity))),
            criteria: "P > C, where P = accounted physical btree sectors * 512 and C = SUM(raw capacity of eligible target members); P already includes replicas".into(),
            evidence: {
                let mut values = eligibility_evidence(members, "btree");
                values.extend(members.iter().map(|d| format!("capacity({})={} bytes", d.name, d.allocation.capacity_bytes.unwrap())));
                values.push(format!("P={physical} bytes > C={capacity} bytes; shortfall={} bytes", physical - capacity));
                values.push(format!("Context only: metadata target backlog={}", format_optional_bytes(snapshot.reconcile.metadata_pending("target"))));
                values
            },
        });
    } else if let Some(replicas) = replicas
        && ordinary_durability
        && members.len() as u64 >= replicas
        && snapshot.reconcile.metadata_pending("replicas") == Some(0)
    {
        // Necessary capacity condition for a uniform r-copy layout: each
        // member can hold at most one logical copy. Extra space on one large
        // member cannot compensate for missing space on other members.
        // Physical/r is an estimate (over-replication is not measured here).
        let copy = physical.div_ceil(replicas);
        let coverage: u128 = members
            .iter()
            .map(|d| d.allocation.capacity_bytes.unwrap().min(copy) as u128)
            .sum();
        if coverage < physical as u128 {
            report.findings.push(Finding {
                id: format!("metadata:{}:layout", report.target),
                severity: Severity::Warning,
                summary: format!("Metadata target {}: unequal member sizes may constrain {replicas} replicas", report.target),
                detail: format!("Assuming a uniform {replicas}-copy layout, estimated logical metadata is {}. Capacity on distinct members cannot accommodate that layout, even though aggregate capacity fits. Verify replica accounting; this estimate excludes over-replication, failure domains and allocation reserves.", format_optional_bytes(Some(copy))),
                criteria: "unit durability AND eligible_count >= r AND metadata replica backlog=0 AND P<=C AND SUM(min(member_capacity, ceil(P/r))) < P; assumes a uniform r-copy layout".into(),
                evidence: {
                    let mut values = eligibility_evidence(members, "btree");
                    values.push(format!("P={physical} bytes, C={capacity} bytes, r={replicas}, L=ceil(P/r)={copy} bytes; replica backlog=0"));
                    values.extend(members.iter().map(|d| format!("min(capacity({})={}, L={copy})={} bytes", d.name, d.allocation.capacity_bytes.unwrap(), d.allocation.capacity_bytes.unwrap().min(copy))));
                    values.push(format!("Coverage={coverage} bytes < P={physical} bytes"));
                    values
                },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn every_snapshot_rule_carries_its_equation_and_input_values() {
        let mut examples = vec![
            pool(&[200, 200, 200], 900),
            pool(&[200, 200], 100),
            pool(&[500, 80, 80], 300),
        ];
        let mut no_members = pool(&[200, 200, 200], 300);
        for device in &mut no_members.devices {
            device.allocation.state = Some("ro".into());
        }
        examples.push(no_members);
        let mut gc = pool(&[200, 200, 200], 300);
        gc.copygc.needs_gc.insert("nvme0n1p3".into(), true);
        gc.copygc
            .calculated_wait
            .insert("nvme0n1p3".into(), "-292M".into());
        let gc_report = analyze(&gc).remove(0);
        assert!(
            gc_report.findings[0]
                .evidence
                .iter()
                .any(|s| s.contains("-292M"))
        );
        examples.push(gc);
        let mut rules = std::collections::HashSet::new();
        for example in examples {
            for report in analyze(&example) {
                for finding in report.findings {
                    assert!(
                        !finding.criteria.is_empty(),
                        "{} has no criteria",
                        finding.id
                    );
                    assert!(!finding.evidence.is_empty(), "{} has no inputs", finding.id);
                    rules.insert(if finding.id.contains(":gc:") {
                        "gc".into()
                    } else {
                        finding.id.rsplit(':').next().unwrap().to_string()
                    });
                }
            }
        }
        assert_eq!(
            rules,
            ["capacity", "replicas", "layout", "no-members", "gc"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
    }

    fn pool(capacities: &[u64], physical_gib: u64) -> FsSnapshot {
        FsSnapshot {
            devices: capacities
                .iter()
                .enumerate()
                .map(|(i, size)| DeviceInfo {
                    index: i as u32,
                    name: format!("nvme{i}n1p3"),
                    label: Some(format!("ssd.nvme.{i}")),
                    allocation: MemberAllocation {
                        capacity_bytes: Some(size * GIB),
                        free_bytes: Some(20 * GIB),
                        state: Some("rw".into()),
                        online: Some(true),
                        durability: Some(1),
                        data_allowed: Some(vec!["btree".into(), "user".into(), "journal".into()]),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .collect(),
            target_configs: vec![TargetConfig {
                role: "metadata",
                target: "ssd.nvme".into(),
                device_name: None,
            }],
            btree_disk_bytes: Some(physical_gib * GIB),
            options: HashMap::from([("metadata_replicas".into(), "3".into())]),
            reconcile: ReconcileStatus {
                work: vec![ReconcileWork {
                    category: "replicas".into(),
                    metadata_bytes: Some(0),
                    data_bytes: Some(0),
                }],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn reported_pool_metadata_exceeds_nvme_target_before_reserves() {
        let mut snap = pool(&[977, 977, 932], 0);
        snap.btree_disk_bytes = Some(7_821_903_360 * 512);
        for (device, buckets) in snap.devices.iter_mut().zip([1_000_000, 1_000_000, 953_869]) {
            device.allocation.capacity_bytes = Some(buckets * (1 << 20));
        }
        let report = analyze(&snap).remove(0);
        assert_eq!(report.capacity_bytes, Some(2_953_869 * (1 << 20)));
        assert_eq!(report.findings.len(), 1);
        assert!(report.findings[0].summary.contains("3.64 TiB"));
        assert!(report.findings[0].summary.contains("2.82 TiB"));
        assert!(report.findings[0].detail.contains("845.14 GiB"));
        assert_eq!(report.findings[0].severity, Severity::Warning);
    }

    #[test]
    fn healthy_replicated_footprint_is_not_multiplied_again() {
        let snap = pool(&[200, 200, 200], 300);
        let report = analyze(&snap).remove(0);
        assert!(report.findings.is_empty());
        assert_eq!(report.capacity_bytes, Some(600 * GIB));
    }

    #[test]
    fn unequal_members_need_distinct_copy_capacity_but_extra_members_can_help() {
        let report = analyze(&pool(&[500, 80, 80], 300)).remove(0);
        assert_eq!(report.findings.len(), 1);
        assert!(report.findings[0].id.ends_with(":layout"));
        assert!(report.findings[0].detail.contains("Assuming a uniform"));
        assert!(
            analyze(&pool(&[500, 80, 80, 80], 300))[0]
                .findings
                .is_empty()
        );
    }

    #[test]
    fn label_boundaries_and_non_writable_members_are_respected() {
        let mut snap = pool(&[200, 200, 200, 200], 300);
        snap.devices[3].label = Some("ssd.nvme2.0".into());
        assert_eq!(analyze(&snap)[0].members.len(), 3);
        snap.devices[2].allocation.state = Some("ro".into());
        let report = analyze(&snap).remove(0);
        assert_eq!(report.eligible_members, Some(2));
        assert_eq!(report.capacity_bytes, Some(400 * GIB));
        assert!(report.findings.iter().any(|f| f.id.ends_with(":replicas")));
        assert!(
            report
                .notes
                .iter()
                .any(|s| s.contains("Excluded") && s.contains("nvme2n1p3"))
        );
    }

    #[test]
    fn unknown_inputs_do_not_become_zero_capacity_or_healthy_state() {
        let mut snap = pool(&[200, 200, 200], 900);
        snap.devices[0].allocation.capacity_bytes = None;
        assert_eq!(analyze(&snap)[0].capacity_bytes, None);
        assert!(analyze(&snap)[0].findings.is_empty());
        snap.devices[0].allocation.state = None;
        assert_eq!(analyze(&snap)[0].eligible_members, None);
        assert!(analyze(&snap)[0].findings.is_empty());
        snap.devices[0].label = None;
        assert!(analyze(&snap)[0].findings.is_empty());
        snap.devices.clear();
        assert!(analyze(&snap)[0].findings.is_empty());
    }

    #[test]
    fn device_alias_does_not_depend_on_labels_and_data_allowed_is_checked() {
        let mut snap = pool(&[200, 200, 200], 100);
        snap.target_configs[0].target = "/dev/disk/by-id/example-part3".into();
        assert!(analyze(&snap)[0].findings.is_empty()); // unresolved, not an empty group
        snap.target_configs[0].device_name = Some("nvme0n1p3".into());
        snap.devices[0].label = None;
        snap.devices[0].allocation.data_allowed = Some(vec!["user".into()]);
        let report = analyze(&snap).remove(0);
        assert_eq!(report.members, ["nvme0n1p3"]);
        assert_eq!(report.eligible_members, Some(0));
        assert!(report.findings[0].id.ends_with(":no-members"));
    }

    #[test]
    fn unusual_durability_and_replica_transitions_disable_layout_estimates() {
        let mut snap = pool(&[500, 80, 80], 300);
        snap.devices[0].allocation.durability = Some(2);
        assert!(analyze(&snap)[0].findings.is_empty());
        snap.devices[0].allocation.durability = None;
        assert!(analyze(&snap)[0].findings.is_empty());
        snap.devices[0].allocation.durability = Some(1);
        snap.reconcile.work[0].metadata_bytes = Some(10 * GIB);
        assert!(analyze(&snap)[0].findings.is_empty());
        snap.reconcile.work.clear();
        assert!(analyze(&snap)[0].findings.is_empty());
    }

    #[test]
    fn kernel_gc_pressure_is_reported_even_when_capacity_is_unknown() {
        let mut snap = pool(&[200, 200, 200], 300);
        snap.copygc.running = Some(true);
        snap.copygc.needs_gc.insert("nvme1n1p3".into(), true);
        snap.devices[1].allocation.state = None;
        let report = analyze(&snap).remove(0);
        assert_eq!(report.findings.len(), 1);
        assert!(report.findings[0].summary.contains("nvme1n1p3 needs GC"));
        assert!(report.findings[0].detail.contains("not proof"));
        snap.copygc.needs_gc.clear();
        assert!(analyze(&snap)[0].findings.is_empty());
    }
}
