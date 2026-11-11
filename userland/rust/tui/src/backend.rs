//! A ratatui backend for Anshi's framebuffer console
//! (kernel/src/console/fbconsole.rs). The built-in backends (crossterm,
//! termion, ...) need std; this one only needs `write()` on fd 1.
//!
//! The console understands a small ANSI subset, so that's all this
//! emits: CUP (`ESC[row;colH`), ED/EL (`ESC[nJ`, `ESC[nK`) and SGR for
//! the 16 ANSI colors (foreground and background), bold, dim and
//! reverse video. ratatui colors outside those 16 (indexed 16-255, RGB)
//! are mapped to the nearest of them; other modifiers are dropped. Its font
//! is ASCII plus a few extra glyphs (crate::console_has()): those are
//! sent as UTF-8, and any other symbol a widget draws is mapped to a
//! stand-in.
//!
//! One console quirk shapes `size()`: writing the last column wraps the
//! cursor immediately (no deferred wrap), so writing the bottom-right
//! cell would scroll the whole screen up a line. The backend therefore
//! reports one row fewer than the console has and never touches its
//! last row.

use alloc::vec::Vec;
use core::convert::Infallible;
use core::fmt::Write;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};

pub struct AnshiBackend {
    out: Vec<u8>,
    /// Where the console's cursor is after the last byte queued in `out`
    /// -- lets consecutive cells on a row skip the CUP sequence.
    cursor: Option<(u16, u16)>,
    /// The SGR state the console is in after everything queued so far.
    pen: Pen,
}

impl AnshiBackend {
    pub fn new() -> Self {
        AnshiBackend { out: Vec::with_capacity(16 * 1024), cursor: None, pen: Pen::PLAIN }
    }

    fn goto(&mut self, x: u16, y: u16) {
        let _ = write!(Bytes(&mut self.out), "\x1b[{};{}H", y + 1, x + 1);
        self.cursor = Some((x, y));
    }

    fn set_pen(&mut self, pen: Pen) {
        if pen != self.pen {
            pen.write_sgr(&mut self.out);
            self.pen = pen;
        }
    }
}

/// Everything about how a cell is drawn that the console can show.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: Option<u8>, // palette index 0-15, None = default
    bg: Option<u8>,
    bold: bool,
    dim: bool,
    rev: bool,
}

impl Pen {
    const PLAIN: Pen = Pen { fg: None, bg: None, bold: false, dim: false, rev: false };

    fn of(cell: &Cell) -> Pen {
        Pen {
            fg: palette_index(cell.fg),
            bg: palette_index(cell.bg),
            bold: cell.modifier.contains(Modifier::BOLD),
            dim: cell.modifier.contains(Modifier::DIM),
            rev: cell.modifier.contains(Modifier::REVERSED),
        }
    }

    /// One SGR sequence that resets everything, then sets this pen.
    fn write_sgr(self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        if self.bold {
            out.extend_from_slice(b";1");
        }
        if self.dim {
            out.extend_from_slice(b";2");
        }
        if self.rev {
            out.extend_from_slice(b";7");
        }
        if let Some(i) = self.fg {
            let _ = write!(Bytes(out), ";{}", if i < 8 { 30 + i } else { 90 + i - 8 });
        }
        if let Some(i) = self.bg {
            let _ = write!(Bytes(out), ";{}", if i < 8 { 40 + i } else { 100 + i - 8 });
        }
        out.push(b'm');
    }
}

/// The 16 ANSI colors, roughly as the console draws them
/// (kernel/src/console/fbconsole.rs's PALETTE) -- only used to pick the
/// nearest one for an RGB or 256-color value.
const PALETTE_RGB: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), (0xD0, 0x50, 0x4F), (0x60, 0xB0, 0x50), (0xD8, 0xA8, 0x40),
    (0x4A, 0x80, 0xD8), (0xB0, 0x60, 0xC8), (0x40, 0xA8, 0xB8), (0xC0, 0xC0, 0xC0),
    (0x5C, 0x5C, 0x5C), (0xFF, 0x70, 0x70), (0x90, 0xE0, 0x80), (0xFF, 0xD8, 0x70),
    (0x70, 0xB0, 0xFF), (0xE0, 0x90, 0xFF), (0x70, 0xE0, 0xF0), (0xFF, 0xFF, 0xFF),
];

