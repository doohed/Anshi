//! The scripting builtins: test/[, read, export, set, shift, local,
//! source, eval, exec, the loop and function exits, type, command, env,
//! sleep.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::commands::{self, is_builtin, resolve_program};
use crate::shell::{Flow, Res, Shell};
use crate::syntax::is_name;
use crate::term;

pub fn run(sh: &mut Shell, argv: &[String]) -> Res {
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let count = |default: usize| -> Result<usize, i32> {
        match args.first() {
            None => Ok(default),
            Some(n) => n.parse().ok().filter(|&n| n > 0).ok_or(1),
        }
    };
    match argv[0].as_str() {
        "true" | ":" => Ok(0),
        "false" => Ok(1),
        "test" => Ok(test(&args)),
        "[" => {
            if args.last() != Some(&"]") {
                term::error("[: missing ]");
                return Ok(2);
            }
            Ok(test(&args[..args.len() - 1]))
        }
        "read" => Ok(read(sh, &args)),
        "export" => Ok(export(sh, &args)),
        "unset" => {
            let funcs = args.first() == Some(&"-f");
            for name in args.iter().filter(|a| !a.starts_with('-')) {
                if funcs {
                    sh.funcs.remove(*name);
                } else {
                    sh.unset(name);
                }
            }
            Ok(0)
        }
        "local" => {
            for a in &args {
                let (name, value) = a.split_once('=').unwrap_or((a, ""));
                if !is_name(name) {
                    term::error(&format!("local: {name}: not a valid name"));
                    return Ok(1);
                }
                if !sh.make_local(name) {
                    term::error("local: only inside a function");
                    return Ok(1);
                }
                sh.set_var(name, value);
            }
            Ok(0)
        }
        "set" => Ok(set(sh, &args)),
        "shift" => {
            let Ok(n) = count(1) else { return Ok(1) };
            if n > sh.params.len() {
                return Ok(1);
            }
            sh.params.drain(..n);
            Ok(0)
        }
        "source" | "." => {
            let Some((path, rest)) = args.split_first() else {
                term::error(&format!("{}: needs a file", argv[0]));
                return Ok(2);
            };
            let params = (!rest.is_empty()).then(|| rest.iter().map(|s| String::from(*s)).collect());
            sh.source(path, params)
        }
        "eval" => sh.eval(&args.join(" ")),
        "exec" => {
            let Some(name) = argv.get(1) else { return Ok(0) };
            let Some(path) = resolve_program(name) else {
                term::error(&format!("exec: {name}: not found"));
                return Ok(127);
            };
            sh.exec(&path, &argv[1..], &[]);
            Ok(126)
        }
        "return" => {
            let n = args.first().and_then(|n| n.parse().ok()).unwrap_or(sh.status);
            Err(Flow::Return(n))
        }
        "break" | "continue" => {
            if sh.loop_depth == 0 {
                term::error(&format!("{}: only meaningful in a loop", argv[0]));
                return Ok(1);
            }
            let Ok(n) = count(1) else {
                term::error(&format!("{}: bad count", argv[0]));
                return Ok(1);
            };
            let n = n.min(sh.loop_depth);
            Err(if argv[0] == "break" { Flow::Break(n) } else { Flow::Continue(n) })
        }
        "type" => {
            let mut status = 0;
            for name in &args {
                match describe(sh, name) {
                    Some(what) => commands::println(&format!("{name} is {what}")),
                    None => {
                        term::error(&format!("type: {name}: not found"));
                        status = 1;
                    }
                }
            }
            Ok(status)
        }
        "command" => command(sh, argv),
        "env" => {
            let mut text = String::new();
            for (name, v) in sh.vars.iter().filter(|(_, v)| v.exported) {
                text.push_str(&format!("{name}={}\n", v.value));
            }
            anshi::write_all(1, text.as_bytes());
            Ok(0)
        }
        "sleep" => Ok(sleep(&args)),
        other => {
            term::error(&format!("{other}: not a builtin"));
            Ok(127)
        }
    }
}

