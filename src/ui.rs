//! TUI rendering with ratatui — btop-inspired visual style.

use crate::app::{App, Focus};
use crate::theme;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Borders, Cell, Chart, Dataset, GraphType,
    Paragraph, Row, Table, Wrap,
};
use ratatui::Frame;

// ── Helpers ──

fn rounded_block_styled(title: Span<'_>, border_color: Color) -> Block<'_> {
    Block::default()
        .title(Line::from(vec![Span::raw(" "), title, Span::raw(" ")]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
}

fn focused_border(app: &App, panel: Focus) -> Style {
    if app.focus == panel {
        theme::border_focused()
    } else {
        theme::border_dim()
    }
}

// ── Gradient meter bar rendering ──

fn render_gauge(f: &mut Frame, area: Rect, label: &str, pct: f64, label_style: Style) {
    if area.width < 4 || area.height < 1 {
        return;
    }
    let clamped = pct.clamp(0.0, 100.0);
    let label_len = label.len() as u16 + 1;
    let bar_width = area.width.saturating_sub(label_len + 6);
    let filled = ((clamped / 100.0) * bar_width as f64) as u16;

    let mut spans = vec![
        Span::styled(format!("{label} "), label_style),
    ];

    for i in 0..bar_width {
        if i < filled {
            let frac = i as f64 / bar_width as f64;
            let color = theme::gradient_color(frac);
            spans.push(Span::styled("\u{2501}", Style::default().fg(color)));
        } else {
            spans.push(Span::styled("\u{2500}", Style::default().fg(theme::BORDER_DIM)));
        }
    }

    let pct_color = theme::gradient_color(clamped / 100.0);
    spans.push(Span::styled(
        format!(" {:>3.0}%", clamped),
        Style::default().fg(pct_color),
    ));

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Styled footer key hint: [key] label
fn key_hint<'a>(key: &'a str, label: &'a str) -> Vec<Span<'a>> {
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default().fg(theme::FG).bg(theme::KEY_BG),
        ),
        Span::styled(format!("{label} "), theme::dim()),
    ]
}

// ── Main draw entry ──

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header (slim bar, no border)
            Constraint::Min(10),  // body
            Constraint::Length(1), // footer (single line menu bar)
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

    let title_line = Line::from(vec![
        Span::styled(
            format!(" nasty-top v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme::ACCENT).bg(theme::HEADER_BG).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default().bg(theme::HEADER_BG)),
        Span::styled(
            format!("{} ({})", fs.fs_name, fs.mount_point),
            Style::default().fg(theme::FG).bg(theme::HEADER_BG),
        ),
        Span::styled("  ", Style::default().bg(theme::HEADER_BG)),
        Span::styled(
            format!("{:.1}/{:.1} GiB ", used_gb, total_gb),
            Style::default().fg(theme::FG).bg(theme::HEADER_BG),
        ),
        Span::styled(
            format!("({:.1}%)", pct),
            Style::default().fg(theme::gradient_color(pct / 100.0)).bg(theme::HEADER_BG),
        ),
        Span::styled("  ", Style::default().bg(theme::HEADER_BG)),
        Span::styled(
            format!("{}x {}{} ", replicas, compression, fs_indicator),
            Style::default().fg(theme::DIM).bg(theme::HEADER_BG),
        ),
    ]);

    // Fill the rest of the line with background color
    let para = Paragraph::new(title_line).style(Style::default().bg(theme::HEADER_BG));
    f.render_widget(para, area);
}

fn draw_body(f: &mut Frame, app: &App, area: Rect) {
    if app.show_options {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(60),
                Constraint::Percentage(40),
            ])
            .split(area);

        draw_metrics_panel(f, app, columns[0]);
        draw_tuning_panel(f, app, columns[1]);
    } else {
        draw_metrics_panel(f, app, area);
    }
}

fn draw_metrics_panel(f: &mut Frame, app: &App, area: Rect) {
    let focus_style = focused_border(app, Focus::Metrics);

    let has_labels = app.rates.as_ref()
        .map(|r| r.devices.iter().any(|d| d.label.is_some()))
        .unwrap_or(false);
    let left_col_width: u16 = if has_labels { 25 } else { 20 };

    let dev_count = if app.show_counters {
        app.counter_deltas.len().clamp(10, 30)
    } else if app.show_blocked {
        app.time_stats_view.len().clamp(10, 30)
    } else if app.show_processes {
        20
    } else {
        app.rates.as_ref().map(|r| r.devices.len()).unwrap_or(0)
    };
    let dev_height = (dev_count + 4) as u16;

    // Dynamic background height: compact when no stalls
    let bg_count = app.current.background.len() as u16;
    let bg_height = if !app.stall_events.is_empty() {
        (bg_count + 3 + app.stall_events.len().min(5) as u16).min(14)
    } else {
        bg_count + 2 // borders + content, no wasted space
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(12),        // sparklines
            Constraint::Min(dev_height),   // device table (gets extra space)
            Constraint::Length(bg_height), // background (dynamic)
        ])
        .split(area);

    // ── Sparklines: 3 columns ──
    {
        let spark_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(left_col_width),
                Constraint::Percentage(50),
                Constraint::Percentage(50),
            ])
            .split(chunks[0]);

        draw_system_panel(f, app, spark_cols[0], focus_style);

        draw_io_chart(
            f, app, spark_cols[1],
            "READ",
            theme::READ, theme::READ_DIM,
            "io_read_bytes_sec", "io_read_iops", "avg_read_latency_us",
        );

        draw_io_chart(
            f, app, spark_cols[2],
            "WRITE",
            theme::WRITE, theme::WRITE_DIM,
            "io_write_bytes_sec", "io_write_iops", "avg_write_latency_us",
        );
    }

    // ── Device / Process / Blocked / Counter table ──
    if app.show_counters {
        draw_counter_table(f, app, chunks[1], focus_style);
    } else if app.show_blocked {
        draw_blocked_table(f, app, chunks[1], focus_style);
    } else if app.show_processes {
        draw_process_table(f, app, chunks[1], focus_style);
    } else {
        draw_device_table(f, app, chunks[1], focus_style, has_labels, left_col_width);
    }

    // ── Background + Stalls ──
    draw_background(f, app, chunks[2], focus_style);
}

