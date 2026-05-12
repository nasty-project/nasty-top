//! Tuning advisor — surfaces informational hints when known bcachefs
//! pressure signals fire. Hints are advisory only; the user decides
//! whether to act on them.

use crate::app::App;

/// An advisory hint about a pressure signal and a candidate sysfs change.
#[derive(Debug, Clone)]
pub struct Proposal {
    /// Human-readable reason the hint fired.
    pub reason: String,
    /// The sysfs option the hint references (used as the dismissal key).
    pub option: String,
    /// Example command the user could run themselves. Shown dimmed —
    /// nasty-top does not apply it automatically.
    pub command: String,
}

/// Analyze current app state and return the highest-priority proposal, if any.
pub fn evaluate(app: &App) -> Option<Proposal> {
    let proposal = evaluate_inner(app);
    // Filter out dismissed proposals
    proposal.filter(|p| !app.is_dismissed(&p.option))
}

fn evaluate_inner(app: &App) -> Option<Proposal> {
    let opts = &app.current.options;
    let sysfs_base = format!("/sys/fs/bcachefs/{}/options", app.fs.uuid);

    // Rule 1: Journal fill > 80% → lower journal_reclaim_delay
    let (jdirty, jtotal) = app.current.journal_fill;
    let jpct = if jtotal > 0 { jdirty as f64 / jtotal as f64 * 100.0 } else { 0.0 };
    if jpct > 80.0 {
        let current: u64 = opts.get("journal_reclaim_delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        if current > 10 {
            let new_val = (current / 2).max(10);
            return Some(Proposal {
                reason: format!("Journal {:.0}% full — reclaim faster", jpct),
                option: "journal_reclaim_delay".into(),
                command: format!("echo {} > {}/journal_reclaim_delay", new_val, sysfs_base),
            });
        }
    }

    // Rule 2: Journal fill > 50% with watermark not "normal" → lower flush delay
    if jpct > 50.0 && app.current.journal_watermark != "stripe" {
        let current: u64 = opts.get("journal_flush_delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        if current > 100 {
            let new_val = (current / 2).max(100);
            return Some(Proposal {
                reason: format!("Journal {:.0}% full (watermark: {}) — flush more often", jpct, app.current.journal_watermark),
                option: "journal_flush_delay".into(),
                command: format!("echo {} > {}/journal_flush_delay", new_val, sysfs_base),
            });
        }
    }

    // Helper: check if a blocked counter is actively increasing (not just historical)
    let blocked_delta = |name: &str| -> Option<(u64, f64)> {
        let curr = app.current.blocked_stats.iter().find(|(n, _, _)| n == name)?;
        let prev = app.previous.as_ref()?.blocked_stats.iter().find(|(n, _, _)| n == name)?;
        let delta = curr.1.saturating_sub(prev.1);
        if delta > 0 { Some((delta, curr.2)) } else { None }
    };

    // Rule 2b: blocked_journal_low_on_space actively increasing → lower journal_flush_delay
    if let Some((delta, recent_us)) = blocked_delta("journal_low_on_space") {
        let current: u64 = opts.get("journal_flush_delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        if current > 100 {
            let new_val = (current / 2).max(100);
            return Some(Proposal {
                reason: format!("journal_low_on_space +{} blocks (mean {:.1}ms)", delta, recent_us / 1000.0),
                option: "journal_flush_delay".into(),
                command: format!("echo {} > {}/journal_flush_delay", new_val, sysfs_base),
            });
        }
    }

    // Rule 2c: blocked_write_buffer_full actively increasing → lower journal_flush_delay
    if let Some((delta, recent_us)) = blocked_delta("write_buffer_full") {
        let current: u64 = opts.get("journal_flush_delay")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        if current > 100 {
            let new_val = (current / 2).max(100);
            return Some(Proposal {
                reason: format!("write_buffer_full +{} blocks (mean {:.1}ms)", delta, recent_us / 1000.0),
                option: "journal_flush_delay".into(),
                command: format!("echo {} > {}/journal_flush_delay", new_val, sysfs_base),
            });
        }
    }

    // Rule 2d: blocked_allocate actively increasing → increase gc_reserve_percent
    if let Some((delta, recent_us)) = blocked_delta("allocate") {
        let gc_pct: u64 = opts.get("gc_reserve_percent")
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);
        if gc_pct < 20 {
            let new_val = (gc_pct + 4).min(20);
            return Some(Proposal {
                reason: format!("allocate +{} blocks (mean {:.1}ms)", delta, recent_us / 1000.0),
                option: "gc_reserve_percent".into(),
                command: format!("echo {} > {}/gc_reserve_percent", new_val, sysfs_base),
            });
        }
    }

    // Rule 3: Write stalls while copygc is running → disable copygc
    let copygc_on = opts.get("copygc_enabled").map(|v| v == "1").unwrap_or(false);
    let has_write_stalls = app.stall_events.iter().any(|e| {
        e.direction == "write" && e.time.elapsed().as_secs() < 60
    });
    if has_write_stalls && copygc_on {
        let copygc_active = app.current.background.iter()
            .any(|(k, v)| k == "copygc" && v.contains("on"));
        if copygc_active {
            return Some(Proposal {
                reason: "Write stalls while copygc active — disable copygc".into(),
                option: "copygc_enabled".into(),
                command: format!("echo 0 > {}/copygc_enabled", sysfs_base),
            });
        }
    }

    None
}
