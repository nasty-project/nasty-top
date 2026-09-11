mod advisor;
mod app;
mod diagnostics;
mod metrics;
mod peers;
mod sysfs;
mod targets;
mod theme;
mod topology;
mod trends;
mod tuning;
mod ui;

use app::App;
use clap::Parser;
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::time::{Duration, Instant};

const MAX_INTERVAL_SECS: f64 = 86_400.0;

#[derive(Parser)]
#[command(
    name = "nasty-top",
    about = "A top-like TUI for bcachefs filesystems",
    version
)]
struct Cli {
    /// Filesystem name or UUID to monitor (defaults to first discovered).
    #[arg(long, short)]
    filesystem: Option<String>,

    /// Refresh interval in seconds.
    #[arg(long, short = 't', default_value = "2", value_parser = parse_interval)]
    interval: f64,
}

fn parse_interval(value: &str) -> Result<f64, String> {
    let interval = value
        .parse::<f64>()
        .map_err(|_| "interval must be a number".to_string())?;
    if !interval.is_finite()
        || interval <= 0.0
        || interval > MAX_INTERVAL_SECS
        || Duration::from_secs_f64(interval).is_zero()
    {
        return Err(format!(
            "interval must be at least 1 nanosecond and at most {MAX_INTERVAL_SECS} seconds"
        ));
    }
    Ok(interval)
}

struct TerminalSession {
    alternate_screen: bool,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        let mut session = Self {
            alternate_screen: false,
        };
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        session.alternate_screen = true;
        execute!(stdout, EnterAlternateScreen)?;
        Ok(session)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.alternate_screen {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, LeaveAlternateScreen, Show);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Discover filesystems
    let filesystems = sysfs::discover();
    if filesystems.is_empty() {
        eprintln!("No mounted bcachefs filesystems found.");
        std::process::exit(1);
    }

    let fs_index = if let Some(ref target) = cli.filesystem {
        filesystems
            .iter()
            .position(|f| f.fs_name == *target || f.uuid == *target)
            .unwrap_or_else(|| {
                eprintln!("Filesystem '{target}' not found.");
                std::process::exit(1);
            })
    } else {
        0
    };

    eprintln!(
        "Monitoring: {} ({}) [{}/{}]",
        filesystems[fs_index].fs_name,
        filesystems[fs_index].uuid,
        fs_index + 1,
        filesystems.len()
    );

    // Setup terminal. The guard restores raw mode and the alternate screen on
    // every return path, including errors and unwinding panics.
    let _terminal_session = TerminalSession::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(filesystems, fs_index);
    let tick_dur = Duration::from_secs_f64(cli.interval);
    app.expected_interval = tick_dur;
    run(&mut terminal, &mut app, tick_dur)?;

    Ok(())
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    tick_dur: Duration,
) -> io::Result<()> {
    let mut next_tick = Instant::now() + tick_dur;

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Poll only until the current deadline. Input never resets the timer,
        // so sustained key activity cannot starve metric refreshes.
        let timeout = next_tick.saturating_duration_since(Instant::now());
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && handle_key(app, key)
        {
            next_tick = Instant::now() + tick_dur;
        }

        if app.should_quit {
            break;
        }

        if Instant::now() >= next_tick {
            app.tick();
            next_tick = Instant::now() + tick_dur;
        }
    }

    Ok(())
}

