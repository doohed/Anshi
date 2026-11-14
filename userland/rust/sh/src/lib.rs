//! `sh` -- the Anshi shell. The kernel boots straight into it, and tile
//! runs one in every pane.
//!
//! - a POSIX-shaped language (syntax.rs, expand.rs, shell.rs): pipes,
//!   redirections, `&&`/`||`, variables, `$(...)`, `$((...))`, globbing,
//!   `if`/`for`/`while`/`case`, functions, scripts;
//! - builtins for files, text and scripting (commands.rs, builtins.rs,
//!   tools.rs);
//! - interactively: a two-line prompt with the last command's status and
//!   duration (prompt.rs), the line colored as it's typed (highlight.rs),
//!   grey suggestions from history, Tab completion and history search
//!   (editor.rs, complete.rs, history.rs), and `> ` continuation lines
//!   for an unfinished `if`, quote or pipe.
//!
//! `sh` runs interactively on a terminal; `sh -c 'commands' [args]`,
//! `sh script [args]` and `... | sh` run text instead. `-e` and `-x`
//! may come first, as with `set`.
//!
//! A `no_std` static library like every Rust program here: see
//! ../../Cargo.toml for how it's linked into an executable.

#![no_std]

extern crate alloc;

mod builtins;
mod commands;
mod complete;
mod config;
mod configure;
mod editor;
mod expand;
mod highlight;
mod history;
mod prompt;
mod shell;
mod syntax;
mod term;
mod tools;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{CStr, c_char, c_int};

use config::Config;
use editor::ReadResult;
use history::History;
use prompt::State;
use shell::{Done, Shell};
use syntax::ParseError;
use term::{Out, color};

fn args(argc: c_int, argv: *const *const c_char) -> Vec<String> {
    (0..argc.max(0) as usize)
        .map(|i| unsafe { CStr::from_ptr(*argv.add(i)) }.to_string_lossy().into_owned())
        .collect()
}

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const c_char) -> c_int {
    let argv = args(argc, argv);
    let mut rest = &argv[argv.len().min(1)..];
    let (mut errexit, mut xtrace, mut command) = (false, false, None);
    while let Some(first) = rest.first() {
        match first.as_str() {
            "-c" => {
                let Some(text) = rest.get(1) else {
                    term::error("-c: needs the commands to run");
                    return 2;
                };
                command = Some(text.clone());
                rest = &rest[2..];
                break;
            }
            "-e" => errexit = true,
            "-x" => xtrace = true,
            "-ex" | "-xe" => (errexit, xtrace) = (true, true),
            "--" => {
                rest = &rest[1..];
                break;
            }
            _ => break,
        }
        rest = &rest[1..];
    }

    let script = command.is_none().then(|| rest.first().cloned()).flatten();
    let interactive = command.is_none() && script.is_none() && anshi::isatty(0);
    let mut sh = Shell::new(argv.first().map_or("sh", String::as_str), interactive);
    sh.errexit = errexit;
    sh.xtrace = xtrace;

    if let Some(text) = command {
        // sh -c 'text' name args: $0 is name, $1... the rest.
        if let Some((name, params)) = rest.split_first() {
            sh.arg0 = name.clone();
            sh.params = params.to_vec();
        }
        finish(sh.run_text(&text, "-c"))
    } else if let Some(path) = script {
        let Some(text) = commands::read_text(&path) else {
            term::error(&format!("{path}: no such file"));
            return 127;
        };
        sh.arg0 = path.clone();
        sh.params = rest[1..].to_vec();
        finish(sh.run_text(&text, &path))
    } else if !interactive {
        let text = String::from_utf8_lossy(&anshi::read_to_end(0)).into_owned();
        finish(sh.run_text(&text, "stdin"))
    } else {
        interact(&mut sh)
    }
}

fn finish(done: Done) -> c_int {
    match done {
        Done::Status(s) | Done::Exit(s) => s,
    }
}

/// Joins a continuation line onto what came before: with a newline,
/// which the parser reads like `;` -- the history keeps it on one line,
/// so there it becomes `; ` or, after `then`, `|` and the like, a space.
fn history_join(acc: &mut String, line: &str) {
    let prev = acc.trim_end();
    let glue = if prev.is_empty()
        || prev.ends_with(['|', '&', ';', '(', '{', '\\'])
        || ["then", "do", "else", "in"].iter().any(|k| prev.ends_with(k))
    {
        " "
    } else {
        "; "
    };
    acc.push_str(glue);
    acc.push_str(line.trim());
}

fn interact(sh: &mut Shell) -> c_int {
    sh.history = History::load();
    let (config, configured) = Config::load();
    sh.config = config;
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
        let p = prompt::render(&sh.config, &state, term::width(), fresh);
        fresh = false;
        anshi::write_all(1, p.header.as_bytes());
        let mut text = match editor::read_line(&sh.history, &p.prefix, p.prefix_width) {
            ReadResult::Eof => break,
            ReadResult::Cancelled => continue,
            ReadResult::Clear => {
                anshi::write_all(1, b"\x1b[0m\x1b[2J\x1b[H");
                fresh = true;
                continue;
            }
            ReadResult::Line(line) => line,
        };
        if text.trim().is_empty() {
            continue;
        }
        let mut flat = String::from(text.trim());

        // An unfinished `if`, quote or pipe: more lines, after "> ".
        let mut cont = Out::new();
        cont.styled(color::DIM, "> ");
        let node = loop {
            match syntax::parse(&text) {
                Ok(node) => break Some(node),
                Err(ParseError::Syntax(msg, _)) => {
                    term::error(&msg);
                    state.status = 2;
                    break None;
                }
                Err(ParseError::Incomplete) => match editor::read_line(&sh.history, &cont.0, 2) {
                    ReadResult::Line(more) => {
                        text.push('\n');
                        text.push_str(&more);
                        history_join(&mut flat, &more);
                    }
                    ReadResult::Eof => return sh.status,
                    _ => break None,
                },
            }
        };
        sh.history.add(&flat);
        let Some(node) = node else { continue };

        let started = anshi::now_ms();
        let done = sh.run_top(&node);
        anshi::write_all(1, b"\x1b[0m");
        highlight::set_functions(sh.funcs.keys().cloned().collect());
        match done {
            Done::Exit(s) => return s,
            Done::Status(s) => {
                state.status = s;
                state.duration_ms = anshi::now_ms().saturating_sub(started);
            }
        }
        let word = flat.as_str();
        if word == "clear" || word == "configure" {
            fresh = true;
            state.duration_ms = 0;
        }
    }
    anshi::write_all(1, b"\x1b[0m");
    sh.status
}
