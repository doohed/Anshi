//! `configure`: a step-by-step wizard for how the prompt looks.
//!
//! Each step asks one thing and shows every answer as a live preview:
//! the real prompt renderer (prompt.rs) drawing a short sample session
//! with that answer applied on top of the ones already chosen. A digit
//! picks; `r` starts over; `q` leaves without changing anything. Steps
//! that don't apply are skipped (the frame and connecting line only
//! exist on two lines; segment ends only with backgrounds). The result
//! is shown once more, and saved to `/.sh_prompt` on `y`.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::config::{Config, Ends, Fill, Style};
use crate::prompt::{self, State};
use crate::term::{self, Key, Out, color};

#[derive(Clone, Copy)]
enum Step {
    Style,
    Lines,
    Frame,
    Fill,
    Ends,
    Glyphs,
    System,
    Spacing,
}

const STEPS: [Step; 8] = [Step::Style, Step::Lines, Step::Frame, Step::Fill, Step::Ends, Step::Glyphs, Step::System, Step::Spacing];

/// The question for `step` and its answers (each the draft with that
/// answer applied), or None when the step doesn't apply to `draft`.
fn question(step: Step, d: Config) -> Option<(&'static str, Vec<(&'static str, Config)>)> {
    let with = |f: &dyn Fn(&mut Config)| {
        let mut c = d;
        f(&mut c);
        c
    };
    Some(match step {
        Step::Style => (
            "Prompt style",
            alloc::vec![
                ("Lean -- colored text", with(&|c| c.style = Style::Lean)),
                ("Classic -- one grey band", with(&|c| c.style = Style::Classic)),
                ("Rainbow -- a color per segment", with(&|c| c.style = Style::Rainbow)),
            ],
        ),
        Step::Lines => (
            "Prompt lines",
            alloc::vec![
                ("One line", with(&|c| c.two_lines = false)),
                ("Two lines -- the command gets a line of its own", with(&|c| c.two_lines = true)),
            ],
        ),
        Step::Frame if d.two_lines => (
            "Prompt frame",
            alloc::vec![("No frame", with(&|c| c.frame = false)), ("Frame on the left", with(&|c| c.frame = true))],
        ),
        Step::Fill if d.two_lines => (
            "Prompt connection",
            alloc::vec![
                ("Disconnected", with(&|c| c.fill = Fill::Blank)),
                ("Dotted", with(&|c| c.fill = Fill::Dotted)),
                ("Solid", with(&|c| c.fill = Fill::Solid)),
            ],
        ),
        Step::Ends if d.style != Style::Lean => (
            "Segment ends",
            alloc::vec![
                ("Sharp", with(&|c| c.ends = Ends::Sharp)),
                ("Round", with(&|c| c.ends = Ends::Round)),
                ("Flat", with(&|c| c.ends = Ends::Flat)),
            ],
        ),
        Step::Glyphs => (
            "Symbols",
            alloc::vec![
                ("Glyphs", with(&|c| c.glyphs = true)),
                ("Plain text", with(&|c| c.glyphs = false)),
            ],
        ),
        Step::System => (
            "Show the system name?",
            alloc::vec![("Yes", with(&|c| c.show_system = true)), ("No", with(&|c| c.show_system = false))],
        ),
        Step::Spacing => (
            "Prompt spacing",
            alloc::vec![
                ("Compact", with(&|c| c.sparse = false)),
                ("Sparse -- a blank line before each prompt", with(&|c| c.sparse = true)),
            ],
        ),
        _ => return None,
    })
}

/// Runs the wizard on a copy of `current`. Returns the new config if it
/// was saved.
pub fn run(current: &Config) -> Option<Config> {
    let _raw = anshi::RawMode::enable();
    let result = wizard(*current);
    let mut out = Out::new();
    out.push("\x1b[0m\x1b[2J\x1b[H");
    out.flush();
    let cfg = result?;
    if !cfg.save() {
        term::error("configure: couldn't write /.sh_prompt");
    }
    Some(cfg)
}

