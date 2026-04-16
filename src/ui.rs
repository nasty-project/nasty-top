//! TUI rendering with ratatui.

use crate::app::{App, Focus};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Row, Sparkline, Table, Wrap,
};
use ratatui::Frame;

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(10),  // body
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_body(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let fs = &app.fs;
    let snap = &app.current;

    let used_gb = snap.space_used as f64 / (1024.0 * 1024.0 * 1024.0);
    let total_gb = snap.space_total as f64 / (1024.0 * 1024.0 * 1024.0);
    let pct = if snap.space_total > 0 {
        snap.space_used as f64 / snap.space_total as f64 * 100.0
    } else {
        0.0
    };

    let replicas = snap
        .options
        .get("data_replicas")
        .map(|s| s.as_str())
        .unwrap_or("?");
    let compression = snap
        .options
        .get("compression")
        .map(|s| s.as_str())
        .unwrap_or("none");

    let title = format!(
        " nasty-top │ {} ({}) │ {:.1} GiB / {:.1} GiB ({:.1}%) │ {}x {}",
        fs.fs_name, fs.mount_point, used_gb, total_gb, pct, replicas, compression
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let para = Paragraph::new(title).block(block);
    f.render_widget(para, area);
}

fn draw_body(f: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(60), // metrics
            Constraint::Percentage(40), // tuning
        ])
        .split(area);

    draw_metrics_panel(f, app, columns[0]);
    draw_tuning_panel(f, app, columns[1]);
}

