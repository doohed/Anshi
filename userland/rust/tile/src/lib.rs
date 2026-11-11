//! `tile` -- a fixed-grid terminal multiplexer: up to four independent
//! `sh` processes side by side, each with its own virtual terminal, so
//! anything that runs on the console runs in a pane too -- including
//! full-screen programs like `scarf` and `play`.
//!
//! ```text
//! tile          two panes, side by side
//! tile 4        a 2x2 grid
//! ```
//!
//! How a pane works (pane.rs): `sh` is forked with pipes for stdin and
//! stdout and its window size set to the pane's (TIOCSWINSZ, which
//! everything it runs inherits). Its output is fed through a terminal
//! emulator (vt.rs) that understands the same ANSI subset as the real
//! console, and the emulated screens are composed into one ratatui frame
//! (ui.rs) -- ratatui's diffing means only changed cells reach the
//! console. Keys go to the focused pane's stdin; programs reading keys
//! with poll_key() get them from that pipe too (the kernel routes
//! poll_key() to a piped stdin).
//!
//! Keys: Ctrl-b is the prefix, as in tmux. Ctrl-b then a digit focuses
//! that pane, `o` the next one, `q` quits, and a second Ctrl-b sends a
//! literal Ctrl-b to the pane (scarf uses it to toggle its sidebar).
//!
//! Nothing here blocks: pipes never do, and keys are polled. When a pass
//! over the keyboard and every pane finds nothing to do, tile yields the
//! CPU rather than spinning, so the programs in the panes get to run.

#![no_std]

extern crate alloc;

mod pane;
mod ui;
mod vt;

use alloc::vec::Vec;
use core::ffi::{CStr, c_char, c_int};

use anshi::KeyPoll;
use ratatui::layout::Rect;

use pane::Pane;

const MAX_PANES: usize = 4;
const CTRL_B: u8 = 0x02;
const SHELL: &CStr = c"/bin/sh";

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const c_char) -> c_int {
    let mut count = 2;
    if argc > 1 {
        let arg = unsafe { CStr::from_ptr(*argv.add(1)) }.to_bytes();
        match core::str::from_utf8(arg).ok().and_then(|s| s.parse::<usize>().ok()) {
            Some(n) if (1..=MAX_PANES).contains(&n) => count = n,
            _ => {
                anshi::write_all(1, b"usage: tile [1-4]\n");
                return 1;
            }
        }
    }

    let Some(raw_mode) = anshi::RawMode::enable() else {
        anshi::write_all(1, b"tile: cannot put the terminal in raw mode\n");
        return 1;
    };
    let mut terminal = anshi_tui::init();
    let size = terminal.size().unwrap_or_else(|e| match e {});
    let (slots, dividers) = ui::layout(Rect::new(0, 0, size.width, size.height), count);

    let mut panes: Vec<Pane> = Vec::new();
    for slot in &slots {
        let others: Vec<c_int> = panes.iter().flat_map(|p| p.fds()).collect();
        match Pane::spawn(SHELL, slot.content.width, slot.content.height, &others) {
            Some(p) => panes.push(p),
            None => {
                anshi_tui::restore();
                anshi::write_all(1, b"tile: failed to start a pane\n");
                return 1;
            }
        }
    }

    let mut focused = 0;
    let mut prefix = false;
    let mut quitting = false;
    let mut dirty = true;
    while panes.iter().any(Pane::alive) {
        let mut busy = false;

        // Keys: route to the focused pane, or handle a Ctrl-b command.
        while let KeyPoll::Key(key) = anshi::poll_key() {
            busy = true;
            if quitting {
                continue;
            }
            if !prefix {
                if key == CTRL_B {
                    prefix = true;
                    dirty = true; // the header shows it
                } else {
                    panes[focused].send(key);
                }
                continue;
            }
            prefix = false;
            match key {
                b'1'..=b'9' if ((key - b'1') as usize) < panes.len() => focused = (key - b'1') as usize,
                b'o' => focused = (focused + 1) % panes.len(),
                b'q' => {
                    // Closing each pane's stdin makes its sh exit (and a
                    // scarf/play running in it quit first); pump() reaps.
                    quitting = true;
                    for p in &mut panes {
                        p.hang_up();
                    }
                }
                CTRL_B => panes[focused].send(CTRL_B),
                _ => {} // anything else after the prefix is dropped
            }
            dirty = true;
        }

        for p in &mut panes {
            if p.pump() {
                busy = true;
                dirty = true;
            }
        }

        if dirty {
            let _ = terminal.draw(|frame| ui::render(frame, &panes, &slots, &dividers, focused, prefix));
            dirty = false;
        }
        if !busy {
            anshi::sched_yield();
        }
    }

    anshi_tui::restore();
    drop(raw_mode);
    anshi::write_all(1, b"tile: done\n");
    0
}
