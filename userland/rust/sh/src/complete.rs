//! Tab completion: what could go where the cursor is.
//!
//! The first word on the line completes to builtins and programs (the
//! current directory's and /bin's); any other word completes to paths,
//! listing whichever directory the word so far points into. Each
//! candidate carries a short description for the selection list.

use alloc::string::String;
use alloc::vec::Vec;

use anshi::CString;

use crate::commands::{BUILTINS, is_program};

#[derive(Clone)]
pub struct Candidate {
    /// What replaces the word being completed.
    pub text: String,
    /// What the list shows (the last path component, `/` on directories).
    pub display: String,
    pub desc: &'static str,
    pub is_dir: bool,
}

/// The word the cursor is at the end of: its start offset in `line`,
/// and whether it's the command (first) word.
pub fn word_at(line: &str, cursor: usize) -> (usize, bool) {
    let before = &line[..cursor];
    let start = before.rfind(' ').map_or(0, |i| i + 1);
    let first = before[..start].trim().is_empty();
    (start, first)
}

pub fn candidates(line: &str, cursor: usize) -> Vec<Candidate> {
    let (start, first) = word_at(line, cursor);
    let word = &line[start..cursor];
    let mut out = if first && !word.contains('/') { commands(word) } else { paths(word) };
    out.sort_by(|a, b| a.text.cmp(&b.text));
    out.dedup_by(|a, b| a.text == b.text);
    out
}

fn commands(prefix: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = BUILTINS
        .iter()
        .filter(|b| b.name.starts_with(prefix))
        .map(|b| Candidate { text: b.name.into(), display: b.name.into(), desc: b.desc, is_dir: false })
        .collect();
    for (dir, desc) in [(".", "program here"), ("/bin", "program")] {
        let Some(entries) = CString::new(dir).ok().and_then(|p| anshi::read_dir(&p)) else { continue };
        for e in entries {
            let path = if dir == "." { e.name.clone() } else { alloc::format!("{dir}/{}", e.name) };
            if !e.is_dir && e.name.starts_with(prefix) && is_program(&path) {
                out.push(Candidate { text: e.name.clone(), display: e.name, desc, is_dir: false });
            }
        }
    }
    out
}

/// `word` split at its last `/`: the directory to list (as typed) and
/// the name prefix to match in it.
fn paths(word: &str) -> Vec<Candidate> {
    let (dir_part, prefix) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let listed = if dir_part.is_empty() { "." } else { dir_part };
    let Some(entries) = CString::new(listed).ok().and_then(|p| anshi::read_dir(&p)) else {
        return Vec::new();
    };
    entries
        .into_iter()
        // Dotfiles stay hidden unless the word asks for them.
        .filter(|e| e.name.starts_with(prefix) && (!e.name.starts_with('.') || prefix.starts_with('.')))
        .map(|e| {
            let slash = if e.is_dir { "/" } else { "" };
            let mut text = String::from(dir_part);
            text.push_str(&e.name);
            text.push_str(slash);
            let mut display = e.name.clone();
            display.push_str(slash);
            Candidate { text, display, desc: if e.is_dir { "directory" } else { "file" }, is_dir: e.is_dir }
        })
        .collect()
}

/// The longest prefix every candidate's text shares.
pub fn common_prefix(cands: &[Candidate]) -> String {
    let Some(first) = cands.first() else {
        return String::new();
    };
    let mut len = first.text.len();
    for c in &cands[1..] {
        len = len.min(first.text.bytes().zip(c.text.bytes()).take_while(|(a, b)| a == b).count());
    }
    String::from(&first.text[..len])
}
