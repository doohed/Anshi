//! The builtins -- their table, the file commands, and dispatch to the
//! scripting (builtins.rs) and text (tools.rs) ones -- and finding
//! programs in the current directory or /bin.
//!
//! File commands go through the kernel's own filesystem operations
//! (anshi::unlink/rename/copy -- the same ones the kernel shell uses),
//! and anything that changes the filesystem is followed by a sync, so
//! it survives a reboot. The kernel shell's spellings (`create dir x`,
//! `delete file x`, ...) are accepted alongside the short ones.
//!
//! Output goes to fd 1 and errors to fd 2, so redirections and pipes
//! work on builtins as on programs.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use anshi::CString;

use crate::shell::{Flow, Res, Shell};
use crate::term::{self, Out, color};
use crate::{builtins, configure, tools};

#[derive(Clone, Copy, PartialEq)]
pub enum Group {
    Files,
    Text,
    Scripting,
    Shell,
}

pub struct Builtin {
    pub name: &'static str,
    pub usage: &'static str,
    pub desc: &'static str,
    pub group: Group,
}

const fn b(name: &'static str, usage: &'static str, desc: &'static str, group: Group) -> Builtin {
    Builtin { name, usage, desc, group }
}

pub const BUILTINS: &[Builtin] = &[
    b("cd", "cd [dir]", "change directory (no dir: $HOME)", Group::Files),
    b("pwd", "pwd", "print the current directory", Group::Files),
    b("ls", "ls [dir]", "list a directory", Group::Files),
    b("cat", "cat [file]...", "print files (no file: stdin)", Group::Files),
    b("echo", "echo [-n] [-e] [text]", "print text (-n: no newline, -e: \\n \\t ...)", Group::Files),
    b("write", "write <file> <text>", "replace a file's contents", Group::Files),
    b("mkdir", "mkdir <dir>...", "make directories", Group::Files),
    b("touch", "touch <file>...", "create empty files", Group::Files),
    b("rm", "rm <path>...", "remove files or whole directories", Group::Files),
    b("cp", "cp <src> <dest>", "copy a file or directory", Group::Files),
    b("mv", "mv <src> <dest>", "move or rename", Group::Files),
    b("create", "create dir|file <name>", "make a directory or empty file", Group::Files),
    b("delete", "delete [dir|file] <path>", "same as rm", Group::Files),
    b("copy", "copy [dir|file] <src> <dest>", "same as cp", Group::Files),
    b("move", "move [dir|file] <src> <dest>", "same as mv", Group::Files),
    b("sync", "sync", "write the filesystem to disk now", Group::Files),
    b("grep", "grep [-ivcnq] <text> [file]...", "lines containing text (fixed strings)", Group::Text),
    b("head", "head [-n N] [file]", "the first N lines (10)", Group::Text),
    b("tail", "tail [-n N] [file]", "the last N lines (10)", Group::Text),
    b("wc", "wc [-lwc] [file]...", "count lines, words, bytes", Group::Text),
    b("sort", "sort [-rnu] [file]", "sort lines", Group::Text),
    b("seq", "seq [first [step]] last", "print a sequence of numbers", Group::Text),
    b("printf", "printf <format> [arg]...", "formatted output (%s %d %x %c %%)", Group::Text),
    b("basename", "basename <path> [suffix]", "the last part of a path", Group::Text),
    b("dirname", "dirname <path>", "all but the last part", Group::Text),
    b("test", "test <expr>, [ <expr> ]", "check files, strings, numbers", Group::Scripting),
    b("[", "[ <expr> ]", "same as test", Group::Scripting),
    b("true", "true", "succeed", Group::Scripting),
    b("false", "false", "fail", Group::Scripting),
    b(":", ":", "do nothing, successfully", Group::Scripting),
    b("read", "read [-r] [-p prompt] <var>...", "read a line into variables", Group::Scripting),
    b("export", "export [name[=value]]...", "pass variables to programs", Group::Scripting),
    b("unset", "unset [-f] <name>...", "remove variables (or functions)", Group::Scripting),
    b("local", "local <name[=value]>...", "variables for this function only", Group::Scripting),
    b("set", "set [-ex] [+ex] [-- args]", "options, $1...; no args: list variables", Group::Scripting),
    b("shift", "shift [n]", "drop $1 (n of them)", Group::Scripting),
    b("source", "source <file> [args]", "run a script in this shell (also .)", Group::Scripting),
    b(".", ". <file> [args]", "same as source", Group::Scripting),
    b("eval", "eval <text>...", "run text as a command", Group::Scripting),
    b("exec", "exec <program> [args]", "replace the shell with a program", Group::Scripting),
    b("return", "return [n]", "leave a function", Group::Scripting),
    b("break", "break [n]", "leave a loop", Group::Scripting),
    b("continue", "continue [n]", "next round of a loop", Group::Scripting),
    b("type", "type <name>...", "say what a name runs", Group::Scripting),
    b("command", "command [-v] <name> [args]", "run (or -v: find) a builtin or program", Group::Scripting),
    b("env", "env", "list exported variables", Group::Scripting),
    b("sleep", "sleep <seconds>", "wait", Group::Scripting),
    b("exit", "exit [n]", "leave the shell", Group::Scripting),
    b("clear", "clear", "clear the screen", Group::Shell),
    b("history", "history", "list past commands", Group::Shell),
    b("configure", "configure", "choose how the prompt looks", Group::Shell),
    b("help", "help", "show this help", Group::Shell),
];

pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|b| b.name == name)
}

fn magic(path: &str) -> [u8; 4] {
    let mut magic = [0u8; 4];
    if let Some(mut f) = CString::new(path).ok().and_then(|p| anshi::File::open(&p)) {
        f.read_full(&mut magic);
    }
    magic
}

/// Whether `path` is an ELF binary.
pub fn is_elf(path: &str) -> bool {
    &magic(path) == b"\x7fELF"
}

/// Whether `path` can be run: an ELF binary, or a script starting `#!`.
pub fn is_program(path: &str) -> bool {
    !anshi::is_dir(path) && {
        let m = magic(path);
        &m == b"\x7fELF" || m.starts_with(b"#!")
    }
}

/// Where `name` would run from, if it's a program: a path as given when
/// it contains `/`, else the current directory, else /bin.
pub fn resolve_program(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') {
        return is_program(name).then(|| String::from(name));
    }
    [String::from(name), format!("/bin/{name}")].into_iter().find(|p| is_program(p))
}

/// A whole file as text.
pub fn read_text(path: &str) -> Option<String> {
    if anshi::is_dir(path) {
        return None;
    }
    let f = CString::new(path).ok().and_then(|p| anshi::File::open(&p))?;
    Some(String::from_utf8_lossy(&anshi::read_to_end(f.fd())).into_owned())
}

pub fn println(s: &str) {
    let mut out = Out::new();
    out.push(s).push("\n");
    out.flush();
}

/// Runs builtin `argv[0]`.
pub fn run(sh: &mut Shell, argv: &[String]) -> Res {
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let ok = |b: bool| Ok(if b { 0 } else { 1 });
    match argv[0].as_str() {
        "cd" => {
            let home = String::from(sh.var("HOME").unwrap_or("/"));
            let dir = args.first().copied().unwrap_or(&home);
            let done = cd(dir);
            if done {
                let pwd = anshi::getcwd().unwrap_or_else(|| String::from("/"));
                sh.set_var("PWD", &pwd);
            }
            ok(done)
        }
        "pwd" => {
            println(&anshi::getcwd().unwrap_or_else(|| String::from("/")));
            Ok(0)
        }
        "ls" => ok(ls(args.first().copied().unwrap_or("."))),
        "cat" => ok(cat(&args)),
        "echo" => {
            echo(&args);
            Ok(0)
        }
        "write" => ok(write_file(&args)),
        "mkdir" => ok(each(&args, "mkdir", anshi::mkdir, MKDIR_FAIL)),
        "touch" => ok(each(&args, "touch", touch, CREATE_FAIL)),
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
                Ok(0)
            } else {
                term::error("sync: no disk to write to");
                Ok(1)
            }
        }
        "clear" => {
            anshi::write_all(1, b"\x1b[0m\x1b[2J\x1b[H");
            Ok(0)
        }
        "history" => {
            let mut out = Out::new();
            for i in 0..sh.history.len() {
                let _ = write!(out.sgr(color::DIM), "{:>4}  ", i + 1);
                out.sgr(color::RESET).push(sh.history.get(i)).push("\n");
            }
            out.flush();
            Ok(0)
        }
        "help" => {
            help();
            Ok(0)
        }
        "configure" => {
            if let Some(c) = configure::run(&sh.config) {
                sh.config = c;
            }
            Ok(0)
        }
        "exit" => {
            let code = match args.first() {
                Some(n) => n.parse().unwrap_or(2),
                None => sh.status,
            };
            Err(Flow::Exit(code))
        }
        name if tools::is_tool(name) => Ok(tools::run(name, &args)),
        _ => builtins::run(sh, argv),
    }
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

