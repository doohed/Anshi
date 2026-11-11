//! Drawing the prompt from a Config (config.rs).
//!
//! The pieces are *segments*: the system name, the working directory,
//! the last command's duration (when slow) and its status. How they're
//! drawn depends on the style:
//!
//! ```text
//! lean      Anshi /code/project ·················· 2.3s ✔
//! classic   [ Anshi  /code/project ]············[ 2.3s  ✔ ]   one grey band
//! rainbow   [ Anshi ][ /code/project ]··········[ 2.3s ][ ✔ ]   a color each
//! ```
//!
//! with the blocks' ends sharp, round or flat. On two lines, the left
//! segments sit on the first line and the right ones at its far end,
//! joined by a blank, dotted or solid line, optionally framed by `╭─`
//! and `╰─`; the command is typed on the second line after `❯` (or
//! `>`), green or red to match the last status. On one line, the
//! duration and a failing status join the left segments and `❯`
//! follows them.
//!
//! The arrows, rounds, frame and icons are glyphs the console draws
//! beyond ASCII (kernel/src/console/font.rs). Everything fits
//! itself to the width: the path shortens from the left first, then the
//! system name goes, then the right-hand segments.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::config::{Config, Ends, Fill, Style};

/// Commands shorter than this don't show their duration.
const SHOW_DURATION_MS: u64 = 500;
/// On one line, the input keeps at least this many columns.
const MIN_INPUT: usize = 24;

const GREY: u8 = 8; // frame, fill, and the classic band
const BLACK: u8 = 0;

pub struct State {
    pub cwd: String,
    pub status: i32,
    pub duration_ms: u64,
}

pub struct Rendered {
    /// Printed once, before the input line (blank line, first line).
    pub header: String,
    /// Drawn at the start of the input line on every redraw.
    pub prefix: String,
    /// `prefix`'s width in columns.
    pub prefix_width: usize,
}

struct Seg {
    text: String,
    /// Rainbow: text and background colors.
    fg: u8,
    bg: u8,
    /// Lean and classic: the text color.
    plain: u8,
    /// Classic only, when it differs from `plain`.
    classic: Option<u8>,
    bold: bool,
}

/// Styled text being built, with its width in columns.
struct Line {
    s: String,
    w: usize,
}

impl Line {
    fn new() -> Self {
        Line { s: String::new(), w: 0 }
    }

    fn put(&mut self, text: &str, fg: Option<u8>, bg: Option<u8>, bold: bool) {
        let mut sgr = String::from("\x1b[0");
        if bold {
            sgr.push_str(";1");
        }
        if let Some(i) = fg {
            sgr.push_str(&format!(";{}", if i < 8 { 30 + i as u32 } else { 90 + i as u32 - 8 }));
        }
        if let Some(i) = bg {
            sgr.push_str(&format!(";{}", if i < 8 { 40 + i as u32 } else { 100 + i as u32 - 8 }));
        }
        sgr.push('m');
        self.s.push_str(&sgr);
        self.s.push_str(text);
        self.w += text.chars().count();
    }

    fn append(&mut self, other: &Line) {
        self.s.push_str(&other.s);
        self.w += other.w;
    }
}

pub fn render(cfg: &Config, st: &State, width: usize, fresh: bool) -> Rendered {
    let usable = width.saturating_sub(1).max(8); // the last column wraps: never touch it
    let ok = st.status == 0;
    let mut header = String::new();
    if cfg.sparse && !fresh {
        header.push('\n');
    }

    let mark = {
        let mut l = Line::new();
        if cfg.two_lines && cfg.frame {
            l.put("╰─", Some(GREY), None, false);
        }
        l.put(if cfg.glyphs { "❯" } else { ">" }, Some(if ok { 10 } else { 9 }), None, true);
        l.put(" ", None, None, false);
        l.s.push_str("\x1b[0m");
        l
    };

    if cfg.two_lines {
        let frame = if cfg.frame {
            let mut l = Line::new();
            l.put(if cfg.style == Style::Lean { "╭─ " } else { "╭─" }, Some(GREY), None, false);
            l
        } else {
            Line::new()
        };
        // Try with everything, then without the system name, then
        // without the right side; the path takes whatever room is left.
        for (sys, right_side) in [(cfg.show_system, true), (false, true), (false, false)] {
            let right_segs = if right_side { right(cfg, st, true) } else { Vec::new() };
            let r = block(cfg, &right_segs, Side::Right);
            let bare = block(cfg, &left(sys, String::new()), Side::Left);
            let gap = if right_segs.is_empty() { 0 } else { 5 };
            let room = usable.saturating_sub(frame.w + bare.w + r.w + gap);
            let last = !right_side;
            if room >= 8 || last {
                let l = block(cfg, &left(sys, shorten(&st.cwd, room)), Side::Left);
                let mut line = Line::new();
                line.append(&frame);
                line.append(&l);
                if !right_segs.is_empty() {
                    let n = usable.saturating_sub(line.w + r.w);
                    let fill_char = match cfg.fill {
                        Fill::Blank => " ",
                        Fill::Dotted => "·",
                        Fill::Solid => "─",
                    };
                    let mut fill = String::from(" ");
                    for _ in 0..n.saturating_sub(2) {
                        fill.push_str(fill_char);
                    }
                    if n >= 2 {
                        fill.push(' ');
                    }
                    line.put(&fill, Some(GREY), None, false);
                    line.append(&r);
                }
                header.push_str(&line.s);
                header.push_str("\x1b[0m\n");
                break;
            }
        }
        return Rendered { header, prefix: mark.s, prefix_width: mark.w };
    }

    // One line: duration and a failure join the left segments, then ❯.
    let budget = usable.saturating_sub(MIN_INPUT);
    for sys in [cfg.show_system, false] {
        let mut extra = right(cfg, st, false);
        let mut segs = left(sys, String::new());
        segs.append(&mut extra);
        let bare = block(cfg, &segs, Side::Left);
        let room = budget.saturating_sub(bare.w + 1 + mark.w);
        if room >= 8 || !sys {
            let mut segs = left(sys, shorten(&st.cwd, room.max(4)));
            segs.append(&mut right(cfg, st, false));
            let mut line = block(cfg, &segs, Side::Left);
            line.put(" ", None, None, false);
            line.append(&mark);
            return Rendered { header, prefix: line.s, prefix_width: line.w };
        }
    }
    unreachable!()
}

