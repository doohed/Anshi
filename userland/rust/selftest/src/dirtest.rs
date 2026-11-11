//! Test payload: proves mkdir()/chdir()/O_CREAT work together -- creates
//! a directory, moves into it, creates a brand new file there via a
//! *relative* path (proving cwd resolution), writes to it, then reopens
//! and reads it back. A follow-up `ls`/`cat` from the shell confirms the
//! directory and file landed in the real VFS tree, not a private copy.

use core::ffi::c_int;

use anshi::{println, sys};

use crate::{Argv, text};

#[unsafe(no_mangle)]
pub extern "C" fn dirtest_main(_argc: c_int, _argv: Argv) -> c_int {
    if unsafe { sys::mkdir(c"/dirtest_dir".as_ptr()) } != 0 {
        println!("mkdir failed");
        return 1;
    }
    println!("mkdir /dirtest_dir: OK");

    if unsafe { sys::chdir(c"/dirtest_dir".as_ptr()) } != 0 {
        println!("chdir failed");
        return 1;
    }
    println!("chdir /dirtest_dir: OK");

    let fd = unsafe { sys::open(c"newfile.txt".as_ptr(), sys::O_WRONLY | sys::O_CREAT) };
    if fd < 0 {
        println!("open(newfile.txt, O_WRONLY|O_CREAT) failed");
        return 1;
    }
    anshi::write_all(fd, b"created via O_CREAT + a relative path\n");
    sys::close(fd);
    println!("created and wrote newfile.txt");

    let fd = unsafe { sys::open(c"newfile.txt".as_ptr(), sys::O_RDONLY) };
    if fd < 0 {
        println!("reopen for read failed");
        return 1;
    }
    let mut buf = [0u8; 63];
    let n = unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) }.max(0) as usize;
    sys::close(fd);
    anshi::print!("read back: {}", text(&buf[..n]));
    0
}
