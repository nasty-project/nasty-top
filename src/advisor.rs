//! Rank diagnostic evidence and snapshot constraints for the footer.

use crate::app::App;
use crate::diagnostics::Status;
use crate::targets::Severity;

#[derive(Debug, Clone)]
pub struct Proposal {
    pub reason: String,
    pub id: String,
    pub severity: Severity,
    pub detail: String,
    pub command: Option<String>,
}

pub fn evaluate(app: &App) -> Option<Proposal> {
    let mut proposals: Vec<_> = app
        .diagnostics
        .findings
        .values()
        .filter(|d| d.status == Status::Active)
        .map(|d| Proposal {
            id: d.id.clone(),
            reason: d.summary.clone(),
            severity: d.severity,
            detail: d.action.clone(),
            command: None,
        })
        .collect();
    proposals.extend(snapshot_proposals(app));
    select_proposal(proposals, |id| app.is_dismissed(id))
}

/// These are snapshot observations, not claims of sustained activity. Retain
/// all of them in the Advisor view, including muted footer hints.
pub fn snapshot_proposals(app: &App) -> Vec<Proposal> {
    let mut proposals: Vec<_> = app
        .target_reports
        .iter()
        .flat_map(|r| &r.findings)
        .map(|f| Proposal {
            id: f.id.clone(),
            reason: f.summary.clone(),
            severity: f.severity,
            detail: f.detail.clone(),
            command: None,
        })
        .collect();
    let copygc_on = app
        .current
        .options
        .get("copygc_enabled")
        .is_some_and(|v| v == "1");
    let has_write_stalls = app
        .stall_events
        .iter()
        .any(|e| e.direction == "write" && e.time.elapsed().as_secs() < 60);
    if copygc_stall_pressure(copygc_on, has_write_stalls, &app.current.background) {
        proposals.push(Proposal {
            id: "copygc-pressure".into(), severity: Severity::Warning,
            reason: "Write stalls coincide with copygc — inspect member headroom in Targets [v]".into(),
            detail: "Recent write stalls and current copygc activity coincide. Inspect target headroom; this does not prove that GC caused the stalls or that disabling it would help.".into(),
            command: None,
        });
    }
    proposals
}

fn select_proposal(
    mut proposals: Vec<Proposal>,
    dismissed: impl Fn(&str) -> bool,
) -> Option<Proposal> {
    proposals.sort_by_key(|p| std::cmp::Reverse(p.severity));
    proposals.into_iter().find(|p| !dismissed(&p.id))
}

fn copygc_stall_pressure(
    copygc_enabled: bool,
    has_recent_write_stalls: bool,
    background: &[(String, String)],
) -> bool {
    copygc_enabled
        && has_recent_write_stalls
        && background
            .iter()
            .any(|(name, state)| name == "copygc" && state.starts_with("working"))
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
                detail: String::new(),
                command: None,
            },
            Proposal {
                id: "metadata:gc:31".into(),
                severity: Severity::Warning,
                reason: "pressure".into(),
                detail: String::new(),
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
            assert!(!copygc_stall_pressure(
                true,
                true,
                &[("copygc".into(), state.into())]
            ));
        }
        let background = vec![("copygc".into(), "working".into())];
        assert!(copygc_stall_pressure(true, true, &background));
        assert!(!copygc_stall_pressure(false, true, &background));
        assert!(!copygc_stall_pressure(true, false, &background));
        assert!(!copygc_stall_pressure(
            true,
            true,
            &[("rebalance".into(), "working".into())]
        ));
    }
}
