//! Bounded, timestamped peer-latency persistence and target-headroom history.

use crate::peers::PeerSample;
use crate::sysfs::FsSnapshot;
use crate::targets::TargetReport;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const PEER_AFTER: Duration = Duration::from_secs(30);
pub const PEER_MIN_SAMPLES: usize = 3;
const BASELINE_WINDOW: Duration = Duration::from_secs(300);
pub const HEADROOM_WINDOW: Duration = Duration::from_secs(600);
pub const HEADROOM_MIN: Duration = Duration::from_secs(120);
pub const ALLOC_INTERVAL: Duration = Duration::from_secs(10);
const MIN_LOSS_BYTES: u64 = 64 << 20;
const MIN_LOSS_DIVISOR: u64 = 10;
const MIN_DECREASES: usize = 3;
const DIRECTION_PERCENT: usize = 70;
const PRESSURE_PERCENT: usize = 80;

struct BaselinePoint {
    start: Instant,
    end: Instant,
    latency_ms: f64,
    ops: u64,
}

#[derive(Default)]
pub struct LatencyHistory {
    signature: String,
    last: Option<Instant>,
    since: Option<Instant>,
    samples: usize,
    baseline: VecDeque<BaselinePoint>,
}

pub struct LatencyEvidence {
    pub seconds: f64,
    pub samples: usize,
    pub baseline_ms: Option<f64>,
}

impl LatencyHistory {
    pub fn observe(
        &mut self,
        signature: &str,
        sample: Option<&PeerSample>,
        at: Instant,
        gap: Duration,
    ) -> Option<LatencyEvidence> {
        let Some(sample) = sample else {
            *self = Self::default();
            return None;
        };
        if self.signature != signature
            || self.last.is_some_and(|old| {
                at.checked_duration_since(old)
                    .is_none_or(|dt| dt.is_zero() || dt > gap)
            })
        {
            *self = Self {
                signature: signature.into(),
                ..Default::default()
            };
        }
        self.last = Some(at);
        while self
            .baseline
            .front()
            .is_some_and(|p| at.duration_since(p.start) > BASELINE_WINDOW)
        {
            self.baseline.pop_front();
        }
        if sample.outlier {
            let since = *self.since.get_or_insert(at);
            self.samples = self.samples.saturating_add(1);
            if at.duration_since(since) < PEER_AFTER || self.samples < PEER_MIN_SAMPLES {
                return None;
            }
            let baseline_ms = self
                .baseline
                .front()
                .zip(self.baseline.back())
                .filter(|(first, last)| {
                    self.baseline.len() >= 3 && last.end.duration_since(first.start) >= PEER_AFTER
                })
                .and_then(|_| {
                    let ops = self.baseline.iter().map(|p| p.ops as f64).sum::<f64>();
                    (ops > 0.0)
                        .then(|| self.baseline.iter().map(|p| p.latency_ms).sum::<f64>() / ops)
                });
            return Some(LatencyEvidence {
                seconds: at.duration_since(since).as_secs_f64(),
                samples: self.samples,
                baseline_ms,
            });
        }
        self.since = None;
        self.samples = 0;
        let latency_ms = sample.await_ms * sample.completed as f64;
        if let Some(last) = self.baseline.back_mut()
            && at.duration_since(last.start) < ALLOC_INTERVAL
        {
            last.end = at;
            last.ops = last.ops.saturating_add(sample.completed);
            last.latency_ms += latency_ms;
        } else {
            self.baseline.push_back(BaselinePoint {
                start: at,
                end: at,
                ops: sample.completed,
                latency_ms,
            });
        }
        None
    }
}

