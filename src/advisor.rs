//! Tuning advisor — surfaces informational hints when known bcachefs
//! pressure signals fire. Hints are advisory only; the user decides
//! whether to act on them.

use crate::app::App;
use crate::targets::Severity;

/// An advisory finding. Not every constraint has a candidate sysfs change.
#[derive(Debug, Clone)]
pub struct Proposal {
    /// Human-readable reason the hint fired.
    pub reason: String,
    /// Stable rule/target/device identity, also used as the dismissal key.
    pub id: String,
    pub severity: Severity,
    /// Example command the user could run themselves. Shown dimmed —
    /// nasty-top does not apply it automatically.
    pub command: Option<String>,
}

/// Analyze current app state and return the highest-priority proposal, if any.
pub fn evaluate(app: &App) -> Option<Proposal> {
    let mut proposals: Vec<_> = app
        .target_reports
        .iter()
        .flat_map(|r| &r.findings)
        .map(|finding| Proposal {
            id: finding.id.clone(),
            reason: finding.summary.clone(),
            severity: finding.severity,
            command: None,
        })
        .collect();
    proposals.extend(evaluate_inner(app));
    select_proposal(proposals, |id| app.is_dismissed(id))
}

fn select_proposal(
    mut proposals: Vec<Proposal>,
    dismissed: impl Fn(&str) -> bool,
) -> Option<Proposal> {
    proposals.sort_by_key(|p| std::cmp::Reverse(p.severity));
    // Dismiss before selection, so muting one problem reveals the next one.
    proposals.into_iter().find(|p| !dismissed(&p.id))
}

fn evaluate_inner(app: &App) -> Vec<Proposal> {
    let mut proposals = Vec::new();
    let opts = &app.current.options;
    let sysfs_base = format!("/sys/fs/bcachefs/{}/options", app.fs.uuid);

    // Rule 1: Journal fill > 80% → lower journal_reclaim_delay
    let (jdirty, jtotal) = app.current.journal_fill;
    let jpct = if jtotal > 0 {
        jdirty as f64 / jtotal as f64 * 100.0
    } else {
        0.0
    };
    if jpct > 80.0 {
        let current: u64 = opts
            .get("journal_reclaim_delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        if current > 10 {
            let new_val = (current / 2).max(10);
            proposals.push(Proposal {
                reason: format!("Journal {:.0}% full — reclaim faster", jpct),
                id: "journal_reclaim_delay".into(),
                severity: Severity::Warning,
                command: Some(format!(
                    "echo {} > {}/journal_reclaim_delay",
                    new_val, sysfs_base
                )),
            });
        }
    }

    // Rule 2: Journal fill > 50% with watermark not "normal" → lower flush delay
    if jpct > 50.0 && app.current.journal_watermark != "stripe" {
        let current: u64 = opts
            .get("journal_flush_delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        if current > 100 {
            let new_val = (current / 2).max(100);
            proposals.push(Proposal {
                reason: format!(
                    "Journal {:.0}% full (watermark: {}) — flush more often",
                    jpct, app.current.journal_watermark
                ),
                id: "journal_flush_delay".into(),
                severity: Severity::Warning,
                command: Some(format!(
                    "echo {} > {}/journal_flush_delay",
                    new_val, sysfs_base
                )),
            });
        }
    }

    // Helper: check if a blocked counter is actively increasing (not just historical)
    let blocked_delta = |name: &str| -> Option<(u64, f64)> {
        let curr = app
            .current
            .blocked_stats
            .iter()
            .find(|(n, _, _)| n == name)?;
        let prev = app
            .previous
            .as_ref()?
            .blocked_stats
            .iter()
            .find(|(n, _, _)| n == name)?;
        let delta = curr.1.saturating_sub(prev.1);
        if delta > 0 {
            Some((delta, curr.2))
        } else {
            None
        }
    };

    // Rule 2b: blocked_journal_low_on_space actively increasing → lower journal_flush_delay
    if let Some((delta, recent_us)) = blocked_delta("journal_low_on_space") {
        let current: u64 = opts
            .get("journal_flush_delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        if current > 100 {
            let new_val = (current / 2).max(100);
            proposals.push(Proposal {
                reason: format!(
                    "journal_low_on_space +{} blocks (mean {:.1}ms)",
                    delta,
                    recent_us / 1000.0
                ),
                id: "journal_flush_delay".into(),
                severity: Severity::Warning,
                command: Some(format!(
                    "echo {} > {}/journal_flush_delay",
                    new_val, sysfs_base
                )),
            });
        }
    }

    // Rule 2c: blocked_write_buffer_full actively increasing → lower journal_flush_delay
    if let Some((delta, recent_us)) = blocked_delta("write_buffer_full") {
        let current: u64 = opts
            .get("journal_flush_delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        if current > 100 {
            let new_val = (current / 2).max(100);
            proposals.push(Proposal {
                reason: format!(
                    "write_buffer_full +{} blocks (mean {:.1}ms)",
                    delta,
                    recent_us / 1000.0
                ),
                id: "journal_flush_delay".into(),
                severity: Severity::Warning,
                command: Some(format!(
                    "echo {} > {}/journal_flush_delay",
                    new_val, sysfs_base
                )),
            });
        }
    }

    // Rule 2d: blocked_allocate actively increasing → increase gc_reserve_percent
    if let Some((delta, recent_us)) = blocked_delta("allocate") {
        let gc_pct: u64 = opts
            .get("gc_reserve_percent")
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);
        if gc_pct < 20 {
            let new_val = (gc_pct + 4).min(20);
            proposals.push(Proposal {
                reason: format!(
                    "allocate +{} blocks (mean {:.1}ms)",
                    delta,
                    recent_us / 1000.0
                ),
                id: "gc_reserve_percent".into(),
                severity: Severity::Warning,
                command: Some(format!(
                    "echo {} > {}/gc_reserve_percent",
                    new_val, sysfs_base
                )),
            });
        }
    }

    // GC may be recovering space that allocation needs. Correlation with write
    // stalls does not establish that disabling it will help.
    let copygc_on = opts
        .get("copygc_enabled")
        .map(|v| v == "1")
        .unwrap_or(false);
    let has_write_stalls = app
        .stall_events
        .iter()
        .any(|e| e.direction == "write" && e.time.elapsed().as_secs() < 60);
    if copygc_stall_pressure(copygc_on, has_write_stalls, &app.current.background) {
        proposals.push(Proposal {
            reason: "Write stalls coincide with copygc — inspect member headroom in Targets [v]"
                .into(),
            id: "copygc-pressure".into(),
            severity: Severity::Warning,
            command: None,
        });
    }

    proposals
}