fn draw_metrics_panel(f: &mut Frame, app: &App, area: Rect) {
    let focus_style = if matches!(app.focus, Focus::Metrics) {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),  // throughput sparklines
            Constraint::Length(5),  // latency sparklines
            Constraint::Min(5),    // device table
            Constraint::Length(4), // background ops
        ])
        .split(area);

    // ── IO throughput sparklines ──
    {
        let block = Block::default()
            .title(" IO Throughput ")
            .borders(Borders::ALL)
            .border_style(focus_style);

        let inner = block.inner(chunks[0]);
        f.render_widget(block, chunks[0]);

        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);

        let read_data = sparkline_u64(app.history.get("io_read_bytes_sec"));
        let read_rate = app.history.get("io_read_bytes_sec").last().copied().unwrap_or(0.0);
        let read_spark = Sparkline::default()
            .block(Block::default().title(format!("Read: {}/s", format_bytes(read_rate))))
            .data(&read_data)
            .style(Style::default().fg(Color::Green));
        f.render_widget(read_spark, halves[0]);

        let write_data = sparkline_u64(app.history.get("io_write_bytes_sec"));
        let write_rate = app.history.get("io_write_bytes_sec").last().copied().unwrap_or(0.0);
        let write_spark = Sparkline::default()
            .block(Block::default().title(format!("Write: {}/s", format_bytes(write_rate))))
            .data(&write_data)
            .style(Style::default().fg(Color::Blue));
        f.render_widget(write_spark, halves[1]);
    }

    // ── Latency sparklines ──
    {
        let block = Block::default()
            .title(" IO Latency ")
            .borders(Borders::ALL)
            .border_style(focus_style);

        let inner = block.inner(chunks[1]);
        f.render_widget(block, chunks[1]);

        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);

        let read_data = sparkline_u64(app.history.get("avg_read_latency_us"));
        let read_lat = app.history.get("avg_read_latency_us").last().copied().unwrap_or(0.0);
        let read_spark = Sparkline::default()
            .block(Block::default().title(format!("Read: {:.0} µs", read_lat)))
            .data(&read_data)
            .style(Style::default().fg(latency_color(read_lat)));
        f.render_widget(read_spark, halves[0]);

        let write_data = sparkline_u64(app.history.get("avg_write_latency_us"));
        let write_lat = app.history.get("avg_write_latency_us").last().copied().unwrap_or(0.0);
        let write_spark = Sparkline::default()
            .block(Block::default().title(format!("Write: {:.0} µs", write_lat)))
            .data(&write_data)
            .style(Style::default().fg(latency_color(write_lat)));
        f.render_widget(write_spark, halves[1]);
    }

    // ── Device table ──
    {
        let block = Block::default()
            .title(" Devices ")
            .borders(Borders::ALL)
            .border_style(focus_style);

        let header = Row::new(vec!["Device", "Label", "Read/s", "Write/s", "R Lat", "W Lat", "Errors"])
            .style(Style::default().add_modifier(Modifier::BOLD))
            .bottom_margin(0);

        let rows: Vec<Row> = if let Some(rates) = &app.rates {
            rates
                .devices
                .iter()
                .map(|d| {
                    let err_style = if d.io_errors > 0 {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default()
                    };
                    Row::new(vec![
                        d.name.clone(),
                        d.label.clone().unwrap_or_default(),
                        format_bytes(d.read_bytes_sec),
                        format_bytes(d.write_bytes_sec),
                        format_latency(d.read_latency_ns),
                        format_latency(d.write_latency_ns),
                        format!("{}", d.io_errors),
                    ])
                    .style(err_style)
                })
                .collect()
        } else {
            vec![]
        };

        let widths = [
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(7),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(block);
        f.render_widget(table, chunks[2]);
    }

    // ── Background ops ──
    {
        let block = Block::default()
            .title(" Background ")
            .borders(Borders::ALL)
            .border_style(focus_style);

        let bg = &app.current.background;
        let lines: Vec<Line> = bg
            .iter()
            .map(|(k, v)| {
                let short_v = if v.len() > 60 { &v[..60] } else { v };
                Line::from(vec![
                    Span::styled(
                        format!("{k}: "),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(short_v.to_string()),
                ])
            })
            .collect();

        let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
        f.render_widget(para, chunks[3]);
    }
}

fn draw_tuning_panel(f: &mut Frame, app: &App, area: Rect) {
    let focus_style = if matches!(app.focus, Focus::Tuning) {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),    // options list
            Constraint::Length(5), // markers
        ])
        .split(area);

    // ── Options list ──
    {
        let block = Block::default()
            .title(" Options (Enter to edit) ")
            .borders(Borders::ALL)
            .border_style(focus_style);

        let tuning = &app.tuning;
        let options = &app.current.options;

        let rows: Vec<Row> = tuning
            .option_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let value = options.get(name).map(|s| s.as_str()).unwrap_or("?");

                let display = if tuning.editing && i == tuning.selected {
                    format!("{}▏", tuning.edit_buf)
                } else {
                    value.to_string()
                };

                let style = if i == tuning.selected && matches!(app.focus, Focus::Tuning) {
                    if tuning.editing {
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::REVERSED)
                    }
                } else {
                    Style::default()
                };

                Row::new(vec![name.clone(), display]).style(style)
            })
            .collect();

        let widths = [Constraint::Percentage(55), Constraint::Percentage(45)];
        let table = Table::new(rows, widths).block(block);
        f.render_widget(table, chunks[0]);
    }

    // ── Markers ──
    {
        let block = Block::default()
            .title(" Markers [m]save [1-9]load ")
            .borders(Borders::ALL)
            .border_style(focus_style);

        let lines: Vec<Line> = (0..3)
            .map(|i| {
                let label = app
                    .tuning
                    .markers
                    .get(i)
                    .and_then(|m| m.as_ref())
                    .map(|m| format!("[{}] {}", i + 1, m.label))
                    .unwrap_or_else(|| format!("[{}] (empty)", i + 1));
                Line::from(label)
            })
            .collect();

        let para = Paragraph::new(lines).block(block);
        f.render_widget(para, chunks[1]);
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let status = if let Some(ref msg) = app.status_msg {
        msg.clone()
    } else {
        "[Tab] switch panel  [↑↓] navigate  [Enter] edit  [m] mark  [Esc] cancel  [q] quit"
            .to_string()
    };
    let para = Paragraph::new(status).style(Style::default().fg(Color::DarkGray));
    f.render_widget(para, area);
}

// ── Helpers ──

fn sparkline_u64(data: &[f64]) -> Vec<u64> {
    data.iter().map(|v| *v as u64).collect()
}

fn latency_color(us: f64) -> Color {
    if us < 1000.0 {
        Color::Green
    } else if us < 10000.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn format_bytes(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1_000_000_000.0 {
        format!("{:.1} GB", bytes_per_sec / 1_000_000_000.0)
    } else if bytes_per_sec >= 1_000_000.0 {
        format!("{:.1} MB", bytes_per_sec / 1_000_000.0)
    } else if bytes_per_sec >= 1_000.0 {
        format!("{:.1} KB", bytes_per_sec / 1_000.0)
    } else {
        format!("{:.0} B", bytes_per_sec)
    }
}

fn format_latency(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.1} s", ns as f64 / 1_000_000_000.0)
    } else if ns >= 1_000_000 {
        format!("{:.1} ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.0} µs", ns as f64 / 1_000.0)
    } else {
        format!("{} ns", ns)
    }
}
