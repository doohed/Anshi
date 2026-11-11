//! `sh` -- the Anshi shell. The kernel boots straight into it, and tile
//! runs one in every pane.
//!
//! - a two-line prompt: the working directory, and the last command's
//!   status and (when slow) duration (prompt.rs);
//! - the command line colored as it's typed, so a mistyped command shows
//!   red before Enter (highlight.rs);
//! - grey suggestions from history, Tab completion of commands and paths
//!   with a selectable list, and history search on Up/Down (editor.rs,
//!   complete.rs, history.rs);
//! - the file commands -- ls, cat, mkdir, rm, cp, mv, ... -- built in,
//!   and any other word runs a program from the current directory or
//!   /bin (commands.rs).
//!
//! A `no_std` static library like every Rust program here: see
//! ../../Cargo.toml for how it's linked into an executable.

#![no_std]

extern crate alloc;

mod commands;
mod complete;
mod config;
mod configure;
mod editor;
mod highlight;
mod history;
mod prompt;
mod term;

use alloc::string::String;
use core::ffi::{c_char, c_int};

use commands::Outcome;
use config::Config;
use editor::ReadResult;
use history::History;
use prompt::State;
use term::{Out, color};

#[unsafe(no_mangle)]
pub extern "C" fn main(_argc: c_int, _argv: *const *const c_char) -> c_int {
    let mut history = History::load();
    let (mut config, configured) = Config::load();
    let mut state = State { cwd: String::new(), status: 0, duration_ms: 0 };
    let mut fresh = true;

    // The welcome line, cut down to fit a narrow terminal (a tile pane).
    let hint = if configured { " lists commands, Tab completes" } else { " lists commands, configure styles this prompt" };
    let hint = if term::width() > 16 + 4 + hint.len() { hint } else { " lists commands" };
    let mut out = Out::new();
    out.sgr(color::RESET).styled(color::DIM, "Anshi shell -- ").styled(color::COMMAND, "help").styled(color::DIM, hint).push("\n");
    out.flush();

    loop {
        state.cwd = anshi::getcwd().unwrap_or_else(|| String::from("/"));
        let p = prompt::render(&config, &state, term::width(), fresh);
        fresh = false;
        anshi::write_all(1, p.header.as_bytes());
        match editor::read_line(&history, &p.prefix, p.prefix_width) {
            ReadResult::Eof => break,
            ReadResult::Cancelled => {}
            ReadResult::Clear => {
                anshi::write_all(1, b"\x1b[0m\x1b[2J\x1b[H");
                fresh = true;
            }
            ReadResult::Line(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                history.add(&line);
                let started = anshi::now_ms();
                match commands::run(&line, &history, &mut config) {
                    Outcome::Exit => break,
                    Outcome::Status(status) => {
                        state.status = status;
                        state.duration_ms = anshi::now_ms().saturating_sub(started);
                    }
                }
                let word = line.trim();
                if word == "clear" || word == "configure" {
                    fresh = true;
                    state.duration_ms = 0;
                }
            }
        }
    }
    anshi::write_all(1, b"\x1b[0m");
    0
}
