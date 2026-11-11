//! M13 Phase B test payload: proves *preemptive* scheduling actually
//! works, not just fork()/exec()/wait(). Forks two children that each
//! spin in a tight busy loop -- no syscalls, no voluntary yield point at
//! all -- printing their own tag ('A'/'B') at intervals. With no
//! preemption, one child would always run to completion before the other
//! ever starts, so the output would read "AAAAAAAAAA" then "BBBBBBBBBB"
//! as two clean blocks. If the timer can genuinely interrupt a running
//! process mid-flight and hand the CPU to the other one, the tags
//! visibly interleave instead -- that's the actual, observable proof.

use core::ffi::c_int;
use core::hint::black_box;

use anshi::{print, println, sys};

use crate::Argv;

fn run_child(tag: &str) -> ! {
    for i in 0..8_000_000u64 {
        // black_box keeps the loop a real busy loop: without it the
        // compiler can step straight from one print to the next.
        if black_box(i) % 400_000 == 0 {
            print!("{tag}");
        }
    }
    sys::exit(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn preempttest_main(_argc: c_int, _argv: Argv) -> c_int {
    println!("preempttest: forking two busy-loop children (A and B)");

    let pid1 = sys::fork();
    if pid1 == 0 {
        run_child("A");
    }

    let pid2 = sys::fork();
    if pid2 == 0 {
        run_child("B");
    }

    anshi::waitpid(pid1);
    anshi::waitpid(pid2);

    println!("\npreempttest: both children finished");
    0
}