/// `touch` leaves an existing file alone; create_file would refuse it.
fn touch(path: &str) -> bool {
    anshi::exists(path) || anshi::create_file(path)
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

/// `\n`, `\t`, `\\`, `\e` and friends, for echo -e and printf.
pub fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('e') => out.push('\x1b'),
            Some('a') => out.push('\x07'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn echo(args: &[&str]) {
    let mut newline = true;
    let mut escapes = false;
    let mut rest = args;
    while let Some((first, more)) = rest.split_first() {
        match *first {
            "-n" => newline = false,
            "-e" => escapes = true,
            "-ne" | "-en" => {
                newline = false;
                escapes = true;
            }
            _ => break,
        }
        rest = more;
    }
    let mut text = rest.join(" ");
    if escapes {
        text = unescape(&text);
    }
    if newline {
        text.push('\n');
    }
    anshi::write_all(1, text.as_bytes());
}

fn cat(files: &[&str]) -> bool {
    if files.is_empty() {
        return copy_fd(0);
    }
    let mut all = true;
    for name in files {
        if *name == "-" {
            all &= copy_fd(0);
            continue;
        }
        if anshi::is_dir(name) {
            term::error(&format!("cat: {name}: is a directory"));
            all = false;
            continue;
        }
        let Some(f) = CString::new(*name).ok().and_then(|p| anshi::File::open(&p)) else {
            term::error(&format!("cat: {name}: no such file"));
            all = false;
            continue;
        };
        all &= copy_fd(f.fd());
    }
    all
}

/// Copies `fd` to stdout until end of file.
fn copy_fd(fd: i32) -> bool {
    let mut buf = [0u8; 4096];
    while let Some(n) = anshi::read_wait(fd, &mut buf) {
        if !anshi::write_all(1, &buf[..n]) {
            return false;
        }
    }
    true
}

/// Entries in columns: directories bold blue with a `/`, programs (in
/// /bin) green, files plain. Names starting with `.` are hidden. Into a
/// pipe or file: one name per line, no colors.
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
    if !anshi::isatty(1) {
        let mut text = String::new();
        for e in &entries {
            text.push_str(&e.name);
            text.push('\n');
        }
        anshi::write_all(1, text.as_bytes());
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
    let groups =
        [(Group::Files, "Files"), (Group::Text, "Text"), (Group::Scripting, "Scripting"), (Group::Shell, "Shell")];
    let w = BUILTINS.iter().map(|b| b.usage.len()).max().unwrap_or(0) + 2;
    for (group, title) in groups {
        out.styled(color::OK, title).push("\n");
        for b in BUILTINS.iter().filter(|b| b.group == group) {
            out.push("  ").styled(color::COMMAND, b.name).push(&b.usage[b.name.len()..]);
            let _ = write!(out, "{:pad$}", "", pad = w - b.usage.len());
            out.styled(color::DIM, b.desc).push("\n");
        }
        out.push("\n");
    }
    out.styled(color::OK, "Anything else").push(" runs a program from this directory or /bin (try ");
    out.styled(color::COMMAND, "ls /bin").push(").\n");
    out.push("Pipes ").styled(color::COMMAND, "a | b").push(", lists ").styled(color::COMMAND, "a && b; c");
    out.push(", redirections ").styled(color::COMMAND, "> >> < 2> 2>&1 <<EOF");
    out.push(", variables, ").styled(color::COMMAND, "if for while case").push(" and functions -- see docs/sh.md.\n\n");
    out.styled(color::OK, "Keys").push("\n");
    let keys: &[(&str, &str)] = &[
        ("Tab", "complete a command or path; again to pick from the list"),
        ("Right / End / Ctrl-F", "accept the grey suggestion"),
        ("Up / Down", "history, filtered by what's typed so far"),
        ("Left / Home / Ctrl-A, Ctrl-E", "move within the line"),
        ("Ctrl-W / Ctrl-U / Ctrl-K", "delete word / to start / to end"),
        ("Ctrl-C", "abandon the line; stop a long loop"),
        ("Ctrl-L", "clear the screen"),
    ];
    for (k, d) in keys {
        out.push("  ").styled(color::PATH, k);
        let _ = write!(out, "{:pad$}", "", pad = 30usize.saturating_sub(k.len()));
        out.styled(color::DIM, d).push("\n");
    }
    out.flush();
}
