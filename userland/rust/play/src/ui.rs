//! The player screen.
//!
//! The same look as scarf and tile (anshi_tui::chrome): a "play" tab and
//! a grey rule across the top with the track counter at its end, and
//! keycaps along the bottom. The accent is green while playing and
//! yellow while paused: the tab, the status chip, and the progress and
//! volume meters' fill (on a grey track) all follow it. The spectrum's
//! bars are colored by height like a level meter -- green, then yellow,
//! then red near the top -- and each peak marker takes the color of the
//! row it floats in. The bars and fills are colored spaces, so they draw
//! as solid blocks.
//!
//! The content is a centered column capped at MAX_WIDTH, vertically
//! centered between the header and footer, with the spectrum's height
//! capped too (one row per level at most) -- on a big framebuffer
//! console, stretching everything edge to edge looks sparse, not grand.
//! Geometry is plain Rect arithmetic, no constraint solver: this crate is
//! soft-float, and it all depends only on the terminal size anyway.

use alloc::format;
use alloc::string::String;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

use anshi_tui::chrome::{self, GREY, ON_ACCENT, fg};

use crate::source::TrackInfo;
use crate::spectrum::{MAX_LEVEL, N_BANDS};

const MAX_WIDTH: u16 = 100;
/// Spectrum rows at most: one per level, so bars move in whole cells.
const MAX_SPECTRUM_ROWS: u16 = MAX_LEVEL as u16;
const MIN_SPECTRUM_ROWS: u16 = 3;
/// Rows the content column needs besides the spectrum: track, gap,
/// format, gap, axis, gap, progress, gap, status -- a blank row between
/// any two text lines, so the column reads as separate items.
const FIXED_ROWS: u16 = 9;

/// Axis labels: (band index, text). Bands are log-spaced 60 Hz-7 kHz,
/// see spectrum.rs.
const AXIS: [(usize, &str); 5] = [(0, "60 Hz"), (5, "210"), (10, "735"), (14, "2k"), (19, "7 kHz")];

pub struct View<'a> {
    pub name: &'a str,
    pub track_idx: usize,
    pub track_count: usize,
    pub info: &'a TrackInfo,
    pub elapsed_secs: u32,
    pub volume: i32,
    pub paused: bool,
    pub levels: &'a [u64; N_BANDS],
    pub peaks: &'a [u64; N_BANDS],
}

/// The color of everything that shows the player's state.
fn accent(paused: bool) -> Color {
    if paused { Color::Yellow } else { Color::Green }
}

pub fn render(frame: &mut Frame, v: &View) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    if area.width < 20 || area.height < 4 {
        buf.set_stringn(0, 0, "play: terminal too small", area.width as usize, Style::new());
        return;
    }

    header(buf, area, v);
    footer(buf, area);

    // The content column: centered, capped width, vertically centered
    // in the rows between the header and footer.
    let width = area.width.saturating_sub(4).min(MAX_WIDTH);
    let x = area.x + (area.width - width) / 2;
    let body = area.height - 2;
    let spectrum_rows = body.saturating_sub(FIXED_ROWS + 2).clamp(MIN_SPECTRUM_ROWS, MAX_SPECTRUM_ROWS);
    let content = (FIXED_ROWS + spectrum_rows).min(body);
    let mut y = area.y + 1 + (body - content) / 2;
    let bottom = area.y + area.height - 1; // the footer row
    let accent = accent(v.paused);
    let mut row = |rows: u16| {
        let r = Rect { x, y, width, height: rows.min(bottom.saturating_sub(y)) };
        y = (y + rows).min(bottom);
        r
    };

    track_info(buf, row(3), v);
    row(1);
    Spectrum { levels: v.levels, peaks: v.peaks }.render(row(spectrum_rows), buf);
    axis(buf, row(1));
    row(1);
    progress(buf, row(1), v.elapsed_secs, v.info.total_secs, accent);
    row(1);
    status(buf, row(1), v.paused, v.volume, accent);
}

/// The tab, a rule, and the track counter at the rule's end.
fn header(buf: &mut Buffer, area: Rect, v: &View) {
    let counter = format!(" track {} of {}", v.track_idx, v.track_count);
    let counter_x = area.right().saturating_sub(counter.len() as u16 + 1);
    let x = chrome::tab(buf, area.x + 1, area.y, area.right(), "play", accent(v.paused), ON_ACCENT);
    chrome::rule(buf, x + 1, area.y, counter_x);
    if counter_x > x + 1 {
        chrome::put(buf, counter_x, area.y, area.right(), &counter, fg(Color::Gray));
    }
}

/// Key hints along the bottom row.
fn footer(buf: &mut Buffer, area: Rect) {
    let keys = [("SPACE", "pause"), ("N", "next"), ("Q", "quit"), ("+/-", "volume")];
    chrome::key_hints(buf, Rect { y: area.bottom() - 1, height: 1, ..area }, &keys);
}

fn track_info(buf: &mut Buffer, area: Rect, v: &View) {
    let channels = if v.info.channels == 1 { "mono" } else { "stereo" };
    let rate = if v.info.rate % 1000 == 0 {
        format!("{} kHz", v.info.rate / 1000)
    } else {
        format!("{}.{} kHz", v.info.rate / 1000, v.info.rate % 1000 / 100)
    };
    let codec = if v.info.mp3_kbps > 0 { format!("MP3 {} kbps", v.info.mp3_kbps) } else { "WAV 16-bit PCM".into() };
    buf.set_stringn(area.x, area.y, v.name, area.width as usize, fg(Color::White).add_modifier(Modifier::BOLD));
    if area.height > 2 {
        let details = format!("{codec}   {rate}   {channels}");
        buf.set_stringn(area.x, area.y + 2, details, area.width as usize, fg(Color::Gray));
    }
}

