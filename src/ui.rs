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

    if app.show_help {
        draw_help(f);
    }
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

    let fs_indicator = if app.all_fs.len() > 1 {
        format!(" [{}/{}]", app.fs_index + 1, app.all_fs.len())
    } else {
        String::new()
    };

    let title = format!(
        " nasty-top v{} │ {} ({}) │ {:.1} GiB / {:.1} GiB ({:.1}%) │ {}x {}{}",
        env!("CARGO_PKG_VERSION"),
        fs.fs_name, fs.mount_point, used_gb, total_gb, pct, replicas, compression, fs_indicator
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

    let has_labels = app.rates.as_ref()
        .map(|r| r.devices.iter().any(|d| d.label.is_some()))
        .unwrap_or(false);
    let left_col_width: u16 = if has_labels { 25 } else { 17 };

    // Table height: header + rows + total/padding + 2 borders
    let dev_count = if app.show_counters {
        app.counter_deltas.len().min(30).max(10)
    } else if app.show_blocked {
        app.time_stats_view.len().min(30).max(10)
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
                Constraint::Length(left_col_width),
                Constraint::Percentage(50),
                Constraint::Percentage(50),
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
                Line::from(Span::styled(" Load Avg:", Style::default().fg(Color::DarkGray))),
                Line::from(vec![Span::raw(format!("  1m  {}", load.0))]),
                Line::from(vec![Span::raw(format!("  5m  {}", load.1))]),
                Line::from(vec![Span::raw(format!(" 15m  {}", load.2))]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" IOWait:", Style::default().fg(Color::DarkGray)),
                    {
                        let iow = app.iowait_pct;
                        let color = if iow > 50.0 { Color::Red } else if iow > 20.0 { Color::Yellow } else { Color::Green };
                        Span::styled(format!(" {:.0}%", iow), Style::default().fg(color))
                    },
                ]),
                Line::from(vec![
                    Span::styled(" Journal: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{:.0}%", jpct), Style::default().fg(jcolor).add_modifier(Modifier::BOLD)),
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
            let tp_iops = app.history.get("io_read_iops").last().copied().unwrap_or(0.0);
            let avg = if tp_iops > 0.0 && tp_rate > 0.0 { format_bytes_short(tp_rate / tp_iops) } else { "0B".into() };
            let tp_title = format!("{}/s · {:.0} io/s · avg {avg}", format_bytes(tp_rate), tp_iops);
            let tp_spark = Sparkline::default()
                .block(Block::default().title(
                    Span::styled(tp_title, Style::default().fg(Color::Yellow))
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
            let tp_iops = app.history.get("io_write_iops").last().copied().unwrap_or(0.0);
            let avg = if tp_iops > 0.0 && tp_rate > 0.0 { format_bytes_short(tp_rate / tp_iops) } else { "0B".into() };
            let tp_title = format!("{}/s · {:.0} io/s · avg {avg}", format_bytes(tp_rate), tp_iops);
            let tp_spark = Sparkline::default()
                .block(Block::default().title(
                    Span::styled(tp_title, Style::default().fg(Color::Blue))
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

    // ── Device / Process / Blocked / Counter table ──
    if app.show_counters {
        draw_counter_table(f, app, chunks[1], focus_style);
    } else if app.show_blocked {
        draw_blocked_table(f, app, chunks[1], focus_style);
    } else if app.show_processes {
        draw_process_table(f, app, chunks[1], focus_style);
    } else {
        let bv = |v: f64| -> String { if v > 0.0 { format_bytes_short(v) } else { "—".into() } };

        let dev_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(left_col_width),
                Constraint::Percentage(50),
                Constraint::Percentage(50),
            ])
            .split(chunks[1]);

        // Collect rates data
        let mut sr = [0.0_f64; 5];
        let mut sw = [0.0_f64; 5];
        let mut sum_errs = 0u64;

        struct DevData {
            name: String,
            label: Option<String>,
            rv: Vec<f64>,
            wv: Vec<f64>,
            read_total: f64,
            write_total: f64,
            read_lat: u64,
            write_lat: u64,
            read_active: bool,
            write_active: bool,
            errors: u64,
            util_pct: f64,
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
                    name: d.name.clone(), label: d.label.clone(), rv, wv,
                    read_total: d.read_bytes_sec, write_total: d.write_bytes_sec,
                    read_lat: d.read_latency_ns, write_lat: d.write_latency_ns,
                    read_active: d.read_active, write_active: d.write_active,
                    errors: d.io_errors, util_pct: d.util_pct,
                });
            }
        }

        // Left: Device + Err
        {
            let block = Block::default()
                .title(" Devices ")
                .borders(Borders::ALL)
                .border_style(focus_style);
            let has_labels = devs.iter().any(|d| d.label.is_some());
            let header = if has_labels {
                Row::new(vec!["Device", "Label", "Err", "Utl"])
            } else {
                Row::new(vec!["Device", "Err", "Utl"])
            }.style(Style::default().add_modifier(Modifier::BOLD));

            let mut rows: Vec<Row> = devs.iter().map(|d| {
                let es = if d.errors > 0 { Style::default().fg(Color::Red) } else { Style::default() };
                let uc = if d.util_pct > 80.0 { Color::Red } else if d.util_pct > 50.0 { Color::Yellow } else { Color::Green };
                let util_s = if d.util_pct > 0.0 { format!("{:.0}%", d.util_pct) } else { "—".into() };
                if has_labels {
                    Row::new(vec![
                        Cell::new(d.name.clone()),
                        Cell::new(d.label.clone().unwrap_or_default()).style(Style::default().fg(Color::DarkGray)),
                        Cell::new(format!("{}", d.errors)).style(es),
                        Cell::new(util_s).style(Style::default().fg(uc)),
                    ])
                } else {
                    Row::new(vec![
                        Cell::new(d.name.clone()),
                        Cell::new(format!("{}", d.errors)).style(es),
                        Cell::new(util_s).style(Style::default().fg(uc)),
                    ])
                }
            }).collect();
            let total_style = Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan);
            if has_labels {
                rows.push(Row::new(vec![
                    Cell::new("TOTAL").style(total_style),
                    Cell::new(""),
                    Cell::new(format!("{}", sum_errs)).style(total_style),
                    Cell::new(""),
                ]));
            } else {
                rows.push(Row::new(vec![
                    Cell::new("TOTAL").style(total_style),
                    Cell::new(format!("{}", sum_errs)).style(total_style),
                    Cell::new(""),
                ]));
            }
            let widths = if has_labels {
                vec![Constraint::Length(7), Constraint::Length(6), Constraint::Length(4), Constraint::Length(4)]
            } else {
                vec![Constraint::Length(7), Constraint::Length(4), Constraint::Length(4)]
            };
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

        let widths = [Constraint::Min(20), Constraint::Length(15)];
        let table = Table::new(rows, widths).block(block);
        f.render_widget(table, area);
    }

}