#[derive(Clone)]
pub struct HeadroomPoint {
    pub at: Instant,
    pub free: u64,
    pub pressure: Option<bool>,
    pub gc_running: Option<bool>,
    pub backlog: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct HeadroomSummary {
    pub direction: &'static str,
    pub seconds: f64,
    pub samples: usize,
    pub first_free: u64,
    pub last_free: u64,
    pub sampled_at: Instant,
    pub pressure_samples: usize,
    pub unknown_pressure: usize,
    pub gc_running_samples: usize,
    pub first_backlog: Option<u64>,
    pub last_backlog: Option<u64>,
    pub alert: bool,
    pub decreases: usize,
    pub increases: usize,
    pub recent_reference: Option<(u64, Duration)>,
}

impl HeadroomSummary {
    pub fn criteria(&self) -> String {
        format!(
            "valid unchanged-policy/member history AND coverage >= {}s AND loss >= max({MIN_LOSS_BYTES} bytes, first_free/{MIN_LOSS_DIVISOR}) AND decreases >= {MIN_DECREASES} AND 100*decreases >= {DIRECTION_PERCENT}*(decreases+increases) AND latest_free < reference_free at least {}s earlier AND 100*pressure_samples >= {PRESSURE_PERCENT}*samples",
            HEADROOM_MIN.as_secs(),
            PEER_AFTER.as_secs()
        )
    }
    pub fn indicators(&self) -> Vec<String> {
        vec![
            format!(
                "coverage={:.3}s >= {}s; fresh samples={}",
                self.seconds,
                HEADROOM_MIN.as_secs(),
                self.samples
            ),
            format!(
                "first_free={} bytes; latest_free={} bytes; loss={} bytes >= max({MIN_LOSS_BYTES}, {}/{MIN_LOSS_DIVISOR})={} bytes",
                self.first_free,
                self.last_free,
                self.first_free.saturating_sub(self.last_free),
                self.first_free,
                MIN_LOSS_BYTES.max(self.first_free / MIN_LOSS_DIVISOR)
            ),
            format!(
                "decreases={} >= {MIN_DECREASES}; increases={}; 100*{}={} >= {DIRECTION_PERCENT}*{}={}",
                self.decreases,
                self.increases,
                self.decreases,
                100 * self.decreases,
                self.decreases + self.increases,
                DIRECTION_PERCENT * (self.decreases + self.increases)
            ),
            self.recent_reference
                .map_or("recent reference: unknown".into(), |(free, age)| {
                    format!(
                        "latest_free={} < reference_free={free} bytes; reference age={:.3}s >= {}s",
                        self.last_free,
                        age.as_secs_f64(),
                        PEER_AFTER.as_secs()
                    )
                }),
            format!(
                "100*pressure_samples={} >= {PRESSURE_PERCENT}*samples={}; unknown pressure samples={} (included in denominator)",
                100 * self.pressure_samples,
                PRESSURE_PERCENT * self.samples,
                self.unknown_pressure
            ),
        ]
    }
}

#[derive(Default)]
pub struct HeadroomHistory {
    signature: String,
    points: VecDeque<HeadroomPoint>,
}

impl HeadroomHistory {
    pub fn observe(
        &mut self,
        signature: &str,
        point: HeadroomPoint,
        gap: Duration,
    ) -> HeadroomSummary {
        if self.signature != signature
            || self
                .points
                .back()
                .is_some_and(|last| point.at < last.at || point.at.duration_since(last.at) > gap)
        {
            *self = Self {
                signature: signature.into(),
                ..Default::default()
            };
        }
        // Cached allocator reads must not create fake trend/pressure samples.
        // Bound history even if a caller refreshes allocator tables faster.
        if self
            .points
            .back()
            .is_none_or(|last| point.at.duration_since(last.at) >= ALLOC_INTERVAL)
        {
            while self
                .points
                .front()
                .is_some_and(|p| point.at.duration_since(p.at) > HEADROOM_WINDOW)
            {
                self.points.pop_front();
            }
            self.points.push_back(point);
        }
        let first = self.points.front().unwrap();
        let last = self.points.back().unwrap();
        let seconds = last.at.duration_since(first.at).as_secs_f64();
        let pairs = self.points.iter().zip(self.points.iter().skip(1));
        let mut down = 0usize;
        let mut up = 0usize;
        for (a, b) in pairs {
            if b.free < a.free {
                down += 1;
            } else if b.free > a.free {
                up += 1;
            }
        }
        let direction = if seconds < HEADROOM_MIN.as_secs_f64() {
            "warming up"
        } else if last.free == first.free {
            "steady"
        } else if last.free < first.free && down * 100 >= (down + up) * DIRECTION_PERCENT {
            "falling"
        } else if last.free > first.free && up * 100 >= (down + up) * DIRECTION_PERCENT {
            "rising"
        } else {
            "fluctuating"
        };
        let pressure_samples = self
            .points
            .iter()
            .filter(|p| p.pressure == Some(true))
            .count();
        let recent_reference = self
            .points
            .iter()
            .rev()
            .find(|p| last.at.duration_since(p.at) >= PEER_AFTER)
            .map(|p| (p.free, last.at.duration_since(p.at)));
        let threshold = MIN_LOSS_BYTES.max(first.free / MIN_LOSS_DIVISOR);
        let alert = direction == "falling"
            && down >= MIN_DECREASES
            && recent_reference.is_some_and(|(free, _)| last.free < free)
            && first.free.saturating_sub(last.free) >= threshold
            && pressure_samples * 100 >= self.points.len() * PRESSURE_PERCENT;
        HeadroomSummary {
            direction,
            seconds,
            samples: self.points.len(),
            first_free: first.free,
            last_free: last.free,
            sampled_at: last.at,
            pressure_samples,
            unknown_pressure: self.points.iter().filter(|p| p.pressure.is_none()).count(),
            gc_running_samples: self
                .points
                .iter()
                .filter(|p| p.gc_running == Some(true))
                .count(),
            first_backlog: first.backlog,
            last_backlog: last.backlog,
            alert,
            decreases: down,
            increases: up,
            recent_reference,
        }
    }
}

pub fn headroom_input(
    snap: &FsSnapshot,
    report: &TargetReport,
    now: Instant,
    max_age: Duration,
) -> Option<(String, HeadroomPoint)> {
    let at = snap
        .allocation_sampled_at
        .filter(|at| *at <= now && now.duration_since(*at) <= max_age)?;
    let capacity = report.capacity_bytes.filter(|n| *n > 0)?;
    let free = report.free_bytes.filter(|n| *n <= capacity)?;
    let count = report
        .eligible_members
        .filter(|n| *n > 0 && *n == report.eligible_devices.len())?;
    let mut members = Vec::new();
    let mut pressures = Vec::new();
    for name in &report.members {
        let device = snap.devices.iter().find(|d| &d.name == name)?;
        let eligible = report.eligible_devices.contains(name);
        members.push((
            device.identity(),
            device.allocation.capacity_bytes,
            eligible,
        ));
        if eligible {
            pressures.push(snap.copygc.needs_gc.get(name).copied());
        }
    }
    if pressures.len() != count {
        return None;
    }
    members.sort();
    let mut policy: Vec<_> = snap.options.iter().collect();
    policy.sort();
    let signature = format!("{:?}:{:?}", members, policy);
    let pressure = if pressures.contains(&Some(true)) {
        Some(true)
    } else if pressures.iter().all(|p| *p == Some(false)) {
        Some(false)
    } else {
        None
    };
    let backlog = snap
        .reconcile
        .work
        .iter()
        .find(|w| w.category == "target")
        .and_then(|w| match report.role {
            "metadata" => w.metadata_bytes,
            "background" => w.data_bytes,
            _ => None,
        });
    Some((
        signature,
        HeadroomPoint {
            at,
            free,
            pressure,
            gc_running: snap.copygc.running,
            backlog,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    const GIB: u64 = 1 << 30;

    fn sample(outlier: bool) -> PeerSample {
        PeerSample {
            await_ms: if outlier { 100.0 } else { 10.0 },
            median_ms: 10.0,
            completed: 20,
            queue: 0.5,
            median_queue: 0.5,
            peers: vec!["a".into(), "b".into()],
            outlier,
            floor_ms: 2.0,
            selection_evidence: Vec::new(),
        }
    }
    fn point(at: Instant, free: u64, pressure: Option<bool>) -> HeadroomPoint {
        HeadroomPoint {
            at,
            free,
            pressure,
            gc_running: Some(true),
            backlog: Some(10 * GIB),
        }
    }

    #[test]
    fn latency_requires_persistence_and_keeps_an_earlier_request_weighted_baseline() {
        let at = Instant::now();
        let mut history = LatencyHistory::default();
        for secs in (0..=60).step_by(2) {
            assert!(
                history
                    .observe(
                        "group",
                        Some(&sample(false)),
                        at + Duration::from_secs(secs),
                        Duration::from_secs(6)
                    )
                    .is_none()
            );
        }
        for secs in (62..92).step_by(2) {
            assert!(
                history
                    .observe(
                        "group",
                        Some(&sample(true)),
                        at + Duration::from_secs(secs),
                        Duration::from_secs(6)
                    )
                    .is_none()
            );
        }
        let evidence = history
            .observe(
                "group",
                Some(&sample(true)),
                at + Duration::from_secs(92),
                Duration::from_secs(6),
            )
            .unwrap();
        assert_eq!(evidence.seconds, 30.0);
        assert_eq!(evidence.baseline_ms, Some(10.0));
        history.observe(
            "group",
            None,
            at + Duration::from_secs(94),
            Duration::from_secs(6),
        );
        assert!(
            history
                .observe(
                    "group",
                    Some(&sample(true)),
                    at + Duration::from_secs(96),
                    Duration::from_secs(6)
                )
                .is_none()
        );
        assert!(
            history
                .observe(
                    "different group",
                    Some(&sample(true)),
                    at + Duration::from_secs(98),
                    Duration::from_secs(6)
                )
                .is_none()
        );
        assert_eq!(history.samples, 1);
    }

    #[test]
    fn cached_allocator_samples_are_not_counted_and_history_is_bounded() {
        let at = Instant::now();
        let mut history = HeadroomHistory::default();
        for _ in 0..1000 {
            let summary = history.observe(
                "target",
                point(at, 200 * GIB, Some(true)),
                Duration::from_secs(16),
            );
            assert_eq!(summary.samples, 1);
            assert!(!summary.alert);
        }
        for secs in 1..=1200 {
            let summary = history.observe(
                "target",
                point(at + Duration::from_secs(secs), 200 * GIB, Some(true)),
                Duration::from_secs(16),
            );
            assert!(summary.samples <= 61);
            assert!(summary.seconds <= 600.0);
        }
    }

    #[test]
    fn declining_headroom_requires_pressure_and_continued_decline_then_resolves_on_recovery() {
        let at = Instant::now();
        let mut history = HeadroomHistory::default();
        for i in 0..=60 {
            let summary = history.observe(
                "target",
                point(
                    at + Duration::from_secs(i * 10),
                    (200 - i * 2) * GIB,
                    Some(true),
                ),
                Duration::from_secs(16),
            );
            if i < 12 {
                assert!(!summary.alert);
            } else {
                assert!(summary.alert);
            }
        }
        let recovered = history.observe(
            "target",
            point(at + Duration::from_secs(610), 200 * GIB, Some(false)),
            Duration::from_secs(16),
        );
        assert!(!recovered.alert);
        let reset = history.observe(
            "changed-policy",
            point(at + Duration::from_secs(620), 180 * GIB, Some(true)),
            Duration::from_secs(16),
        );
        assert_eq!(reset.direction, "warming up");
        assert_eq!(reset.samples, 1);
    }

    #[test]
    fn one_off_drops_unknown_pressure_and_gaps_do_not_create_sustained_warnings() {
        let at = Instant::now();
        for pressure in [Some(false), None] {
            let mut history = HeadroomHistory::default();
            for i in 0..=30 {
                let summary = history.observe(
                    "target",
                    point(at + Duration::from_secs(i * 10), (100 - i) * GIB, pressure),
                    Duration::from_secs(16),
                );
                assert!(!summary.alert);
            }
        }
        let mut history = HeadroomHistory::default();
        history.observe(
            "target",
            point(at, 200 * GIB, Some(true)),
            Duration::from_secs(16),
        );
        for i in 1..=30 {
            let summary = history.observe(
                "target",
                point(at + Duration::from_secs(i * 10), 100 * GIB, Some(true)),
                Duration::from_secs(16),
            );
            assert!(!summary.alert);
        }
        let summary = history.observe(
            "target",
            point(at + Duration::from_secs(400), 90 * GIB, Some(true)),
            Duration::from_secs(16),
        );
        assert_eq!(summary.samples, 1);
    }
}
