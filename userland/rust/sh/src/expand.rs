//! Turning words into strings: parameters (`$x`, `${x:-y}`, `$1`,
//! `"$@"`), command substitution, arithmetic, `~`, then -- for what
//! wasn't quoted -- field splitting on spaces, tabs and newlines, and
//! pathname globbing (`*`, `?`, `[...]`). Plus the pattern matcher that
//! globbing, `case` and `${x#pat}` share.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

use anshi::CString;

use crate::shell::{Flow, Shell};
use crate::syntax::{Op, Part, Word};
use crate::term;

/// A field being built: its final text, and the same text as a pattern,
/// with quoted pattern characters backslash-escaped.
#[derive(Default)]
struct Field {
    text: String,
    pat: String,
    /// Holds an unquoted `*`, `?` or `[`.
    glob: bool,
    /// Something went into it, even if only "" -- so it counts as a word.
    started: bool,
}

struct Fields {
    done: Vec<Field>,
    cur: Field,
    /// Whether unquoted expansions are split into separate fields.
    split: bool,
}

fn escape_pattern(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Fields {
    fn quoted(&mut self, s: &str) {
        self.cur.text.push_str(s);
        self.cur.pat.push_str(&escape_pattern(s));
        self.cur.started = true;
    }

    fn unquoted(&mut self, s: &str) {
        self.cur.text.push_str(s);
        self.cur.pat.push_str(s);
        self.cur.glob |= s.contains(['*', '?', '[']);
        self.cur.started = true;
    }

    /// The result of an unquoted expansion: split at whitespace.
    fn expansion(&mut self, s: &str) {
        if !self.split {
            return self.unquoted(s);
        }
        for (i, piece) in s.split([' ', '\t', '\n']).enumerate() {
            if i > 0 {
                self.finish();
            }
            if !piece.is_empty() {
                self.unquoted(piece);
            }
        }
    }

    fn finish(&mut self) {
        let f = core::mem::take(&mut self.cur);
        if f.started {
            self.done.push(f);
        }
    }
}

impl Shell {
    /// Command words: expanded, split and globbed.
    pub fn expand_words(&mut self, words: &[Word]) -> Result<Vec<String>, Flow> {
        let mut out = Vec::new();
        for w in words {
            for f in self.fields(w, true)? {
                if f.glob {
                    let matches = glob(&f.pat);
                    if !matches.is_empty() {
                        out.extend(matches);
                        continue;
                    }
                }
                out.push(f.text);
            }
        }
        Ok(out)
    }

    /// One string, not split or globbed: assignments, redirection
    /// targets, heredocs, the word `case` looks at.
    pub fn expand_one(&mut self, w: &Word) -> Result<String, Flow> {
        Ok(self.fields(w, false)?.into_iter().map(|f| f.text).collect::<Vec<_>>().join(" "))
    }

    /// A pattern (`case`, `${x#pat}`): quoted parts match literally.
    pub fn expand_pattern(&mut self, w: &Word) -> Result<String, Flow> {
        Ok(self.fields(w, false)?.into_iter().map(|f| f.pat).collect::<Vec<_>>().join(" "))
    }

    fn fields(&mut self, w: &Word, split: bool) -> Result<Vec<Field>, Flow> {
        let mut f = Fields { done: Vec::new(), cur: Field::default(), split };
        for part in &w.0 {
            match part {
                Part::Lit(s) => f.unquoted(s),
                Part::Quoted(s) => f.quoted(s),
                Part::Tilde => {
                    let home = String::from(self.var("HOME").unwrap_or("/"));
                    f.quoted(&home);
                }
                Part::Param { name, op: Op::Plain, quoted: true } if name == "@" => {
                    for (i, p) in self.params.clone().iter().enumerate() {
                        if i > 0 {
                            f.finish();
                        }
                        f.quoted(p);
                    }
                }
                Part::Param { name, op, quoted } => {
                    let value = self.param_op(name, op)?;
                    if *quoted { f.quoted(&value) } else { f.expansion(&value) }
                }
                Part::Command { body, quoted } => {
                    let out = self.capture(body);
                    if *quoted { f.quoted(&out) } else { f.expansion(&out) }
                }
                Part::Arith { expr, quoted } => {
                    let text = self.expand_one(expr)?;
                    let n = match arith(self, &text) {
                        Ok(n) => n,
                        Err(e) => {
                            term::error(&format!("$(({})): {e}", text.trim()));
                            return Err(Flow::Abort(1));
                        }
                    };
                    let s = format!("{n}");
                    if *quoted { f.quoted(&s) } else { f.expansion(&s) }
                }
            }
        }
        f.finish();
        Ok(f.done)
    }

    fn param_op(&mut self, name: &str, op: &Op) -> Result<String, Flow> {
        let value = self.param(name);
        let unset_or = |colon: bool, v: &Option<String>| v.as_ref().is_none_or(|v| colon && v.is_empty());
        Ok(match op {
            Op::Plain => value.unwrap_or_default(),
            Op::Length => match name {
                "@" | "*" => format!("{}", self.params.len()),
                _ => format!("{}", value.unwrap_or_default().chars().count()),
            },
            Op::Default(colon, w) => {
                if unset_or(*colon, &value) {
                    self.expand_one(w)?
                } else {
                    value.unwrap_or_default()
                }
            }
            Op::Assign(colon, w) => {
                if unset_or(*colon, &value) {
                    let v = self.expand_one(w)?;
                    self.set_var(name, &v);
                    v
                } else {
                    value.unwrap_or_default()
                }
            }
            Op::Alt(colon, w) => {
                if unset_or(*colon, &value) {
                    String::new()
                } else {
                    self.expand_one(w)?
                }
            }
            Op::Error(colon, w) => {
                if unset_or(*colon, &value) {
                    let msg = self.expand_one(w)?;
                    let msg = if msg.is_empty() { String::from("parameter not set") } else { msg };
                    term::error(&format!("{name}: {msg}"));
                    return Err(Flow::Abort(1));
                }
                value.unwrap_or_default()
            }
            Op::TrimPrefix(longest, w) => {
                let v = value.unwrap_or_default();
                let pat = self.expand_pattern(w)?;
                let cuts: Vec<usize> = v.char_indices().map(|(i, _)| i).chain([v.len()]).collect();
                let mut order: Vec<usize> = cuts;
                if *longest {
                    order.reverse();
                }
                match order.into_iter().find(|&i| fnmatch(&pat, &v[..i])) {
                    Some(i) => String::from(&v[i..]),
                    None => v,
                }
            }
            Op::TrimSuffix(longest, w) => {
                let v = value.unwrap_or_default();
                let pat = self.expand_pattern(w)?;
                let mut order: Vec<usize> = v.char_indices().map(|(i, _)| i).chain([v.len()]).collect();
                if !*longest {
                    order.reverse();
                }
                match order.into_iter().find(|&i| fnmatch(&pat, &v[i..])) {
                    Some(i) => String::from(&v[..i]),
                    None => v,
                }
            }
        })
    }
}

// ---------- patterns ----------

/// Whether `s` matches the shell pattern `pat`: `*`, `?`, `[abc]`,
/// `[a-z]`, `[!x]`, and `\x` for a literal x.
pub fn fnmatch(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = s.chars().collect();
    matches_at(&p, &s)
}

fn matches_at(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('*') => {
            let rest = &p[1..];
            (0..=s.len()).any(|i| matches_at(rest, &s[i..]))
        }
        Some('?') => !s.is_empty() && matches_at(&p[1..], &s[1..]),
        Some('[') => match class(p, s.first().copied()) {
            Some((true, len)) => matches_at(&p[len..], &s[1..]),
            Some((false, _)) => false,
            // No closing ]: a plain [.
            None => s.first() == Some(&'[') && matches_at(&p[1..], &s[1..]),
        },
        Some('\\') if p.len() > 1 => s.first() == Some(&p[1]) && matches_at(&p[2..], &s[1..]),
        Some(c) => s.first() == Some(c) && matches_at(&p[1..], &s[1..]),
    }
}