fn draw_help(f: &mut Frame) {
    let area = f.area();
    let w = 50u16.min(area.width.saturating_sub(4));
    let h = 22u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let popup = Rect::new(x, y, w, h);

    // Clear background
    f.render_widget(ratatui::widgets::Clear, popup);

    let block = Block::default()
        .title(" Help — [?] close ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));

    let help_text = vec![
        Line::from(Span::styled("Views", Style::default().add_modifier(Modifier::BOLD))),
        Line::from("  c  counters (all sysfs counters)"),
        Line::from("  t  blocked stats (time_stats)"),
        Line::from("  p  process IO (top by throughput)"),
        Line::from("  o  options panel (sysfs editing)"),
        Line::from("  f  cycle filesystem"),
        Line::from(""),
        Line::from(Span::styled("Toggles", Style::default().add_modifier(Modifier::BOLD))),
        Line::from("  r  reconcile on/off"),
        Line::from("  g  copygc on/off"),
        Line::from(""),
        Line::from(Span::styled("Options panel", Style::default().add_modifier(Modifier::BOLD))),
        Line::from("  Tab    switch focus"),
        Line::from("  ↑↓    navigate options"),
        Line::from("  Enter  edit selected option"),
        Line::from("  Esc    cancel edit"),
        Line::from(""),
        Line::from(Span::styled("Advisor", Style::default().add_modifier(Modifier::BOLD))),
        Line::from("  Y  apply suggestion"),
        Line::from("  N  dismiss (2 min)"),
        Line::from("  !  never suggest again"),
        Line::from("  C  clear permanent dismissals"),
    ];

    let para = Paragraph::new(help_text).block(block);
    f.render_widget(para, popup);
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
        let mut help = String::from("[?] help  [o] options  [c] counters  [t] blocked  [p] procs  [r] reconcile  [g] copygc  [f] fs  [q] quit");
        if !app.dismissed_permanent.is_empty() {
            help.push_str(&format!("  ({} suppressed — [C] clear)", app.dismissed_permanent.len()));
        }
        let para = Paragraph::new(help).style(Style::default().fg(Color::DarkGray));
        f.render_widget(para, area);
    }
}

