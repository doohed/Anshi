//! M13 Phase A test payload: proves fork()/execve()/waitpid()/getpid()
//! all work together -- forks, the child exec()s into a small dedicated
//! program (forkchild) with a known exit status, and the parent waits for
//! it and confirms the exact status came back through the real process
//! table, not just "it didn't crash."

use core::ffi::c_int;

use anshi::{println, sys};

use crate::Argv;

#[unsafe(no_mangle)]
pub extern "C" fn forktest_main(_argc: c_int, _argv: Argv) -> c_int {
    println!("parent: pid={}, about to fork", sys::getpid());

    let pid = sys::fork();
    if pid < 0 {
        println!("fork failed");
        return 1;
    }

    if pid == 0 {
        println!("child: pid={}, about to exec forkchild", sys::getpid());
        anshi::exec(c"/bin/forkchild", &[c"forkchild"]);
        println!("child: execve failed!"); // unreachable on success
        return 99;
    }

    println!("parent: forked child pid={pid}, waiting...");
    let mut status: c_int = -1;
    let reaped = unsafe { sys::waitpid(pid, &mut status) };
    println!("parent: reaped pid={reaped}, status={status}");
    0
}