fn draw_system_panel(f: &mut Frame, app: &App, area: Rect, focus_style: Style) {
    let block = Block::default()
        .title(Span::styled(" System ", theme::bold(theme::ACCENT)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(focus_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    let load = read_loadavg_parts();

    let (jdirty, jtotal) = app.current.journal_fill;
    let jpct = if jtotal > 0 { jdirty as f64 / jtotal as f64 * 100.0 } else { 0.0 };

    let space_pct = if app.current.space_total > 0 {
        app.current.space_used as f64 / app.current.space_total as f64 * 100.0
    } else {
        0.0
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // load avg
            Constraint::Length(1), // uptime
            Constraint::Length(1), // spacer
            Constraint::Length(1), // iowait gauge
            Constraint::Length(1), // journal gauge
            Constraint::Length(1), // disk gauge
            Constraint::Min(0),
        ])
        .split(inner);

    let load_line = Line::from(vec![
        Span::styled(" Load ", theme::dim()),
        Span::styled(&load.0, Style::default().fg(theme::FG)),
        Span::styled(" \u{2502} ", Style::default().fg(theme::BORDER_DIM)),
        Span::styled(&load.1, Style::default().fg(theme::FG)),
        Span::styled(" \u{2502} ", Style::default().fg(theme::BORDER_DIM)),
        Span::styled(&load.2, Style::default().fg(theme::FG)),
    ]);
    f.render_widget(Paragraph::new(load_line), rows[0]);

    let uptime = read_uptime();
    let up_line = Line::from(vec![
        Span::styled(" Up ", theme::dim()),
        Span::styled(uptime, Style::default().fg(theme::DIM)),
    ]);
    f.render_widget(Paragraph::new(up_line), rows[1]);

    render_gauge(f, rows[3], " IOw", app.iowait_pct, theme::dim());
    render_gauge(f, rows[4], " Jnl", jpct, theme::dim());
    render_gauge(f, rows[5], " Dsk", space_pct, theme::dim());
}

#[allow(clippy::too_many_arguments)]
fn draw_io_chart(
    f: &mut Frame,
    app: &App,
    area: Rect,
    label: &str,
    color: Color,
    fill_color: Color,
    tp_key: &str,
    iops_key: &str,
    lat_key: &str,
) {
    let block = rounded_block_styled(
        Span::styled(label, theme::bold(color)),
        color,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width < 4 || inner.height < 4 {
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(inner);

    // ── Throughput chart ──
    let tp_data = app.history.get(tp_key);
    let tp_rate = tp_data.last().copied().unwrap_or(0.0);
    let tp_iops = app.history.get(iops_key).last().copied().unwrap_or(0.0);
    let tp_title = format!("{}/s  {:.0} io/s", format_bytes(tp_rate), tp_iops);

    draw_braille_area_chart(f, rows[0], tp_data, color, fill_color, &tp_title);

    // ── Latency chart ──
    let lat_data = app.history.get(lat_key);
    let lat_val = lat_data.last().copied().unwrap_or(0.0);
    let lat_title = format!("lat {:.0} \u{00B5}s", lat_val);

    draw_braille_area_chart(f, rows[1], lat_data, color, fill_color, &lat_title);
}

fn draw_braille_area_chart(
    f: &mut Frame,
    area: Rect,
    data: &[f64],
    line_color: Color,
    fill_color: Color,
    title: &str,
) {
    if area.width < 2 || area.height < 2 {
        return;
    }

    let max_val = data.iter().copied().fold(0.0_f64, f64::max).max(1.0);

    let visible_len = (area.width as usize * 2).min(data.len());
    let start = data.len().saturating_sub(visible_len);
    let points: Vec<(f64, f64)> = data[start..]
        .iter()
        .enumerate()
        .map(|(i, &v)| (i as f64, v))
        .collect();

    let fill_points: Vec<(f64, f64)> = data[start..]
        .iter()
        .enumerate()
        .flat_map(|(i, &v)| {
            let steps = (area.height as usize * 4).max(8);
            (0..steps).map(move |s| {
                let y = v * s as f64 / steps as f64;
                (i as f64, y)
            })
        })
        .collect();

    let x_max = points.len().max(1) as f64;

    let datasets = vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Scatter)
            .style(Style::default().fg(fill_color))
            .data(&fill_points),
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(line_color))
            .data(&points),
    ];

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(Span::styled(title, Style::default().fg(line_color)))
        )
        .x_axis(
            Axis::default()
                .bounds([0.0, x_max])
                .style(Style::default().fg(theme::BORDER_DIM))
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, max_val])
                .style(Style::default().fg(theme::BORDER_DIM))
        );

    f.render_widget(chart, area);
}