// ── Helpers ──

fn draw_counter_table(f: &mut Frame, app: &App, area: Rect, border_style: Style) {
    let block = Block::default()
        .title(" Counters — [c] toggle ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let header = Row::new(vec!["Counter", "/tick", "/s", "Total"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let interval = 2.0_f64; // tick interval
    let rows: Vec<Row> = app.counter_deltas.iter().skip(app.view_scroll).map(|(name, delta, total)| {
        let active = *delta > 0;
        let style = if active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let per_sec = if active { format!("{:.0}", *delta as f64 / interval) } else { "0".into() };
        Row::new(vec![
            Cell::new(name.clone()),
            Cell::new(if active { format!("+{}", delta) } else { "0".into() }),
            Cell::new(per_sec),
            Cell::new(format!("{}", total)),
        ]).style(style)
    }).collect();

    let widths = [
        Constraint::Min(35),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(12),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}

fn draw_blocked_table(f: &mut Frame, app: &App, area: Rect, border_style: Style) {
    let block = Block::default()
        .title(" Time Stats — [t] toggle ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let header = Row::new(vec!["Operation", "+/tick", "Count", "Mean", "Recent", "Max"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let fmt_ns = |ns: u64| -> String {
        if ns == 0 { return "—".into(); }
        if ns >= 1_000_000_000 { format!("{:.1}s", ns as f64 / 1e9) }
        else if ns >= 1_000_000 { format!("{:.1}ms", ns as f64 / 1e6) }
        else if ns >= 1_000 { format!("{:.0}µs", ns as f64 / 1e3) }
        else { format!("{}ns", ns) }
    };

    let rows: Vec<Row> = app.time_stats_view.iter().skip(app.view_scroll).map(|ts| {
        let active = ts.count_delta > 0;
        let style = if ts.is_blocked && active && ts.recent_ns > 10_000_000 {
            Style::default().fg(Color::Red)
        } else if ts.is_blocked && active {
            Style::default().fg(Color::Rgb(255, 165, 0))
        } else if active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        Row::new(vec![
            Cell::new(ts.name.clone()),
            Cell::new(if active { format!("+{}", ts.count_delta) } else { "0".into() }),
            Cell::new(format!("{}", ts.count_total)),
            Cell::new(fmt_ns(ts.mean_ns)),
            Cell::new(fmt_ns(ts.recent_ns)),
            Cell::new(fmt_ns(ts.max_ns)),
        ]).style(style)
    }).collect();

    let widths = [
        Constraint::Min(30),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(9),
    ];
    let table = Table::new(rows, widths).header(header).block(block);
    f.render_widget(table, area);
}

fn draw_process_table(f: &mut Frame, app: &App, area: Rect, border_style: Style) {
    let block = Block::default()
        .title(" Processes (by I/O) — [p] toggle ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let header = Row::new(vec!["PID", "Process", "Read/s", "Write/s", "Rate", "Total"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app.process_rates.iter().skip(app.view_scroll).map(|p| {
        let rate = p.read_bytes_sec + p.write_bytes_sec;
        let cumulative = p.total_read + p.total_write;
        let active = rate > 0.0;
        let dim = Style::default().fg(Color::DarkGray);
        Row::new(vec![
            Cell::new(format!("{}", p.pid)).style(if active { Style::default() } else { dim }),
            Cell::new(p.name.clone()).style(if active { Style::default() } else { dim }),
            Cell::new(format_bytes(p.read_bytes_sec)).style(if active { Style::default().fg(Color::Yellow) } else { dim }),
            Cell::new(format_bytes(p.write_bytes_sec)).style(if active { Style::default().fg(Color::Blue) } else { dim }),
            Cell::new(format_bytes(rate)).style(if active { Style::default() } else { dim }),
            Cell::new(format_bytes(cumulative as f64)).style(Style::default().fg(Color::DarkGray)),
        ])
    }).collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Min(15),
        Constraint::Length(10),
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
