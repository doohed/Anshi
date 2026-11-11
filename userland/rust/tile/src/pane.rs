//! A pane: one child process wired to tile through two pipes, plus the
//! virtual terminal its output is drawn into.

use core::ffi::{CStr, c_int};

use crate::vt::Vt;

pub struct Pane {
    pid: i32,
    /// tile's ends: keystrokes go into `stdin`, output comes out of `stdout`.
    stdin: c_int,
    stdout: c_int,
    pub vt: Vt,
    alive: bool,
}

impl Pane {
    /// Starts `path` with fresh pipes as its stdin/stdout and its window
    /// size set to `cols`x`rows` -- what TIOCGWINSZ will report to it and
    /// to everything it runs (kernel/src/drivers/tty.rs's TIOCSWINSZ), so
    /// a full-screen program lays itself out inside the pane. `others`
    /// are earlier panes' fds: fork() copies every open fd and there's
    /// no close-on-exec, so the child closes them itself -- a copy left
    /// open keeps that pane's pipe alive after its own process exits,
    /// which makes quitting hang. Returns None if the pipes or fork fail.
    pub fn spawn(path: &CStr, cols: u16, rows: u16, others: &[c_int]) -> Option<Pane> {
        let (in_r, in_w) = anshi::pipe()?;
        let (out_r, out_w) = anshi::pipe()?;
        match anshi::fork()? {
            0 => {
                anshi::close(in_w);
                anshi::close(out_r);
                for &fd in others {
                    anshi::close(fd);
                }
                anshi::use_as_stdio(in_r, out_w);
                anshi::set_window_size(cols, rows);
                anshi::exec(path, &[path]);
                anshi::exit(127) // exec failed
            }
            pid => {
                anshi::close(in_r);
                anshi::close(out_w);
                Some(Pane { pid, stdin: in_w, stdout: out_r, vt: Vt::new(cols, rows), alive: true })
            }
        }
    }

    pub fn fds(&self) -> [c_int; 2] {
        [self.stdin, self.stdout]
    }

    pub fn alive(&self) -> bool {
        self.alive
    }

    pub fn send(&self, key: u8) {
        if self.alive() {
            anshi::write_all(self.stdin, &[key]);
        }
    }

    /// Feeds everything the process has written so far into the VT.
    /// Returns whether anything changed (output, or the process exiting).
    pub fn pump(&mut self) -> bool {
        if !self.alive() {
            return false;
        }
        let mut changed = false;
        let mut buf = [0u8; 1024];
        loop {
            match anshi::read_fd(self.stdout, &mut buf) {
                Some(0) => return changed,
                Some(n) => {
                    self.vt.feed(&buf[..n]);
                    changed = true;
                }
                None => {
                    // Write end closed and drained: the process (and
                    // anything it ran) is gone. Reap it. The status isn't
                    // kept: with no init process, the kernel's top-level
                    // scheduler loop may already have reaped the zombie
                    // (exec/process.h), in which case waitpid() can't
                    // report it -- better no exit code than a wrong one.
                    anshi::waitpid(self.pid);
                    self.alive = false;
                    anshi::close(self.stdout);
                    anshi::close(self.stdin);
                    return true;
                }
            }
        }
    }

    /// Closes the pane's stdin: its shell sees end-of-input and exits,
    /// and so does a program running in it (scarf/play quit on a closed
    /// stdin). The exit itself is picked up by pump().
    pub fn hang_up(&mut self) {
        if self.alive() {
            anshi::close(self.stdin);
            self.stdin = -1;
        }
    }
}