fn draw_device_table(
    f: &mut Frame,
    app: &App,
    area: Rect,
    focus_style: Style,
    has_labels: bool,
    left_col_width: u16,
) {
    let bv = |v: f64| -> String {
        if v > 0.0 { format_bytes_short(v) } else { "\u{2014}".into() }
    };

    let dev_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(left_col_width),
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(area);

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

    // Left: Device + Err + Util (with mini bar)
    {
        let block = Block::default()
            .title(Span::styled(" Devices ", theme::bold(theme::ACCENT)))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(focus_style);
        let header = if has_labels {
            Row::new(vec!["Device", "Label", "Err", "Utl"])
        } else {
            Row::new(vec!["Device", "Err", "Utl"])
        }.style(theme::bold(theme::FG));

        let mut rows: Vec<Row> = devs.iter().enumerate().map(|(i, d)| {
            let es = if d.errors > 0 { Style::default().fg(theme::RED) } else { Style::default().fg(theme::FG) };
            let uc = if d.util_pct > 80.0 { theme::RED } else if d.util_pct > 50.0 { theme::YELLOW } else { theme::GREEN };
            let util_s = if d.util_pct > 0.0 { format!("{:.0}%", d.util_pct) } else { "\u{2014}".into() };
            let util_cell = Cell::new(util_s).style(Style::default().fg(uc));
            let row_bg = if i % 2 == 1 { Style::default().bg(theme::ROW_ALT) } else { Style::default() };
            if has_labels {
                Row::new(vec![
                    Cell::new(d.name.clone()).style(Style::default().fg(theme::FG)),
                    Cell::new(d.label.clone().unwrap_or_default()).style(theme::dim()),
                    Cell::new(format!("{}", d.errors)).style(es),
                    util_cell,
                ]).style(row_bg)
            } else {
                Row::new(vec![
                    Cell::new(d.name.clone()).style(Style::default().fg(theme::FG)),
                    Cell::new(format!("{}", d.errors)).style(es),
                    util_cell,
                ]).style(row_bg)
            }
        }).collect();
        let total_style = theme::bold(theme::CYAN);
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
            vec![Constraint::Length(7), Constraint::Min(4), Constraint::Length(4), Constraint::Length(5)]
        } else {
            vec![Constraint::Min(6), Constraint::Length(4), Constraint::Length(5)]
        };
        let table = Table::new(rows, widths).header(header).block(block);
        f.render_widget(table, dev_cols[0]);
    }

    // Middle: READ
    {
        let block = rounded_block_styled(
            Span::styled("READ", theme::bold(theme::READ)),
            theme::READ,
        );
        let header = Row::new(vec!["user/s", "btree/s", "jrnl/s", "sb/s", "total/s", "lat"])
            .style(theme::bold(theme::READ));
        let rs = Style::default().fg(theme::READ);
        let mut rows: Vec<Row> = devs.iter().enumerate().map(|(i, d)| {
            let row_bg = if i % 2 == 1 { rs.bg(theme::ROW_ALT) } else { rs };
            Row::new(vec![
                Cell::new(bv(d.rv[0])), Cell::new(bv(d.rv[1])),
                Cell::new(bv(d.rv[2])), Cell::new(bv(d.rv[3])),
                Cell::new(format_bytes_short(d.read_total)),
                if d.read_active {
                    Cell::new(format_latency(d.read_lat)).style(Style::default().fg(theme::latency_color(d.read_lat)))
                } else {
                    Cell::new("\u{2014}")
                },
            ]).style(row_bg)
        }).collect();
        let bs = theme::bold(theme::READ);
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
        let block = rounded_block_styled(
            Span::styled("WRITE", theme::bold(theme::WRITE)),
            theme::WRITE,
        );
        let header = Row::new(vec!["user/s", "btree/s", "jrnl/s", "sb/s", "total/s", "lat"])
            .style(theme::bold(theme::WRITE));
        let ws = Style::default().fg(theme::WRITE);
        let mut rows: Vec<Row> = devs.iter().enumerate().map(|(i, d)| {
            let row_bg = if i % 2 == 1 { ws.bg(theme::ROW_ALT) } else { ws };
            Row::new(vec![
                Cell::new(bv(d.wv[0])), Cell::new(bv(d.wv[1])),
                Cell::new(bv(d.wv[2])), Cell::new(bv(d.wv[3])),
                Cell::new(format_bytes_short(d.write_total)),
                if d.write_active {
                    Cell::new(format_latency(d.write_lat)).style(Style::default().fg(theme::latency_color(d.write_lat)))
                } else {
                    Cell::new("\u{2014}")
                },
            ]).style(row_bg)
        }).collect();
        let bs = theme::bold(theme::WRITE);
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

fn draw_background(f: &mut Frame, app: &App, area: Rect, focus_style: Style) {
    let has_stalls = !app.stall_events.is_empty();
    let title = if has_stalls { "Background \u{2500}\u{2500} STALLS DETECTED" } else { "Background" };
    let border_color = if has_stalls { theme::RED } else { focus_style.fg.unwrap_or(theme::BORDER_DIM) };
    let block = Block::default()
        .title(Span::styled(format!(" {title} "), Style::default().fg(border_color)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    let mut lines: Vec<Line> = app.current.background
        .iter()
        .map(|(k, v)| {
            let color = if v.starts_with("off") {
                theme::DIM
            } else if v.contains("running") || v.contains("on") {
                theme::GREEN
            } else {
                theme::FG
            };
            Line::from(vec![
                Span::styled(format!("{k}: "), theme::bold(theme::FG)),
                Span::styled(v.to_string(), Style::default().fg(color)),
            ])
        })
        .collect();

    if has_stalls {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Recent stalls:",
            theme::bold(theme::RED),
        )));
        let now = std::time::Instant::now();
        for ev in app.stall_events.iter().take(5) {
            let ago = now.duration_since(ev.time).as_secs();
            lines.push(Line::from(Span::styled(
                format!("  {}s ago  {}  {}  {}", ago, ev.device, ev.direction, ev.detail),
                Style::default().fg(theme::RED),
            )));
        }
    }

    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    f.render_widget(para, area);
}