fn describe(sh: &Shell, name: &str) -> Option<String> {
    const KEYWORDS: [&str; 16] = [
        "if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "in", "case", "esac", "{", "}", "!",
    ];
    if KEYWORDS.contains(&name) {
        Some(String::from("a shell keyword"))
    } else if sh.funcs.contains_key(name) {
        Some(String::from("a function"))
    } else if is_builtin(name) {
        Some(String::from("a shell builtin"))
    } else {
        resolve_program(name)
    }
}

/// `command name args`: a builtin or program, skipping functions of that
/// name. `command -v name`: where it is, or nothing (status 1).
fn command(sh: &mut Shell, argv: &[String]) -> Res {
    if argv.get(1).map(String::as_str) == Some("-v") {
        let mut status = 0;
        for name in &argv[2..] {
            if is_builtin(name) || sh.funcs.contains_key(name.as_str()) {
                commands::println(name);
            } else if let Some(p) = resolve_program(name) {
                commands::println(&p);
            } else {
                status = 1;
            }
        }
        return Ok(status);
    }
    let rest = &argv[1..];
    let Some(name) = rest.first() else { return Ok(0) };
    if is_builtin(name) {
        return commands::run(sh, rest);
    }
    let Some(path) = resolve_program(name) else {
        term::error(&format!("{name}: command not found"));
        return Ok(127);
    };
    match anshi::fork() {
        None => {
            term::error("fork failed: too many processes");
            Ok(1)
        }
        Some(0) => {
            sh.exec(&path, rest, &[]);
            anshi::exit(126)
        }
        Some(pid) => Ok(anshi::waitpid(pid)),
    }
}

/// `test`: 0 for true, 1 for false, 2 for a malformed expression.
pub fn test(args: &[&str]) -> i32 {
    match eval_test(args) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(msg) => {
            term::error(&format!("test: {msg}"));
            2
        }
    }
}

fn eval_test(a: &[&str]) -> Result<bool, String> {
    if let Some(i) = a.iter().position(|&x| x == "-o").filter(|&i| i > 0) {
        return Ok(eval_test(&a[..i])? || eval_test(&a[i + 1..])?);
    }
    if let Some(i) = a.iter().position(|&x| x == "-a").filter(|&i| i > 0) {
        return Ok(eval_test(&a[..i])? && eval_test(&a[i + 1..])?);
    }
    match a {
        [] => Ok(false),
        ["!", rest @ ..] => Ok(!eval_test(rest)?),
        ["(", inner @ .., ")"] => eval_test(inner),
        [s] => Ok(!s.is_empty()),
        [op, x] => {
            let path = *x;
            Ok(match *op {
                "-n" => !x.is_empty(),
                "-z" => x.is_empty(),
                "-e" | "-r" | "-w" => anshi::exists(path),
                "-f" => anshi::exists(path) && !anshi::is_dir(path),
                "-d" => anshi::is_dir(path),
                "-x" => commands::is_program(path),
                "-s" => commands::read_text(path).is_some_and(|t| !t.is_empty()),
                "-L" | "-h" | "-p" | "-t" => false,
                _ => return Err(format!("{op}: unknown operator")),
            })
        }
        [x, op, y] => {
            let num = |s: &str| s.trim().parse::<i64>().map_err(|_| format!("{s}: not a number"));
            Ok(match *op {
                "=" | "==" => x == y,
                "!=" => x != y,
                "<" => x < y,
                ">" => x > y,
                "-eq" => num(x)? == num(y)?,
                "-ne" => num(x)? != num(y)?,
                "-lt" => num(x)? < num(y)?,
                "-le" => num(x)? <= num(y)?,
                "-gt" => num(x)? > num(y)?,
                "-ge" => num(x)? >= num(y)?,
                _ => return Err(format!("{op}: unknown operator")),
            })
        }
        _ => Err(String::from("too many arguments")),
    }
}

/// One line from stdin, without its newline; None at end of input.
fn read_line() -> Option<String> {
    let mut line = Vec::new();
    if anshi::isatty(0) {
        // The console hands over a whole edited line at once.
        let mut buf = [0u8; 1024];
        let n = anshi::read_wait(0, &mut buf)?;
        line.extend_from_slice(&buf[..n]);
    } else {
        // Byte by byte, so what's after this line stays for the next read.
        let mut b = [0u8];
        loop {
            match anshi::read_wait(0, &mut b) {
                Some(_) if b[0] == b'\n' => break,
                Some(_) => line.push(b[0]),
                None if line.is_empty() => return None,
                None => break,
            }
        }
    }
    Some(String::from_utf8_lossy(&line).into_owned())
}

