//! TUI rendering with ratatui.

use crate::app::{App, Focus};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Paragraph, Row, Sparkline, Table, Wrap,
};
use ratatui::Frame;

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(10),  // body
            Constraint::Length(2), // footer (proposal can be long)
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
    if app.show_options {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(60), // metrics
                Constraint::Percentage(40), // tuning
            ])
            .split(area);

        draw_metrics_panel(f, app, columns[0]);
        draw_tuning_panel(f, app, columns[1]);
    } else {
        draw_metrics_panel(f, app, area);
    }
}

fn draw_metrics_panel(f: &mut Frame, app: &App, area: Rect) {
    let focus_style = if matches!(app.focus, Focus::Metrics) {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // Table height: header + rows + total/padding + 2 borders
    let dev_count = if app.show_blocked {
        app.current.blocked_stats.len().max(5)
    } else if app.show_processes {
        20
    } else {
        app.rates.as_ref().map(|r| r.devices.len()).unwrap_or(0)
    };
    let dev_height = (dev_count + 4) as u16;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(10),         // sparklines (throughput + latency)
            Constraint::Length(dev_height), // device table
            Constraint::Length(if app.stall_events.is_empty() { 6 } else { 12 }), // background + stalls
        ])
        .split(area);

    // ── Sparklines: 3 columns matching device table layout ──
    {
        let spark_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(13),     // info panel
                Constraint::Percentage(50), // READ sparklines
                Constraint::Percentage(50), // WRITE sparklines
            ])
            .split(chunks[0]);

        // Left: system info
        {
            let block = Block::default()
                .title(" System ")
                .borders(Borders::ALL)
                .border_style(focus_style);
            let load = read_loadavg_parts();
            let uptime = read_uptime();

            let (jdirty, jtotal) = app.current.journal_fill;
            let jpct = if jtotal > 0 { jdirty as f64 / jtotal as f64 * 100.0 } else { 0.0 };
            let jcolor = if jpct > 80.0 { Color::Red } else if jpct > 50.0 { Color::Yellow } else { Color::Green };

            let lines = vec![
                Line::from(Span::styled("Load Avg:", Style::default().fg(Color::DarkGray))),
                Line::from(vec![Span::raw(format!(" 1m  {}", load.0))]),
                Line::from(vec![Span::raw(format!(" 5m  {}", load.1))]),
                Line::from(vec![Span::raw(format!("15m  {}", load.2))]),
                Line::from(""),
                Line::from(Span::styled("Journal:", Style::default().fg(Color::DarkGray))),
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(format!("{:.0}%", jpct), Style::default().fg(jcolor).add_modifier(Modifier::BOLD)),
                ]),
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(app.current.journal_watermark.clone(), Style::default().fg(Color::DarkGray)),
                ]),
            ];
            let para = Paragraph::new(lines).block(block);
            f.render_widget(para, spark_cols[0]);
        }

        // Middle: READ sparklines (throughput + latency stacked)
        {
            let block = Block::default()
                .title(Span::styled(" READ ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow));
            let inner = block.inner(spark_cols[1]);
            f.render_widget(block, spark_cols[1]);

            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(inner);

            let tp_data = sparkline_u64(app.history.get("io_read_bytes_sec"));
            let tp_rate = app.history.get("io_read_bytes_sec").last().copied().unwrap_or(0.0);
            let tp_spark = Sparkline::default()
                .block(Block::default().title(
                    Span::styled(format!("{}/s", format_bytes(tp_rate)), Style::default().fg(Color::Yellow))
                ))
                .data(&tp_data)
                .style(Style::default().fg(Color::Yellow));
            f.render_widget(tp_spark, rows[0]);

            let lat_data = sparkline_u64(app.history.get("avg_read_latency_us"));
            let lat_val = app.history.get("avg_read_latency_us").last().copied().unwrap_or(0.0);
            let lat_spark = Sparkline::default()
                .block(Block::default().title(
                    Span::styled(format!("lat {:.0} µs", lat_val), Style::default().fg(Color::Yellow))
                ))
                .data(&lat_data)
                .style(Style::default().fg(Color::Yellow));
            f.render_widget(lat_spark, rows[1]);
        }

        // Right: WRITE sparklines (throughput + latency stacked)
        {
            let block = Block::default()
                .title(Span::styled(" WRITE ", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue));
            let inner = block.inner(spark_cols[2]);
            f.render_widget(block, spark_cols[2]);

            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(inner);

            let tp_data = sparkline_u64(app.history.get("io_write_bytes_sec"));
            let tp_rate = app.history.get("io_write_bytes_sec").last().copied().unwrap_or(0.0);
            let tp_spark = Sparkline::default()
                .block(Block::default().title(
                    Span::styled(format!("{}/s", format_bytes(tp_rate)), Style::default().fg(Color::Blue))
                ))
                .data(&tp_data)
                .style(Style::default().fg(Color::Blue));
            f.render_widget(tp_spark, rows[0]);

            let lat_data = sparkline_u64(app.history.get("avg_write_latency_us"));
            let lat_val = app.history.get("avg_write_latency_us").last().copied().unwrap_or(0.0);
            let lat_spark = Sparkline::default()
                .block(Block::default().title(
                    Span::styled(format!("lat {:.0} µs", lat_val), Style::default().fg(Color::Blue))
                ))
                .data(&lat_data)
                .style(Style::default().fg(Color::Blue));
            f.render_widget(lat_spark, rows[1]);
        }
    }

    // ── Device / Process / Blocked table ──
    if app.show_blocked {
        draw_blocked_table(f, app, chunks[1], focus_style);
    } else if app.show_processes {
        draw_process_table(f, app, chunks[1], focus_style);
    } else {
        let bv = |v: f64| -> String { if v > 0.0 { format_bytes_short(v) } else { "—".into() } };

        let dev_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(13),     // device + err
                Constraint::Percentage(50), // READ
                Constraint::Percentage(50), // WRITE
            ])
            .split(chunks[1]);

        // Collect rates data
        let mut sr = [0.0_f64; 5];
        let mut sw = [0.0_f64; 5];
        let mut sum_errs = 0u64;

        struct DevData {
            name: String,
            rv: Vec<f64>,
            wv: Vec<f64>,
            read_total: f64,
            write_total: f64,
            read_lat: u64,
            write_lat: u64,
            read_active: bool,
            write_active: bool,
            errors: u64,
        }
        let mut devs: Vec<DevData> = Vec::new();

        if let Some(rates) = &app.rates {
            for d in &rates.devices {
                let rv: Vec<f64> = ["user", "btree", "journal", "sb"].iter()
                    .map(|t| d.read_by_type.get(*t).copied().unwrap_or(0.0)).collect();
                let wv: Vec<f64> = ["user", "btree", "journal", "sb"].iter()
                    .map(|t| d.write_by_type.get(*t).copied().unwrap_or(0.0)).collect();
                for i in 0..4 { sr[i] += rv[i]; sw[i] += wv[i]; }
                sr[4] += d.read_bytes_sec;
                sw[4] += d.write_bytes_sec;
                sum_errs += d.io_errors;
                devs.push(DevData {
                    name: d.name.clone(), rv, wv,
                    read_total: d.read_bytes_sec, write_total: d.write_bytes_sec,
                    read_lat: d.read_latency_ns, write_lat: d.write_latency_ns,
                    read_active: d.read_active, write_active: d.write_active,
                    errors: d.io_errors,
                });
            }
        }

        // Left: Device + Err
        {
            let block = Block::default()
                .title(" Devices ")
                .borders(Borders::ALL)
                .border_style(focus_style);
            let header = Row::new(vec!["Device", "Err"])
                .style(Style::default().add_modifier(Modifier::BOLD));
            let mut rows: Vec<Row> = devs.iter().map(|d| {
                let es = if d.errors > 0 { Style::default().fg(Color::Red) } else { Style::default() };
                Row::new(vec![
                    Cell::new(d.name.clone()),
                    Cell::new(format!("{}", d.errors)).style(es),
                ])
            }).collect();
            rows.push(Row::new(vec![
                Cell::new("TOTAL").style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
                Cell::new(format!("{}", sum_errs)).style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
            ]));
            let widths = [Constraint::Length(7), Constraint::Length(4)];
            let table = Table::new(rows, widths).header(header).block(block);
            f.render_widget(table, dev_cols[0]);
        }

        // Middle: READ
        {
            let block = Block::default()
                .title(Span::styled(" READ ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow));
            let header = Row::new(vec!["user/s", "btree/s", "jrnl/s", "sb/s", "total/s", "lat"])
                .style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Yellow));
            let rs = Style::default().fg(Color::Yellow);
            let mut rows: Vec<Row> = devs.iter().map(|d| {
                Row::new(vec![
                    Cell::new(bv(d.rv[0])), Cell::new(bv(d.rv[1])),
                    Cell::new(bv(d.rv[2])), Cell::new(bv(d.rv[3])),
                    Cell::new(format_bytes_short(d.read_total)),
                    if d.read_active {
                        Cell::new(format_latency(d.read_lat)).style(Style::default().fg(latency_color(d.read_lat)))
                    } else {
                        Cell::new("—")
                    },
                ]).style(rs)
            }).collect();
            let bs = Style::default().add_modifier(Modifier::BOLD).fg(Color::Yellow);
            rows.push(Row::new(vec![
                Cell::new(bv(sr[0])), Cell::new(bv(sr[1])),
                Cell::new(bv(sr[2])), Cell::new(bv(sr[3])),
                Cell::new(format_bytes_short(sr[4])), Cell::new(""),
            ]).style(bs));
            let w = Constraint::Ratio(1, 6);
            let widths = [w, w, w, w, w, w];
            let table = Table::new(rows, widths).header(header).block(block);
            f.render_widget(table, dev_cols[1]);
        }

        // Right: WRITE
        {
            let block = Block::default()
                .title(Span::styled(" WRITE ", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue));
            let header = Row::new(vec!["user/s", "btree/s", "jrnl/s", "sb/s", "total/s", "lat"])
                .style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Blue));
            let ws = Style::default().fg(Color::Blue);
            let mut rows: Vec<Row> = devs.iter().map(|d| {
                Row::new(vec![
                    Cell::new(bv(d.wv[0])), Cell::new(bv(d.wv[1])),
                    Cell::new(bv(d.wv[2])), Cell::new(bv(d.wv[3])),
                    Cell::new(format_bytes_short(d.write_total)),
                    if d.write_active {
                        Cell::new(format_latency(d.write_lat)).style(Style::default().fg(latency_color(d.write_lat)))
                    } else {
                        Cell::new("—")
                    },
                ]).style(ws)
            }).collect();
            let bs = Style::default().add_modifier(Modifier::BOLD).fg(Color::Blue);
            rows.push(Row::new(vec![
                Cell::new(bv(sw[0])), Cell::new(bv(sw[1])),
                Cell::new(bv(sw[2])), Cell::new(bv(sw[3])),
                Cell::new(format_bytes_short(sw[4])), Cell::new(""),
            ]).style(bs));
            let w = Constraint::Ratio(1, 6);
            let widths = [w, w, w, w, w, w];
            let table = Table::new(rows, widths).header(header).block(block);
            f.render_widget(table, dev_cols[2]);
        }
    }

    // ── Background + Stalls ──
    {
        let has_stalls = !app.stall_events.is_empty();
        let title = if has_stalls { " Background ── STALLS DETECTED " } else { " Background " };
        let border_color = if has_stalls { Color::Red } else { focus_style.fg.unwrap_or(Color::DarkGray) };
        let block = Block::default()
            .title(Span::styled(title, Style::default().fg(border_color)))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let mut lines: Vec<Line> = app.current.background
            .iter()
            .map(|(k, v)| {
                let color = if v.starts_with("off") {
                    Color::DarkGray
                } else if v.contains("running") || v.contains("on") {
                    Color::Green
                } else {
                    Color::default()
                };
                Line::from(vec![
                    Span::styled(format!("{k}: "), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(v.to_string(), Style::default().fg(color)),
                ])
            })
            .collect();

        // Append recent stall events
        if has_stalls {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Recent stalls:",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            let now = std::time::Instant::now();
            for ev in app.stall_events.iter().take(5) {
                let ago = now.duration_since(ev.time).as_secs();
                lines.push(Line::from(Span::styled(
                    format!("  {}s ago  {}  {}  {}", ago, ev.device, ev.direction, ev.detail),
                    Style::default().fg(Color::Red),
                )));
            }
        }

        let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
        f.render_widget(para, chunks[2]);
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
    if let Some(ref proposal) = app.proposal {
        // Show proposal with Y to apply
        let line = Line::from(vec![
            Span::styled("SUGGEST: ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(&proposal.reason, Style::default().fg(Color::Yellow)),
            Span::raw("  "),
            Span::styled(&proposal.command, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled("[Y] apply ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled("[N] dismiss ", Style::default().fg(Color::DarkGray)),
            Span::styled("[!] never", Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    } else if let Some(ref msg) = app.status_msg {
        let para = Paragraph::new(msg.clone()).style(Style::default().fg(Color::Green));
        f.render_widget(para, area);
    } else {
        let mut help = String::from("[Tab] switch  [↑↓] navigate  [Enter] edit  [o] options  [p] procs  [t] blocked  [m] mark  [q] quit");
        if !app.dismissed_permanent.is_empty() {
            help.push_str(&format!("  ({} suppressed — [C] clear)", app.dismissed_permanent.len()));
        }
        let para = Paragraph::new(help).style(Style::default().fg(Color::DarkGray));
        f.render_widget(para, area);
    }
}

// ── Helpers ──

fn draw_blocked_table(f: &mut Frame, app: &App, area: Rect, border_style: Style) {
    let block = Block::default()
        .title(" Blocked Stats (time_stats) — [t] toggle ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let header = Row::new(vec!["Blocked On", "+/tick", "Recent Mean", "Status"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app.blocked_deltas.iter().map(|(name, delta, recent_us)| {
        let active = *delta > 0;
        let style = if active && *recent_us > 10_000.0 {
            Style::default().fg(Color::Red)
        } else if active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let mean_str = if active {
            format_duration_us_static(*recent_us)
        } else {
            "—".into()
        };

        let status = if active && *recent_us > 100_000.0 {
            "STALL"
        } else if active && *recent_us > 10_000.0 {
            "slow"
        } else if active {
            "ok"
        } else {
            ""
        };

        Row::new(vec![
            Cell::new(name.clone()),
            Cell::new(if active { format!("+{}", delta) } else { "0".into() }),
            Cell::new(mean_str),
            Cell::new(status),
        ]).style(style)
    }).collect();

    let widths = [
        Constraint::Min(30),
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Length(6),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}

fn draw_process_table(f: &mut Frame, app: &App, area: Rect, border_style: Style) {
    let block = Block::default()
        .title(" Processes (by I/O) — [p] toggle ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let header = Row::new(vec!["PID", "Process", "Read/s", "Write/s", "Total/s"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app.process_rates.iter().map(|p| {
        let total = p.read_bytes_sec + p.write_bytes_sec;
        let active = total > 0.0;
        let dim = Style::default().fg(Color::DarkGray);
        Row::new(vec![
            Cell::new(format!("{}", p.pid)).style(if active { Style::default() } else { dim }),
            Cell::new(p.name.clone()).style(if active { Style::default() } else { dim }),
            Cell::new(format_bytes(p.read_bytes_sec)).style(if active { Style::default().fg(Color::Yellow) } else { dim }),
            Cell::new(format_bytes(p.write_bytes_sec)).style(if active { Style::default().fg(Color::Blue) } else { dim }),
            Cell::new(format_bytes(total)).style(if active { Style::default() } else { dim }),
        ])
    }).collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Min(15),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(10),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}

fn sparkline_u64(data: &[f64]) -> Vec<u64> {
    data.iter().map(|v| *v as u64).collect()
}

fn latency_color(ns: u64) -> Color {
    if ns < 1_000_000 {       // < 1ms
        Color::Green
    } else if ns < 10_000_000 { // < 10ms
        Color::Yellow
    } else if ns < 100_000_000 { // < 100ms
        Color::Rgb(255, 165, 0) // orange
    } else {
        Color::Red              // >= 100ms (stall territory)
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

fn format_bytes_short(v: f64) -> String {
    if v >= 1_000_000_000.0 {
        format!("{:.0}G", v / 1_000_000_000.0)
    } else if v >= 1_000_000.0 {
        format!("{:.0}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{:.0}K", v / 1_000.0)
    } else {
        format!("{:.0}B", v)
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

fn format_type_breakdown(
    read: &std::collections::HashMap<String, f64>,
    write: &std::collections::HashMap<String, f64>,
) -> String {
    let mut parts = Vec::new();
    for t in ["user", "btree", "journal", "sb"] {
        let r = read.get(t).copied().unwrap_or(0.0);
        let w = write.get(t).copied().unwrap_or(0.0);
        if r > 0.0 || w > 0.0 {
            let r_s = if r > 0.0 { format_bytes(r) } else { "—".into() };
            let w_s = if w > 0.0 { format_bytes(w) } else { "—".into() };
            parts.push(format!("{t}:{r_s}/{w_s}"));
        }
    }
    if parts.is_empty() { "—".into() } else { parts.join("  ") }
}

fn format_duration_us_static(us: f64) -> String {
    if us >= 1_000_000.0 {
        format!("{:.1} s", us / 1_000_000.0)
    } else if us >= 1_000.0 {
        format!("{:.1} ms", us / 1_000.0)
    } else {
        format!("{:.0} µs", us)
    }
}

fn read_uptime() -> String {
    let content = std::fs::read_to_string("/proc/uptime").unwrap_or_default();
    let secs: f64 = content.split_whitespace().next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let s = secs as u64;
    let days = s / 86400;
    let hours = (s % 86400) / 3600;
    let mins = (s % 3600) / 60;
    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else {
        format!("{}m", mins)
    }
}

fn read_loadavg_parts() -> (String, String, String) {
    let content = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut parts = content.split_whitespace();
    (
        parts.next().unwrap_or("?").to_string(),
        parts.next().unwrap_or("?").to_string(),
        parts.next().unwrap_or("?").to_string(),
    )
}
