//! Comparable-peer latency measurements. No filesystem-wide HDD/SSD median.

use crate::metrics::{DeviceRate, Rates};
use crate::sysfs::{DeviceInfo, FsSnapshot};
use crate::topology::MediaKind;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct PeerSample {
    pub await_ms: f64,
    pub median_ms: f64,
    pub completed: u64,
    pub queue: f64,
    pub median_queue: f64,
    pub peers: Vec<String>,
    pub outlier: bool,
}

#[derive(Debug, Clone)]
pub struct PeerContext {
    pub description: String,
    pub signature: String,
    pub read: Option<PeerSample>,
    pub write: Option<PeerSample>,
}

impl PeerContext {
    pub fn outlier(&self) -> bool {
        self.read.as_ref().is_some_and(|s| s.outlier)
            || self.write.as_ref().is_some_and(|s| s.outlier)
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

fn cohort<'a>(snap: &'a FsSnapshot, device: &DeviceInfo) -> Option<(String, Vec<&'a DeviceInfo>)> {
    let mut groups = Vec::new();
    for config in &snap.target_configs {
        if matches!(config.target.as_str(), "" | "none" | "(none)") {
            continue;
        }
        if let Some(members) = crate::targets::members_for(config, &snap.devices)
            && members.iter().any(|d| d.identity() == device.identity())
        {
            groups.push((format!("target {}", config.target), members));
        }
    }
    // Prefer the narrowest configured group. Never widen a singleton target
    // just to manufacture peers with a different placement/workload role.
    groups.sort_by(|a, b| a.1.len().cmp(&b.1.len()).then(a.0.cmp(&b.0)));
    if let Some(group) = groups.into_iter().next() {
        return Some(group);
    }
    let label = device.label.as_deref()?;
    if matches!(label, "none" | "(none)") {
        return Some((
            "unlabelled members".into(),
            snap.devices
                .iter()
                .filter(|d| matches!(d.label.as_deref(), Some("none" | "(none)")))
                .collect(),
        ));
    }
    let parent = label.rsplit_once('.').map_or(label, |(parent, _)| parent);
    let members = snap
        .devices
        .iter()
        .filter(|d| {
            d.label
                .as_deref()
                .is_some_and(|label| label.rsplit_once('.').map_or(label, |(p, _)| p) == parent)
        })
        .collect();
    Some((format!("label group {parent}"), members))
}

pub fn compare(
    snap: &FsSnapshot,
    rates: &Rates,
    gap: std::time::Duration,
) -> HashMap<String, PeerContext> {
    let by_key: HashMap<_, _> = rates
        .devices
        .iter()
        .filter(|r| {
            r.sample_seconds.is_finite()
                && r.sample_seconds > 0.0
                && r.sample_seconds <= gap.as_secs_f64()
                && r.read_iops.is_finite()
                && r.write_iops.is_finite()
                && r.read_iops >= 0.0
                && r.write_iops >= 0.0
                && r.avg_queue_depth.is_finite()
                && r.avg_queue_depth >= 0.0
        })
        .map(|r| (r.member_key.as_str(), r))
        .collect();
    let mut result = HashMap::new();
    for device in &snap.devices {
        let key = device.identity();
        let Some(rate) = by_key.get(key.as_str()) else {
            continue;
        };
        let Some(media) = device.topology.media else {
            continue;
        };
        if device.topology.leaves.is_empty() || device.allocation.online != Some(true) {
            continue;
        }
        let Some((group, members)) = cohort(snap, device) else {
            continue;
        };
        let mut members: Vec<_> = members
            .into_iter()
            .filter(|d| d.topology.media == Some(media))
            .collect();
        members.sort_by_key(|d| d.identity());
        let description = format!("{group}, {}", media.label());
        let signature = format!(
            "{description}:{:?}",
            members
                .iter()
                .map(|d| (d.identity(), &d.topology.leaves, d.allocation.online))
                .collect::<Vec<_>>()
        );
        let read = direction(device, rate, &members, &by_key, media, false);
        let write = direction(device, rate, &members, &by_key, media, true);
        result.insert(
            key,
            PeerContext {
                description,
                signature,
                read,
                write,
            },
        );
    }
    result
}

fn direction(
    device: &DeviceInfo,
    rate: &DeviceRate,
    members: &[&DeviceInfo],
    rates: &HashMap<&str, &DeviceRate>,
    media: MediaKind,
    write: bool,
) -> Option<PeerSample> {
    let values = |r: &DeviceRate| {
        if write {
            (r.write_completed, r.write_await_ms, r.write_iops)
        } else {
            (r.read_completed, r.read_await_ms, r.read_iops)
        }
    };
    let (completed, await_ms, iops) = values(rate);
    if !rate.diskstats_interval_valid
        || completed < 5
        || !await_ms.is_finite()
        || !iops.is_finite()
        || iops <= 0.0
    {
        return None;
    }
    let mut used: HashSet<&str> = device.topology.leaves.iter().map(String::as_str).collect();
    let mut peers = Vec::new();
    let mut latencies = Vec::new();
    let mut queues = Vec::new();
    for member in members {
        if member.allocation.online != Some(true)
            || member.topology.leaves.is_empty()
            || member.topology.leaves.len() != device.topology.leaves.len()
            || member
                .topology
                .leaves
                .iter()
                .any(|l| used.contains(l.as_str()))
        {
            continue;
        }
        let key = member.identity();
        let Some(peer) = rates.get(key.as_str()) else {
            continue;
        };
        let (ops, latency, peer_iops) = values(peer);
        if !peer.diskstats_interval_valid || ops < 5 || !latency.is_finite() || peer_iops <= 0.0 {
            continue;
        }
        // Compare each direction separately, with broadly similar request
        // rates, queue depth and read/write mix. This is still observational.
        if !(0.25..=4.0).contains(&(iops / peer_iops))
            || rate.avg_queue_depth > (peer.avg_queue_depth * 2.0).max(1.0)
            || peer.avg_queue_depth > (rate.avg_queue_depth * 2.0).max(1.0)
            || (rate.read_iops / rate.total_iops() - peer.read_iops / peer.total_iops()).abs()
                > 0.25
        {
            continue;
        }
        used.extend(member.topology.leaves.iter().map(String::as_str));
        peers.push(member.name.clone());
        latencies.push(latency);
        queues.push(peer.avg_queue_depth);
    }
    if peers.len() < 2 {
        return None;
    }
    let median_ms = median(latencies);
    Some(PeerSample {
        await_ms,
        median_ms,
        completed,
        queue: rate.avg_queue_depth,
        median_queue: median(queues),
        peers,
        outlier: await_ms > (median_ms * 3.0).max(media.latency_floor_ms()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::{MemberAllocation, TargetConfig};
    use crate::topology::BlockTopology;
    use std::time::Duration;

    fn pool() -> (FsSnapshot, Rates) {
        let devices: Vec<_> = (0..6)
            .map(|i| DeviceInfo {
                name: format!("dm-{i}"),
                member_uuid: Some(format!("uuid-{i}")),
                label: Some(format!("tier.disk{i}")),
                topology: BlockTopology {
                    leaves: vec![format!("physical-{i}")],
                    media: Some(if i < 3 {
                        MediaKind::Rotational
                    } else {
                        MediaKind::Nvme
                    }),
                },
                allocation: MemberAllocation {
                    online: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect();
        let rates = Rates {
            devices: devices
                .iter()
                .enumerate()
                .map(|(i, d)| DeviceRate {
                    name: d.name.clone(),
                    member_key: d.identity(),
                    diskstats_interval_valid: true,
                    read_completed: 20,
                    read_iops: 10.0,
                    read_await_ms: if i < 3 { 32.0 } else { 0.2 },
                    avg_queue_depth: 0.5,
                    sample_seconds: 2.0,
                    ..Default::default()
                })
                .collect(),
        };
        (
            FsSnapshot {
                devices,
                target_configs: vec![TargetConfig {
                    role: "background",
                    target: "tier".into(),
                    device_name: None,
                }],
                ..Default::default()
            },
            rates,
        )
    }

    #[test]
    fn ordinary_hdd_latency_is_not_compared_to_nvme_and_a_real_peer_outlier_is_found() {
        let (snap, mut rates) = pool();
        let results = compare(&snap, &rates, Duration::from_secs(6));
        assert!(!results["uuid-0"].outlier());
        assert_eq!(results["uuid-0"].read.as_ref().unwrap().median_ms, 32.0);
        rates.devices[0].read_await_ms = 185.0;
        assert!(compare(&snap, &rates, Duration::from_secs(6))["uuid-0"].outlier());
        rates.devices[3].read_await_ms = 1.0; // 5x peers but below the NVMe absolute floor
        assert!(!compare(&snap, &rates, Duration::from_secs(6))["uuid-3"].outlier());
    }

    #[test]
    fn insufficient_idle_shared_or_overloaded_peers_do_not_manufacture_an_outlier() {
        let (mut snap, mut rates) = pool();
        rates.devices[0].read_await_ms = 185.0;
        rates.devices[1].read_completed = 0;
        assert!(
            compare(&snap, &rates, Duration::from_secs(6))["uuid-0"]
                .read
                .is_none()
        );
        rates.devices[1].read_completed = 20;
        rates.devices[0].avg_queue_depth = 20.0;
        assert!(
            compare(&snap, &rates, Duration::from_secs(6))["uuid-0"]
                .read
                .is_none()
        );
        rates.devices[0].avg_queue_depth = 0.5;
        snap.devices[1].topology.leaves = snap.devices[0].topology.leaves.clone();
        assert!(
            compare(&snap, &rates, Duration::from_secs(6))["uuid-0"]
                .read
                .is_none()
        );
        snap.devices[0].topology.media = None;
        assert!(!compare(&snap, &rates, Duration::from_secs(6)).contains_key("uuid-0"));
    }

    #[test]
    fn narrow_target_and_directional_workload_context_are_respected() {
        let (mut snap, mut rates) = pool();
        rates.devices[0].read_await_ms = 185.0;
        snap.target_configs.push(TargetConfig {
            role: "metadata",
            target: "/dev/dm-0".into(),
            device_name: Some("dm-0".into()),
        });
        assert!(
            compare(&snap, &rates, Duration::from_secs(6))["uuid-0"]
                .read
                .is_none()
        );
        snap.target_configs.pop();
        rates.devices[1].write_iops = 100.0; // very different read/write mix
        assert!(
            compare(&snap, &rates, Duration::from_secs(6))["uuid-0"]
                .read
                .is_none()
        );
        rates.devices[1].write_iops = 0.0;
        rates.devices[2].sample_seconds = 60.0; // long interval cannot masquerade as a fresh peer
        assert!(
            compare(&snap, &rates, Duration::from_secs(6))["uuid-0"]
                .read
                .is_none()
        );
    }
}
