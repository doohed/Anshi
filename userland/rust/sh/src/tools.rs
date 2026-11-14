//! Small text tools, built in so pipelines have something to feed:
//! grep, head, tail, wc, sort, seq, printf, basename, dirname. Each reads
//! its files, or stdin when given none (or `-`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::commands::unescape;
use crate::term;

const TOOLS: [&str; 9] = ["grep", "head", "tail", "wc", "sort", "seq", "printf", "basename", "dirname"];

pub fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

pub fn run(name: &str, args: &[&str]) -> i32 {
    match name {
        "grep" => grep(args),
        "head" | "tail" => head_tail(name, args),
        "wc" => wc(args),
        "sort" => sort(args),
        "seq" => seq(args),
        "printf" => printf(args),
        "basename" => match args {
            [path, rest @ ..] => {
                let trimmed = path.trim_end_matches('/');
                let mut base = trimmed.rsplit('/').next().unwrap_or("");
                if base.is_empty() {
                    base = "/";
                }
                if let Some(suffix) = rest.first() {
                    base = base.strip_suffix(suffix).filter(|b| !b.is_empty()).unwrap_or(base);
                }
                out(&format!("{base}\n"));
                0
            }
            _ => usage("basename <path> [suffix]"),
        },
        "dirname" => match args {
            [path] => {
                let trimmed = path.trim_end_matches('/');
                let dir = match trimmed.rfind('/') {
                    Some(0) => "/",
                    Some(i) => trimmed[..i].trim_end_matches('/'),
                    None if path.starts_with('/') => "/",
                    None => ".",
                };
                out(&format!("{}\n", if dir.is_empty() { "/" } else { dir }));
                0
            }
            _ => usage("dirname <path>"),
        },
        _ => 127,
    }
}

fn out(s: &str) {
    anshi::write_all(1, s.as_bytes());
}

fn usage(u: &str) -> i32 {
    term::error(&format!("usage: {u}"));
    2
}

/// The text of each input: the named files, or stdin.
fn inputs(name: &str, files: &[&str]) -> Option<Vec<(String, String)>> {
    if files.is_empty() {
        return Some(alloc::vec![(String::from("-"), stdin_text())]);
    }
    let mut all = Vec::new();
    for f in files {
        if *f == "-" {
            all.push((String::from("-"), stdin_text()));
            continue;
        }
        match crate::commands::read_text(f) {
            Some(t) => all.push((String::from(*f), t)),
            None => {
                term::error(&format!("{name}: {f}: no such file"));
                return None;
            }
        }
    }
    Some(all)
}

fn stdin_text() -> String {
    String::from_utf8_lossy(&anshi::read_to_end(0)).into_owned()
}

/// Splits leading `-x` flags (letters out of `allowed`) from the rest.
/// `-n N`-style values are taken by the caller first.
fn flags<'a>(args: &[&'a str], allowed: &str) -> Result<(String, Vec<&'a str>), char> {
    let mut set = String::new();
    let mut rest = Vec::new();
    let mut only_args = false;
    for a in args {
        if !only_args && *a == "--" {
            only_args = true;
        } else if !only_args && a.len() > 1 && a.starts_with('-') {
            for c in a[1..].chars() {
                if !allowed.contains(c) {
                    return Err(c);
                }
                set.push(c);
            }
        } else {
            rest.push(*a);
        }
    }
    Ok((set, rest))
}

