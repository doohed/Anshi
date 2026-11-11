//! Coloring the line as it's typed: the command word blue if it exists
//! (a builtin or a program), red if it doesn't -- so a typo shows before
//! Enter -- arguments naming something that exists cyan, `-options`
//! yellow, everything else plain.

use alloc::vec;
use alloc::vec::Vec;

use crate::commands::{is_builtin, resolve_program};
use crate::term::color;

/// One SGR code string per byte of `line`.
pub fn colors(line: &str) -> Vec<&'static str> {
    let mut out = vec![color::RESET; line.len()];
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut first = true;
    while i < bytes.len() {
        if bytes[i] == b' ' {
            i += 1;
            continue;
        }
        let start = i;
        let quote = matches!(bytes[i], b'"' | b'\'').then_some(bytes[i]);
        i += 1;
        while i < bytes.len() {
            match quote {
                Some(q) if bytes[i] == q => {
                    i += 1;
                    break;
                }
                None if bytes[i] == b' ' => break,
                _ => i += 1,
            }
        }
        let word = line[start..i].trim_matches(|c| c == '"' || c == '\'');
        let c = if first {
            if is_builtin(word) || resolve_program(word).is_some() { color::COMMAND } else { color::ERROR }
        } else if word.starts_with('-') {
            color::OPTION
        } else if anshi::exists(word) {
            color::PATH
        } else {
            color::RESET
        };
        out[start..i].fill(c);
        first = false;
    }
    out
}