fn read(sh: &mut Shell, args: &[&str]) -> i32 {
    let mut raw = false;
    let mut names = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-r" => raw = true,
            "-p" => {
                i += 1;
                anshi::write_all(2, args.get(i).unwrap_or(&"").as_bytes());
            }
            name => names.push(name),
        }
        i += 1;
    }
    if names.is_empty() {
        names.push("REPLY");
    }
    let Some(mut line) = read_line() else { return 1 };
    if !raw {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                out.extend(chars.next());
            } else {
                out.push(c);
            }
        }
        line = out;
    }
    let mut rest = line.trim_start_matches([' ', '\t']);
    for (k, name) in names.iter().enumerate() {
        if k + 1 == names.len() {
            sh.set_var(name, rest.trim_end_matches([' ', '\t']));
            break;
        }
        let end = rest.find([' ', '\t']).unwrap_or(rest.len());
        sh.set_var(name, &rest[..end]);
        rest = rest[end..].trim_start_matches([' ', '\t']);
    }
    0
}

fn export(sh: &mut Shell, args: &[&str]) -> i32 {
    if args.is_empty() || args == ["-p"] {
        let mut text = String::new();
        for (name, v) in sh.vars.iter().filter(|(_, v)| v.exported) {
            text.push_str(&format!("export {name}='{}'\n", v.value));
        }
        anshi::write_all(1, text.as_bytes());
        return 0;
    }
    let mut status = 0;
    for a in args {
        let (name, value) = match a.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (*a, None),
        };
        if !is_name(name) {
            term::error(&format!("export: {name}: not a valid name"));
            status = 1;
            continue;
        }
        if let Some(v) = value {
            sh.set_var(name, v);
        }
        sh.export(name);
    }
    status
}

fn set(sh: &mut Shell, args: &[&str]) -> i32 {
    if args.is_empty() {
        let mut text = String::new();
        for (name, v) in &sh.vars {
            text.push_str(&format!("{name}='{}'\n", v.value));
        }
        anshi::write_all(1, text.as_bytes());
        return 0;
    }
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "--" {
            i += 1;
            break;
        }
        let (on, flags) = match (a.strip_prefix('-'), a.strip_prefix('+')) {
            (Some(f), _) => (true, f),
            (_, Some(f)) => (false, f),
            _ => break,
        };
        for f in flags.chars() {
            match f {
                'e' => sh.errexit = on,
                'x' => sh.xtrace = on,
                _ => {
                    term::error(&format!("set: -{f}: unknown option (only -e and -x)"));
                    return 2;
                }
            }
        }
        i += 1;
    }
    if i < args.len() || args.contains(&"--") {
        sh.params = args[i..].iter().map(|s| String::from(*s)).collect();
    }
    0
}

/// `sleep 2`, `sleep 0.5`. Ctrl-C cuts it short (status 130).
fn sleep(args: &[&str]) -> i32 {
    let Some(arg) = args.first() else {
        term::error("sleep: needs a number of seconds");
        return 2;
    };
    let (whole, frac) = arg.split_once('.').unwrap_or((arg, ""));
    let secs: Option<u64> = if whole.is_empty() { Some(0) } else { whole.parse().ok() };
    let ms_frac: Option<u64> = (frac.len() <= 3 && frac.chars().all(|c| c.is_ascii_digit()))
        .then(|| format!("{frac:0<3}").parse().ok())
        .flatten();
    let (Some(secs), Some(frac)) = (secs, ms_frac) else {
        term::error(&format!("sleep: {arg}: not a number of seconds"));
        return 2;
    };
    let until = anshi::now_ms() + secs * 1000 + frac;
    while anshi::now_ms() < until {
        if matches!(anshi::poll_key(), anshi::KeyPoll::Key(3)) {
            return 130;
        }
        anshi::sched_yield();
    }
    0
}