/// A `[...]` class at the start of `p`: whether `c` is in it, and its
/// length. None if it never closes.
fn class(p: &[char], c: Option<char>) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some('!' | '^'));
    if negate {
        i += 1;
    }
    let start = i;
    let mut hit = false;
    while i < p.len() {
        if p[i] == ']' && i > start {
            let found = c.is_some() && hit != negate;
            return Some((found, i + 1));
        }
        let lo = if p[i] == '\\' && i + 1 < p.len() {
            i += 1;
            p[i]
        } else {
            p[i]
        };
        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|&h| h != ']') {
            let hi = p[i + 2];
            hit |= c.is_some_and(|c| lo <= c && c <= hi);
            i += 3;
        } else {
            hit |= c == Some(lo);
            i += 1;
        }
    }
    None
}

fn has_glob(pat: &str) -> bool {
    let mut escaped = false;
    for c in pat.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '*' | '?' | '[' => return true,
            _ => {}
        }
    }
    false
}

fn unescape(pat: &str) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for c in pat.chars() {
        if c == '\\' && !escaped {
            escaped = true;
            continue;
        }
        escaped = false;
        out.push(c);
    }
    out
}

/// Paths matching `pat`, sorted; empty if none. A name starting with `.`
/// only matches a pattern that starts with one too.
pub fn glob(pat: &str) -> Vec<String> {
    let mut paths = vec![String::from(if pat.starts_with('/') { "/" } else { "" })];
    for comp in pat.split('/').filter(|c| !c.is_empty()) {
        let mut next = Vec::new();
        for base in &paths {
            let join = |name: &str| {
                if base.is_empty() {
                    String::from(name)
                } else if base.ends_with('/') {
                    format!("{base}{name}")
                } else {
                    format!("{base}/{name}")
                }
            };
            if !has_glob(comp) {
                let p = join(&unescape(comp));
                if anshi::exists(&p) {
                    next.push(p);
                }
                continue;
            }
            let dir = if base.is_empty() { "." } else { base.as_str() };
            let Some(entries) = CString::new(dir).ok().and_then(|d| anshi::read_dir(&d)) else { continue };
            for e in entries {
                if (comp.starts_with('.') || !e.name.starts_with('.')) && fnmatch(comp, &e.name) {
                    next.push(join(&e.name));
                }
            }
        }
        paths = next;
    }
    paths.retain(|p| !p.is_empty());
    paths.sort();
    paths
}

