//! `scarf` -- a small modal text editor with vim keybindings and a
//! file-explorer sidebar, for Anshi, drawn with ratatui.
//!
//! ```text
//! scarf                  sidebar on the current directory
//! scarf notes.txt        open a file
//! scarf .                sidebar on the shell's directory
//! ```
//!
//! A `no_std` static library, like every Rust program here: see
//! ../../Cargo.toml for how it's linked into an executable. The pieces:
//! buffer.rs (the text, load/save), sidebar.rs (directory listing),
//! editor.rs (modes and keys), ui.rs (drawing).

#![no_std]

extern crate alloc;

mod buffer;
mod editor;
mod sidebar;
mod ui;

use alloc::string::String;
use core::ffi::{CStr, c_char, c_int};

use anshi::KeyPoll;
use editor::{Editor, Focus, Special};

/// The rest of an escape sequence arrives right behind its ESC (the
/// keyboard driver queues it; tile writes it in one go), so it's polled
/// for briefly: nothing following means a plain Esc keypress.
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

/// After an ESC: the special key it starts, or None for a plain Esc.
/// Sequences this editor doesn't use (Page Up, ...) are swallowed as
/// Some(None) rather than replayed as keystrokes.
fn read_escape() -> Option<Option<Special>> {
    match next_byte() {
        Some(b'[') | Some(b'O') => {}
        _ => return None,
    }
    let mut num = 0u16;
    loop {
        let k = match next_byte() {
            Some(d @ b'0'..=b'9') => {
                num = num.saturating_mul(10).saturating_add((d - b'0') as u16);
                continue;
            }
            Some(b'A') => Special::Up,
            Some(b'B') => Special::Down,
            Some(b'C') => Special::Right,
            Some(b'D') => Special::Left,
            Some(b'H') => Special::Home,
            Some(b'F') => Special::End,
            Some(b'~') => match num {
                1 | 7 => Special::Home,
                4 | 8 => Special::End,
                3 => Special::Delete,
                _ => return Some(None),
            },
            _ => return Some(None),
        };
        return Some(Some(k));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const c_char) -> c_int {
    let Some(raw_mode) = anshi::RawMode::enable() else {
        anshi::write_all(1, b"scarf: cannot put the terminal in raw mode\n");
        return 1;
    };

    // Start where the shell was: a launched process inherits its cwd, and
    // getcwd() turns that into an absolute path for the sidebar header.
    let cwd = anshi::getcwd().unwrap_or_else(|| String::from("/"));
    let mut ed = Editor::new(cwd);

    // The argument, if any, is a directory to browse or a file to open.
    if argc > 1 {
        let arg = unsafe { CStr::from_ptr(*argv.add(1)) }.to_str().unwrap_or("");
        let target = if arg.starts_with('/') { String::from(arg) } else { sidebar::join(&ed.sidebar.cwd, arg) };
        // chdir() is the directory test, not opendir(): opendir() is just
        // open(), which succeeds on a regular file too. chdir() also
        // canonicalises -- getcwd() afterwards turns `scarf .` into
        // "/docs" rather than the literal "/docs/.".
        let is_dir = anshi::CString::new(target.as_str()).is_ok_and(|p| anshi::chdir(&p));
        if is_dir {
            ed.sidebar.cwd = anshi::getcwd().unwrap_or(target);
        } else {
            ed.open(&target);
            ed.focus = Focus::Editor;
        }
    }
    if !ed.sidebar.load() {
        ed.set_message("cannot open directory");
    }
    if ed.message.is_empty() || ed.focus == Focus::Sidebar {
        ed.set_message("scarf -- Ctrl-b files, Ctrl-e switch pane, Enter open, i insert, :w :q");
    }

    let mut terminal = anshi_tui::init();
    while !ed.quit {
        let _ = terminal.draw(|frame| {
            let layout = ui::Layout::new(frame.area(), &ed);
            ed.scroll(layout.text_rows as usize, layout.text_cols as usize);
            ed.sidebar.scroll(layout.list_rows as usize);
            ui::render(frame, &ed, &layout);
        });
        match anshi::read_key() {
            Some(0x1b) => match read_escape() {
                None => ed.key(0x1b),
                Some(Some(k)) => ed.special(k),
                Some(None) => {}
            },
            Some(key) => ed.key(key),
            // stdin closed for good (a tile pane shutting down): there's
            // no way to ask about unsaved changes, so quit, like vim does
            // on EOF.
            None => break,
        }
    }

    anshi_tui::restore();
    drop(raw_mode);
    0
}
