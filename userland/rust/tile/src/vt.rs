//! A virtual terminal: one pane's screen, kept as a grid of cells and
//! updated by feeding it whatever the pane's programs write.
//!
//! It emulates exactly what the real console understands
//! (kernel/src/console/fbconsole.rs), no more and no less -- every
//! program here is written against that console, so a pane has to
//! behave the same:
//!
//! - printable characters (ASCII, or UTF-8 for the console's extra
//!   glyphs) draw at the cursor and advance it, wrapping
//!   *immediately* after the last column (no deferred wrap) and
//!   scrolling once past the last row;
//! - `\n` goes to column 0 of the next row, `\r` to column 0, `\b` steps
//!   back and blanks that cell;
//! - CSI: CUP (`H`/`f`), CUU/CUD/CUF/CUB (`A`-`D`), ED (`J` 0/1/2), EL
//!   (`K` 0/1/2), SGR (`m`: the 16 ANSI colors, bold, dim, reverse,
//!   and their resets), and the
//!   private `?25l`/`?25h` cursor visibility (which the real console
//!   ignores, but a pane needs it to know whether to draw a cursor);
//! - anything else is swallowed silently, like the console does.
//!
//! The immediate-wrap rule matters: it's why anshi-tui's backend never
//! writes a terminal's bottom-right cell, and that holds inside a pane
//! only because the pane wraps and scrolls exactly the same way.

use alloc::vec;
use alloc::vec::Vec;

/// How a cell is drawn: SGR state at the time it was written.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    /// Palette index 0-15; None is the default color.
    pub fg: Option<u8>,
    pub bg: Option<u8>,
    pub bold: bool,
    pub dim: bool,
    pub rev: bool,
}

const PLAIN: Attr = Attr { fg: None, bg: None, bold: false, dim: false, rev: false };

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub attr: Attr,
}

const BLANK: Cell = Cell { ch: ' ', attr: PLAIN };
const MAX_PARAMS: usize = 8;

enum State {
    Normal,
    Esc,
    Csi,
}

pub struct Vt {
    pub cols: u16,
    pub rows: u16,
    cells: Vec<Cell>,
    pub cx: u16,
    pub cy: u16,
    attr: Attr,
    /// Whether the program wants a cursor shown (`?25h`/`?25l`).
    pub cursor_visible: bool,
    state: State,
    /// A UTF-8 character in progress: bits so far, continuation bytes left.
    utf8: (u32, u8),
    params: [u16; MAX_PARAMS],
    nparams: usize,
    private: bool,
}

impl Vt {
    pub fn new(cols: u16, rows: u16) -> Vt {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Vt {
            cols,
            rows,
            cells: vec![BLANK; cols as usize * rows as usize],
            cx: 0,
            cy: 0,
            attr: PLAIN,
            cursor_visible: true,
            state: State::Normal,
            utf8: (0, 0),
            params: [0; MAX_PARAMS],
            nparams: 0,
            private: false,
        }
    }

    pub fn cell(&self, x: u16, y: u16) -> Cell {
        self.cells[y as usize * self.cols as usize + x as usize]
    }

    fn set(&mut self, x: u16, y: u16, cell: Cell) {
        let i = y as usize * self.cols as usize + x as usize;
        self.cells[i] = cell;
    }

    /// Blanks `count` cells from (x, y) along the row -- always to plain
    /// background, like the console's erase_cells(), never to a
    /// reverse-video space.
    fn erase(&mut self, x: u16, y: u16, count: u16) {
        for i in x..(x + count).min(self.cols) {
            self.set(i, y, BLANK);
        }
    }

    fn erase_rows(&mut self, from: u16, to: u16) {
        for y in from..to {
            self.erase(0, y, self.cols);
        }
    }

    fn scroll_up(&mut self) {
        let cols = self.cols as usize;
        self.cells.drain(..cols);
        self.cells.extend(core::iter::repeat_n(BLANK, cols));
    }

    fn next_row(&mut self) {
        self.cy += 1;
        if self.cy >= self.rows {
            self.scroll_up();
            self.cy = self.rows - 1;
        }
    }

