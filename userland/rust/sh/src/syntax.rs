//! The shell language: source text in, a tree of commands out.
//!
//! A POSIX-shaped subset: simple commands with `NAME=value` prefixes and
//! redirections (`<`, `>`, `>>`, `2>`, `2>&1`, `&>`, `<<` heredocs);
//! pipelines (`a | b`, `! a`); lists (`;`, newlines, `&&`, `||`);
//! `if`/`elif`/`else`, `while`, `until`, `for`, `case`, `{ ...; }`,
//! `( ... )` and functions (`name() { ...; }`). Words keep track of
//! what was quoted, so expand.rs knows what to split and glob: `'...'`,
//! `"..."`, `\x`, `$name`, `${name...}`, `$(...)`, `` `...` ``, `$((...))`
//! and a leading `~`. Background jobs (`&`) aren't supported -- there's
//! no way to wait for one later.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

/// One piece of a word.
#[derive(Clone)]
pub enum Part {
    /// Unquoted text: globbed (`*`, `?`, `[`), never split.
    Lit(String),
    /// Quoted or escaped text, taken as it is.
    Quoted(String),
    /// `$name`, `${name...}`, `$1`, `$?`, ...
    Param { name: String, op: Op, quoted: bool },
    /// `$(...)` or `` `...` ``.
    Command { body: Box<Node>, quoted: bool },
    /// `$((...))`; the expression is expanded, then evaluated.
    Arith { expr: Word, quoted: bool },
    /// A leading unquoted `~`: $HOME.
    Tilde,
}

/// What `${name...}` does with the value.
#[derive(Clone)]
pub enum Op {
    Plain,
    /// `${#name}`
    Length,
    /// `${name:-w}` (colon: an empty value counts as unset) / `${name-w}`
    Default(bool, Word),
    /// `${name:=w}` / `${name=w}`
    Assign(bool, Word),
    /// `${name:+w}` / `${name+w}`
    Alt(bool, Word),
    /// `${name:?w}` / `${name?w}`
    Error(bool, Word),
    /// `${name#pat}` / `${name##pat}` (true: the longest match)
    TrimPrefix(bool, Word),
    /// `${name%pat}` / `${name%%pat}`
    TrimSuffix(bool, Word),
}

#[derive(Clone, Default)]
pub struct Word(pub Vec<Part>);

impl Word {
    /// The word's text when it's plain unquoted text -- how keywords and
    /// names are recognized.
    pub fn literal(&self) -> Option<&str> {
        match self.0.as_slice() {
            [Part::Lit(s)] => Some(s),
            _ => None,
        }
    }

