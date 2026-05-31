//! Color theme — btop-inspired truecolor palette.

use ratatui::style::{Color, Modifier, Style};

// ── Base colors ──

/// Dark background tint (slightly blue-shifted).
pub const BG: Color = Color::Rgb(20, 20, 30);

/// Primary text.
pub const FG: Color = Color::Rgb(200, 200, 210);

/// Dim/inactive text. Bright enough to keep ≥4.5:1 contrast against both
/// `BG` and `ROW_ALT` (WCAG AA for body text) — the previous (80,80,100)
/// dropped to ~2:1 on alternating rows and went invisible after 256-color
/// quantization on screen / vt100 palettes (issue #11).
pub const DIM: Color = Color::Rgb(140, 140, 160);

/// Bright accent for selected items.
pub const ACCENT: Color = Color::Rgb(100, 220, 240);

/// Border color when focused.
pub const BORDER_FOCUS: Color = Color::Rgb(90, 180, 200);

/// Border color when unfocused.
pub const BORDER_DIM: Color = Color::Rgb(55, 55, 75);

// ── Semantic colors ──

/// Read IO / throughput.
pub const READ: Color = Color::Rgb(240, 190, 60);

/// Read IO dimmer variant (area fill).
pub const READ_DIM: Color = Color::Rgb(100, 80, 25);

/// Write IO / throughput.
pub const WRITE: Color = Color::Rgb(80, 140, 240);

/// Write IO dimmer variant (area fill).
pub const WRITE_DIM: Color = Color::Rgb(30, 55, 100);

/// Success / healthy / low.
pub const GREEN: Color = Color::Rgb(80, 220, 120);

/// Warning / moderate.
pub const YELLOW: Color = Color::Rgb(240, 200, 60);

/// Error / critical / high.
pub const RED: Color = Color::Rgb(240, 70, 70);

/// Orange for elevated states.
pub const ORANGE: Color = Color::Rgb(240, 150, 50);

/// Cyan for totals/summaries.
pub const CYAN: Color = Color::Rgb(80, 200, 220);

/// Alternating row background tint. Slightly brighter than the previous
/// (28,28,40) so the zebra effect survives 256-color quantization on
/// terminals that collapse near-identical shades to the same palette
/// index — the old value was indistinguishable from `BG` outside of
/// truecolor (issue #11).
pub const ROW_ALT: Color = Color::Rgb(35, 35, 50);

/// Header bar background.
pub const HEADER_BG: Color = Color::Rgb(30, 35, 50);

/// Footer key highlight.
pub const KEY_BG: Color = Color::Rgb(50, 55, 75);

// ── Gradient stops for meter bars ──

/// Low → high gradient for gauges (green → yellow → red).
pub const GAUGE_GRADIENT: [(f64, Color); 5] = [
    (0.0, Color::Rgb(40, 180, 100)),
    (0.25, Color::Rgb(80, 220, 120)),
    (0.50, Color::Rgb(220, 210, 60)),
    (0.75, Color::Rgb(240, 150, 50)),
    (1.0, Color::Rgb(240, 70, 70)),
];

/// Interpolate between gradient stops.
pub fn gradient_color(pct: f64) -> Color {
    let t = pct.clamp(0.0, 1.0);
    // Find the two stops we're between.
    let mut lo = GAUGE_GRADIENT[0];
    let mut hi = GAUGE_GRADIENT[GAUGE_GRADIENT.len() - 1];
    for i in 0..GAUGE_GRADIENT.len() - 1 {
        if t >= GAUGE_GRADIENT[i].0 && t <= GAUGE_GRADIENT[i + 1].0 {
            lo = GAUGE_GRADIENT[i];
            hi = GAUGE_GRADIENT[i + 1];
            break;
        }
    }
    let range = hi.0 - lo.0;
    let frac = if range > 0.0 { (t - lo.0) / range } else { 0.0 };
    lerp_color(lo.1, hi.1, frac)
}

fn lerp_color(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let r = (r1 as f64 + (r2 as f64 - r1 as f64) * t) as u8;
            let g = (g1 as f64 + (g2 as f64 - g1 as f64) * t) as u8;
            let b = (b1 as f64 + (b2 as f64 - b1 as f64) * t) as u8;
            Color::Rgb(r, g, b)
        }
        _ => b,
    }
}

// ── Convenience styles ──

pub fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::default().fg(DIM)
}

pub fn border_focused() -> Style {
    Style::default().fg(BORDER_FOCUS)
}

pub fn border_dim() -> Style {
    Style::default().fg(BORDER_DIM)
}

pub fn latency_color(ns: u64) -> Color {
    if ns < 1_000_000 {
        GREEN
    } else if ns < 10_000_000 {
        YELLOW
    } else if ns < 100_000_000 {
        ORANGE
    } else {
        RED
    }
}
