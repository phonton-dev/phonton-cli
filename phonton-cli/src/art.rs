//! Ink & Photon: the TUI's ASCII art and motion.
//!
//! Surfaces are ink and paper. Colour is light: the φ logo's own pixel
//! spectrum appears only on the φ, the logo sweep and the photon on the loop
//! track. Everything else uses flat signal colours. Every animated helper
//! takes `Option<usize>` ticks; `None` renders the still frame.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub const INK: Color = Color::Rgb(11, 11, 12);
pub const PANEL: Color = Color::Rgb(18, 18, 20);
pub const PAPER: Color = Color::Rgb(236, 235, 230);
pub const MUTED: Color = Color::Rgb(163, 161, 154);
pub const DIM: Color = Color::Rgb(102, 100, 94);
pub const RULE: Color = Color::Rgb(54, 53, 50);
pub const VERIFIED: Color = Color::Rgb(139, 227, 139);
pub const RUNNING: Color = Color::Rgb(255, 179, 92);
pub const FAILED: Color = Color::Rgb(255, 107, 107);
pub const PHOTON: Color = Color::Rgb(95, 224, 232);

/// The φ logo's spectrum, left to right: amber, rose, violet, blue, cyan.
const SPECTRUM: [(u8, u8, u8); 5] = [
    (255, 169, 77),
    (255, 79, 139),
    (155, 69, 217),
    (47, 111, 240),
    (79, 221, 230),
];

fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (236, 235, 230),
    }
}

fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::Rgb(ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

/// Sample the logo spectrum at `t` in `0..=1`.
pub fn spectrum(t: f32) -> Color {
    let t = t.clamp(0.0, 1.0) * 4.0;
    let i = (t.floor() as usize).min(3);
    mix(SPECTRUM[i], SPECTRUM[i + 1], t - i as f32)
}

/// A single dot orbiting a braille cell.
const PHOTON_SPIN: [char; 8] = ['⠁', '⠂', '⠄', '⡀', '⢀', '⠠', '⠐', '⠈'];

/// Photon spinner frame for a UI tick (80 ms).
pub fn spinner(tick: usize) -> char {
    PHOTON_SPIN[(tick / 2) % PHOTON_SPIN.len()]
}

// ---------------------------------------------------------------------------
// φ
// ---------------------------------------------------------------------------

pub const PHI_WIDTH: u16 = 26;
pub const PHI_HEIGHT: u16 = 14;
const RAMP: [char; 10] = [' ', '.', ':', '-', '=', '+', '*', '#', '%', '@'];

/// The φ logo as ASCII, from its own pixels and colours: two pixel rows per
/// text row. With `tick`, a light wave runs diagonally across it.
pub fn phi(tick: Option<usize>) -> Vec<Line<'static>> {
    (0..PHI_HEIGHT as usize)
        .map(|row| {
            let spans: Vec<Span<'static>> = (0..PHI_WIDTH as usize)
                .map(|col| phi_cell(row, col, tick))
                .collect();
            Line::from(spans)
        })
        .collect()
}

fn phi_cell(row: usize, col: usize, tick: Option<usize>) -> Span<'static> {
    let hex = |v: u32| ((v >> 16) as u8, (v >> 8) as u8, v as u8);
    let (top, bottom) = (PHI_PX[row * 2][col], PHI_PX[row * 2 + 1][col]);
    let wave = tick
        .map(|t| (t as f32 * 0.21 - (col as f32 * 0.3 + row as f32 * 0.6)).sin())
        .unwrap_or(0.0);
    let level = |n: usize| ((wave + 1.0) * 0.5 * n as f32).floor().min(n as f32 - 1.0) as usize;
    let (ch, color, bold) = match (top, bottom) {
        (0, 0) => return Span::raw(" "),
        (t, 0) => (['`', '\'', '"'][level(3)], hex(t), false),
        (0, b) => (['.', ',', '_'][level(3)], hex(b), false),
        (t, b) => {
            let (a, c) = (hex(t), hex(b));
            let avg = (
                ((a.0 as u16 + c.0 as u16) / 2) as u8,
                ((a.1 as u16 + c.1 as u16) / 2) as u8,
                ((a.2 as u16 + c.2 as u16) / 2) as u8,
            );
            let idx = (7.0 + wave * 2.0).round().clamp(4.0, 9.0) as usize;
            (RAMP[idx], avg, true)
        }
    };
    let lit = mix(color, (255, 255, 255), wave.max(0.0) * 0.38);
    let style = Style::default().fg(lit);
    Span::styled(
        ch.to_string(),
        if bold {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        },
    )
}