fn left(sys: bool, path: String) -> Vec<Seg> {
    let mut v = Vec::new();
    if sys {
        v.push(Seg { text: "Anshi".into(), fg: BLACK, bg: 5, plain: 13, classic: None, bold: false });
    }
    // White reads better than blue on the classic grey band.
    v.push(Seg { text: path, fg: 15, bg: 4, plain: 12, classic: Some(15), bold: true });
    v
}

/// Duration (when slow) and status. `always_status`: show `✔` on
/// success too (two lines); one line only shows a failure.
fn right(cfg: &Config, st: &State, always_status: bool) -> Vec<Seg> {
    let mut v = Vec::new();
    if st.duration_ms >= SHOW_DURATION_MS {
        v.push(Seg { text: fmt_ms(st.duration_ms), fg: BLACK, bg: 3, plain: 11, classic: None, bold: false });
    }
    if st.status == 0 {
        if always_status {
            let t = if cfg.glyphs { "✔" } else { "ok" };
            v.push(Seg { text: t.into(), fg: BLACK, bg: 2, plain: 10, classic: None, bold: true });
        }
    } else {
        let t = if cfg.glyphs { format!("✘ {}", st.status) } else { format!("x {}", st.status) };
        v.push(Seg { text: t, fg: 15, bg: 1, plain: 9, classic: None, bold: true });
    }
    v
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// One side's segments drawn in the configured style.
fn block(cfg: &Config, segs: &[Seg], side: Side) -> Line {
    let mut l = Line::new();
    if segs.is_empty() {
        return l;
    }
    if cfg.style == Style::Lean {
        for (i, s) in segs.iter().enumerate() {
            if i > 0 {
                l.put(" ", None, None, false);
            }
            l.put(&s.text, Some(s.plain), None, s.bold);
        }
        return l;
    }

    let rainbow = cfg.style == Style::Rainbow;
    let bg = |s: &Seg| if rainbow { s.bg } else { GREY };
    let fg = |s: &Seg| if rainbow { s.fg } else { s.classic.unwrap_or(s.plain) };
    // (outer end, inner divider) glyphs for each side and shape.
    let (start, sep, end) = match (side, cfg.ends) {
        (Side::Left, Ends::Sharp) => ("", "\u{E0B0}", "\u{E0B0}"),
        (Side::Left, Ends::Round) => ("\u{E0B6}", "\u{E0B4}", "\u{E0B4}"),
        (Side::Right, Ends::Sharp) => ("\u{E0B2}", "\u{E0B2}", ""),
        (Side::Right, Ends::Round) => ("\u{E0B6}", "\u{E0B6}", "\u{E0B4}"),
        (_, Ends::Flat) => ("", "", ""),
    };
    let thin = match (side, cfg.ends) {
        (Side::Left, Ends::Sharp) => "\u{E0B1}",
        (Side::Left, Ends::Round) => "\u{E0B5}",
        (Side::Right, Ends::Sharp) => "\u{E0B3}",
        (Side::Right, Ends::Round) => "\u{E0B7}",
        (_, Ends::Flat) => "│",
    };

    if !start.is_empty() {
        l.put(start, Some(bg(&segs[0])), None, false);
    }
    for (i, s) in segs.iter().enumerate() {
        if i > 0 {
            let prev = &segs[i - 1];
            if !rainbow {
                // Classic: one band, a thin dark divider.
                l.put(thin, Some(BLACK), Some(GREY), false);
            } else if !sep.is_empty() {
                // The arrow is drawn in the color of the segment it
                // points out of, over the one it points into.
                match side {
                    Side::Left => l.put(sep, Some(bg(prev)), Some(bg(s)), false),
                    Side::Right => l.put(sep, Some(bg(s)), Some(bg(prev)), false),
                }
            }
        }
        l.put(&format!(" {} ", s.text), Some(fg(s)), Some(bg(s)), s.bold);
    }
    if !end.is_empty() {
        l.put(end, Some(bg(&segs[segs.len() - 1])), None, false);
    }
    l
}

/// `path` cut to `room` columns by dropping its start: `…/project/src`.
fn shorten(path: &str, room: usize) -> String {
    let n = path.chars().count();
    if n <= room {
        return String::from(path);
    }
    let keep = room.saturating_sub(1);
    let tail: String = path.chars().skip(n - keep).collect();
    format!("…{tail}")
}

/// `850ms`, `3.2s`, `1m05s`.
fn fmt_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}.{}s", ms / 1000, ms % 1000 / 100)
    } else {
        format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000)
    }
}