fn grep(args: &[&str]) -> i32 {
    let (f, rest) = match flags(args, "ivcnqh") {
        Ok(x) => x,
        Err(c) => return usage(&format!("grep [-ivcnqh] <text> [file]... (no -{c})")),
    };
    let Some((pattern, files)) = rest.split_first() else { return usage("grep [-ivcnqh] <text> [file]...") };
    let fold = f.contains('i');
    let needle = if fold { pattern.to_lowercase() } else { String::from(*pattern) };
    let Some(inputs) = inputs("grep", files) else { return 2 };
    let label = inputs.len() > 1 && !f.contains('h');
    let color = anshi::isatty(1) && !f.contains('v') && !needle.is_empty();
    let mut found = false;
    let mut text = String::new();
    for (name, body) in &inputs {
        let mut count = 0;
        for (n, line) in body.lines().enumerate() {
            let hay = if fold { line.to_lowercase() } else { String::from(line) };
            if hay.contains(needle.as_str()) == f.contains('v') {
                continue;
            }
            found = true;
            count += 1;
            if f.contains('q') {
                return 0;
            }
            if f.contains('c') {
                continue;
            }
            if label {
                text.push_str(&format!("{name}:"));
            }
            if f.contains('n') {
                text.push_str(&format!("{}:", n + 1));
            }
            if color && hay.len() == line.len() {
                // Each match in red; positions from the folded copy line
                // up with the line when folding kept every length.
                let mut at = 0;
                while let Some(i) = hay[at..].find(needle.as_str()) {
                    let s = at + i;
                    text.push_str(&line[at..s]);
                    text.push_str(&format!("\x1b[1;91m{}\x1b[0m", &line[s..s + needle.len()]));
                    at = s + needle.len();
                }
                text.push_str(&line[at..]);
            } else {
                text.push_str(line);
            }
            text.push('\n');
        }
        if f.contains('c') {
            text.push_str(&if label { format!("{name}:{count}\n") } else { format!("{count}\n") });
        }
    }
    out(&text);
    if found { 0 } else { 1 }
}

/// `-n N`, `-nN` or `-N` out of `args`.
fn line_count(args: &[&str]) -> Result<(usize, Vec<String>), ()> {
    let mut n = 10;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "-n" {
            i += 1;
            n = args.get(i).and_then(|v| v.parse().ok()).ok_or(())?;
        } else if let Some(v) = a.strip_prefix("-n").or_else(|| a.strip_prefix('-')).filter(|v| !v.is_empty()) {
            n = v.parse().map_err(|_| ())?;
        } else {
            rest.push(String::from(a));
        }
        i += 1;
    }
    Ok((n, rest))
}

fn head_tail(name: &str, args: &[&str]) -> i32 {
    let Ok((n, files)) = line_count(args) else { return usage(&format!("{name} [-n N] [file]...")) };
    let files: Vec<&str> = files.iter().map(String::as_str).collect();
    let Some(inputs) = inputs(name, &files) else { return 1 };
    let mut text = String::new();
    for (i, (file, body)) in inputs.iter().enumerate() {
        if inputs.len() > 1 {
            text.push_str(&format!("{}==> {file} <==\n", if i > 0 { "\n" } else { "" }));
        }
        let lines: Vec<&str> = body.lines().collect();
        let picked =
            if name == "head" { &lines[..n.min(lines.len())] } else { &lines[lines.len().saturating_sub(n)..] };
        for l in picked {
            text.push_str(l);
            text.push('\n');
        }
    }
    out(&text);
    0
}

fn wc(args: &[&str]) -> i32 {
    let (f, files) = match flags(args, "lwc") {
        Ok(x) => x,
        Err(_) => return usage("wc [-lwc] [file]..."),
    };
    let Some(inputs) = inputs("wc", &files) else { return 1 };
    let f = if f.is_empty() { String::from("lwc") } else { f };
    let mut total = [0usize; 3];
    let mut text = String::new();
    let mut line = |counts: [usize; 3], label: &str| {
        let mut s = String::new();
        for (i, k) in ['l', 'w', 'c'].iter().enumerate() {
            if f.contains(*k) {
                s.push_str(&format!("{:>8}", counts[i]));
            }
        }
        if label != "-" {
            s.push(' ');
            s.push_str(label);
        }
        text.push_str(s.trim_start());
        text.push('\n');
    };
    for (name, body) in &inputs {
        let counts = [body.matches('\n').count(), body.split_whitespace().count(), body.len()];
        for i in 0..3 {
            total[i] += counts[i];
        }
        line(counts, name);
    }
    if inputs.len() > 1 {
        line(total, "total");
    }
    out(&text);
    0
}

fn sort(args: &[&str]) -> i32 {
    let (f, files) = match flags(args, "rnu") {
        Ok(x) => x,
        Err(_) => return usage("sort [-rnu] [file]..."),
    };
    let Some(inputs) = inputs("sort", &files) else { return 1 };
    let mut lines: Vec<&str> = inputs.iter().flat_map(|(_, b)| b.lines()).collect();
    if f.contains('n') {
        let key = |l: &str| -> i64 {
            let t = l.trim_start();
            let end = t
                .char_indices()
                .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && c == '-')))
                .map_or(t.len(), |(i, _)| i);
            t[..end].parse().unwrap_or(0)
        };
        lines.sort_by(|a, b| key(a).cmp(&key(b)).then(a.cmp(b)));
    } else {
        lines.sort();
    }
    if f.contains('u') {
        lines.dedup();
    }
    if f.contains('r') {
        lines.reverse();
    }
    let mut text = String::new();
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    out(&text);
    0
}