    fn push_char(&mut self, c: char, quoted: bool) {
        match (self.0.last_mut(), quoted) {
            (Some(Part::Lit(s)), false) | (Some(Part::Quoted(s)), true) => s.push(c),
            _ => self.0.push(if quoted { Part::Quoted(c.into()) } else { Part::Lit(c.into()) }),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum RedirOp {
    /// `<`
    In,
    /// `>`
    Out,
    /// `>>`
    Append,
    /// `&>`: stdout and stderr both
    Both,
    /// `<&n` / `>&n` (`-` closes)
    Dup,
    /// `<<` / `<<-`: the target is the body
    Heredoc,
}

#[derive(Clone)]
pub struct Redir {
    pub fd: i32,
    pub op: RedirOp,
    pub target: Word,
}

#[derive(Clone)]
pub enum Node {
    Simple {
        assigns: Vec<(String, Word)>,
        words: Vec<Word>,
        redirs: Vec<Redir>,
    },
    Pipeline {
        cmds: Vec<Node>,
        bang: bool,
    },
    /// `first && a || b ...`: true for &&.
    AndOr {
        first: Box<Node>,
        rest: Vec<(bool, Node)>,
    },
    List(Vec<Node>),
    If {
        branches: Vec<(Node, Node)>,
        other: Option<Box<Node>>,
    },
    Loop {
        cond: Box<Node>,
        body: Box<Node>,
        until: bool,
    },
    For {
        var: String,
        items: Option<Vec<Word>>,
        body: Box<Node>,
    },
    Case {
        word: Word,
        arms: Vec<(Vec<Word>, Node)>,
    },
    Group(Box<Node>),
    Subshell(Box<Node>),
    FuncDef {
        name: String,
        body: Box<Node>,
    },
    Redirected {
        node: Box<Node>,
        redirs: Vec<Redir>,
    },
}

pub enum ParseError {
    /// The text ended in the middle of something (an open quote, an `if`
    /// without `fi`): an interactive shell reads another line.
    Incomplete,
    /// The message and the line it's on (1-based).
    Syntax(String, usize),
}

/// Parses a whole script or command line.
pub fn parse(src: &str) -> Result<Node, ParseError> {
    let mut p = Parser { src: src.chars().collect(), pos: 0, peeked: None };
    let node = p.list(&[])?;
    match p.next()? {
        Tok::Eof => Ok(node),
        t => Err(p.unexpected(&t)),
    }
}

/// Whether `s` can be a variable or function name.
pub fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[derive(Clone)]
enum Tok {
    Word(Word),
    /// Digits right before `<` or `>`: the fd a redirection is for.
    IoNum(i32),
    Op(&'static str),
    Newline,
    Eof,
}

const OPS: [&str; 16] = ["&&", "||", ";;", "<<-", "<<", ">>", "&>", ">&", "<&", "|", "&", ";", "(", ")", "<", ">"];
const KEYWORDS: [&str; 15] =
    ["if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "in", "case", "esac", "{", "}"];

fn is_meta(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')')
}

/// How text inside a word is read.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// A word on the command line: ends at a metacharacter.
    Word,
    /// Like Word but spaces and operators are just text -- the operand of
    /// `${name:-...}`.
    Operand,
    /// Inside `"..."` (ends at the `"`), or a heredoc body or arithmetic
    /// (ends at the end of the text): only `$`, `` ` `` and some
    /// backslashes are special.
    Dquote,
}

struct Parser {
    src: Vec<char>,
    pos: usize,
    peeked: Option<Tok>,
}

impl Parser {
    fn line(&self) -> usize {
        1 + self.src[..self.pos.min(self.src.len())].iter().filter(|&&c| c == '\n').count()
    }

    fn syntax(&self, msg: impl Into<String>) -> ParseError {
        ParseError::Syntax(msg.into(), self.line())
    }

    fn unexpected(&self, t: &Tok) -> ParseError {
        match t {
            Tok::Eof => ParseError::Incomplete,
            Tok::Newline => self.syntax("syntax error near unexpected newline"),
            Tok::Op(op) => self.syntax(format!("syntax error near unexpected '{op}'")),
            Tok::IoNum(n) => self.syntax(format!("syntax error near unexpected '{n}'")),
            Tok::Word(w) => {
                let text = w.literal().unwrap_or("word");
                self.syntax(format!("syntax error near unexpected '{text}'"))
            }
        }
    }

    fn cur(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn at(&self, s: &str) -> bool {
        s.chars().enumerate().all(|(i, c)| self.src.get(self.pos + i) == Some(&c))
    }

    // ---------- tokens ----------

    fn peek(&mut self) -> Result<&Tok, ParseError> {
        if self.peeked.is_none() {
            let t = self.lex()?;
            self.peeked = Some(t);
        }
        Ok(self.peeked.as_ref().unwrap())
    }

    fn next(&mut self) -> Result<Tok, ParseError> {
        self.peek()?;
        Ok(self.peeked.take().unwrap())
    }

    /// Whether the next token is the keyword `kw`.
    fn peek_kw(&mut self, kw: &str) -> Result<bool, ParseError> {
        Ok(matches!(self.peek()?, Tok::Word(w) if w.literal() == Some(kw)))
    }

    fn peek_op(&mut self, op: &str) -> Result<bool, ParseError> {
        Ok(matches!(self.peek()?, Tok::Op(o) if *o == op))
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), ParseError> {
        let t = self.next()?;
        match &t {
            Tok::Word(w) if w.literal() == Some(kw) => Ok(()),
            _ => Err(self.unexpected(&t)),
        }
    }

    fn skip_newlines(&mut self) -> Result<(), ParseError> {
        while matches!(self.peek()?, Tok::Newline) {
            self.next()?;
        }
        Ok(())
    }

    fn lex(&mut self) -> Result<Tok, ParseError> {
        loop {
            match self.cur() {
                Some(' ' | '\t') => self.pos += 1,
                Some('\\') if self.src.get(self.pos + 1) == Some(&'\n') => self.pos += 2,
                Some('#') => {
                    while self.cur().is_some_and(|c| c != '\n') {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
        let Some(c) = self.cur() else { return Ok(Tok::Eof) };
        if c == '\n' {
            self.pos += 1;
            return Ok(Tok::Newline);
        }
        if let Some(op) = OPS.iter().find(|op| self.at(op)) {
            self.pos += op.len();
            return Ok(Tok::Op(op));
        }
        let start = self.pos;
        let word = self.text(Mode::Word)?;
        // `2>file`: digits run straight into a redirection.
        if matches!(self.cur(), Some('<' | '>')) {
            let digits: String = self.src[start..self.pos].iter().collect();
            if let Some(n) = digits.parse().ok().filter(|_| digits.chars().all(|c| c.is_ascii_digit())) {
                return Ok(Tok::IoNum(n));
            }
        }
        Ok(Tok::Word(word))
    }

    // ---------- words ----------

    /// Reads text in `mode` from the current position.
    fn text(&mut self, mode: Mode) -> Result<Word, ParseError> {
        let mut w = Word::default();
        let quoted = mode == Mode::Dquote;
        let start = self.pos;
        while let Some(c) = self.cur() {
            match c {
                c if mode == Mode::Word && is_meta(c) => break,
                '"' if mode == Mode::Dquote => break,
                '~' if mode == Mode::Word && self.pos == start => {
                    let next = self.src.get(self.pos + 1).copied();
                    if next.is_none_or(|n| n == '/' || is_meta(n)) {
                        w.0.push(Part::Tilde);
                    } else {
                        w.push_char('~', false);
                    }
                    self.pos += 1;
                }
                '\'' if !quoted => {
                    self.pos += 1;
                    let end =
                        (self.pos..self.src.len()).find(|&i| self.src[i] == '\'').ok_or(ParseError::Incomplete)?;
                    let s: String = self.src[self.pos..end].iter().collect();
                    w.0.push(Part::Quoted(s));
                    self.pos = end + 1;
                }
                '"' => {
                    self.pos += 1;
                    let inner = self.text(Mode::Dquote)?;
                    if self.cur() != Some('"') {
                        return Err(ParseError::Incomplete);
                    }
                    self.pos += 1;
                    // An empty "" still makes a word.
                    if inner.0.is_empty() {
                        w.0.push(Part::Quoted(String::new()));
                    }
                    w.0.extend(inner.0);
                }
                '\\' => {
                    self.pos += 1;
                    match self.cur() {
                        None => w.push_char('\\', quoted),
                        Some('\n') => self.pos += 1,
                        Some(e) if quoted && !matches!(e, '$' | '`' | '"' | '\\') => {
                            w.push_char('\\', true);
                        }
                        Some(e) => {
                            w.push_char(e, true);
                            self.pos += 1;
                        }
                    }
                }
                '$' => self.dollar(&mut w, quoted)?,
                '`' => {
                    self.pos += 1;
                    let end = self.find_close('`', '`')?;
                    let body: String = self.src[self.pos..end].iter().collect();
                    self.pos = end + 1;
                    w.0.push(Part::Command { body: Box::new(sub_parse(&body)?), quoted });
                }
                c => {
                    w.push_char(c, quoted);
                    self.pos += 1;
                }
            }
        }
        Ok(w)
    }

    /// Where the `close` matching an already-consumed `open` is, skipping
    /// quotes and nested pairs.
    fn find_close(&self, open: char, close: char) -> Result<usize, ParseError> {
        let mut depth = 0;
        let mut i = self.pos;
        while i < self.src.len() {
            match self.src[i] {
                '\\' => i += 1,
                '\'' if open != '`' => {
                    i += 1;
                    while i < self.src.len() && self.src[i] != '\'' {
                        i += 1;
                    }
                }
                '"' if open != '`' => {
                    i += 1;
                    while i < self.src.len() && self.src[i] != '"' {
                        if self.src[i] == '\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                }
                c if c == close && depth == 0 => return Ok(i),
                c if c == close => depth -= 1,
                c if c == open => depth += 1,
                _ => {}
            }
            i += 1;
        }
        Err(ParseError::Incomplete)
    }

    fn dollar(&mut self, w: &mut Word, quoted: bool) -> Result<(), ParseError> {
        self.pos += 1;
        match self.cur() {
            Some('(') if self.src.get(self.pos + 1) == Some(&'(') => {
                self.pos += 2;
                let end = self.find_close('(', ')')?;
                if self.src.get(end + 1) != Some(&')') {
                    return Err(self.syntax("missing )) in $(( ))"));
                }
                let expr = sub_text(&self.src[self.pos..end], Mode::Dquote)?;
                self.pos = end + 2;
                w.0.push(Part::Arith { expr, quoted });
            }
            Some('(') => {
                self.pos += 1;
                let end = self.find_close('(', ')')?;
                let body: String = self.src[self.pos..end].iter().collect();
                self.pos = end + 1;
                w.0.push(Part::Command { body: Box::new(sub_parse(&body)?), quoted });
            }
            Some('{') => {
                self.pos += 1;
                let end = self.find_close('{', '}')?;
                let inner: Vec<char> = self.src[self.pos..end].to_vec();
                self.pos = end + 1;
                w.0.push(braced(&inner, quoted).ok_or_else(|| self.syntax("bad ${...} substitution"))?);
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let start = self.pos;
                while self.cur().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                    self.pos += 1;
                }
                let name = self.src[start..self.pos].iter().collect();
                w.0.push(Part::Param { name, op: Op::Plain, quoted });
            }
            Some(c) if c.is_ascii_digit() || "?$#@*!-".contains(c) => {
                self.pos += 1;
                w.0.push(Part::Param { name: c.into(), op: Op::Plain, quoted });
            }
            _ => w.push_char('$', quoted),
        }
        Ok(())
    }

    // ---------- commands ----------

    /// Commands separated by `;` or newlines, up to a keyword in `terms`,
    /// `)`, `;;` or the end.
    fn list(&mut self, terms: &[&str]) -> Result<Node, ParseError> {
        let mut items = Vec::new();
        loop {
            self.skip_newlines()?;
            match self.peek()? {
                Tok::Eof => break,
                Tok::Op(")" | ";;") => break,
                Tok::Word(w) if w.literal().is_some_and(|k| terms.contains(&k)) => break,
                _ => {}
            }
            items.push(self.and_or()?);
            match self.peek()? {
                Tok::Op(";") | Tok::Newline => {
                    self.next()?;
                }
                Tok::Op("&") => return Err(self.syntax("background jobs (&) aren't supported")),
                _ => break,
            }
        }
        if !terms.is_empty() && matches!(self.peek()?, Tok::Eof) {
            return Err(ParseError::Incomplete);
        }
        Ok(if items.len() == 1 { items.pop().unwrap() } else { Node::List(items) })
    }

    fn and_or(&mut self) -> Result<Node, ParseError> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            let and = match self.peek()? {
                Tok::Op("&&") => true,
                Tok::Op("||") => false,
                _ => break,
            };
            self.next()?;
            self.skip_newlines()?;
            rest.push((and, self.pipeline()?));
        }
        Ok(if rest.is_empty() { first } else { Node::AndOr { first: Box::new(first), rest } })
    }

    fn pipeline(&mut self) -> Result<Node, ParseError> {
        let bang = self.peek_kw("!")?;
        if bang {
            self.next()?;
        }
        let mut cmds = vec![self.command()?];
        while self.peek_op("|")? {
            self.next()?;
            self.skip_newlines()?;
            cmds.push(self.command()?);
        }
        Ok(if cmds.len() == 1 && !bang { cmds.pop().unwrap() } else { Node::Pipeline { cmds, bang } })
    }

    fn command(&mut self) -> Result<Node, ParseError> {
        let kw = match self.peek()? {
            Tok::Word(w) => w.literal().filter(|k| KEYWORDS.contains(k) || *k == "function").map(String::from),
            Tok::Op("(") => Some(String::from("(")),
            _ => None,
        };
        let node = match kw.as_deref() {
            Some("if") => self.if_clause()?,
            Some("while") => self.loop_clause(false)?,
            Some("until") => self.loop_clause(true)?,
            Some("for") => self.for_clause()?,
            Some("case") => self.case_clause()?,
            Some("{") => {
                self.next()?;
                let body = self.list(&["}"])?;
                self.expect_kw("}")?;
                Node::Group(Box::new(body))
            }
            Some("(") => {
                self.next()?;
                let body = self.list(&[])?;
                match self.next()? {
                    Tok::Op(")") => Node::Subshell(Box::new(body)),
                    t => return Err(self.unexpected(&t)),
                }
            }
            Some("function") => {
                self.next()?;
                let name = match self.next()? {
                    Tok::Word(w) if w.literal().is_some_and(is_name) => String::from(w.literal().unwrap()),
                    t => return Err(self.unexpected(&t)),
                };
                if self.peek_op("(")? {
                    self.next()?;
                    match self.next()? {
                        Tok::Op(")") => {}
                        t => return Err(self.unexpected(&t)),
                    }
                }
                self.skip_newlines()?;
                return Ok(Node::FuncDef { name, body: Box::new(self.command()?) });
            }
            Some(k) if k != "in" => {
                let t = self.next()?;
                return Err(self.unexpected(&t));
            }
            _ => return self.simple(),
        };
        // A compound command can carry redirections too: `done < file`.
        let mut redirs = Vec::new();
        while let Some(r) = self.redir()? {
            redirs.push(r);
        }
        Ok(if redirs.is_empty() { node } else { Node::Redirected { node: Box::new(node), redirs } })
    }

    /// A redirection, if one is next.
    fn redir(&mut self) -> Result<Option<Redir>, ParseError> {
        let (fd, op) = match self.peek()? {
            &Tok::IoNum(n) => {
                self.next()?;
                match self.next()? {
                    Tok::Op(op) => (Some(n), op),
                    t => return Err(self.unexpected(&t)),
                }
            }
            Tok::Op(op @ ("<" | ">" | ">>" | "&>" | ">&" | "<&" | "<<" | "<<-")) => {
                let op = *op;
                self.next()?;
                (None, op)
            }
            _ => return Ok(None),
        };
        let target = match self.next()? {
            Tok::Word(w) => w,
            t => return Err(self.unexpected(&t)),
        };
        let (default_fd, op) = match op {
            "<" => (0, RedirOp::In),
            ">" => (1, RedirOp::Out),
            ">>" => (1, RedirOp::Append),
            "&>" => (1, RedirOp::Both),
            ">&" => (1, RedirOp::Dup),
            "<&" => (0, RedirOp::Dup),
            "<<" | "<<-" => {
                let body = self.heredoc(&target, op == "<<-")?;
                return Ok(Some(Redir { fd: fd.unwrap_or(0), op: RedirOp::Heredoc, target: body }));
            }
            _ => return Err(self.syntax(format!("syntax error near '{op}'"))),
        };
        Ok(Some(Redir { fd: fd.unwrap_or(default_fd), op, target }))
    }

    /// Takes a heredoc's body -- the lines after this one, up to the
    /// delimiter -- out of the source. A quoted delimiter means the body is
    /// taken literally; otherwise `$` and `` ` `` work in it.
    fn heredoc(&mut self, delim: &Word, strip_tabs: bool) -> Result<Word, ParseError> {
        let mut literal = false;
        let mut name = String::new();
        for part in &delim.0 {
            match part {
                Part::Lit(s) => name.push_str(s),
                Part::Quoted(s) => {
                    name.push_str(s);
                    literal = true;
                }
                _ => return Err(self.syntax("a heredoc delimiter can't be expanded")),
            }
        }
        let start = (self.pos..self.src.len()).find(|&i| self.src[i] == '\n').ok_or(ParseError::Incomplete)? + 1;
        let mut body = String::new();
        let mut i = start;
        loop {
            if i >= self.src.len() {
                return Err(ParseError::Incomplete);
            }
            let end = (i..self.src.len()).find(|&j| self.src[j] == '\n').unwrap_or(self.src.len());
            let mut line: String = self.src[i..end].iter().collect();
            if strip_tabs {
                line = String::from(line.trim_start_matches('\t'));
            }
            i = (end + 1).min(self.src.len());
            if line == name {
                break;
            }
            body.push_str(&line);
            body.push('\n');
        }
        self.src.drain(start..i);
        if literal {
            return Ok(Word(vec![Part::Quoted(body)]));
        }
        let chars: Vec<char> = body.chars().collect();
        sub_text(&chars, Mode::Dquote)
    }

    fn simple(&mut self) -> Result<Node, ParseError> {
        let mut assigns = Vec::new();
        let mut words: Vec<Word> = Vec::new();
        let mut redirs = Vec::new();
        loop {
            if let Some(r) = self.redir()? {
                redirs.push(r);
                continue;
            }
            let Tok::Word(w) = self.peek()? else { break };
            let w = w.clone();
            self.next()?;
            if words.is_empty() {
                if let Some(a) = assignment(&w) {
                    assigns.push(a);
                    continue;
                }
            }
            words.push(w);
            // `name() { ...; }`
            if words.len() == 1 && assigns.is_empty() && redirs.is_empty() && self.peek_op("(")? {
                let name = words[0].literal().filter(|n| is_name(n)).map(String::from);
                let Some(name) = name else { return Err(self.syntax("syntax error near unexpected '('")) };
                self.next()?;
                match self.next()? {
                    Tok::Op(")") => {}
                    t => return Err(self.unexpected(&t)),
                }
                self.skip_newlines()?;
                return Ok(Node::FuncDef { name, body: Box::new(self.command()?) });
            }
        }
        if assigns.is_empty() && words.is_empty() && redirs.is_empty() {
            let t = self.next()?;
            return Err(self.unexpected(&t));
        }
        Ok(Node::Simple { assigns, words, redirs })
    }

    fn if_clause(&mut self) -> Result<Node, ParseError> {
        self.next()?;
        let mut branches = Vec::new();
        let mut other = None;
        loop {
            let cond = self.list(&["then"])?;
            self.expect_kw("then")?;
            let body = self.list(&["elif", "else", "fi"])?;
            branches.push((cond, body));
            if self.peek_kw("elif")? {
                self.next()?;
                continue;
            }
            if self.peek_kw("else")? {
                self.next()?;
                other = Some(Box::new(self.list(&["fi"])?));
            }
            self.expect_kw("fi")?;
            return Ok(Node::If { branches, other });
        }
    }

    fn loop_clause(&mut self, until: bool) -> Result<Node, ParseError> {
        self.next()?;
        let cond = self.list(&["do"])?;
        self.expect_kw("do")?;
        let body = self.list(&["done"])?;
        self.expect_kw("done")?;
        Ok(Node::Loop { cond: Box::new(cond), body: Box::new(body), until })
    }

    fn for_clause(&mut self) -> Result<Node, ParseError> {
        self.next()?;
        let var = match self.next()? {
            Tok::Word(w) if w.literal().is_some_and(is_name) => String::from(w.literal().unwrap()),
            t => return Err(self.unexpected(&t)),
        };
        self.skip_newlines()?;
        let mut items = None;
        if self.peek_kw("in")? {
            self.next()?;
            let mut list = Vec::new();
            while let Tok::Word(w) = self.peek()? {
                let w = w.clone();
                self.next()?;
                list.push(w);
            }
            items = Some(list);
        }
        if self.peek_op(";")? {
            self.next()?;
        }
        self.skip_newlines()?;
        self.expect_kw("do")?;
        let body = self.list(&["done"])?;
        self.expect_kw("done")?;
        Ok(Node::For { var, items, body: Box::new(body) })
    }

    fn case_clause(&mut self) -> Result<Node, ParseError> {
        self.next()?;
        let word = match self.next()? {
            Tok::Word(w) => w,
            t => return Err(self.unexpected(&t)),
        };
        self.skip_newlines()?;
        self.expect_kw("in")?;
        let mut arms = Vec::new();
        loop {
            self.skip_newlines()?;
            if self.peek_kw("esac")? {
                self.next()?;
                return Ok(Node::Case { word, arms });
            }
            if self.peek_op("(")? {
                self.next()?;
            }
            let mut patterns = Vec::new();
            loop {
                match self.next()? {
                    Tok::Word(w) => patterns.push(w),
                    t => return Err(self.unexpected(&t)),
                }
                match self.next()? {
                    Tok::Op("|") => continue,
                    Tok::Op(")") => break,
                    t => return Err(self.unexpected(&t)),
                }
            }
            let body = self.list(&["esac"])?;
            arms.push((patterns, body));
            if self.peek_op(";;")? {
                self.next()?;
            } else if !self.peek_kw("esac")? {
                let t = self.next()?;
                return Err(self.unexpected(&t));
            }
        }
    }
}

/// `NAME=value` at the start of a simple command.
fn assignment(w: &Word) -> Option<(String, Word)> {
    let Some(Part::Lit(first)) = w.0.first() else { return None };
    let (name, rest) = first.split_once('=')?;
    if !is_name(name) {
        return None;
    }
    let mut value = Word::default();
    if !rest.is_empty() {
        value.0.push(Part::Lit(String::from(rest)));
    }
    value.0.extend(w.0[1..].iter().cloned());
    Some((String::from(name), value))
}

/// A `$(...)` or backquoted command's body.
fn sub_parse(body: &str) -> Result<Node, ParseError> {
    let mut p = Parser { src: body.chars().collect(), pos: 0, peeked: None };
    let node = p.list(&[])?;
    match p.next()? {
        Tok::Eof => Ok(node),
        t => Err(p.unexpected(&t)),
    }
}

/// Text read in `mode` on its own (an operand, a heredoc, arithmetic).
fn sub_text(chars: &[char], mode: Mode) -> Result<Word, ParseError> {
    let mut p = Parser { src: chars.to_vec(), pos: 0, peeked: None };
    let mut w = Word::default();
    // In Dquote mode a lone `"` would stop the reader; here it's text.
    loop {
        w.0.extend(p.text(mode)?.0);
        match p.cur() {
            Some(c) => {
                w.push_char(c, mode == Mode::Dquote);
                p.pos += 1;
            }
            None => return Ok(w),
        }
    }
}

/// The inside of `${...}`.
fn braced(inner: &[char], quoted: bool) -> Option<Part> {
    let s: String = inner.iter().collect();
    if let Some(name) = s.strip_prefix('#').filter(|n| is_name(n) || n.len() == 1 && "@*#?".contains(*n)) {
        return Some(Part::Param { name: String::from(name), op: Op::Length, quoted });
    }
    let name_len = if s.starts_with(|c: char| c.is_ascii_digit()) {
        s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len())
    } else if s.starts_with(|c: char| "?$#@*!-".contains(c)) {
        1
    } else {
        s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(s.len())
    };
    if name_len == 0 {
        return None;
    }
    let name = String::from(&s[..name_len]);
    let rest = &inner[name_len..];
    if rest.is_empty() {
        return Some(Part::Param { name, op: Op::Plain, quoted });
    }
    let (colon, rest) = if rest[0] == ':' { (true, &rest[1..]) } else { (false, rest) };
    let (&kind, operand) = rest.split_first()?;
    let (op_chars, operand) = match (kind, operand.first().copied()) {
        ('#', Some('#')) | ('%', Some('%')) if !colon => (2, &operand[1..]),
        _ => (1, operand),
    };
    let word = sub_text(operand, Mode::Operand).ok()?;
    let op = match (kind, colon, op_chars) {
        ('-', c, 1) => Op::Default(c, word),
        ('=', c, 1) => Op::Assign(c, word),
        ('+', c, 1) => Op::Alt(c, word),
        ('?', c, 1) => Op::Error(c, word),
        ('#', false, n) => Op::TrimPrefix(n == 2, word),
        ('%', false, n) => Op::TrimSuffix(n == 2, word),
        _ => return None,
    };
    Some(Part::Param { name, op, quoted })
}