fn draw_tuning_panel(f: &mut Frame, app: &App, area: Rect) {
    let focus_style = focused_border(app, Focus::Tuning);

    let block = Block::default()
        .title(Span::styled(" Options ", theme::bold(theme::ACCENT)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
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
                format!("{}\u{2581}", tuning.edit_buf)
            } else {
                value.to_string()
            };

            let style = if i == tuning.selected && matches!(app.focus, Focus::Tuning) {
                if tuning.editing {
                    Style::default().fg(theme::YELLOW).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::ACCENT).add_modifier(Modifier::REVERSED)
                }
            } else if i % 2 == 1 {
                Style::default().fg(theme::FG).bg(theme::ROW_ALT)
            } else {
                Style::default().fg(theme::FG)
            };

            Row::new(vec![name.clone(), display]).style(style)
        })
        .collect();

    let widths = [Constraint::Min(20), Constraint::Length(15)];
    let table = Table::new(rows, widths).block(block);
    f.render_widget(table, area);
}

fn draw_help(f: &mut Frame) {
    let area = f.area();
    let w = 50u16.min(area.width.saturating_sub(4));
    let h = 22u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let popup = Rect::new(x, y, w, h);

    f.render_widget(ratatui::widgets::Clear, popup);

    let block = Block::default()
        .title(Span::styled(" Help \u{2500} [?] close ", theme::bold(theme::ACCENT)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::ACCENT))
        .style(Style::default().bg(theme::BG));

    let help_text = vec![
        Line::from(Span::styled("Views", theme::bold(theme::ACCENT))),
        Line::from(Span::styled("  c  counters (all sysfs counters)", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  t  blocked stats (time_stats)", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  p  process IO (top by throughput)", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  o  options panel (sysfs editing)", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  f  cycle filesystem", Style::default().fg(theme::FG))),
        Line::from(""),
        Line::from(Span::styled("Toggles", theme::bold(theme::ACCENT))),
        Line::from(Span::styled("  r  reconcile on/off", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  g  copygc on/off", Style::default().fg(theme::FG))),
        Line::from(""),
        Line::from(Span::styled("Options panel", theme::bold(theme::ACCENT))),
        Line::from(Span::styled("  Tab    switch focus", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  \u{2191}\u{2193}    navigate options", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  Enter  edit selected option", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  Esc    cancel edit", Style::default().fg(theme::FG))),
        Line::from(""),
        Line::from(Span::styled("Advisor", theme::bold(theme::ACCENT))),
        Line::from(Span::styled("  Y  apply suggestion", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  N  dismiss (2 min)", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  !  never suggest again", Style::default().fg(theme::FG))),
        Line::from(Span::styled("  C  clear permanent dismissals", Style::default().fg(theme::FG))),
    ];

    let para = Paragraph::new(help_text).block(block);
    f.render_widget(para, popup);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if let Some(ref proposal) = app.proposal {
        let mut spans = vec![
            Span::styled(" SUGGEST ", Style::default().fg(theme::BG).bg(theme::YELLOW).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {} ", proposal.reason), Style::default().fg(theme::YELLOW)),
            Span::styled(&proposal.command, theme::bold(theme::FG)),
            Span::raw("  "),
        ];
        spans.extend(key_hint("Y", "apply"));
        spans.extend(key_hint("N", "dismiss"));
        spans.extend(key_hint("!", "never"));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    } else if let Some(ref msg) = app.status_msg {
        let para = Paragraph::new(Line::from(vec![
            Span::styled(" \u{2713} ", Style::default().fg(theme::BG).bg(theme::GREEN).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {msg}"), Style::default().fg(theme::GREEN)),
        ]));
        f.render_widget(para, area);
    } else {
        let mut spans: Vec<Span> = Vec::new();
        spans.extend(key_hint("?", "help"));
        spans.extend(key_hint("o", "options"));
        spans.extend(key_hint("c", "counters"));
        spans.extend(key_hint("t", "blocked"));
        spans.extend(key_hint("p", "procs"));
        spans.extend(key_hint("r", "reconcile"));
        spans.extend(key_hint("g", "copygc"));
        spans.extend(key_hint("f", "fs"));
        spans.extend(key_hint("q", "quit"));
        if !app.dismissed_permanent.is_empty() {
            spans.push(Span::styled(
                format!("  {} suppressed ", app.dismissed_permanent.len()),
                theme::dim(),
            ));
            spans.extend(key_hint("C", "clear"));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

// ── Alternate view tables ──

fn draw_counter_table(f: &mut Frame, app: &App, area: Rect, border_style: Style) {
    let block = Block::default()
        .title(Span::styled(" Counters ", theme::bold(theme::ACCENT)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style);

    let header = Row::new(vec!["Counter", "/tick", "/s", "Total"])
        .style(theme::bold(theme::FG));

    let interval = 2.0_f64;
    let rows: Vec<Row> = app.counter_deltas.iter().skip(app.view_scroll).enumerate().map(|(i, (name, delta, total))| {
        let active = *delta > 0;
        let base = if active {
            Style::default().fg(theme::READ)
        } else {
            theme::dim()
        };
        let style = if i % 2 == 1 { base.bg(theme::ROW_ALT) } else { base };
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
        .title(Span::styled(" Time Stats ", theme::bold(theme::ACCENT)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style);

    let header = Row::new(vec!["Operation", "+/tick", "Count", "Mean", "Recent", "Max"])
        .style(theme::bold(theme::FG));

    let fmt_ns = |ns: u64| -> String {
        if ns == 0 { return "\u{2014}".into(); }
        if ns >= 1_000_000_000 { format!("{:.1}s", ns as f64 / 1e9) }
        else if ns >= 1_000_000 { format!("{:.1}ms", ns as f64 / 1e6) }
        else if ns >= 1_000 { format!("{:.0}\u{00B5}s", ns as f64 / 1e3) }
        else { format!("{}ns", ns) }
    };

    let rows: Vec<Row> = app.time_stats_view.iter().skip(app.view_scroll).enumerate().map(|(i, ts)| {
        let active = ts.count_delta > 0;
        let base = if ts.is_blocked && active && ts.recent_ns > 10_000_000 {
            Style::default().fg(theme::RED)
        } else if ts.is_blocked && active {
            Style::default().fg(theme::ORANGE)
        } else if active {
            Style::default().fg(theme::READ)
        } else {
            theme::dim()
        };
        let style = if i % 2 == 1 { base.bg(theme::ROW_ALT) } else { base };

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
        .title(Span::styled(" Processes (by I/O) ", theme::bold(theme::ACCENT)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style);

    let header = Row::new(vec!["PID", "Process", "Read/s", "Write/s", "Rate", "Total"])
        .style(theme::bold(theme::FG));

    let rows: Vec<Row> = app.process_rates.iter().skip(app.view_scroll).enumerate().map(|(i, p)| {
        let rate = p.read_bytes_sec + p.write_bytes_sec;
        let cumulative = p.total_read + p.total_write;
        let active = rate > 0.0;
        let row_bg = if i % 2 == 1 { Some(theme::ROW_ALT) } else { None };
        let dim_s = |bg: Option<Color>| {
            let s = theme::dim();
            if let Some(c) = bg { s.bg(c) } else { s }
        };
        let fg_s = |color: Color, bg: Option<Color>| {
            let s = Style::default().fg(color);
            if let Some(c) = bg { s.bg(c) } else { s }
        };
        Row::new(vec![
            Cell::new(format!("{}", p.pid)).style(if active { fg_s(theme::FG, row_bg) } else { dim_s(row_bg) }),
            Cell::new(p.name.clone()).style(if active { fg_s(theme::FG, row_bg) } else { dim_s(row_bg) }),
            Cell::new(format_bytes(p.read_bytes_sec)).style(if active { fg_s(theme::READ, row_bg) } else { dim_s(row_bg) }),
            Cell::new(format_bytes(p.write_bytes_sec)).style(if active { fg_s(theme::WRITE, row_bg) } else { dim_s(row_bg) }),
            Cell::new(format_bytes(rate)).style(if active { fg_s(theme::FG, row_bg) } else { dim_s(row_bg) }),
            Cell::new(format_bytes(cumulative as f64)).style(dim_s(row_bg)),
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

// ── Formatting helpers ──

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
        format!("{:.0} \u{00B5}s", ns as f64 / 1_000.0)
    } else {
        format!("{} ns", ns)
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
