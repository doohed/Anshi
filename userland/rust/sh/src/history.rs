//! Command history: what the suggestions and Up/Down search draw on.
//!
//! Kept in `/.sh_history`, rewritten after every command so a new shell
//! (another tile pane, or after a reboot once something has synced the
//! disk) starts with it. Repeating a command moves it to the end rather
//! than storing it twice.

use alloc::string::String;
use alloc::vec::Vec;

use anshi::{CString, File};

const FILE: &str = "/.sh_history";
const MAX: usize = 500;

pub struct History {
    items: Vec<String>,
}

impl History {
    /// No history: a script's shell neither reads nor writes the file.
    pub fn empty() -> History {
        History { items: Vec::new() }
    }

    pub fn load() -> History {
        let mut items = Vec::new();
        let file = CString::new(FILE).ok().and_then(|p| File::open(&p));
        if let Some(mut f) = file {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 512];
            loop {
                let n = f.read(&mut chunk);
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            for line in bytes.split(|&b| b == b'\n') {
                if let Ok(s) = core::str::from_utf8(line) {
                    if !s.trim().is_empty() {
                        items.push(String::from(s));
                    }
                }
            }
        }
        let excess = items.len().saturating_sub(MAX);
        items.drain(..excess);
        History { items }
    }

    pub fn add(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        self.items.retain(|l| l != line);
        self.items.push(String::from(line));
        if self.items.len() > MAX {
            self.items.remove(0);
        }
        self.save();
    }

    fn save(&self) {
        let Some(mut f) = CString::new(FILE).ok().and_then(|p| File::create(&p)) else {
            return;
        };
        let mut out = Vec::new();
        for l in &self.items {
            out.extend_from_slice(l.as_bytes());
            out.push(b'\n');
        }
        f.write_all(&out);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn get(&self, i: usize) -> &str {
        &self.items[i]
    }

    /// The most recent entry that starts with `prefix`, is longer than
    /// it, and passes `usable` -- the suggestion shown after the cursor.
    pub fn suggest(&self, prefix: &str, usable: impl Fn(&str) -> bool) -> Option<&str> {
        if prefix.is_empty() {
            return None;
        }
        self.items
            .iter()
            .rev()
            .find(|l| l.len() > prefix.len() && l.starts_with(prefix) && usable(l))
            .map(|l| l.as_str())
    }

    /// Up/Down search: the next entry starting with `prefix`, older
    /// (`older`) or newer than index `from` (None = past the newest).
    /// Returns its index, or None when there are no more.
    pub fn search(&self, prefix: &str, from: Option<usize>, older: bool) -> Option<usize> {
        let matches = |i: &usize| self.items[*i].starts_with(prefix) && self.items[*i] != prefix;
        if older {
            let end = from.unwrap_or(self.items.len());
            (0..end).rev().find(matches)
        } else {
            let start = from? + 1;
            (start..self.items.len()).find(matches)
        }
    }
}
