mod advisor;
mod app;
mod metrics;
mod sysfs;
mod tuning;
mod ui;

use app::App;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "nasty-top", about = "Interactive bcachefs tuning workbench")]
struct Cli {
    /// Filesystem name or UUID to monitor (defaults to first discovered).
    #[arg(long, short)]
    filesystem: Option<String>,

    /// Refresh interval in seconds.
    #[arg(long, short = 't', default_value = "2")]
    interval: f64,
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

    eprintln!("Monitoring: {} ({}) [{}/{}]",
        filesystems[fs_index].fs_name, filesystems[fs_index].uuid,
        fs_index + 1, filesystems.len());

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(filesystems, fs_index);
    let tick_dur = Duration::from_secs_f64(cli.interval);

    // Main loop
    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        // Poll for events with timeout = tick interval
        if event::poll(tick_dur)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                // If editing an option, send keys to the edit buffer
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
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') => app.should_quit = true,
                    KeyCode::Char('y') | KeyCode::Char('Y') => app.apply_proposal(),
                    KeyCode::Char('n') | KeyCode::Char('N') => app.dismiss_proposal(),
                    KeyCode::Char('!') => app.dismiss_permanent(),
                    KeyCode::Char('C') => app.clear_dismissals(),
                    KeyCode::Char('c') => {
                        app.show_counters = !app.show_counters;
                        if app.show_counters { app.show_processes = false; app.show_blocked = false; }
                    }
                    KeyCode::Char('r') => app.toggle_reconcile(),
                    KeyCode::Char('t') => {
                        app.show_blocked = !app.show_blocked;
                        if app.show_blocked { app.show_processes = false; app.show_counters = false; }
                    }
                    KeyCode::Char('p') => {
                        app.show_processes = !app.show_processes;
                        if app.show_processes {
                            app.show_blocked = false;
                            app.show_counters = false;
                            // Reset baseline so first tick shows rates
                            app.prev_proc_io = sysfs::read_all_process_io();
                        }
                    }
                    KeyCode::Char('o') => {
                        app.show_options = !app.show_options;
                        if !app.show_options {
                            app.focus = app::Focus::Metrics;
                        }
                    }
                    KeyCode::Tab => app.toggle_focus(),
                    KeyCode::Up | KeyCode::Char('k') => {
                        if matches!(app.focus, app::Focus::Tuning) {
                            app.tuning.scroll_up();
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if matches!(app.focus, app::Focus::Tuning) {
                            app.tuning.scroll_down();
                        }
                    }
                    KeyCode::Enter => app.handle_enter(),
                    KeyCode::Char('f') => app.switch_fs(),
                    KeyCode::Esc => {
                        app.status_msg = None;
                    }
                    _ => {}
                }
            }
        } else {
            // Tick — no event within interval
            app.tick();
        }

        if app.should_quit {
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
