//! Running the tree syntax.rs builds: variables and functions, control
//! flow, redirections, pipelines, subshells, and starting programs.
//!
//! Programs and pipeline stages run in fork()ed children whose stdin and
//! stdout are dup2()ed onto pipes or files first. A builtin with a
//! redirection runs here in the shell, its fds saved with dup() and put
//! back afterwards. Pipes never block in this kernel; everything that
//! waits for one (anshi::read_wait, write_all) yields until it can go on.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use anshi::CString;

use crate::commands;
use crate::config::Config;
use crate::history::History;
use crate::syntax::{self, Node, ParseError, Redir, RedirOp};
use crate::term;

/// How deep functions may call each other before it's taken as runaway
/// recursion: each level costs stack, and there are 128 KiB of it.
const MAX_CALL_DEPTH: usize = 48;
/// A pipe holds this much; a longer heredoc needs a writer process.
const PIPE_BUF: usize = 4096;

#[derive(Clone)]
pub struct Var {
    pub value: String,
    pub exported: bool,
}

/// Ways out of the middle of running something.
pub enum Flow {
    Break(usize),
    Continue(usize),
    Return(i32),
    Exit(i32),
    /// Stop this whole command line or script, with this status: an
    /// expansion error, a runaway recursion, Ctrl-C in a loop.
    Abort(i32),
}

pub type Res = Result<i32, Flow>;

/// How running some text ended.
pub enum Done {
    Status(i32),
    /// `exit`: the shell should end, with this status.
    Exit(i32),
}

pub struct Shell {
    pub vars: BTreeMap<String, Var>,
    pub funcs: BTreeMap<String, Node>,
    /// $1, $2, ...
    pub params: Vec<String>,
    /// $0
    pub arg0: String,
    /// $?
    pub status: i32,
    /// set -e: exit when a command fails (outside conditions).
    pub errexit: bool,
    /// set -x: print each command before running it.
    pub xtrace: bool,
    pub interactive: bool,
    /// Above 0 while running a condition (`if`, `while`, `&&`, `!`),
    /// where a failure is an answer, not an error for set -e.
    pub in_condition: usize,
    pub loop_depth: usize,
    /// One frame per function call: what `local` replaced, to put back.
    pub locals: Vec<Vec<(String, Option<Var>)>>,
    /// The status of the last command substitution, for a command that is
    /// only assignments.
    pub subst_status: Option<i32>,
    /// Heredoc writer processes still to be waited for.
    pub writers: Vec<i32>,
    pub history: History,
    pub config: Config,
}

impl Shell {
    pub fn new(arg0: &str, interactive: bool) -> Shell {
        let mut sh = Shell {
            vars: BTreeMap::new(),
            funcs: BTreeMap::new(),
            params: Vec::new(),
            arg0: String::from(arg0),
            status: 0,
            errexit: false,
            xtrace: false,
            interactive,
            in_condition: 0,
            loop_depth: 0,
            locals: Vec::new(),
            subst_status: None,
            writers: Vec::new(),
            history: History::empty(),
            config: Config::default(),
        };
        for (name, value) in anshi::env() {
            sh.vars.insert(name, Var { value, exported: true });
        }
        for (name, default) in [("HOME", "/"), ("PATH", "/bin")] {
            if !sh.vars.contains_key(name) {
                sh.vars.insert(String::from(name), Var { value: String::from(default), exported: true });
            }
        }
        let pwd = anshi::getcwd().unwrap_or_else(|| String::from("/"));
        sh.set_var("PWD", &pwd);
        sh.export("PWD");
        sh
    }

