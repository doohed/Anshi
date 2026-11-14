//! The interactive line editor.
//!
//! - **Suggestions.** As you type, the rest of the most recent matching
//!   command from history appears after the cursor in grey (or, with no
//!   history match, the only possible completion of the current word).
//!   Right, End or Ctrl-F at the end of the line accepts it.
//! - **Highlighting.** The line is colored as it's typed (highlight.rs).
//! - **Completion.** Tab completes the word at the cursor (complete.rs):
//!   one candidate is filled in; several are filled in as far as they
//!   agree, and if that's no further, listed below the line. Tab again
//!   cycles a selection through the list, filling each in; Up/Down move
//!   it a row; Enter keeps the choice, any other key carries on editing.
//! - **History.** Up/Down walk through past commands that start with
//!   whatever was typed before the first Up.
//!
//! Drawing: the input line is redrawn in full on every key -- `\r`,
//! the prompt's part of the line (prompt.rs), the visible part of the line, `ESC[J` to clear the
//! rest of the line and anything below (an old completion list), then
//! the list if open, then the cursor moved back. The console draws no
//! cursor of its own, so the editor draws one: the character under it in
//! reverse video. A line longer than the terminal scrolls sideways
//! rather than wrapping, and nothing is ever written into the last
//! column -- the console wraps the moment a glyph lands there.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::complete::{self, Candidate};
use crate::highlight;
use crate::history::History;
use crate::term::{self, Key, Out, color};

/// Completion-list rows shown at once.
const LIST_ROWS: usize = 8;

pub enum ReadResult {
    Line(String),
    /// Ctrl-C: the line was abandoned.
    Cancelled,
    /// Ctrl-L: clear the screen and prompt again.
    Clear,
    /// stdin closed for good.
    Eof,
}

struct List {
    cands: Vec<Candidate>,
    sel: Option<usize>,
    /// Where the word being completed starts.
    start: usize,
    /// First row shown, when there are more than LIST_ROWS.
    top: usize,
}

struct Editor<'a> {
    hist: &'a History,
    /// The prompt's part of the input line (styled), and its width.
    prefix: &'a str,
    prefix_w: usize,
    buf: String,
    cur: usize,
    scroll: usize,
    /// History navigation: the entry shown, and what was typed before it began.
    hist_pos: Option<usize>,
    hist_prefix: Option<String>,
    list: Option<List>,
}

/// Reads one line, with the terminal in raw mode only while it does --
/// a program the line then runs gets the normal (cooked) terminal.
/// `prefix` is the prompt's part of the input line, `prefix_w` columns
/// wide (prompt.rs).
pub fn read_line(hist: &History, prefix: &str, prefix_w: usize) -> ReadResult {
    let _raw = anshi::RawMode::enable();
    let mut ed = Editor {
        hist,
        prefix,
        prefix_w,
        buf: String::new(),
        cur: 0,
        scroll: 0,
        hist_pos: None,
        hist_prefix: None,
        list: None,
    };
    loop {
        ed.render(false);
        if let Some(result) = ed.handle(term::read_key()) {
            ed.list = None;
            ed.render(true);
            let mut out = Out::new();
            if let ReadResult::Cancelled = result {
                out.styled(color::DIM, " ^C");
            }
            out.push("\n");
            out.flush();
            return result;
        }
    }
}

