//! The program forktest (and pipetest) exec()s its forked child into -- a
//! small, dedicated target so the parent can verify an exact, known exit
//! status came back through waitpid().

use core::ffi::c_int;

use anshi::{println, sys};

use crate::Argv;

#[unsafe(no_mangle)]
pub extern "C" fn forkchild_main(_argc: c_int, _argv: Argv) -> c_int {
    println!("forkchild: pid={}, hello from exec()'d child!", sys::getpid());
    7
}
