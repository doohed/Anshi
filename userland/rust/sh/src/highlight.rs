//! Coloring the line as it's typed: a word in command position (first,
//! or after `|`, `;`, `&&`, `then`, ...) blue if it exists -- a builtin,
//! keyword, function or program -- and red if it doesn't, so a typo
//! shows before Enter; arguments naming something that exists cyan,
//! `-options` yellow, everything else plain.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

use crate::commands::{is_builtin, resolve_program};
use crate::term::color;

const KEYWORDS: [&str; 17] = [
    "if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "case", "esac", "in", "{", "}", "!",
    "function",
];

/// The shell's function names, for coloring them as commands -- the
/// interactive loop refreshes it after each line.
struct Functions(UnsafeCell<Vec<String>>);

// Single-threaded: there's nothing else to race with.
unsafe impl Sync for Functions {}

static FUNCTIONS: Functions = Functions(UnsafeCell::new(Vec::new()));

pub fn set_functions(names: Vec<String>) {
    unsafe { *FUNCTIONS.0.get() = names };
}

fn is_function(name: &str) -> bool {
    unsafe { (*FUNCTIONS.0.get()).iter().any(|f| f == name) }
}

/// Whether a word following `before` would be a command name.
pub fn command_position(before: &str) -> bool {
    let t = before.trim_end();
    if t.ends_with("((") {
        return false; // arithmetic: $((6*7))
    }
    if t.is_empty() || t.ends_with(['|', ';', '&', '(', '{']) {
        return true;
    }
    let last = t.rsplit([' ', '\t']).next().unwrap_or("");
    matches!(last, "then" | "do" | "else" | "elif" | "if" | "while" | "until" | "!")
}

/// Whether `word` is something to run.
pub fn is_command(word: &str) -> bool {
    KEYWORDS.contains(&word) || is_builtin(word) || is_function(word) || resolve_program(word).is_some()
}

/// One SGR code string per byte of `line`.
pub fn colors(line: &str) -> Vec<&'static str> {
    let mut out = vec![color::RESET; line.len()];
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b' ' | b'\t' | b'|' | b';' | b'&' | b'(' | b')' | b'<' | b'>') {
            i += 1;
            continue;
        }
        let start = i;
        let mut quote: Option<u8> = None;
        while i < bytes.len() {
            match (quote, bytes[i]) {
                (Some(q), b) if b == q => quote = None,
                (Some(_), _) => {}
                (None, b'"' | b'\'') => quote = Some(bytes[i]),
                (None, b' ' | b'\t' | b'|' | b';' | b'&' | b'(' | b')' | b'<' | b'>') => break,
                _ => {}
            }
            i += 1;
        }
        let word = line[start..i].trim_matches(|c| c == '"' || c == '\'');
        let before = &line[..start];
        let redirect_target = before.trim_end().ends_with(['<', '>']);
        let c = if !redirect_target && command_position(before) {
            if word.contains('=') && !word.starts_with('=') {
                color::RESET // NAME=value
            } else if word.starts_with('$') || is_command(word) {
                color::COMMAND
            } else {
                color::ERROR
            }
        } else if word.starts_with('-') {
            color::OPTION
        } else if anshi::exists(word) {
            color::PATH
        } else {
            color::RESET
        };
        out[start..i].fill(c);
    }
    out
}