// ---------------------------------------------------------------------------
// PHONTON wordmark
// ---------------------------------------------------------------------------

/// The ANSI Shadow wordmark. Do not swap for the compact pixel logo.
pub const LOGO: &[&str] = &[
    "██████╗ ██╗  ██╗ ██████╗ ███╗   ██╗████████╗ ██████╗ ███╗   ██╗",
    "██╔══██╗██║  ██║██╔═══██╗████╗  ██║╚══██╔══╝██╔═══██╗████╗  ██║",
    "██████╔╝███████║██║   ██║██╔██╗ ██║   ██║   ██║   ██║██╔██╗ ██║",
    "██╔═══╝ ██╔══██║██║   ██║██║╚██╗██║   ██║   ██║   ██║██║╚██╗██║",
    "██║     ██║  ██║╚██████╔╝██║ ╚████║   ██║   ╚██████╔╝██║ ╚████║",
    "╚═╝     ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═══╝   ╚═╝    ╚═════╝ ╚═╝  ╚═══╝",
];
/// Rows drawn by [`logo`]: the wordmark plus its rule.
pub const LOGO_ROWS: u16 = LOGO.len() as u16 + 1;
const LOGO_COLS: usize = 63;
/// Ticks between photon sweeps (80 ms ticks) and ticks one sweep takes.
const SWEEP_PERIOD: usize = 90;
const SWEEP_TICKS: usize = 26;

/// Photon position along the wordmark for a tick, or `None` between sweeps.
fn sweep_x(tick: Option<usize>) -> Option<f32> {
    let phase = tick? % SWEEP_PERIOD;
    (phase < SWEEP_TICKS)
        .then(|| -10.0 + phase as f32 / SWEEP_TICKS as f32 * (LOGO_COLS as f32 + 20.0))
}

/// Paper wordmark with ink shadows; every few seconds a photon sweeps across
/// it, lighting cells with the spectrum colour at that column.
pub fn logo(tick: Option<usize>) -> Vec<Line<'static>> {
    let sweep = sweep_x(tick);
    let mut lines: Vec<Line<'static>> = LOGO
        .iter()
        .enumerate()
        .map(|(row, text)| {
            let spans: Vec<Span<'static>> = text
                .chars()
                .enumerate()
                .map(|(col, ch)| {
                    if ch == ' ' {
                        return Span::raw(" ");
                    }
                    let shadow = ch != '█';
                    let base = if shadow { rgb(DIM) } else { rgb(PAPER) };
                    let glow = sweep
                        .map(|x| {
                            let d = (col as f32 - x - row as f32 * 1.2).abs();
                            (1.0 - d / 5.0).max(0.0)
                        })
                        .unwrap_or(0.0);
                    let lit = rgb(spectrum(col as f32 / LOGO_COLS as f32));
                    let color = mix(base, lit, glow * if shadow { 0.7 } else { 0.9 });
                    let style = Style::default().fg(color);
                    Span::styled(
                        ch.to_string(),
                        if shadow {
                            style
                        } else {
                            style.add_modifier(Modifier::BOLD)
                        },
                    )
                })
                .collect();
            Line::from(spans)
        })
        .collect();
    let rule: Vec<Span<'static>> = (0..LOGO_COLS)
        .map(|col| match sweep {
            Some(x) if (col as f32 - x - 6.0).abs() < 0.5 => Span::styled(
                "●",
                Style::default()
                    .fg(spectrum(col as f32 / LOGO_COLS as f32))
                    .add_modifier(Modifier::BOLD),
            ),
            Some(x) if col as f32 - x - 6.0 < 0.0 && col as f32 - x - 6.0 > -8.0 => Span::styled(
                "━",
                Style::default().fg(mix(
                    rgb(RULE),
                    rgb(spectrum(col as f32 / LOGO_COLS as f32)),
                    1.0 + (col as f32 - x - 6.0) / 8.0,
                )),
            ),
            _ => Span::styled("─", Style::default().fg(RULE)),
        })
        .collect();
    lines.push(Line::from(rule));
    lines
}

