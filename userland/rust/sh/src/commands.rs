//! Running a command line: builtins, and programs found in the current
//! directory or /bin.
//!
//! File commands go through the kernel's own filesystem operations
//! (anshi::unlink/rename/copy -- the same ones the kernel shell uses),
//! and anything that changes the filesystem is followed by a sync, so
//! it survives a reboot. The kernel shell's spellings (`create dir x`,
//! `delete file x`, ...) are accepted alongside the short ones.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::CStr;
use core::fmt::Write;

use anshi::CString;

use crate::config::Config;
use crate::configure;
use crate::history::History;
use crate::term::{self, Out, color};

pub struct Builtin {
    pub name: &'static str,
    pub usage: &'static str,
    pub desc: &'static str,
}

pub const BUILTINS: &[Builtin] = &[
    Builtin { name: "cd", usage: "cd [dir]", desc: "change directory (no dir: /)" },
    Builtin { name: "pwd", usage: "pwd", desc: "print the current directory" },
    Builtin { name: "ls", usage: "ls [dir]", desc: "list a directory" },
    Builtin { name: "cat", usage: "cat <file>...", desc: "print files" },
    Builtin { name: "echo", usage: "echo [text]", desc: "print text" },
    Builtin { name: "write", usage: "write <file> <text>", desc: "replace a file's contents" },
    Builtin { name: "mkdir", usage: "mkdir <dir>...", desc: "make directories" },
    Builtin { name: "touch", usage: "touch <file>...", desc: "create empty files" },
    Builtin { name: "rm", usage: "rm <path>...", desc: "remove files or whole directories" },
    Builtin { name: "cp", usage: "cp <src> <dest>", desc: "copy a file or directory" },
    Builtin { name: "mv", usage: "mv <src> <dest>", desc: "move or rename" },
    Builtin { name: "create", usage: "create dir|file <name>", desc: "make a directory or empty file" },
    Builtin { name: "delete", usage: "delete [dir|file] <path>", desc: "same as rm" },
    Builtin { name: "copy", usage: "copy [dir|file] <src> <dest>", desc: "same as cp" },
    Builtin { name: "move", usage: "move [dir|file] <src> <dest>", desc: "same as mv" },
    Builtin { name: "sync", usage: "sync", desc: "write the filesystem to disk now" },
    Builtin { name: "clear", usage: "clear", desc: "clear the screen" },
    Builtin { name: "history", usage: "history", desc: "list past commands" },
    Builtin { name: "configure", usage: "configure", desc: "choose how the prompt looks" },
    Builtin { name: "help", usage: "help", desc: "show this help" },
    Builtin { name: "exit", usage: "exit", desc: "leave the shell" },
];

pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|b| b.name == name)
}

/// Whether `path` is a program: a file starting with the ELF magic.
pub fn is_program(path: &str) -> bool {
    let Some(mut f) = CString::new(path).ok().and_then(|p| anshi::File::open(&p)) else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_full(&mut magic) == 4 && &magic == b"\x7fELF"
}

/// Where `name` would run from, if it's a program: a path as given when
/// it contains `/`, else the current directory, else /bin.
pub fn resolve_program(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let candidates: Vec<String> =
        if name.contains('/') { alloc::vec![String::from(name)] } else { alloc::vec![String::from(name), format!("/bin/{name}")] };
    candidates.into_iter().find(|p| is_program(p))
}