fn seq(args: &[&str]) -> i32 {
    let nums: Option<Vec<i64>> = args.iter().map(|a| a.parse().ok()).collect();
    let (first, step, last) = match nums.as_deref() {
        Some([last]) => (1, 1, *last),
        Some([first, last]) => (*first, 1, *last),
        Some([first, step, last]) if *step != 0 => (*first, *step, *last),
        _ => return usage("seq [first [step]] last"),
    };
    let mut text = String::new();
    let mut i = first;
    while (step > 0 && i <= last) || (step < 0 && i >= last) {
        text.push_str(&format!("{i}\n"));
        i += step;
        if text.len() > 64 * 1024 {
            out(&text);
            text.clear();
        }
    }
    out(&text);
    0
}

/// printf: %s %d %i %u %x %X %o %c %b %%, with `-`/`0` flags, a width
/// and a precision. The format is reused while arguments remain.
fn printf(args: &[&str]) -> i32 {
    let Some((format, mut rest)) = args.split_first() else { return usage("printf <format> [arg]...") };
    let fmt: Vec<char> = unescape(format).chars().collect();
    let mut text = String::new();
    let mut status = 0;
    loop {
        let mut used = false;
        let mut i = 0;
        while i < fmt.len() {
            let c = fmt[i];
            i += 1;
            if c != '%' {
                text.push(c);
                continue;
            }
            if fmt.get(i) == Some(&'%') {
                text.push('%');
                i += 1;
                continue;
            }
            let mut left = false;
            let mut zero = false;
            while let Some(&f) = fmt.get(i).filter(|f| **f == '-' || **f == '0') {
                left |= f == '-';
                zero |= f == '0';
                i += 1;
            }
            let mut width = 0usize;
            while let Some(d) = fmt.get(i).and_then(|c| c.to_digit(10)) {
                width = width * 10 + d as usize;
                i += 1;
            }
            let mut precision = None;
            if fmt.get(i) == Some(&'.') {
                i += 1;
                let mut p = 0;
                while let Some(d) = fmt.get(i).and_then(|c| c.to_digit(10)) {
                    p = p * 10 + d as usize;
                    i += 1;
                }
                precision = Some(p);
            }
            let Some(&conv) = fmt.get(i) else { break };
            i += 1;
            let arg = rest.first().copied().unwrap_or("");
            if !rest.is_empty() {
                rest = &rest[1..];
                used = true;
            }
            let mut num = |s: &str| -> i64 {
                let t = s.trim();
                if let Some(c) = t.strip_prefix('\'').or_else(|| t.strip_prefix('"')) {
                    return c.chars().next().map_or(0, |c| c as i64);
                }
                t.parse().unwrap_or_else(|_| {
                    if !t.is_empty() {
                        term::error(&format!("printf: {t}: not a number"));
                        status = 1;
                    }
                    0
                })
            };
            let body = match conv {
                's' => String::from(match precision {
                    Some(p) => &arg[..arg.char_indices().nth(p).map_or(arg.len(), |(i, _)| i)],
                    None => arg,
                }),
                'b' => unescape(arg),
                'c' => arg.chars().next().map(String::from).unwrap_or_default(),
                'd' | 'i' => format!("{}", num(arg)),
                'u' => format!("{}", num(arg) as u64),
                'x' => format!("{:x}", num(arg)),
                'X' => format!("{:X}", num(arg)),
                'o' => format!("{:o}", num(arg)),
                other => {
                    term::error(&format!("printf: %{other}: unknown conversion"));
                    return 1;
                }
            };
            let pad = width.saturating_sub(body.chars().count());
            if left {
                text.push_str(&body);
                text.extend(core::iter::repeat_n(' ', pad));
            } else if zero && "diuxXo".contains(conv) {
                let (sign, digits) = body.strip_prefix('-').map_or(("", body.as_str()), |d| ("-", d));
                text.push_str(sign);
                text.extend(core::iter::repeat_n('0', pad));
                text.push_str(digits);
            } else {
                text.extend(core::iter::repeat_n(' ', pad));
                text.push_str(&body);
            }
        }
        if rest.is_empty() || !used {
            break;
        }
    }
    out(&text);
    status
}
