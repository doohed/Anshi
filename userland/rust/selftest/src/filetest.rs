//! M11 test payload: proves open()/read()/write()/lseek()/close() reach
//! the real VFS, not a copy -- reads a known fixture file (written by
//! kmain's self-test wiring at boot, kernel/src/main.rs), prints it, then
//! modifies it in place and appends to it so a follow-up `cat` from the
//! shell can confirm the change actually landed.

use core::ffi::c_int;

use anshi::{println, sys};

use crate::{Argv, text};

#[unsafe(no_mangle)]
pub extern "C" fn filetest_main(_argc: c_int, _argv: Argv) -> c_int {
    let path = c"/filetest.txt";

    let fd = unsafe { sys::open(path.as_ptr(), sys::O_RDONLY) };
    if fd < 0 {
        println!("open(/filetest.txt, O_RDONLY) failed");
        return 1;
    }
    let mut buf = [0u8; 127];
    let n = unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) };
    if n < 0 {
        println!("read failed");
        return 1;
    }
    println!("read {n} bytes: {}", text(&buf[..n as usize]));
    sys::close(fd);

    let fd = unsafe { sys::open(path.as_ptr(), sys::O_WRONLY) };
    if fd < 0 {
        println!("open(/filetest.txt, O_WRONLY) failed");
        return 1;
    }
    let patch = b"PATCHED";
    let w = unsafe { sys::write(fd, patch.as_ptr(), patch.len()) };
    println!("wrote {w} bytes at offset 0");

    sys::lseek(fd, 0, sys::SEEK_END);
    let tail = b" [appended]";
    unsafe { sys::write(fd, tail.as_ptr(), tail.len()) };
    sys::close(fd);

    println!("filetest done");
    0
}