fn operation_is_working(background: &[(String, String)], operation: &str) -> bool {
    background
        .iter()
        .any(|(name, state)| name == operation && state.starts_with("working"))
}

fn copygc_stall_pressure(
    copygc_enabled: bool,
    has_recent_write_stalls: bool,
    background: &[(String, String)],
) -> bool {
    copygc_enabled && has_recent_write_stalls && operation_is_working(background, "copygc")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_muted_high_priority_finding_does_not_hide_other_constraints() {
        let proposals = vec![
            Proposal {
                id: "metadata:capacity".into(),
                severity: Severity::Critical,
                reason: "capacity".into(),
                command: None,
            },
            Proposal {
                id: "metadata:gc:31".into(),
                severity: Severity::Warning,
                reason: "pressure".into(),
                command: None,
            },
        ];
        let selected = select_proposal(proposals, |id| id == "metadata:capacity").unwrap();
        assert_eq!(selected.id, "metadata:gc:31");
        assert!(selected.command.is_none());
    }

    #[test]
    fn gc_stall_hint_reports_correlation_without_disabling_collection() {
        let mut app = App::new(
            vec![crate::sysfs::BcachefsFs {
                uuid: "test".into(),
                mount_point: "/".into(),
                fs_name: "test".into(),
                sysfs: "/nonexistent-nasty-top-test".into(),
            }],
            0,
        );
        app.current
            .options
            .insert("copygc_enabled".into(), "1".into());
        app.current.background = vec![("copygc".into(), "working".into())];
        app.stall_events.push(crate::app::StallEvent {
            time: std::time::Instant::now(),
            device: "fs".into(),
            direction: "write",
            detail: "slow".into(),
        });
        let proposal = evaluate(&app).unwrap();
        assert_eq!(proposal.id, "copygc-pressure");
        assert!(proposal.command.is_none());
        assert!(proposal.reason.contains("coincide"));
        app.proposal = Some(proposal);
        app.dismiss_permanent();
        assert!(app.is_dismissed("copygc-pressure"));
        app.fs.uuid = "another-fs".into();
        assert!(!app.is_dismissed("copygc-pressure"));
    }

    #[test]
    fn copygc_hint_requires_enabled_working_copygc_and_write_stalls() {
        for state in ["off", "idle", "enabled"] {
            let background = vec![("copygc".to_string(), state.to_string())];
            assert!(!copygc_stall_pressure(true, true, &background));
        }

        let background = vec![("copygc".to_string(), "working".to_string())];
        assert!(copygc_stall_pressure(true, true, &background));
        assert!(!copygc_stall_pressure(false, true, &background));
        assert!(!copygc_stall_pressure(true, false, &background));

        let rebalance = vec![("rebalance".to_string(), "working".to_string())];
        assert!(!copygc_stall_pressure(true, true, &rebalance));
    }
}