// ---------------------------------------------------------------------------
// Loop track
// ---------------------------------------------------------------------------

pub const STAGES: [&str; 6] = ["goal", "plan", "edit", "verify", "review", "remember"];

/// Where a run is on the ADE loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    /// No run: a photon idles around the whole loop.
    Idle,
    /// Working on this stage index.
    Active(usize),
    /// Stopped at this stage index.
    Failed(usize),
    /// Every stage finished.
    Complete,
}

/// `goal ── plan ── edit ── verify ── review ── remember` with the run's
/// position lit. `width` picks a long or short connector.
pub fn loop_track(track: Track, tick: Option<usize>, width: u16) -> Line<'static> {
    let n = if width >= 76 { 4 } else { 2 };
    let total = (STAGES.len() - 1) * n;
    let idle_pos = tick.map(|t| (t / 2) % total);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, label) in STAGES.iter().enumerate() {
        let (marker, mstyle, lstyle) = match track {
            Track::Complete => (
                '✓',
                Style::default().fg(VERIFIED),
                Style::default().fg(PAPER),
            ),
            Track::Idle => ('·', Style::default().fg(DIM), Style::default().fg(MUTED)),
            Track::Active(s) | Track::Failed(s) if i < s => (
                '✓',
                Style::default().fg(VERIFIED),
                Style::default().fg(MUTED),
            ),
            Track::Active(s) if i == s => (
                tick.map(spinner).unwrap_or('●'),
                Style::default().fg(PHOTON).add_modifier(Modifier::BOLD),
                Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
            ),
            Track::Failed(s) if i == s => (
                '✗',
                Style::default().fg(FAILED).add_modifier(Modifier::BOLD),
                Style::default().fg(FAILED).add_modifier(Modifier::BOLD),
            ),
            _ => ('·', Style::default().fg(DIM), Style::default().fg(DIM)),
        };
        spans.push(Span::styled(marker.to_string(), mstyle));
        spans.push(Span::raw(" "));
        spans.push(Span::styled((*label).to_string(), lstyle));
        if i + 1 == STAGES.len() {
            break;
        }
        spans.push(Span::raw(" "));
        for k in 0..n {
            let at = i * n + k;
            let t = (at as f32 + 0.5) / total as f32;
            let photon = Span::styled(
                "●",
                Style::default()
                    .fg(spectrum(t))
                    .add_modifier(Modifier::BOLD),
            );
            let span = match track {
                Track::Idle if idle_pos == Some(at) => photon,
                Track::Active(s) if i == s && tick.map(|t| (t / 2) % n) == Some(k) => photon,
                Track::Complete => Span::styled("─", Style::default().fg(MUTED)),
                Track::Active(s) | Track::Failed(s) if i < s => {
                    Span::styled("─", Style::default().fg(MUTED))
                }
                _ => Span::styled("─", Style::default().fg(RULE)),
            };
            spans.push(span);
        }
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Receipts
// ---------------------------------------------------------------------------

/// What a receipt can honestly claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Checks ran and passed with no findings.
    Verified,
    /// Checks passed but left findings to read.
    Review,
    /// Syntax or build passed; no test ran.
    Partial,
    /// No check produced evidence.
    Unverified,
    /// The run ended without a reviewable change.
    Failed,
}

/// Ticks a receipt spends counting up before it stamps.
pub const COUNT_TICKS: usize = 12;

/// Ease a number from zero to `value` over [`COUNT_TICKS`].
pub fn count_up(value: u64, since: Option<usize>) -> u64 {
    let Some(since) = since else {
        return value;
    };
    let t = (since as f64 / COUNT_TICKS as f64).min(1.0);
    let eased = 1.0 - (1.0 - t).powi(3);
    (value as f64 * eased).round() as u64
}

