//! Terminal plumbing: decoding keys (including the escape sequences for
//! arrows, Home/End and Delete) and building ANSI output.
//!
//! The shell draws with plain escape codes rather than a full-screen UI
//! library: it's line-oriented and scrolls like any terminal program.
//! The console (kernel/src/console/fbconsole.rs) understands cursor
//! movement, erase, and SGR for the 16 ANSI colors, bold, dim and
//! reverse -- that's all this uses. The console draws no cursor of its
//! own, so the line editor draws one (a reverse-video cell).

use alloc::string::String;
use core::fmt::Write;

use anshi::KeyPoll;

pub enum Key {
    Char(u8),
    Enter,
    Tab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Esc,
    /// Ctrl plus a letter, as the letter (`Ctrl(b'a')` for Ctrl-A).
    Ctrl(u8),
    /// A sequence this shell doesn't use (Page Up, F-keys, ...).
    Other,
    /// stdin is closed for good (a tile pane shutting down).
    Eof,
}

/// The next keypress (waits for one).
pub fn read_key() -> Key {
    let Some(b) = anshi::read_key() else {
        return Key::Eof;
    };
    match b {
        b'\r' | b'\n' => Key::Enter,
        b'\t' => Key::Tab,
        0x7f | 0x08 => Key::Backspace,
        0x1b => escape(),
        1..=26 => Key::Ctrl(b'a' + b - 1),
        _ => Key::Char(b),
    }
}

/// The rest of an escape sequence's bytes arrive right behind the ESC
/// (the keyboard driver queues them; tile writes them in one go), so
/// they're polled for without waiting long: if nothing follows, it was
/// a bare Esc keypress.
fn next_byte() -> Option<u8> {
    for _ in 0..4 {
        match anshi::poll_key() {
            KeyPoll::Key(b) => return Some(b),
            KeyPoll::Closed => return None,
            KeyPoll::Empty => anshi::sched_yield(),
        }
    }
    None
}

fn escape() -> Key {
    match next_byte() {
        Some(b'[') | Some(b'O') => {}
        _ => return Key::Esc,
    }
    let mut num: u16 = 0;
    loop {
        match next_byte() {
            Some(d @ b'0'..=b'9') => num = num.saturating_mul(10).saturating_add((d - b'0') as u16),
            Some(b';') => {}
            Some(b'A') => return Key::Up,
            Some(b'B') => return Key::Down,
            Some(b'C') => return Key::Right,
            Some(b'D') => return Key::Left,
            Some(b'H') => return Key::Home,
            Some(b'F') => return Key::End,
            Some(b'~') => {
                return match num {
                    1 | 7 => Key::Home,
                    4 | 8 => Key::End,
                    3 => Key::Delete,
                    _ => Key::Other,
                };
            }
            _ => return Key::Other,
        }
    }
}

/// Terminal width in columns.
pub fn width() -> usize {
    anshi::window_size().0 as usize
}

// ---------- output ----------

/// SGR codes used across the shell -- one place to tune the look.
pub mod color {
    pub const RESET: &str = "0";
    pub const DIM: &str = "90"; // grey: suggestions, rules, descriptions
    pub const COMMAND: &str = "1;94"; // a command that exists
    pub const ERROR: &str = "1;91"; // a command that doesn't, error text
    pub const PATH: &str = "36"; // an argument naming something that exists
    pub const OPTION: &str = "33"; // -flags
    pub const DIR: &str = "1;94"; // directories in listings
    pub const PROGRAM: &str = "92"; // programs in listings
    pub const OK: &str = "1;92";
}

/// An output buffer written with one write() -- one console flush per
/// redraw, not one per escape code.
pub struct Out(pub String);

impl Out {
    pub fn new() -> Self {
        Out(String::with_capacity(512))
    }

    pub fn push(&mut self, s: &str) -> &mut Self {
        self.0.push_str(s);
        self
    }

    pub fn sgr(&mut self, codes: &str) -> &mut Self {
        let _ = write!(self.0, "\x1b[{codes}m");
        self
    }

    /// `text` in `codes`, then back to plain.
    pub fn styled(&mut self, codes: &str, text: &str) -> &mut Self {
        self.sgr(codes).push(text).sgr(color::RESET)
    }

    pub fn flush(&mut self) {
        anshi::write_all(1, self.0.as_bytes());
        self.0.clear();
    }
}

impl Write for Out {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.push_str(s);
        Ok(())
    }
}

/// Prints `msg` as an error: `sh: ` in red, then the message.
pub fn error(msg: &str) {
    let mut out = Out::new();
    out.styled(color::ERROR, "sh: ").push(msg).push("\n");
    out.flush();
}