/// Handle one key press. Returns true when the metric deadline should restart.
fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    // Ctrl-C quits from any mode (including option edit).
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        app.should_quit = true;
        return false;
    }

    // If editing an option, send keys to the edit buffer.
    if app.tuning.editing {
        match key.code {
            KeyCode::Enter => app.handle_enter(),
            KeyCode::Esc => app.tuning.cancel_edit(),
            KeyCode::Backspace => {
                app.tuning.edit_buf.pop();
            }
            KeyCode::Char(c) => app.tuning.edit_buf.push(c),
            _ => {}
        }
        return false;
    }

    if app.inspected_hint.is_some() {
        match key.code {
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Esc | KeyCode::Char('w') => app.inspected_hint = None,
            KeyCode::Up | KeyCode::Char('k') => app.hint_scroll = app.hint_scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                app.hint_scroll = app.hint_scroll.saturating_add(1)
            }
            KeyCode::PageUp => app.hint_scroll = app.hint_scroll.saturating_sub(10),
            KeyCode::PageDown => app.hint_scroll = app.hint_scroll.saturating_add(10),
            KeyCode::Home => app.hint_scroll = 0,
            _ => {}
        }
        return false;
    }

    let mut reset_tick_deadline = false;
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.show_help = !app.show_help,
        KeyCode::Char('w') => app.explain_hint(),
        KeyCode::Char('n') | KeyCode::Char('N') => app.dismiss_proposal(),
        KeyCode::Char('!') => app.dismiss_permanent(),
        KeyCode::Char('C') => app.clear_dismissals(),
        KeyCode::Char('c') => {
            app.show_counters = !app.show_counters;
            if app.show_counters {
                app.show_processes = false;
                app.show_blocked = false;
                app.show_targets = false;
                app.show_advisor = false;
            }
            app.view_scroll = 0;
        }
        KeyCode::Char('r') => app.toggle_option("reconcile_enabled"),
        KeyCode::Char('a') => {
            app.show_advisor = !app.show_advisor;
            if app.show_advisor {
                app.show_counters = false;
                app.show_blocked = false;
                app.show_processes = false;
                app.show_targets = false;
            }
            app.view_scroll = 0;
        }
        KeyCode::Char('v') => {
            app.show_targets = !app.show_targets;
            if app.show_targets {
                app.show_counters = false;
                app.show_blocked = false;
                app.show_processes = false;
                app.show_advisor = false;
            }
            app.view_scroll = 0;
        }
        KeyCode::Char('g') => app.toggle_option("copygc_enabled"),
        KeyCode::Char('t') => {
            app.show_blocked = !app.show_blocked;
            if app.show_blocked {
                app.show_processes = false;
                app.show_counters = false;
                app.show_targets = false;
                app.show_advisor = false;
            }
            app.view_scroll = 0;
        }
        KeyCode::Char('p') => {
            app.show_processes = !app.show_processes;
            if app.show_processes {
                app.show_blocked = false;
                app.show_counters = false;
                app.show_targets = false;
                app.show_advisor = false;
                app.view_scroll = 0;
                // Reset baseline so first tick shows rates.
                app.prev_proc_io = sysfs::read_all_process_io();
            }
        }
        KeyCode::Char('o') => app.toggle_options(),
        KeyCode::Char('s') => app.toggle_device_sort(),
        KeyCode::Tab => app.toggle_focus(),
        KeyCode::Up | KeyCode::Char('k') => {
            if matches!(app.focus, app::Focus::Tuning) {
                app.tuning.scroll_up();
            } else if app.show_counters
                || app.show_blocked
                || app.show_processes
                || app.show_targets
                || app.show_advisor
            {
                app.view_scroll = app.view_scroll.saturating_sub(1);
            } else {
                app.scroll_devices_up();
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if matches!(app.focus, app::Focus::Tuning) {
                app.tuning.scroll_down();
            } else if app.show_counters
                || app.show_blocked
                || app.show_processes
                || app.show_targets
                || app.show_advisor
            {
                app.view_scroll += 1;
            } else {
                app.scroll_devices_down();
            }
        }
        KeyCode::Enter => app.handle_enter(),
        KeyCode::Char('f') => reset_tick_deadline = app.switch_fs(),
        KeyCode::Esc => {
            app.clear_status();
        }
        _ => {}
    }
    reset_tick_deadline
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn why_hint_keeps_the_selected_inputs_frozen_and_has_its_own_navigation() {
        let mut app = App::new(
            vec![sysfs::BcachefsFs {
                uuid: "test".into(),
                mount_point: "/".into(),
                fs_name: "test".into(),
                sysfs: "/nonexistent-nasty-top-test".into(),
            }],
            0,
        );
        app.proposal = Some(advisor::Proposal {
            id: "gc".into(),
            reason: "GC pressure".into(),
            severity: targets::Severity::Warning,
            detail: "Inspect allocation".into(),
            command: None,
            criteria: "calculated_wait <= 0".into(),
            evidence: vec!["calculated_wait=-292M".into()],
        });
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
        );
        app.proposal.as_mut().unwrap().evidence[0] = "calculated_wait=10G".into();
        assert_eq!(
            app.inspected_hint.as_ref().unwrap().evidence[0],
            "calculated_wait=-292M"
        );
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        );
        assert_eq!(app.hint_scroll, 10);
        assert_eq!(app.view_scroll, 0);
        handle_key(&mut app, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.hint_scroll, 0);
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.inspected_hint.is_none());
        app.proposal = None;
        app.show_help = true;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
        );
        assert!(app.inspected_hint.is_none());
        assert!(!app.show_help);
        assert!(
            app.status_msg
                .as_deref()
                .unwrap()
                .contains("No current hint")
        );
    }

    #[test]
    fn advisor_navigation_is_exclusive_with_other_detail_views() {
        let mut app = App::new(
            vec![sysfs::BcachefsFs {
                uuid: "test".into(),
                mount_point: "/".into(),
                fs_name: "test".into(),
                sysfs: "/nonexistent-nasty-top-test".into(),
            }],
            0,
        );
        app.show_counters = true;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert!(app.show_advisor);
        assert!(!app.show_counters);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
        assert_eq!(app.view_scroll, 1);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        );
        assert!(app.show_targets);
        assert!(!app.show_advisor);
        assert_eq!(app.view_scroll, 0);
    }

    #[test]
    fn interval_must_be_finite_positive_and_bounded() {
        assert_eq!(parse_interval("0.25"), Ok(0.25));
        assert_eq!(parse_interval("86400"), Ok(86_400.0));

        for invalid in ["0", "-1", "1e-300", "NaN", "inf", "86401", "not-a-number"] {
            assert!(parse_interval(invalid).is_err(), "accepted {invalid}");
        }

        assert!(Cli::try_parse_from(["nasty-top", "--interval", "0"]).is_err());
        assert_eq!(
            Cli::try_parse_from(["nasty-top", "--interval", "0.25"])
                .unwrap()
                .interval,
            0.25
        );
    }
}