fn wizard(current: Config) -> Option<Config> {
    'restart: loop {
        let mut draft = current;
        let mut i = 0;
        while i < STEPS.len() {
            let Some((title, answers)) = question(STEPS[i], draft) else {
                i += 1;
                continue;
            };
            let applicable = STEPS.iter().filter(|s| question(**s, draft).is_some()).count();
            let number = STEPS[..i].iter().filter(|s| question(**s, draft).is_some()).count() + 1;
            screen(title, number, applicable, &answers);
            loop {
                match term::read_key() {
                    Key::Char(d @ b'1'..=b'9') if ((d - b'1') as usize) < answers.len() => {
                        draft = answers[(d - b'1') as usize].1;
                        break;
                    }
                    Key::Char(b'r') => continue 'restart,
                    Key::Char(b'q') | Key::Esc | Key::Ctrl(b'c') | Key::Eof => return None,
                    _ => {}
                }
            }
            i += 1;
        }

        // The result, and whether to keep it.
        let mut out = Out::new();
        out.push("\x1b[0m\x1b[2J\x1b[H");
        heading(&mut out, "Your prompt", None);
        preview(&mut out, &draft);
        out.push("\n");
        out.styled(color::OK, "(y)").push(" Save and use it     ");
        out.styled(color::OK, "(r)").push(" Start over     ");
        out.styled(color::OK, "(n)").push(" Quit without saving\n");
        out.flush();
        loop {
            match term::read_key() {
                Key::Char(b'y') | Key::Enter => return Some(draft),
                Key::Char(b'r') => continue 'restart,
                Key::Char(b'n') | Key::Char(b'q') | Key::Esc | Key::Ctrl(b'c') | Key::Eof => return None,
                _ => {}
            }
        }
    }
}

fn heading(out: &mut Out, title: &str, step: Option<(usize, usize)>) {
    out.styled("1", "Prompt configuration");
    if let Some((n, of)) = step {
        let _ = write!(out.sgr(color::DIM), "   step {n} of {of}");
        out.sgr(color::RESET);
    }
    out.push("\n\n").styled("1;97", title).push("\n\n");
}

fn screen(title: &str, n: usize, of: usize, answers: &[(&str, Config)]) {
    let mut out = Out::new();
    out.push("\x1b[0m\x1b[2J\x1b[H");
    heading(&mut out, title, Some((n, of)));
    for (i, (label, cfg)) in answers.iter().enumerate() {
        let _ = write!(out.sgr(color::OK), "({})", i + 1);
        out.sgr(color::RESET).push(" ").styled("1", label).push("\n");
        preview(&mut out, cfg);
        out.push("\n");
    }
    out.styled(color::DIM, "(r) Start over     (q) Quit without saving").push("\n");
    out.flush();
}

/// A two-prompt sample session in `cfg`, indented: a command that took
/// 2.3 seconds, its output, and the next prompt -- enough to show the
/// duration and status segments, and the spacing between prompts.
fn preview(out: &mut Out, cfg: &Config) {
    const INDENT: &str = "    ";
    let width = term::width().saturating_sub(INDENT.len()).min(90);
    let cwd = String::from("/code/project");
    let first = prompt::render(cfg, &State { cwd: cwd.clone(), status: 0, duration_ms: 0 }, width, true);
    let second = prompt::render(cfg, &State { cwd, status: 0, duration_ms: 2300 }, width, false);

    let indent_lines = |out: &mut Out, text: &str| {
        for line in text.split_inclusive('\n') {
            out.push(INDENT).push(line);
        }
    };
    indent_lines(out, &first.header);
    out.push(INDENT).push(&first.prefix).styled(color::COMMAND, "play").styled(color::PATH, " song.mp3").push("\n");
    out.push(INDENT).push("play: done\n");
    indent_lines(out, &second.header);
    out.push(INDENT).push(&second.prefix).push("\n");
}