impl Editor<'_> {
    fn handle(&mut self, key: Key) -> Option<ReadResult> {
        // Any key but the list's own closes it, keeping what it filled in.
        let list_key = matches!(key, Key::Tab | Key::Up | Key::Down | Key::Enter | Key::Ctrl(b'p' | b'n'));
        if !list_key {
            self.list = None;
        }
        match key {
            Key::Char(c) if (32..127).contains(&c) => {
                self.buf.insert(self.cur, c as char);
                self.cur += 1;
                self.edited();
            }
            Key::Backspace => {
                if self.cur > 0 {
                    self.cur -= 1;
                    self.buf.remove(self.cur);
                    self.edited();
                }
            }
            Key::Delete | Key::Ctrl(b'd') => {
                if self.cur < self.buf.len() {
                    self.buf.remove(self.cur);
                    self.edited();
                }
            }
            Key::Left | Key::Ctrl(b'b') => self.cur = self.cur.saturating_sub(1),
            Key::Home | Key::Ctrl(b'a') => self.cur = 0,
            Key::Right | Key::Ctrl(b'f') => {
                if self.cur < self.buf.len() {
                    self.cur += 1;
                } else {
                    self.accept_suggestion();
                }
            }
            Key::End | Key::Ctrl(b'e') => {
                if self.cur < self.buf.len() {
                    self.cur = self.buf.len();
                } else {
                    self.accept_suggestion();
                }
            }
            Key::Up | Key::Ctrl(b'p') => {
                if self.list.is_some() {
                    self.list_move_rows(-1);
                } else {
                    self.history(true);
                }
            }
            Key::Down | Key::Ctrl(b'n') => {
                if self.list.is_some() {
                    self.list_move_rows(1);
                } else {
                    self.history(false);
                }
            }
            Key::Tab => self.complete(),
            Key::Enter => {
                if self.list.as_ref().is_some_and(|l| l.sel.is_some()) {
                    self.list = None; // keep the choice, keep editing
                } else {
                    return Some(ReadResult::Line(self.buf.clone()));
                }
            }
            Key::Ctrl(b'c') => return Some(ReadResult::Cancelled),
            Key::Ctrl(b'l') => return Some(ReadResult::Clear),
            Key::Ctrl(b'u') => {
                self.buf.drain(..self.cur);
                self.cur = 0;
                self.edited();
            }
            Key::Ctrl(b'k') => {
                self.buf.truncate(self.cur);
                self.edited();
            }
            Key::Ctrl(b'w') => {
                let bytes = self.buf.as_bytes();
                let mut i = self.cur;
                while i > 0 && bytes[i - 1] == b' ' {
                    i -= 1;
                }
                while i > 0 && bytes[i - 1] != b' ' {
                    i -= 1;
                }
                self.buf.drain(i..self.cur);
                self.cur = i;
                self.edited();
            }
            Key::Eof => return Some(ReadResult::Eof),
            _ => {}
        }
        None
    }

    /// Typing ends a history walk: Up starts a new one from the new text.
    fn edited(&mut self) {
        self.hist_pos = None;
        self.hist_prefix = None;
    }

    fn set_line(&mut self, line: String) {
        self.buf = line;
        self.cur = self.buf.len();
    }

    fn history(&mut self, older: bool) {
        let prefix = self.hist_prefix.get_or_insert_with(|| self.buf.clone()).clone();
        match self.hist.search(&prefix, self.hist_pos, older) {
            Some(i) => {
                self.hist_pos = Some(i);
                self.set_line(String::from(self.hist.get(i)));
            }
            None if !older && self.hist_pos.is_some() => {
                // Back past the newest: what was typed before.
                self.hist_pos = None;
                self.set_line(prefix);
            }
            None => {}
        }
    }

    // ---------- suggestions ----------

    /// What to show greyed after the cursor (only at the end of the line).
    fn suggestion(&self) -> Option<String> {
        if self.cur != self.buf.len() || self.buf.trim().is_empty() || self.list.is_some() {
            return None;
        }
        // Only commands that would run now: a typo from history (or a
        // program since deleted) isn't worth suggesting.
        let runnable = |line: &str| highlight::is_command(line.split(' ').next().unwrap_or(""));
        if let Some(h) = self.hist.suggest(&self.buf, runnable) {
            return Some(String::from(&h[self.buf.len()..]));
        }
        let (start, _) = complete::word_at(&self.buf, self.cur);
        if start == self.cur {
            return None; // no word started yet: too early to guess
        }
        let cands = complete::candidates(&self.buf, self.cur);
        match cands.as_slice() {
            [only] if only.text.len() > self.cur - start => Some(String::from(&only.text[self.cur - start..])),
            _ => None,
        }
    }

    fn accept_suggestion(&mut self) {
        if let Some(s) = self.suggestion() {
            self.buf.push_str(&s);
            self.cur = self.buf.len();
            self.edited();
        }
    }

    // ---------- completion ----------

    /// Replaces `buf[start..cur]` with `text`, cursor after it.
    fn replace_word(&mut self, start: usize, text: &str) {
        self.buf.replace_range(start..self.cur, text);
        self.cur = start + text.len();
    }

    fn complete(&mut self) {
        if let Some(list) = &mut self.list {
            let next = list.sel.map_or(0, |s| (s + 1) % list.cands.len());
            list.sel = Some(next);
            let (start, text) = (list.start, list.cands[next].text.clone());
            self.replace_word(start, &text);
            return;
        }
        let (start, _) = complete::word_at(&self.buf, self.cur);
        let cands = complete::candidates(&self.buf, self.cur);
        match cands.len() {
            0 => {}
            1 => {
                let c = &cands[0];
                let text = if c.is_dir { c.text.clone() } else { format!("{} ", c.text) };
                self.replace_word(start, &text);
            }
            _ => {
                let common = complete::common_prefix(&cands);
                if common.len() > self.cur - start {
                    self.replace_word(start, &common);
                } else {
                    self.list = Some(List { cands, sel: None, start, top: 0 });
                }
            }
        }
        self.edited();
    }

    /// Moves the list selection a row up (-1) or down (+1).
    fn list_move_rows(&mut self, delta: isize) {
        let cols = self.list_layout().1;
        let Some(list) = &mut self.list else { return };
        let n = list.cands.len() as isize;
        let cur = list.sel.map_or(if delta > 0 { -(cols as isize) } else { n }, |s| s as isize);
        let next = (cur + delta * cols as isize).clamp(0, n - 1) as usize;
        list.sel = Some(next);
        let (start, text) = (list.start, list.cands[next].text.clone());
        self.replace_word(start, &text);
    }

    /// (column width, columns) for the list at the current width.
    fn list_layout(&self) -> (usize, usize) {
        let Some(list) = &self.list else { return (1, 1) };
        let usable = term::width().saturating_sub(1).max(1);
        let name_w = list.cands.iter().map(|c| c.display.len()).max().unwrap_or(0);
        let desc_w = list.cands.iter().map(|c| c.desc.len()).max().unwrap_or(0);
        let col_w = (name_w + 2 + desc_w + 3).min(usable);
        (col_w, (usable / col_w).max(1))
    }

    // ---------- drawing ----------

    fn render(&mut self, done: bool) {
        let usable = term::width().saturating_sub(1).max(4);
        let avail = usable.saturating_sub(self.prefix_w).max(4);
        let colors = highlight::colors(&self.buf);
        let sugg = if done { None } else { self.suggestion() };

        // Keep the cursor in view (one extra column for it at the end).
        if self.cur < self.scroll {
            self.scroll = self.cur;
        }
        if self.cur >= self.scroll + avail {
            self.scroll = self.cur + 1 - avail;
        }
        if done {
            self.scroll = self.buf.len().saturating_sub(avail - 1).min(self.scroll);
        }

        let mut out = Out::new();
        out.push("\r").push(self.prefix);
        let mut pen = String::new();
        let mut set = |out: &mut Out, code: String| {
            if code != pen {
                out.sgr(&code);
                pen = code;
            }
        };
        let end = (self.scroll + avail).min(self.buf.len());
        for i in self.scroll..end {
            let cursor = !done && i == self.cur;
            set(&mut out, format!("0;{}{}", colors[i], if cursor { ";7" } else { "" }));
            out.push(&self.buf[i..i + 1]);
        }
        let mut shown = end - self.scroll;
        // The cursor at the end of the line sits on the suggestion's first
        // character, or a blank.
        if !done && self.cur == self.buf.len() && shown < avail {
            let s = sugg.as_deref().unwrap_or("");
            let under = if s.is_empty() { " " } else { &s[..1] };
            set(&mut out, format!("0;{};7", color::DIM));
            out.push(under);
            shown += 1;
            let rest = &s[s.len().min(1)..];
            let fit = rest.len().min(avail - shown);
            set(&mut out, format!("0;{}", color::DIM));
            out.push(&rest[..fit]);
        }
        out.sgr(color::RESET).push("\x1b[J");

        let rows = if done { 0 } else { self.render_list(&mut out, usable) };
        if rows > 0 {
            let _ = write!(out, "\x1b[{rows}A");
        }
        let col = self.prefix_w + self.cur.saturating_sub(self.scroll);
        let _ = write!(out, "\r\x1b[{col}C");
        out.flush();
    }

    /// Draws the completion list under the line; returns the rows used.
    fn render_list(&mut self, out: &mut Out, usable: usize) -> usize {
        let (col_w, cols) = self.list_layout();
        let Some(list) = &mut self.list else { return 0 };
        let n = list.cands.len();
        let total_rows = n.div_ceil(cols);
        let shown_rows = total_rows.min(LIST_ROWS);
        if let Some(sel) = list.sel {
            let row = sel / cols;
            if row < list.top {
                list.top = row;
            } else if row >= list.top + shown_rows {
                list.top = row + 1 - shown_rows;
            }
        }
        let name_w = list.cands.iter().map(|c| c.display.len()).max().unwrap_or(0);

        for r in list.top..list.top + shown_rows {
            out.push("\r\n");
            for c in 0..cols {
                let i = r * cols + c;
                let Some(cand) = list.cands.get(i) else { break };
                let selected = list.sel == Some(i);
                let name_color = if cand.is_dir {
                    color::DIR
                } else if cand.desc.starts_with("program") {
                    color::PROGRAM
                } else if cand.desc != "file" {
                    color::COMMAND
                } else {
                    color::RESET
                };
                let rev = if selected { ";7" } else { "" };
                let mut cell = format!("{:w$}  {}", cand.display, cand.desc, w = name_w);
                cell.truncate(col_w.saturating_sub(1));
                let (name, desc) = cell.split_at(cand.display.len().min(cell.len()));
                out.sgr(&format!("0;{name_color}{rev}")).push(name);
                out.sgr(&format!("0;{}{rev}", color::DIM)).push(desc);
                out.sgr(color::RESET);
                let pad = col_w.min(usable) - cell.len();
                let _ = write!(out, "{:pad$}", "");
            }
            out.push("\x1b[K");
        }
        let mut rows = shown_rows;
        if total_rows > shown_rows {
            out.push("\r\n");
            let note = format!(
                "rows {}-{} of {} -- Tab cycles, Up/Down move",
                list.top + 1,
                list.top + shown_rows,
                total_rows
            );
            out.styled(color::DIM, &note[..note.len().min(usable)]).push("\x1b[K");
            rows += 1;
        }
        rows
    }
}
