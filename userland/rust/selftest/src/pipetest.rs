//! M19 test payload: proves pipe()/use_as_stdio() work before anything
//! (sh, tile) depends on them.
//!
//! Four checks: a basic write-then-read round trip through one pipe; the
//! non-blocking contract pipes are built around (empty-but-open reads as
//! EAGAIN, not a block -- see kernel/src/exec/pipe.rs's own comment on
//! why blocking would deadlock this kernel's ring-3-only preemption;
//! closed reads as 0, real EOF -- Linux's O_NONBLOCK pipe); the pattern
//! tile needs -- fork() a child, make pipes its terminal with
//! use_as_stdio() *before* execve(), and read its captured output back
//! after it exits; and the one sh needs for `a | b` -- dup2() a pipe onto
//! the child's fd 1.

use core::ffi::c_int;

use anshi::{print, println, sys};

use crate::{Argv, text};

fn pipe() -> [c_int; 2] {
    let mut fds = [-1; 2];
    unsafe { sys::pipe(fds.as_mut_ptr()) };
    fds
}

/// Reads `fd` until EOF into `buf`: the byte count, or None if it ran
/// out of room or failed before EOF.
fn drain(fd: c_int, buf: &mut [u8]) -> Option<usize> {
    let mut total = 0;
    while total < buf.len() {
        match unsafe { sys::read(fd, buf[total..].as_mut_ptr(), buf.len() - total) } {
            0 => return Some(total),
            n if n > 0 => total += n as usize,
            _ => return None,
        }
    }
    None
}

#[unsafe(no_mangle)]
pub extern "C" fn pipetest_main(_argc: c_int, _argv: Argv) -> c_int {
    println!("pipetest: pid={}", sys::getpid());
    let mut buf = [0u8; 127];

    // --- 1: basic round trip ---
    let fds = pipe();
    if fds[0] < 0 {
        println!("pipe() failed");
        return 1;
    }
    let msg = b"hello through a pipe";
    let wn = unsafe { sys::write(fds[1], msg.as_ptr(), msg.len()) };
    if wn != msg.len() as i64 {
        println!("FAIL: pipe write returned {wn}, expected {}", msg.len());
        return 1;
    }
    let rn = unsafe { sys::read(fds[0], buf.as_mut_ptr(), buf.len()) };
    if rn < 0 {
        println!("FAIL: pipe read failed");
        return 1;
    }
    let got = &buf[..rn as usize];
    if got != msg {
        println!("FAIL: roundtrip mismatch, got '{}'", text(got));
        return 1;
    }
    println!("OK: basic pipe roundtrip ('{}')", text(got));
    sys::close(fds[0]);
    sys::close(fds[1]);

    // --- 2: non-blocking contract ---
    let fds2 = pipe();
    let n2 = unsafe { sys::read(fds2[0], buf.as_mut_ptr(), 1) };
    if n2 != sys::EAGAIN {
        println!("FAIL: expected EAGAIN (no data yet) from an empty open pipe, got {n2}");
        return 1;
    }
    println!("OK: empty-but-open pipe read returns EAGAIN, not a block");
    sys::close(fds2[1]); // Close the write end -- now it should read as EOF.
    let n3 = unsafe { sys::read(fds2[0], buf.as_mut_ptr(), 1) };
    if n3 != 0 {
        println!("FAIL: expected 0 (EOF) once the write end closed, got {n3}");
        return 1;
    }
    println!("OK: closed write end reads as 0 (EOF)");
    sys::close(fds2[0]);

    // --- 3: the real pattern -- redirect a child's stdio, exec it,
    // capture its output, confirm EOF shows up once it exits. ---
    let input = pipe();
    let output = pipe();
    let pid = sys::fork();
    if pid < 0 {
        println!("FAIL: fork failed");
        return 1;
    }
    if pid == 0 {
        sys::close(input[1]);
        sys::close(output[0]);
        sys::use_as_stdio(input[0], output[1]);
        anshi::exec(c"/bin/forkchild", &[c"forkchild"]);
        return 99; // unreachable on success
    }
    sys::close(input[0]);
    sys::close(output[1]);

    // Reaps the child -- process exit already closed its end of `output`
    // (process_close_stdio_pipes()), so draining below will see real EOF,
    // not just "quiet for now".
    let status = anshi::waitpid(pid);
    let Some(total) = drain(output[0], &mut buf) else {
        println!("FAIL: never saw EOF on the child's captured stdout");
        return 1;
    };
    print!("OK: captured from exec()'d child (status={status}): {}", text(&buf[..total]));
    sys::close(input[1]);
    sys::close(output[0]);

    // --- 4: dup2() a pipe onto a child's fd 1, as sh does for `a | b`. ---
    let out = pipe();
    let pid = sys::fork();
    if pid < 0 {
        println!("FAIL: fork failed");
        return 1;
    }
    if pid == 0 {
        if sys::dup2(out[1], 1) != 1 {
            return 98;
        }
        sys::close(out[0]);
        sys::close(out[1]);
        anshi::exec(c"/bin/forkchild", &[c"forkchild"]);
        return 99;
    }
    sys::close(out[1]);
    let status = anshi::waitpid(pid);
    let Some(total) = drain(out[0], &mut buf) else {
        println!("FAIL: never saw EOF on the dup2()'d child's stdout");
        return 1;
    };
    if total == 0 {
        println!("FAIL: nothing came through the dup2()'d stdout (status={status})");
        return 1;
    }
    print!("OK: captured through dup2() (status={status}): {}", text(&buf[..total]));
    sys::close(out[0]);

    println!("pipetest done");
    0
}