/// Splits a command line into words: whitespace-separated, with `"..."`
/// or `'...'` quoting for words containing spaces.
pub fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, ' ' | '\t') => {
                if in_word {
                    words.push(core::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

pub enum Outcome {
    /// The command's exit status (0 = success).
    Status(i32),
    Exit,
}

pub fn run(line: &str, history: &History, config: &mut Config) -> Outcome {
    let words = split_words(line);
    let Some(cmd) = words.first() else {
        return Outcome::Status(0);
    };
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    let ok = |b: bool| Outcome::Status(if b { 0 } else { 1 });

    match cmd.as_str() {
        "exit" => Outcome::Exit,
        "cd" => ok(cd(args.first().copied().unwrap_or("/"))),
        "pwd" => {
            println(&anshi::getcwd().unwrap_or_else(|| String::from("/")));
            Outcome::Status(0)
        }
        "ls" => ok(ls(args.first().copied().unwrap_or("."))),
        "cat" => ok(cat(&args)),
        "echo" => {
            println(&args.join(" "));
            Outcome::Status(0)
        }
        "write" => ok(write_file(&args)),
        "mkdir" => ok(each(&args, "mkdir", anshi::mkdir, MKDIR_FAIL)),
        "touch" => ok(each(&args, "touch", anshi::create_file, CREATE_FAIL)),
        "rm" => ok(each(&args, "rm", anshi::unlink, RM_FAIL)),
        "delete" => ok(each(strip_kind(&args), "delete", anshi::unlink, RM_FAIL)),
        "cp" => ok(two(&args, "cp", anshi::copy)),
        "copy" => ok(two(strip_kind(&args), "copy", anshi::copy)),
        "mv" => ok(two(&args, "mv", anshi::rename)),
        "move" => ok(two(strip_kind(&args), "move", anshi::rename)),
        "create" => ok(create(&args)),
        "sync" => {
            if anshi::sync() {
                println("filesystem written to disk");
                Outcome::Status(0)
            } else {
                term::error("sync: no disk to write to");
                Outcome::Status(1)
            }
        }
        "clear" => {
            anshi::write_all(1, b"\x1b[0m\x1b[2J\x1b[H");
            Outcome::Status(0)
        }
        "history" => {
            let mut out = Out::new();
            for i in 0..history.len() {
                let _ = write!(out.sgr(color::DIM), "{:>4}  ", i + 1);
                out.sgr(color::RESET).push(history.get(i)).push("\n");
            }
            out.flush();
            Outcome::Status(0)
        }
        "help" => {
            help();
            Outcome::Status(0)
        }
        "configure" => {
            if let Some(c) = configure::run(config) {
                *config = c;
            }
            Outcome::Status(0)
        }
        _ => run_program(cmd, &words),
    }
}

fn println(s: &str) {
    let mut out = Out::new();
    out.push(s).push("\n");
    out.flush();
}

/// After anything that changes the filesystem: make it persistent,
/// quietly (a boot without a disk simply has nothing to sync to).
fn persist() {
    anshi::sync();
}

/// The kernel shell's `delete dir x` / `copy file a b` spelling: the
/// kind word is optional here, since the filesystem knows which it is.
fn strip_kind<'a>(args: &'a [&'a str]) -> &'a [&'a str] {
    match args.first() {
        Some(&"dir") | Some(&"file") => &args[1..],
        _ => args,
    }
}

fn cd(dir: &str) -> bool {
    let ok = CString::new(dir).is_ok_and(|p| anshi::chdir(&p));
    if !ok {
        term::error(&format!("cd: {dir}: no such directory"));
    }
    ok
}

const MKDIR_FAIL: &str = "can't create (it exists, or its parent doesn't)";
const CREATE_FAIL: &str = "can't create (no such directory to put it in)";
const RM_FAIL: &str = "can't remove (it doesn't exist, or a shell is inside it)";

/// `op` on each argument, reporting failures with `why`; syncs after.
fn each(args: &[&str], name: &str, op: fn(&str) -> bool, why: &str) -> bool {
    if args.is_empty() {
        term::error(&format!("{name}: missing operand (see `help`)"));
        return false;
    }
    let mut all = true;
    for a in args {
        if !op(a) {
            term::error(&format!("{name}: {a}: {why}"));
            all = false;
        }
    }
    persist();
    all
}

fn two(args: &[&str], name: &str, op: fn(&str, &str) -> bool) -> bool {
    let [src, dest] = args else {
        term::error(&format!("{name}: needs <src> <dest>"));
        return false;
    };
    let ok = op(src, dest);
    if ok {
        persist();
    } else {
        term::error(&format!("{name}: {src} -> {dest}: failed (no such source, or the destination exists)"));
    }
    ok
}

fn create(args: &[&str]) -> bool {
    match args {
        ["dir", rest @ ..] => each(rest, "create", anshi::mkdir, MKDIR_FAIL),
        ["file", rest @ ..] => each(rest, "create", anshi::create_file, CREATE_FAIL),
        _ => {
            term::error("usage: create dir|file <name>");
            false
        }
    }
}

fn write_file(args: &[&str]) -> bool {
    let Some((file, text)) = args.split_first() else {
        term::error("usage: write <file> <text>");
        return false;
    };
    let mut content = text.join(" ");
    content.push('\n');
    let f = CString::new(*file).ok().and_then(|p| anshi::File::create(&p));
    let ok = f.is_some_and(|mut f| f.write_all(content.as_bytes()));
    if ok {
        persist();
    } else {
        term::error(&format!("write: {file}: cannot write"));
    }
    ok
}

fn cat(files: &[&str]) -> bool {
    if files.is_empty() {
        term::error("usage: cat <file>");
        return false;
    }
    let mut all = true;
    for name in files {
        if anshi::is_dir(name) {
            term::error(&format!("cat: {name}: is a directory"));
            all = false;
            continue;
        }
        let Some(mut f) = CString::new(*name).ok().and_then(|p| anshi::File::open(&p)) else {
            term::error(&format!("cat: {name}: no such file"));
            all = false;
            continue;
        };
        let mut buf = [0u8; 512];
        loop {
            let n = f.read(&mut buf);
            if n == 0 {
                break;
            }
            anshi::write_all(1, &buf[..n]);
        }
    }
    all
}

/// Entries in columns: directories bold blue with a `/`, programs (in
/// /bin) green, files plain. Names starting with `.` are hidden.
fn ls(dir: &str) -> bool {
    let Some(mut entries) = CString::new(dir).ok().and_then(|p| anshi::read_dir(&p)) else {
        term::error(&format!("ls: {dir}: no such directory"));
        return false;
    };
    entries.retain(|e| !e.name.starts_with('.')); // dotfiles (.sh_history) stay hidden
    entries.sort_by(|a, b| (!a.is_dir, &a.name).cmp(&(!b.is_dir, &b.name)));
    if entries.is_empty() {
        return true;
    }
    let in_bin = anshi::getcwd().is_some_and(|c| (c == "/bin" && dir == ".") || dir == "/bin" || dir == "/bin/");
    let shown: Vec<(String, &str)> = entries
        .iter()
        .map(|e| {
            if e.is_dir {
                (format!("{}/", e.name), color::DIR)
            } else if in_bin {
                (e.name.clone(), color::PROGRAM)
            } else {
                (e.name.clone(), color::RESET)
            }
        })
        .collect();
    let col_w = shown.iter().map(|(n, _)| n.len()).max().unwrap_or(0) + 2;
    let cols = ((term::width().saturating_sub(1)) / col_w).max(1);
    let mut out = Out::new();
    for (i, (name, c)) in shown.iter().enumerate() {
        out.styled(c, name);
        let last_in_row = (i + 1) % cols == 0 || i + 1 == shown.len();
        if last_in_row {
            out.push("\n");
        } else {
            let _ = write!(out, "{:w$}", "", w = col_w - name.len());
        }
    }
    out.flush();
    true
}

fn help() {
    let mut out = Out::new();
    out.styled(color::OK, "Commands").push("\n");
    let w = BUILTINS.iter().map(|b| b.usage.len()).max().unwrap_or(0) + 2;
    for b in BUILTINS {
        out.push("  ").styled(color::COMMAND, b.name).push(&b.usage[b.name.len()..]);
        let _ = write!(out, "{:pad$}", "", pad = w - b.usage.len());
        out.styled(color::DIM, b.desc).push("\n");
    }
    out.push("\n");
    out.styled(color::OK, "Anything else").push(" runs a program from this directory or /bin (try ");
    out.styled(color::COMMAND, "ls /bin").push(").\n\n");
    out.styled(color::OK, "Keys").push("\n");
    let keys: &[(&str, &str)] = &[
        ("Tab", "complete a command or path; again to pick from the list"),
        ("Right / End / Ctrl-F", "accept the grey suggestion"),
        ("Up / Down", "history, filtered by what's typed so far"),
        ("Left / Home / Ctrl-A, Ctrl-E", "move within the line"),
        ("Ctrl-W / Ctrl-U / Ctrl-K", "delete word / to start / to end"),
        ("Ctrl-C", "abandon the line"),
        ("Ctrl-L", "clear the screen"),
    ];
    for (k, d) in keys {
        out.push("  ").styled(color::PATH, k);
        let _ = write!(out, "{:pad$}", "", pad = 30usize.saturating_sub(k.len()));
        out.styled(color::DIM, d).push("\n");
    }
    out.flush();
}

/// fork + exec + wait. The program gets the terminal in normal (cooked)
/// mode -- the line editor only switches to raw mode while it's reading
/// a line -- and plain attributes before and after, whatever it leaves.
fn run_program(name: &str, words: &[String]) -> Outcome {
    let Some(path) = resolve_program(name) else {
        term::error(&format!("{name}: command not found"));
        return Outcome::Status(127);
    };
    let Ok(cpath) = CString::new(path.as_str()) else {
        return Outcome::Status(127);
    };
    let cargs: Vec<CString> = words.iter().filter_map(|w| CString::new(w.as_str()).ok()).collect();
    let argv: Vec<&CStr> = cargs.iter().map(|c| c.as_c_str()).collect();

    match anshi::fork() {
        None => {
            term::error("fork failed");
            Outcome::Status(1)
        }
        Some(0) => {
            anshi::exec(&cpath, &argv);
            term::error(&format!("{name}: cannot run"));
            anshi::exit(126)
        }
        Some(pid) => {
            let status = anshi::waitpid(pid);
            anshi::write_all(1, b"\x1b[0m");
            Outcome::Status(status)
        }
    }
}