// ---------- arithmetic ----------

/// Evaluates `$((...))`: integers, variables by name, the C operators
/// (unary `- + ! ~`; `* / %`; `+ -`; `<< >>`; `< <= > >=`; `== !=`;
/// `&`; `^`; `|`; `&&`; `||`) and parentheses.
fn arith(sh: &Shell, text: &str) -> Result<i64, String> {
    let mut a = Arith { s: text.as_bytes(), i: 0, sh };
    let v = a.expr(0)?;
    a.skip_ws();
    if a.i < a.s.len() {
        return Err(format!("unexpected '{}'", &text[a.i..]));
    }
    Ok(v)
}

struct Arith<'a> {
    s: &'a [u8],
    i: usize,
    sh: &'a Shell,
}

const BINARY: [(&str, u8); 18] = [
    ("||", 1),
    ("&&", 2),
    ("==", 6),
    ("!=", 6),
    ("<=", 7),
    (">=", 7),
    ("<<", 8),
    (">>", 8),
    ("|", 3),
    ("^", 4),
    ("&", 5),
    ("<", 7),
    (">", 7),
    ("+", 9),
    ("-", 9),
    ("*", 10),
    ("/", 10),
    ("%", 10),
];

impl Arith<'_> {
    fn skip_ws(&mut self) {
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn expr(&mut self, min: u8) -> Result<i64, String> {
        let mut left = self.unary()?;
        loop {
            self.skip_ws();
            let Some(&(op, prec)) = BINARY.iter().find(|(op, _)| self.s[self.i..].starts_with(op.as_bytes())) else {
                return Ok(left);
            };
            if prec < min {
                return Ok(left);
            }
            self.i += op.len();
            let right = self.expr(prec + 1)?;
            left = match op {
                "||" => (left != 0 || right != 0) as i64,
                "&&" => (left != 0 && right != 0) as i64,
                "==" => (left == right) as i64,
                "!=" => (left != right) as i64,
                "<=" => (left <= right) as i64,
                ">=" => (left >= right) as i64,
                "<" => (left < right) as i64,
                ">" => (left > right) as i64,
                "<<" => left.wrapping_shl(right as u32),
                ">>" => left.wrapping_shr(right as u32),
                "|" => left | right,
                "^" => left ^ right,
                "&" => left & right,
                "+" => left.wrapping_add(right),
                "-" => left.wrapping_sub(right),
                "*" => left.wrapping_mul(right),
                "/" | "%" if right == 0 => return Err(String::from("division by zero")),
                "/" => left.wrapping_div(right),
                _ => left.wrapping_rem(right),
            };
        }
    }

    fn unary(&mut self) -> Result<i64, String> {
        self.skip_ws();
        let Some(&c) = self.s.get(self.i) else { return Err(String::from("missing operand")) };
        match c {
            b'-' | b'+' | b'!' | b'~' => {
                self.i += 1;
                let v = self.unary()?;
                Ok(match c {
                    b'-' => v.wrapping_neg(),
                    b'+' => v,
                    b'!' => (v == 0) as i64,
                    _ => !v,
                })
            }
            b'(' => {
                self.i += 1;
                let v = self.expr(0)?;
                self.skip_ws();
                if self.s.get(self.i) != Some(&b')') {
                    return Err(String::from("missing )"));
                }
                self.i += 1;
                Ok(v)
            }
            b'0'..=b'9' => {
                let start = self.i;
                while self.s.get(self.i).is_some_and(|c| c.is_ascii_alphanumeric()) {
                    self.i += 1;
                }
                let t = core::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
                let parsed = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                    i64::from_str_radix(hex, 16)
                } else if t.len() > 1 && t.starts_with('0') {
                    i64::from_str_radix(&t[1..], 8)
                } else {
                    t.parse()
                };
                parsed.map_err(|_| format!("bad number '{t}'"))
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = self.i;
                while self.s.get(self.i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
                    self.i += 1;
                }
                let name = core::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
                let value = self.sh.var(name).unwrap_or("").trim();
                if value.is_empty() {
                    return Ok(0);
                }
                value.parse().map_err(|_| format!("{name}: not a number ('{value}')"))
            }
            _ => Err(format!("unexpected '{}'", c as char)),
        }
    }
}
