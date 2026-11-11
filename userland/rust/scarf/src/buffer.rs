//! The text being edited: lines of bytes, plus loading and saving.
//!
//! Lines are raw bytes, not `String`s: files are whatever the VFS holds,
//! and the console can only show ASCII anyway (ui.rs maps the rest). The
//! buffer always holds at least one line, so the cursor always has
//! somewhere to be.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use anshi::{CString, File};

pub struct Buffer {
    pub lines: Vec<Vec<u8>>,
    /// Empty when the buffer has never been named (`:w <path>` names it).
    pub filename: String,
    /// Unsaved changes.
    pub dirty: bool,
}

impl Buffer {
    pub fn new() -> Self {
        Buffer { lines: vec![Vec::new()], filename: String::new(), dirty: false }
    }

    /// Loads `path`, replacing the buffer. A path that doesn't exist yet
    /// is how you start a new file -- same as vim; it's created on the
    /// first `:w`. Returns the status message to show.
    pub fn open(&mut self, path: &str) -> &'static str {
        let file = CString::new(path).ok().and_then(|p| File::open(&p));
        self.filename = String::from(path);
        self.dirty = false;
        self.lines = vec![Vec::new()];
        let Some(mut file) = file else {
            return "new file";
        };

        let mut lines = Vec::new();
        let mut line = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let n = file.read(&mut chunk);
            if n == 0 {
                break;
            }
            for &b in &chunk[..n] {
                match b {
                    b'\n' => lines.push(core::mem::take(&mut line)),
                    b'\r' => {}
                    _ => line.push(b),
                }
            }
        }
        if !line.is_empty() {
            lines.push(line); // final line, no trailing newline
        }
        if !lines.is_empty() {
            self.lines = lines;
        }
        "opened"
    }

    /// Writes every line back out, each followed by `\n`.
    pub fn save(&mut self) -> Result<&'static str, &'static str> {
        if self.filename.is_empty() {
            return Err("no file name -- use :w <path>");
        }
        let mut file = CString::new(self.filename.as_str())
            .ok()
            .and_then(|p| File::create(&p))
            .ok_or("cannot open file for writing")?;
        let mut out = Vec::new();
        for line in &self.lines {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
        if !file.write_all(&out) {
            return Err("write failed");
        }
        self.dirty = false;
        Ok("written")
    }

    pub fn insert_line(&mut self, at: usize, line: Vec<u8>) {
        self.lines.insert(at, line);
        self.dirty = true;
    }

    /// Removes a line, keeping at least one (empty) line in the buffer.
    pub fn delete_line(&mut self, at: usize) {
        if at < self.lines.len() {
            self.lines.remove(at);
            self.dirty = true;
        }
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
    }
}