fn nearest(r: u8, g: u8, b: u8) -> u8 {
    let dist = |&(pr, pg, pb): &(u8, u8, u8)| {
        let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
        d(r, pr) + d(g, pg) + d(b, pb)
    };
    (0..16).min_by_key(|&i| dist(&PALETTE_RGB[i])).unwrap_or(7) as u8
}

fn palette_index(c: Color) -> Option<u8> {
    Some(match c {
        Color::Reset => return None,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        Color::Indexed(i) if i < 16 => i,
        Color::Indexed(i) if i >= 232 => {
            let v = 8 + (i - 232) * 10; // the 24-step grey ramp
            nearest(v, v, v)
        }
        Color::Indexed(i) => {
            let i = i - 16; // the 6x6x6 cube
            let level = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
            nearest(level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        Color::Rgb(r, g, b) => nearest(r, g, b),
    })
}

/// Lets `write!` format straight into the output byte buffer.
struct Bytes<'a>(&'a mut Vec<u8>);

impl Write for Bytes<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// ASCII stand-in for a cell's symbol.
/// The character to draw for a cell's symbol: itself when the console
/// has a glyph for it (crate::console_has()), else an ASCII stand-in.
fn shown(symbol: &str) -> char {
    let mut chars = symbol.chars();
    let c = match (chars.next(), chars.next()) {
        (None, _) => return ' ',
        (Some(c), None) => c,
        _ => return '?', // a grapheme cluster -- no way to show it
    };
    if crate::console_has(c) {
        return c;
    }
    match c {
        '━' | '═' => '─',
        '┃' | '║' => '│',
        '▇' | '▆' | '▅' | '▄' | '▃' | '▂' | '▁' | '■' => '█',
        _ => '?',
    }
}

impl Backend for AnshiBackend {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Infallible>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, cell) in content {
            if self.cursor != Some((x, y)) {
                self.goto(x, y);
            }
            self.set_pen(Pen::of(cell));
            let c = shown(cell.symbol());
            self.out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            self.cursor = Some((x + 1, y));
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Infallible> {
        self.out.extend_from_slice(b"\x1b[?25l");
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Infallible> {
        self.out.extend_from_slice(b"\x1b[?25h");
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Infallible> {
        let (x, y) = self.cursor.unwrap_or((0, 0));
        Ok(Position { x, y })
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Infallible> {
        let p = position.into();
        self.goto(p.x, p.y);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Infallible> {
        self.set_pen(Pen::PLAIN);
        self.out.extend_from_slice(b"\x1b[2J");
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Infallible> {
        self.set_pen(Pen::PLAIN);
        self.out.extend_from_slice(match clear_type {
            ClearType::All => b"\x1b[2J",
            ClearType::AfterCursor => b"\x1b[0J",
            ClearType::BeforeCursor => b"\x1b[1J",
            ClearType::CurrentLine => b"\x1b[2K",
            ClearType::UntilNewLine => b"\x1b[0K",
        });
        Ok(())
    }

    fn size(&self) -> Result<Size, Infallible> {
        let (cols, rows) = anshi::window_size();
        Ok(Size { width: cols, height: rows.saturating_sub(1).max(1) })
    }

    fn window_size(&mut self) -> Result<WindowSize, Infallible> {
        Ok(WindowSize { columns_rows: self.size()?, pixels: Size { width: 0, height: 0 } })
    }

    fn flush(&mut self) -> Result<(), Infallible> {
        anshi::write_all(1, &self.out);
        self.out.clear();
        Ok(())
    }
}