/// The receipt stamp; blank dots while the numbers are still counting.
pub fn stamp(verdict: Verdict, since: Option<usize>) -> Span<'static> {
    let (text, color) = match verdict {
        Verdict::Verified => (" ✓ VERIFIED ", VERIFIED),
        Verdict::Review => (" ! REVIEW FINDINGS ", RUNNING),
        Verdict::Partial => (" ◐ NO TESTS RAN ", RUNNING),
        Verdict::Unverified => (" ○ UNVERIFIED ", MUTED),
        Verdict::Failed => (" ✗ FAILED ", FAILED),
    };
    match since {
        Some(s) if s < COUNT_TICKS => Span::styled(" · · · ", Style::default().fg(DIM)),
        Some(s) if s == COUNT_TICKS => Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        _ => Span::styled(
            text,
            Style::default()
                .bg(color)
                .fg(INK)
                .add_modifier(Modifier::BOLD),
        ),
    }
}

/// `label ........ value`, filling `width` cells.
pub fn leader(label: &str, value: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let used = label.chars().count() + value.iter().map(|s| s.width()).sum::<usize>() + 2;
    let dots = width.saturating_sub(used).max(2);
    let mut spans = vec![
        Span::styled(label.to_string(), Style::default().fg(MUTED)),
        Span::styled(format!(" {} ", ".".repeat(dots)), Style::default().fg(RULE)),
    ];
    spans.extend(value);
    Line::from(spans)
}

/// Frame `body` in a box-drawing border `width` cells wide, with a title on
/// the left of the top rule and an optional badge on the right.
pub fn boxed(
    title: &str,
    badge: Option<Span<'static>>,
    body: Vec<Line<'static>>,
    width: usize,
    border: Color,
) -> Vec<Line<'static>> {
    let b = Style::default().fg(border);
    let inner = width.saturating_sub(4);
    let badge_w = badge.as_ref().map(|s| s.width() + 2).unwrap_or(0);
    let title_w = title.chars().count() + 3;
    let mut top = vec![
        Span::styled("┌─ ", b),
        Span::styled(
            title.to_string(),
            Style::default().fg(PAPER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", b),
    ];
    let fill = width.saturating_sub(title_w + badge_w + 2);
    top.push(Span::styled("─".repeat(fill), b));
    if let Some(badge) = badge {
        top.push(Span::styled(" ", b));
        top.push(badge);
        top.push(Span::styled(" ", b));
    }
    top.push(Span::styled("┐", b));
    let mut out = vec![Line::from(top)];
    for line in body {
        let pad = inner.saturating_sub(line.width());
        let mut spans = vec![Span::styled("│ ", b)];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(" │", b));
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        format!("└{}┘", "─".repeat(width.saturating_sub(2))),
        b,
    )));
    out
}

/// `━━━━────` line gauge.
pub fn gauge(frac: f32, width: usize, color: Color) -> Vec<Span<'static>> {
    let filled = (frac.clamp(0.0, 1.0) * width as f32).round() as usize;
    vec![
        Span::styled("━".repeat(filled), Style::default().fg(color)),
        Span::styled("─".repeat(width - filled), Style::default().fg(RULE)),
    ]
}

/// Render styled lines as 24-bit ANSI text for plain stdout (no TUI).
pub fn ansi(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let mut out = String::new();
            for span in &line.spans {
                match span.style.fg {
                    Some(Color::Rgb(r, g, b)) => {
                        let bold = if span.style.add_modifier.contains(Modifier::BOLD) {
                            "1;"
                        } else {
                            ""
                        };
                        out.push_str(&format!(
                            "\x1b[{bold}38;2;{r};{g};{b}m{}\x1b[0m",
                            span.content
                        ));
                    }
                    _ => out.push_str(&span.content),
                }
            }
            out
        })
        .collect()
}

/// Thousands separators for counters.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

