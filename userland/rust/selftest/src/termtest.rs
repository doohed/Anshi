//! Test payload: proves TCGETS/TCSETS reach the real per-task termios
//! (kernel/src/exec/syscall.rs's sys_ioctl()) and that clearing
//! ICANON/ECHO genuinely disables the kernel's line-buffering and echo --
//! reads a few raw keystrokes one at a time (no Enter needed, nothing
//! echoed but this program's own explicit print), then restores the
//! original settings before exiting. Needs REAL interactively-typed
//! keystrokes to prove anything; a scripted `\r`-terminated line doesn't
//! exercise the raw-mode path at all.

use core::ffi::c_int;

use anshi::sys::{self, Termios};
use anshi::println;

use crate::Argv;

#[unsafe(no_mangle)]
pub extern "C" fn termtest_main(_argc: c_int, _argv: Argv) -> c_int {
    let mut orig = Termios { c_iflag: 0, c_oflag: 0, c_cflag: 0, c_lflag: 0, c_line: 0, c_cc: [0; 19] };
    if unsafe { sys::ioctl(0, sys::TCGETS, (&raw mut orig).cast()) } != 0 {
        println!("tcgetattr failed");
        return 1;
    }
    println!("default c_lflag: {:x} (ICANON|ECHO expected)", orig.c_lflag);

    let mut raw = orig;
    raw.c_lflag &= !(sys::ICANON | sys::ECHO);
    raw.c_cc[sys::VMIN] = 1;
    raw.c_cc[sys::VTIME] = 0;
    if unsafe { sys::ioctl(0, sys::TCSETS, (&raw mut raw).cast()) } != 0 {
        println!("tcsetattr(raw) failed");
        return 1;
    }

    println!("raw mode on -- type 3 keys:");
    for _ in 0..3 {
        let mut c = 0u8;
        if unsafe { sys::read(0, &mut c, 1) } != 1 {
            println!("raw read failed");
            break;
        }
        println!("got byte: {c:x} '{}'", c as char);
    }

    if unsafe { sys::ioctl(0, sys::TCSETS, (&raw mut orig).cast()) } != 0 {
        println!("tcsetattr(restore) failed");
        return 1;
    }
    println!("termtest done");
    0
}