    // ---------- variables ----------

    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(|v| v.value.as_str())
    }

    pub fn set_var(&mut self, name: &str, value: &str) {
        match self.vars.get_mut(name) {
            Some(v) => v.value = String::from(value),
            None => {
                self.vars.insert(String::from(name), Var { value: String::from(value), exported: false });
            }
        }
    }

    pub fn export(&mut self, name: &str) {
        self.vars.entry(String::from(name)).or_insert(Var { value: String::new(), exported: true }).exported = true;
    }

    pub fn unset(&mut self, name: &str) {
        self.vars.remove(name);
    }

    /// `local name`: the current value is put back when the function returns.
    pub fn make_local(&mut self, name: &str) -> bool {
        let old = self.vars.get(name).cloned();
        let Some(frame) = self.locals.last_mut() else { return false };
        if !frame.iter().any(|(n, _)| n == name) {
            frame.push((String::from(name), old));
        }
        true
    }

    /// The environment a program gets: every exported variable, with
    /// `extra` (a command's `NAME=value` prefixes) on top.
    fn environment(&self, extra: &[(String, String)]) -> Vec<CString> {
        let mut env: BTreeMap<&str, &str> =
            self.vars.iter().filter(|(_, v)| v.exported).map(|(n, v)| (n.as_str(), v.value.as_str())).collect();
        for (n, v) in extra {
            env.insert(n, v);
        }
        env.iter().filter_map(|(n, v)| CString::new(format!("{n}={v}")).ok()).collect()
    }

    // ---------- running text ----------

    /// Parses and runs a command line or script. A syntax error is
    /// reported (as `sh: <where>: line N: ...`) and nothing runs.
    pub fn run_text(&mut self, src: &str, origin: &str) -> Done {
        match syntax::parse(src) {
            Ok(node) => self.run_top(&node),
            Err(ParseError::Incomplete) => {
                term::error(&format!("{origin}: syntax error: unexpected end of file"));
                self.status = 2;
                Done::Status(2)
            }
            Err(ParseError::Syntax(msg, line)) => {
                term::error(&format!("{origin}: line {line}: {msg}"));
                self.status = 2;
                Done::Status(2)
            }
        }
    }

    pub fn run_top(&mut self, node: &Node) -> Done {
        let status = match self.run(node) {
            Ok(s) | Err(Flow::Return(s) | Flow::Abort(s)) => s,
            Err(Flow::Break(_) | Flow::Continue(_)) => self.status,
            Err(Flow::Exit(s)) => {
                self.status = s;
                return Done::Exit(s);
            }
        };
        self.status = status;
        Done::Status(status)
    }

    /// Runs a node in a fork()ed child, which then exits with its status.
    fn child_exit(&mut self, node: &Node) -> ! {
        self.interactive = false;
        let r = match node {
            Node::Simple { .. } => self.run_simple(node, true),
            _ => self.run(node),
        };
        let status = match r {
            Ok(s) | Err(Flow::Exit(s) | Flow::Return(s) | Flow::Abort(s)) => s,
            Err(_) => self.status,
        };
        anshi::exit(status)
    }

    /// Runs a condition: set -e doesn't apply to it.
    fn condition(&mut self, node: &Node) -> Res {
        self.in_condition += 1;
        let r = self.run(node);
        self.in_condition -= 1;
        r
    }

    /// set -e: a failed command ends the shell, unless it was a condition.
    fn check_errexit(&self, status: i32) -> Res {
        if status != 0 && self.errexit && self.in_condition == 0 {
            return Err(Flow::Exit(status));
        }
        Ok(status)
    }

    pub fn run(&mut self, node: &Node) -> Res {
        let status = match node {
            Node::Simple { .. } => {
                let s = self.run_simple(node, false)?;
                self.status = s;
                return self.check_errexit(s);
            }
            Node::Pipeline { cmds, bang } => {
                if *bang {
                    self.in_condition += 1;
                }
                let r = if cmds.len() == 1 { self.run(&cmds[0]) } else { self.run_pipeline(cmds) };
                if *bang {
                    self.in_condition -= 1;
                }
                let s = r?;
                let s = if *bang { (s == 0) as i32 } else { s };
                self.status = s;
                return if *bang { Ok(s) } else { self.check_errexit(s) };
            }
            Node::AndOr { first, rest } => {
                let mut s = self.condition(first)?;
                for (i, (and, next)) in rest.iter().enumerate() {
                    self.status = s;
                    if (*and && s != 0) || (!*and && s == 0) {
                        continue;
                    }
                    s = if i + 1 == rest.len() { self.run(next)? } else { self.condition(next)? };
                }
                s
            }
            Node::List(items) => {
                let mut s = self.status;
                for item in items {
                    s = self.run(item)?;
                }
                s
            }
            Node::If { branches, other } => {
                let mut s = 0;
                let mut taken = false;
                for (cond, body) in branches {
                    if self.condition(cond)? == 0 {
                        s = self.run(body)?;
                        taken = true;
                        break;
                    }
                }
                if let Some(o) = other.as_ref().filter(|_| !taken) {
                    s = self.run(o)?;
                }
                s
            }
            Node::Loop { cond, body, until } => {
                self.loop_depth += 1;
                let r = self.run_loop(cond, body, *until);
                self.loop_depth -= 1;
                r?
            }
            Node::For { var, items, body } => {
                let list = match items {
                    Some(words) => self.expand_words(words)?,
                    None => self.params.clone(),
                };
                self.loop_depth += 1;
                let r = self.run_for(var, &list, body);
                self.loop_depth -= 1;
                r?
            }
            Node::Case { word, arms } => {
                let value = self.expand_one(word)?;
                let mut s = 0;
                'arms: for (patterns, body) in arms {
                    for p in patterns {
                        let pat = self.expand_pattern(p)?;
                        if crate::expand::fnmatch(&pat, &value) {
                            s = self.run(body)?;
                            break 'arms;
                        }
                    }
                }
                s
            }
            Node::Group(body) => self.run(body)?,
            Node::Subshell(body) => {
                let s = self.subshell(body);
                self.status = s;
                return self.check_errexit(s);
            }
            Node::FuncDef { name, body } => {
                self.funcs.insert(name.clone(), (**body).clone());
                0
            }
            Node::Redirected { node, redirs } => {
                let Some(saved) = self.apply_redirs(redirs, true) else { return Ok(1) };
                let r = self.run(node);
                self.restore(saved);
                r?
            }
        };
        self.status = status;
        Ok(status)
    }

    /// Ctrl-C stops a loop that has been going a while. Only checked
    /// after the first second, so a quick loop doesn't eat keystrokes
    /// meant for whatever runs after it.
    fn interrupted(&self, started: u64) -> bool {
        anshi::now_ms() - started > 1000 && matches!(anshi::poll_key(), anshi::KeyPoll::Key(3) | anshi::KeyPoll::Closed)
    }

    /// What a loop does with a break/continue from its body: Some(result)
    /// to stop looping.
    fn loop_flow(r: Res, status: &mut i32) -> Option<Res> {
        match r {
            Ok(s) => {
                *status = s;
                None
            }
            Err(Flow::Break(n)) if n > 1 => Some(Err(Flow::Break(n - 1))),
            Err(Flow::Break(_)) => Some(Ok(*status)),
            Err(Flow::Continue(n)) if n > 1 => Some(Err(Flow::Continue(n - 1))),
            Err(Flow::Continue(_)) => None,
            Err(e) => Some(Err(e)),
        }
    }

    fn run_loop(&mut self, cond: &Node, body: &Node, until: bool) -> Res {
        let started = anshi::now_ms();
        let mut status = 0;
        loop {
            if self.interrupted(started) {
                anshi::eprintln!("^C");
                return Err(Flow::Abort(130));
            }
            let c = match self.condition(cond) {
                Ok(c) => c,
                Err(e) => match Self::loop_flow(Err(e), &mut status) {
                    Some(r) => return r,
                    None => continue,
                },
            };
            if (c == 0) == until {
                return Ok(status);
            }
            if let Some(r) = Self::loop_flow(self.run(body), &mut status) {
                return r;
            }
        }
    }

    fn run_for(&mut self, var: &str, list: &[String], body: &Node) -> Res {
        let started = anshi::now_ms();
        let mut status = 0;
        for item in list {
            if self.interrupted(started) {
                anshi::eprintln!("^C");
                return Err(Flow::Abort(130));
            }
            self.set_var(var, item);
            if let Some(r) = Self::loop_flow(self.run(body), &mut status) {
                return r;
            }
        }
        Ok(status)
    }

    // ---------- commands ----------

    /// A simple command. `exec_direct`: this is a child that exists only
    /// to run it, so a program replaces it instead of forking again.
    fn run_simple(&mut self, node: &Node, exec_direct: bool) -> Res {
        let Node::Simple { assigns, words, redirs } = node else { return self.run(node) };
        self.subst_status = None;
        let argv = self.expand_words(words)?;
        let mut env = Vec::new();
        for (name, value) in assigns {
            env.push((name.clone(), self.expand_one(value)?));
        }
        if argv.is_empty() {
            for (name, value) in &env {
                self.set_var(name, value);
            }
            if !redirs.is_empty() {
                let Some(saved) = self.apply_redirs(redirs, true) else { return Ok(1) };
                self.restore(saved);
            }
            return Ok(self.subst_status.unwrap_or(0));
        }
        if self.xtrace {
            let mut line = String::from("+");
            for (n, v) in &env {
                line.push_str(&format!(" {n}={v}"));
            }
            for a in &argv {
                line.push(' ');
                line.push_str(a);
            }
            anshi::eprintln!("{line}");
        }

        let name = argv[0].as_str();
        if let Some(body) = self.funcs.get(name).cloned() {
            for (n, v) in &env {
                self.set_var(n, v);
            }
            let Some(saved) = self.apply_redirs(redirs, true) else { return Ok(1) };
            let r = self.call(&body, argv[1..].to_vec());
            self.restore(saved);
            return r;
        }
        if commands::is_builtin(name) {
            for (n, v) in &env {
                self.set_var(n, v);
            }
            let Some(saved) = self.apply_redirs(redirs, true) else { return Ok(1) };
            let r = commands::run(self, &argv);
            self.restore(saved);
            self.reap_writers();
            return r;
        }

        let Some(path) = commands::resolve_program(name) else {
            term::error(&format!("{name}: command not found"));
            return Ok(127);
        };
        if exec_direct {
            if self.apply_redirs(redirs, false).is_none() {
                return Ok(1);
            }
            self.exec(&path, &argv, &env);
            return Ok(126);
        }
        match anshi::fork() {
            None => {
                term::error("fork failed: too many processes");
                Ok(1)
            }
            Some(0) => {
                self.interactive = false;
                if self.apply_redirs(redirs, false).is_none() {
                    anshi::exit(1);
                }
                self.exec(&path, &argv, &env);
                anshi::exit(126)
            }
            Some(pid) => {
                let status = anshi::waitpid(pid);
                self.reap_writers();
                Ok(status)
            }
        }
    }

    /// Replaces this process with the program at `path`; returns only if
    /// that failed. A script (a file starting with `#!`) runs in a new sh.
    pub fn exec(&mut self, path: &str, argv: &[String], extra_env: &[(String, String)]) {
        let env = self.environment(extra_env);
        let env_refs: Vec<&core::ffi::CStr> = env.iter().map(|c| c.as_c_str()).collect();
        let script = !commands::is_elf(path);
        let mut args: Vec<CString> = Vec::new();
        if script {
            args.push(CString::new("sh").unwrap());
            args.extend(CString::new(path).ok());
            args.extend(argv[1..].iter().filter_map(|a| CString::new(a.as_str()).ok()));
        } else {
            args.extend(argv.iter().filter_map(|a| CString::new(a.as_str()).ok()));
        }
        let arg_refs: Vec<&core::ffi::CStr> = args.iter().map(|c| c.as_c_str()).collect();
        let program = if script { String::from("/bin/sh") } else { String::from(path) };
        if let Ok(p) = CString::new(program) {
            anshi::exec_env(&p, &arg_refs, &env_refs);
        }
        term::error(&format!("{}: cannot run", argv[0]));
    }

    /// Calls a function with `args` as $1, $2, ...
    pub fn call(&mut self, body: &Node, args: Vec<String>) -> Res {
        if self.locals.len() >= MAX_CALL_DEPTH {
            term::error("functions nested too deeply (runaway recursion?)");
            return Err(Flow::Abort(1));
        }
        let saved = core::mem::replace(&mut self.params, args);
        self.locals.push(Vec::new());
        let depth = self.loop_depth;
        self.loop_depth = 0;
        let r = self.run(body);
        self.loop_depth = depth;
        for (name, old) in self.locals.pop().unwrap_or_default().into_iter().rev() {
            match old {
                Some(v) => {
                    self.vars.insert(name, v);
                }
                None => {
                    self.vars.remove(&name);
                }
            }
        }
        self.params = saved;
        match r {
            Err(Flow::Return(s)) => {
                self.status = s;
                Ok(s)
            }
            other => other,
        }
    }

    /// Runs `body` in a fork()ed copy of the shell: what it changes
    /// (variables, the directory) stays there.
    fn subshell(&mut self, body: &Node) -> i32 {
        match anshi::fork() {
            None => {
                term::error("fork failed: too many processes");
                1
            }
            Some(0) => self.child_exit(body),
            Some(pid) => anshi::waitpid(pid),
        }
    }

    fn run_pipeline(&mut self, cmds: &[Node]) -> Res {
        let mut prev: Option<i32> = None;
        let mut pids = Vec::new();
        for (i, cmd) in cmds.iter().enumerate() {
            let pipe = if i + 1 < cmds.len() {
                match anshi::pipe() {
                    Some(p) => Some(p),
                    None => {
                        term::error("pipe failed: too many open files");
                        break;
                    }
                }
            } else {
                None
            };
            match anshi::fork() {
                None => {
                    term::error("fork failed: too many processes");
                    if let Some((r, w)) = pipe {
                        anshi::close(r);
                        anshi::close(w);
                    }
                    break;
                }
                Some(0) => {
                    if let Some(r) = prev {
                        anshi::dup2(r, 0);
                        anshi::close(r);
                    }
                    if let Some((r, w)) = pipe {
                        anshi::close(r);
                        anshi::dup2(w, 1);
                        anshi::close(w);
                    }
                    self.child_exit(cmd);
                }
                Some(pid) => pids.push(pid),
            }
            if let Some(r) = prev.take() {
                anshi::close(r);
            }
            prev = pipe.map(|(r, w)| {
                anshi::close(w);
                r
            });
        }
        if let Some(r) = prev {
            anshi::close(r);
        }
        let mut status = 1;
        for pid in pids {
            status = anshi::waitpid(pid);
        }
        Ok(status)
    }

    // ---------- redirections ----------

    /// Points fds at files and pipes as `redirs` say. With `save`, the
    /// original fds are kept (dup()ed) for restore(). None, after
    /// reporting why, if one failed -- anything already done is undone.
    pub fn apply_redirs(&mut self, redirs: &[Redir], save: bool) -> Option<Vec<(i32, Option<i32>)>> {
        let mut saved: Vec<(i32, Option<i32>)> = Vec::new();
        for r in redirs {
            let targets: &[i32] = if r.op == RedirOp::Both { &[1, 2] } else { core::slice::from_ref(&r.fd) };
            if save {
                for &fd in targets {
                    if !saved.iter().any(|(f, _)| *f == fd) {
                        saved.push((fd, anshi::dup(fd)));
                    }
                }
            }
            match self.redirect(r) {
                Ok(()) => {}
                Err(msg) => {
                    term::error(&msg);
                    self.restore(saved);
                    return None;
                }
            }
        }
        Some(saved)
    }

    fn redirect(&mut self, r: &Redir) -> Result<(), String> {
        let source = match r.op {
            RedirOp::Heredoc => {
                let body = self.expand_one(&r.target).map_err(|_| String::from("heredoc: expansion failed"))?;
                self.heredoc_fd(&body)?
            }
            RedirOp::Dup => {
                let to = self.expand_one(&r.target).map_err(|_| String::from("bad redirection"))?;
                if to == "-" {
                    anshi::close(r.fd);
                    return Ok(());
                }
                let from: i32 = to.parse().map_err(|_| format!("{to}: bad file descriptor"))?;
                if !anshi::dup2(from, r.fd) {
                    return Err(format!("{from}: bad file descriptor"));
                }
                return Ok(());
            }
            op => {
                let name = self.expand_one(&r.target).map_err(|_| String::from("bad redirection"))?;
                let path = CString::new(name.as_str()).map_err(|_| format!("{name}: bad file name"))?;
                let file = match op {
                    RedirOp::In => anshi::File::open(&path).filter(|_| !anshi::is_dir(&name)),
                    RedirOp::Append => anshi::File::append(&path),
                    _ => anshi::File::create(&path),
                };
                let Some(file) = file else {
                    return Err(match op {
                        RedirOp::In if anshi::is_dir(&name) => format!("{name}: is a directory"),
                        RedirOp::In => format!("{name}: no such file"),
                        _ => format!("{name}: cannot create (no such directory, or it is one)"),
                    });
                };
                let fd = file.fd();
                if op == RedirOp::Both {
                    anshi::dup2(fd, 1);
                    anshi::dup2(fd, 2);
                } else {
                    anshi::dup2(fd, r.fd);
                }
                return Ok(());
            }
        };
        anshi::dup2(source, r.fd);
        anshi::close(source);
        Ok(())
    }

    /// A pipe fd that reads back `body`: written straight in when it fits,
    /// else by a writer process (waited for after the command).
    fn heredoc_fd(&mut self, body: &str) -> Result<i32, String> {
        let (r, w) = anshi::pipe().ok_or_else(|| String::from("heredoc: too many open files"))?;
        if body.len() < PIPE_BUF {
            anshi::write_all(w, body.as_bytes());
            anshi::close(w);
            return Ok(r);
        }
        match anshi::fork() {
            None => {
                anshi::close(r);
                anshi::close(w);
                Err(String::from("heredoc: fork failed"))
            }
            Some(0) => {
                anshi::close(r);
                anshi::write_all(w, body.as_bytes());
                anshi::exit(0)
            }
            Some(pid) => {
                anshi::close(w);
                self.writers.push(pid);
                Ok(r)
            }
        }
    }

    fn reap_writers(&mut self) {
        for pid in core::mem::take(&mut self.writers) {
            anshi::waitpid(pid);
        }
    }

    pub fn restore(&mut self, saved: Vec<(i32, Option<i32>)>) {
        for (fd, copy) in saved.into_iter().rev() {
            match copy {
                Some(c) => {
                    anshi::dup2(c, fd);
                    anshi::close(c);
                }
                None => anshi::close(fd),
            }
        }
    }

    /// `$(...)`: runs `node` with its stdout into a pipe and returns what
    /// it printed, without the trailing newlines.
    pub fn capture(&mut self, node: &Node) -> String {
        let Some((r, w)) = anshi::pipe() else {
            term::error("command substitution: too many open files");
            return String::new();
        };
        match anshi::fork() {
            None => {
                anshi::close(r);
                anshi::close(w);
                term::error("fork failed: too many processes");
                String::new()
            }
            Some(0) => {
                anshi::close(r);
                anshi::dup2(w, 1);
                anshi::close(w);
                self.child_exit(node)
            }
            Some(pid) => {
                anshi::close(w);
                let out = anshi::read_to_end(r);
                anshi::close(r);
                let status = anshi::waitpid(pid);
                self.status = status;
                self.subst_status = Some(status);
                let mut text = String::from_utf8_lossy(&out).into_owned();
                while text.ends_with('\n') {
                    text.pop();
                }
                text
            }
        }
    }

    /// `source file`: runs a script in this shell.
    pub fn source(&mut self, path: &str, args: Option<Vec<String>>) -> Res {
        let Some(text) = commands::read_text(path) else {
            term::error(&format!("{path}: no such file"));
            return Ok(1);
        };
        let node = match syntax::parse(&text) {
            Ok(n) => n,
            Err(ParseError::Incomplete) => {
                term::error(&format!("{path}: syntax error: unexpected end of file"));
                return Ok(2);
            }
            Err(ParseError::Syntax(msg, line)) => {
                term::error(&format!("{path}: line {line}: {msg}"));
                return Ok(2);
            }
        };
        let saved = args.map(|a| core::mem::replace(&mut self.params, a));
        let r = self.run(&node);
        if let Some(p) = saved {
            self.params = p;
        }
        match r {
            Err(Flow::Return(s)) => Ok(s),
            other => other,
        }
    }

    /// `eval`: parses and runs text here.
    pub fn eval(&mut self, text: &str) -> Res {
        match syntax::parse(text) {
            Ok(node) => self.run(&node),
            Err(ParseError::Incomplete) => {
                term::error("eval: syntax error: unexpected end of file");
                Ok(2)
            }
            Err(ParseError::Syntax(msg, _)) => {
                term::error(&format!("eval: {msg}"));
                Ok(2)
            }
        }
    }

    /// A variable or special parameter's value: $?, $$, $#, $0, $1..., $@.
    pub fn param(&self, name: &str) -> Option<String> {
        match name {
            "?" => Some(format!("{}", self.status)),
            "$" => Some(format!("{}", anshi::getpid())),
            "#" => Some(format!("{}", self.params.len())),
            "0" => Some(self.arg0.clone()),
            "@" | "*" => Some(self.params.join(" ")),
            "-" => Some(format!("{}{}", if self.errexit { "e" } else { "" }, if self.xtrace { "x" } else { "" })),
            "!" => None,
            n if n.chars().all(|c| c.is_ascii_digit()) => {
                n.parse::<usize>().ok().and_then(|i| self.params.get(i - 1).cloned())
            }
            n => self.var(n).map(String::from),
        }
    }
}