    fn put(&mut self, ch: char) {
        self.set(self.cx, self.cy, Cell { ch, attr: self.attr });
        self.cx += 1;
        if self.cx >= self.cols {
            self.cx = 0;
            self.next_row();
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    fn byte(&mut self, b: u8) {
        // UTF-8, decoded the way the console does: a malformed sequence
        // draws one '?'.
        if self.utf8.1 > 0 {
            if b & 0xC0 == 0x80 {
                self.utf8 = ((self.utf8.0 << 6) | (b & 0x3F) as u32, self.utf8.1 - 1);
                if self.utf8.1 == 0 {
                    self.put(char::from_u32(self.utf8.0).unwrap_or('?'));
                }
                return;
            }
            self.utf8.1 = 0;
            self.put('?');
        }
        if b >= 0x80 {
            self.utf8 = match b {
                0xC0..=0xDF => ((b & 0x1F) as u32, 1),
                0xE0..=0xEF => ((b & 0x0F) as u32, 2),
                0xF0..=0xF7 => ((b & 0x07) as u32, 3),
                _ => {
                    self.put('?');
                    (0, 0)
                }
            };
            return;
        }
        match self.state {
            State::Esc => {
                if b == b'[' {
                    self.state = State::Csi;
                    self.params = [0; MAX_PARAMS];
                    self.nparams = 0;
                    self.private = false;
                } else {
                    self.state = State::Normal; // not a CSI -- drop it
                }
            }
            State::Csi => match b {
                b'?' if self.nparams == 0 => self.private = true,
                b'0'..=b'9' => {
                    if self.nparams == 0 {
                        self.nparams = 1;
                    }
                    let p = &mut self.params[self.nparams - 1];
                    *p = p.saturating_mul(10).saturating_add((b - b'0') as u16);
                }
                b';' => {
                    // An omitted parameter still takes a slot.
                    if self.nparams == 0 {
                        self.nparams = 1;
                    }
                    if self.nparams < MAX_PARAMS {
                        self.nparams += 1;
                    }
                }
                _ => {
                    if self.private {
                        self.private_csi(b);
                    } else {
                        self.csi(b);
                    }
                    self.state = State::Normal;
                }
            },
            State::Normal => match b {
                0x1b => self.state = State::Esc,
                b'\n' => {
                    self.cx = 0;
                    self.next_row();
                }
                b'\r' => self.cx = 0,
                0x08 => {
                    if self.cx > 0 {
                        self.cx -= 1;
                        self.set(self.cx, self.cy, Cell { ch: ' ', attr: self.attr });
                    }
                }
                b' '..=b'~' => self.put(b as char),
                _ => {} // other control bytes, and non-ASCII: the console has no glyph
            },
        }
    }

    /// Parameter `n`, or `fallback` if it was omitted (or 0).
    fn param_or(&self, n: usize, fallback: u16) -> u16 {
        if n >= self.nparams || self.params[n] == 0 { fallback } else { self.params[n] }
    }

    fn csi(&mut self, fin: u8) {
        let (cols, rows) = (self.cols, self.rows);
        match fin {
            b'H' | b'f' => {
                self.cy = (self.param_or(0, 1) - 1).min(rows - 1);
                self.cx = (self.param_or(1, 1) - 1).min(cols - 1);
            }
            b'A' => self.cy = self.cy.saturating_sub(self.param_or(0, 1)),
            b'B' => self.cy = self.cy.saturating_add(self.param_or(0, 1)).min(rows - 1),
            b'C' => self.cx = self.cx.saturating_add(self.param_or(0, 1)).min(cols - 1),
            b'D' => self.cx = self.cx.saturating_sub(self.param_or(0, 1)),
            b'J' => match if self.nparams > 0 { self.params[0] } else { 0 } {
                2 => self.erase_rows(0, rows),
                1 => {
                    self.erase_rows(0, self.cy);
                    self.erase(0, self.cy, self.cx + 1);
                }
                _ => {
                    self.erase(self.cx, self.cy, cols - self.cx);
                    self.erase_rows(self.cy + 1, rows);
                }
            },
            b'K' => match if self.nparams > 0 { self.params[0] } else { 0 } {
                2 => self.erase(0, self.cy, cols),
                1 => self.erase(0, self.cy, self.cx + 1),
                _ => self.erase(self.cx, self.cy, cols - self.cx),
            },
            b'm' => {
                if self.nparams == 0 {
                    self.attr = PLAIN;
                }
                for i in 0..self.nparams {
                    let a = &mut self.attr;
                    match self.params[i] {
                        0 => *a = PLAIN,
                        1 => a.bold = true,
                        2 => a.dim = true,
                        7 => a.rev = true,
                        22 => (a.bold, a.dim) = (false, false),
                        27 => a.rev = false,
                        p @ 30..=37 => a.fg = Some((p - 30) as u8),
                        39 => a.fg = None,
                        p @ 40..=47 => a.bg = Some((p - 40) as u8),
                        49 => a.bg = None,
                        p @ 90..=97 => a.fg = Some((p - 90 + 8) as u8),
                        p @ 100..=107 => a.bg = Some((p - 100 + 8) as u8),
                        _ => {} // underline, 256-color, ...: not on this console
                    }
                }
            }
            _ => {}
        }
    }

    fn private_csi(&mut self, fin: u8) {
        if self.nparams > 0 && self.params[0] == 25 {
            match fin {
                b'h' => self.cursor_visible = true,
                b'l' => self.cursor_visible = false,
                _ => {}
            }
        }
    }
}