/// Bar geometry shared by the spectrum and its axis: N_BANDS bars of
/// equal width spanning exactly `width` columns, so they line up with
/// the progress and status rows below. Columns that don't divide evenly
/// are spread one apiece across the gaps.
struct Bars {
    bar: u16,
    gap: u16,
    spare: u16,
}

impl Bars {
    fn new(width: u16) -> Bars {
        let n = N_BANDS as u16;
        let gap = if width >= 3 * n { 1 } else { 0 };
        let bar = ((width + gap) / n).saturating_sub(gap).max(1);
        let used = n * bar + (n - 1) * gap;
        Bars { bar, gap, spare: width.saturating_sub(used) }
    }

    /// Column offset of band `i`.
    fn x(&self, i: usize) -> u16 {
        let i = i as u16;
        let n = N_BANDS as u16;
        i * (self.bar + self.gap) + i * self.spare / (n - 1)
    }
}

/// Solid bars (colored spaces) with a `─` peak marker floating above
/// each one while its peak holds.
struct Spectrum<'a> {
    levels: &'a [u64; N_BANDS],
    peaks: &'a [u64; N_BANDS],
}

impl Widget for Spectrum<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let bars = Bars::new(area.width);
        let bar = bars.bar;
        let rows = area.height as u64;
        let to_rows = |level: u64| (level * rows + MAX_LEVEL / 2) / MAX_LEVEL;
        for band in 0..N_BANDS {
            let x = area.x + bars.x(band);
            if x + bar > area.right() {
                break;
            }
            let height = to_rows(self.levels[band]) as u16;
            let peak = to_rows(self.peaks[band]) as u16;
            for i in 0..height {
                let y = area.bottom() - 1 - i;
                buf.set_string(x, y, " ".repeat(bar as usize), Style::new().bg(level_color(i, area.height)));
            }
            if peak > height {
                let y = area.bottom() - peak;
                buf.set_string(x, y, "─".repeat(bar as usize), fg(level_color(peak - 1, area.height)));
            }
        }
    }
}

/// A level meter's color for row `i` (0 = bottom) of `rows`: green for
/// the lower 60%, yellow up to 85%, red above.
fn level_color(i: u16, rows: u16) -> Color {
    let pct = (i as u32 + 1) * 100 / rows.max(1) as u32;
    if pct <= 60 {
        Color::Green
    } else if pct <= 85 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Frequency labels under the bands they name, skipping any that would
/// collide with the previous one on a narrow terminal.
fn axis(buf: &mut Buffer, area: Rect) {
    if area.height == 0 {
        return;
    }
    let bars = Bars::new(area.width);
    let mut free_from = area.x;
    for (i, (band, label)) in AXIS.iter().enumerate() {
        let len = label.len() as u16;
        let (bar_x, bar) = (area.x + bars.x(*band), bars.bar);
        // First label left-aligned to its bar, last right-aligned,
        // the rest centered.
        let x = match i {
            0 => bar_x,
            _ if i == AXIS.len() - 1 => (bar_x + bar).saturating_sub(len),
            _ => (bar_x + bar / 2).saturating_sub(len / 2),
        };
        if x >= free_from && x + len <= area.right() {
            buf.set_string(x, area.y, label, fg(GREY));
            free_from = x + len + 1;
        }
    }
}

/// A meter: `filled` of `width` cells solid in `color`, the rest a grey
/// track.
fn meter(buf: &mut Buffer, x: u16, y: u16, width: u16, filled: u16, color: Color) {
    let filled = filled.min(width);
    buf.set_string(x, y, " ".repeat(filled as usize), Style::new().bg(color));
    buf.set_string(x + filled, y, "─".repeat((width - filled) as usize), fg(GREY));
}

fn clock(secs: u32) -> String {
    format!("{:02}:{:02}", (secs / 60).min(99), secs % 60)
}

/// `01:02  ######────────  03:45`
fn progress(buf: &mut Buffer, area: Rect, elapsed: u32, total: u32, accent: Color) {
    if area.height == 0 || area.width < 16 {
        return;
    }
    let (left, right) = (clock(elapsed), clock(total));
    let track = area.width - 14;
    let filled = if total > 0 { (elapsed.min(total) as u64 * track as u64 / total as u64) as u16 } else { 0 };
    buf.set_string(area.x, area.y, left, fg(Color::White));
    meter(buf, area.x + 7, area.y, track, filled, accent);
    buf.set_string(area.right() - 5, area.y, right, fg(Color::Gray));
}

/// A `PLAYING`/`PAUSED` chip left, volume meter right.
fn status(buf: &mut Buffer, area: Rect, paused: bool, volume: i32, accent: Color) {
    if area.height == 0 {
        return;
    }
    let label = if paused { "PAUSED" } else { "PLAYING" };
    chrome::tab(buf, area.x, area.y, area.right(), label, accent, ON_ACCENT);

    const VOL_CELLS: u16 = 20;
    let label = format!("{volume:>3}%");
    let width = 4 + VOL_CELLS + 1 + label.len() as u16;
    if area.width < width + 12 {
        return;
    }
    let x = area.right() - width;
    buf.set_string(x, area.y, "VOL ", fg(Color::Gray));
    // 0-200%, so 100% is half full.
    meter(buf, x + 4, area.y, VOL_CELLS, (volume.clamp(0, 200) as u16 * VOL_CELLS) / 200, Color::Cyan);
    buf.set_string(area.right() - label.len() as u16, area.y, label, fg(Color::White));
}