// φ pixels sampled from the logo PNG (32-cell grid, cropped to the glyph).
#[rustfmt::skip]
const PHI_PX: [[u32; 26]; 28] = [
    [0,0,0,0,0,0xffa95b,0,0,0,0,0,0,0,0,0,0xff588a,0xff5891,0xfe49bb,0xff4cc0,0xff4cc0,0,0,0,0,0,0],
    [0,0,0,0,0xffab5c,0xff9566,0xff5f8e,0,0,0,0,0,0,0,0xff5598,0xff549d,0xfd46b7,0xd335c1,0xd234c1,0xb331c0,0xa132bd,0xcf36c3,0,0,0,0],
    [0,0,0,0xffa858,0xffa467,0xff6d75,0xff4da1,0,0,0,0,0,0,0xff637f,0xf441bc,0xf43fb7,0xde36bd,0xcc34c5,0xc733c4,0xab32c7,0x7035c4,0x8631bc,0x7437c4,0,0,0],
    [0,0,0,0xff927a,0xff5c7e,0xff5785,0,0,0,0,0,0,0xff5f8f,0xff56a4,0xf642bc,0xda35bc,0xd936c4,0,0,0,0x6e33c0,0x6e35c0,0x6e3bcd,0x4a41cd,0,0],
    [0,0xff8270,0xff7977,0xff5584,0xff5199,0,0,0,0,0,0,0xff5bbe,0xff57bc,0xf03cb6,0xbf30b9,0,0,0,0,0,0,0,0x3f45d1,0x2d4cd2,0x0079d5,0],
    [0,0xff6573,0xff5c7d,0xff5793,0xff47a9,0,0,0,0,0,0,0xff55be,0xff52bd,0xf03dba,0x962fbe,0,0,0,0,0,0,0,0x324cd5,0x005ece,0x0079d4,0],
    [0,0xff5c82,0xff5887,0,0,0,0,0,0,0,0,0xff56c1,0xfe47bc,0xd431b5,0,0,0,0,0,0,0,0,0x0154d1,0x005ece,0x007cd5,0],
    [0xffa065,0xff5a83,0xff5789,0,0,0,0,0,0,0,0xff6fd5,0xfd45bb,0xe334b2,0xd330b1,0,0,0,0,0,0,0,0,0x005cce,0x005ece,0x007ed5,0x0090d8],
    [0xff7b76,0xff5b8b,0xff4fa6,0,0,0,0,0,0,0,0xfd52ca,0xec3dbd,0xe238b8,0xb132bd,0,0,0,0,0,0,0,0,0x005bce,0x0077d6,0x0096d9,0x00a6de],
    [0xff5a96,0xf43cad,0xde35b2,0,0,0,0,0,0,0,0xfd4ec4,0xc830b8,0xb62fb6,0x8530ba,0,0,0,0,0,0,0,0,0x006ad2,0x0075d5,0x0097d9,0x00a7dc],
    [0xff5896,0xf03bad,0xdc35b3,0,0,0,0,0,0,0,0xfc4bc2,0xc330b9,0xb62fb4,0x5f34c1,0,0,0,0,0,0,0,0,0x006bd0,0x0077d5,0x009dd9,0x00abdc],
    [0xff56b6,0xdf36b6,0xdf38be,0,0,0,0,0,0,0,0xda33be,0xbe30b8,0x8431bc,0x5e34c0,0,0,0,0,0,0,0,0,0x025cc7,0x0092d9,0x00a1db,0x34cae0],
    [0xfe49bb,0xd934b4,0xb230ba,0,0,0,0,0,0,0,0xc231c6,0x8a31b6,0x8432bb,0x6136c1,0,0,0,0,0,0,0,0,0x0085dd,0x00a2db,0x4ecedc,0x51d5e3],
    [0xdf38bd,0xc530b5,0xb02fb7,0xb430b9,0,0,0,0,0,0,0xba2eba,0x8a31c0,0x8532bc,0x463ac7,0,0,0,0,0,0,0,0,0x009ddd,0x00a7dc,0x55d7e4,0x56d6e3],
    [0,0xb430bc,0xb02fba,0xb02fb6,0,0,0,0,0,0,0xaa2cb4,0x8332c0,0x4e37c5,0x463bc9,0,0,0,0,0,0,0,0,0x00a9de,0x3ed1e3,0x56d8e4,0],
    [0,0xb82dad,0xaf31bd,0xaf30ba,0x7c31be,0,0,0,0,0,0xa52cb4,0x5038c5,0x5038c7,0x493ac9,0,0,0,0,0,0,0,0x0093d8,0x41d1e3,0x57d5e2,0x63dbe4,0],
    [0,0,0xad32c1,0xa431c0,0x7432bd,0x7335c1,0,0,0,0,0x702ec7,0x5139c7,0x4d38c7,0x0154ce,0,0,0,0,0,0x008dd9,0x008fd9,0x00b3df,0x5bd9e3,0x5bd8e2,0,0],
    [0,0,0xa331c2,0x7334c3,0x7334c3,0x6f36c1,0x6735bd,0x5f38c4,0,0,0x5630c4,0x523cca,0x224acc,0x0058cd,0,0,0,0,0x00a0db,0x00abdc,0x02b6de,0x55d7e4,0x5bd9e4,0x62d9e1,0,0],
    [0,0,0,0,0x6f33c3,0x6d33c2,0x6335bf,0x6337c3,0x6335bf,0x5f36c0,0x5c38c4,0x2449cc,0x0157cb,0x0156cc,0x0087d7,0x0086d8,0x0084d7,0x00a0dc,0x00a1dc,0x18c6e1,0x55d6e3,0x52d6e3,0,0,0,0],
    [0,0,0,0,0,0,0x5d32c3,0x4c36c5,0x4936c3,0x4338c5,0x403ecb,0x005bce,0x0058ce,0x0158c9,0x009edc,0x009fde,0x009ddb,0x00a1dc,0x43d4e5,0x52d4e3,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0x0c45cc,0x005bce,0x005acd,0x006dd0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0x024bd1,0x0071d7,0x0072d4,0x0070d0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0x0054ce,0x0073d5,0x0070d4,0x008bd6,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0x005bcc,0x0073d5,0x0072d5,0x0093d7,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0x0072cb,0x0093d7,0x0098d9,0x01b1da,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0x00a3d5,0x00aade,0x17c3de,0x1ec4de,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0x06acd4,0x36cde2,0x3fcfe2,0x3fcde0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0x0fb7d8,0x56d6e3,0x5bd5e1,0x5cd6e1,0,0,0,0,0,0,0,0,0,0,0,0],
];

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn phi_keeps_the_logo_silhouette() {
        let still = phi(None);
        assert_eq!(still.len(), PHI_HEIGHT as usize);
        assert!(still.iter().all(|l| l.width() == PHI_WIDTH as usize));
        // The stem: the bottom rows are a narrow column of ink.
        let last = text(&still[13]);
        assert_eq!(last.trim().chars().count(), 4, "{last:?}");
        // Animation changes glyphs but never the silhouette.
        let lit = phi(Some(37));
        for (a, b) in still.iter().zip(&lit) {
            let blank = |l: &Line| text(l).chars().map(|c| c == ' ').collect::<Vec<_>>();
            assert_eq!(blank(a), blank(b));
        }
    }

    #[test]
    fn loop_track_marks_progress_and_failure() {
        let active = text(&loop_track(Track::Active(2), None, 80));
        assert!(active.starts_with("✓ goal"), "{active}");
        assert!(active.contains("● edit"), "{active}");
        assert!(active.contains("· verify"), "{active}");
        let failed = text(&loop_track(Track::Failed(3), None, 80));
        assert!(failed.contains("✗ verify"), "{failed}");
        let done = text(&loop_track(Track::Complete, None, 60));
        assert_eq!(done.matches('✓').count(), 6, "{done}");
        assert!(done.chars().count() <= 64, "{done}");
    }

    #[test]
    fn receipts_count_up_then_stamp() {
        assert_eq!(count_up(325, Some(0)), 0);
        assert!(count_up(325, Some(4)) < 325);
        assert_eq!(count_up(325, Some(COUNT_TICKS)), 325);
        assert_eq!(count_up(325, None), 325);
        assert_eq!(stamp(Verdict::Verified, Some(2)).content, " · · · ");
        assert!(stamp(Verdict::Verified, None).content.contains("VERIFIED"));
    }

    #[test]
    fn boxes_and_leaders_fill_their_width() {
        let body = vec![leader("tokens in", vec![Span::raw("325")], 30)];
        let framed = boxed("receipt", Some(Span::raw("ok")), body, 34, RULE);
        assert!(framed.iter().all(|l| l.width() == 34), "{framed:?}");
        assert_eq!(thousands(41203), "41,203");
        assert_eq!(thousands(999), "999");
    }
}
