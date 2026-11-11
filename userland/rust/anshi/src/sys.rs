//! The raw syscalls: thin `int $0x80` stubs over Anshi's syscall ABI
//! (vector 0x80, Linux-numbered, number in rax and arguments in rdi,
//! rsi, rdx -- see kernel/src/exec/syscall.rs and docs/syscalls.md).
//! Each returns the kernel's result unchanged, -1 for failure; the safe
//! API in lib.rs is built on top.
//!
//! These are the only place userland talks to the kernel.

use core::arch::asm;
use core::ffi::{c_char, c_int};

pub const O_RDONLY: c_int = 0;
pub const O_WRONLY: c_int = 1;
pub const O_CREAT: c_int = 0x40;
pub const O_TRUNC: c_int = 0x200;

pub const SEEK_SET: c_int = 0;
pub const SEEK_CUR: c_int = 1;
pub const SEEK_END: c_int = 2;

pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;
pub const TIOCGWINSZ: u64 = 0x5413;
pub const TIOCSWINSZ: u64 = 0x5414;
pub const ICANON: u32 = 0x0002;
pub const ECHO: u32 = 0x0008;
pub const VMIN: usize = 6;
pub const VTIME: usize = 5;

pub const CLOCK_MONOTONIC: c_int = 1;

pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;

/// Linux's struct termios layout -- see kernel/src/drivers/tty.rs.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; 19],
}

/// One directory entry per getdents() -- a deliberately simplified ABI,
/// not Linux's batched getdents64 (kernel/src/exec/syscall.rs's
/// sys_getdents()). d_name's size must match kernel/src/fs/vfs.rs's
/// NAME_MAX.
#[repr(C)]
pub struct Dirent {
    pub d_type: u8,
    pub d_name: [u8; 64],
}

#[repr(C)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
pub struct Winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

/// fork()'s "returns twice" needs nothing special here: the kernel
/// builds the child's first resume to look exactly like a return from
/// this same `int $0x80`, with rax=0 (kernel/src/exec/process.rs's
/// fork()), and every other register is preserved across it.
unsafe fn syscall3(number: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!("int 0x80", inlateout("rax") number as i64 => ret, in("rdi") a1, in("rsi") a2, in("rdx") a3);
    }
    ret
}

pub unsafe fn read(fd: c_int, buf: *mut u8, len: usize) -> i64 {
    unsafe { syscall3(0, fd as u64, buf as u64, len as u64) }
}

pub unsafe fn write(fd: c_int, buf: *const u8, len: usize) -> i64 {
    unsafe { syscall3(1, fd as u64, buf as u64, len as u64) }
}

pub unsafe fn open(path: *const c_char, flags: c_int) -> c_int {
    unsafe { syscall3(2, path as u64, flags as u64, 0) as c_int }
}

pub fn close(fd: c_int) -> c_int {
    unsafe { syscall3(3, fd as u64, 0, 0) as c_int }
}

pub fn lseek(fd: c_int, offset: i64, whence: c_int) -> i64 {
    unsafe { syscall3(8, fd as u64, offset as u64, whence as u64) }
}

/// `new_end == 0` queries the current break. Returns the new break,
/// which is short of `new_end` (or -1) when the heap can't grow.
pub fn brk(new_end: usize) -> i64 {
    unsafe { syscall3(12, new_end as u64, 0, 0) }
}

pub unsafe fn ioctl(fd: c_int, request: u64, argp: *mut u8) -> i64 {
    unsafe { syscall3(16, fd as u64, request, argp as u64) }
}

/// fds[0] is the read end, fds[1] the write end.
pub unsafe fn pipe(fds: *mut c_int) -> c_int {
    unsafe { syscall3(22, fds as u64, 0, 0) as c_int }
}

/// Gives up the rest of this time slice to the next runnable process.
pub fn sched_yield() -> c_int {
    unsafe { syscall3(24, 0, 0, 0) as c_int }
}

pub fn getpid() -> c_int {
    unsafe { syscall3(39, 0, 0, 0) as c_int }
}

pub fn fork() -> c_int {
    unsafe { syscall3(57, 0, 0, 0) as c_int }
}

/// `argv` is NULL-terminated. Only returns on failure.
pub unsafe fn execve(path: *const c_char, argv: *const *const c_char) -> c_int {
    unsafe { syscall3(59, path as u64, argv as u64, 0) as c_int }
}

pub fn exit(code: c_int) -> ! {
    unsafe { syscall3(60, code as u64, 0, 0) };
    loop {}
}

pub unsafe fn waitpid(pid: c_int, status: *mut c_int) -> c_int {
    unsafe { syscall3(61, pid as u64, status as u64, 0) as c_int }
}

pub unsafe fn getcwd(buf: *mut c_char, size: usize) -> c_int {
    unsafe { syscall3(79, buf as u64, size as u64, 0) as c_int }
}

pub unsafe fn chdir(path: *const c_char) -> c_int {
    unsafe { syscall3(80, path as u64, 0, 0) as c_int }
}

/// A `dest` that's an existing directory means into it.
pub unsafe fn rename(src: *const c_char, dest: *const c_char) -> c_int {
    unsafe { syscall3(82, src as u64, dest as u64, 0) as c_int }
}

pub unsafe fn mkdir(path: *const c_char) -> c_int {
    unsafe { syscall3(83, path as u64, 0, 0) as c_int }
}

/// Unlike Linux, removes directories too, recursively.
pub unsafe fn unlink(path: *const c_char) -> c_int {
    unsafe { syscall3(87, path as u64, 0, 0) as c_int }
}

/// Writes the whole filesystem to disk -- nothing is persistent until
/// something calls it.
pub fn sync() -> c_int {
    unsafe { syscall3(162, 0, 0, 0) as c_int }
}

/// One entry per call: 1 for an entry, 0 at the end, -1 if `fd` isn't a
/// directory.
pub unsafe fn getdents(fd: c_int, out: *mut Dirent) -> i64 {
    unsafe { syscall3(217, fd as u64, out as u64, 0) }
}

/// Only CLOCK_MONOTONIC (time since boot, 10 ms resolution): there's no
/// real-time clock.
pub unsafe fn clock_gettime(clock: c_int, ts: *mut Timespec) -> c_int {
    unsafe { syscall3(228, clock as u64, ts as u64, 0) as c_int }
}

// Anshi-native syscalls, 900+, no Linux equivalent.

/// Configures the single playback stream: `rate_hz` 44100 or 48000,
/// `channels` 1 or 2.
pub fn audio_open(rate_hz: u32, channels: u32) -> c_int {
    unsafe { syscall3(900, rate_hz as u64, channels as u64, 0) as c_int }
}

/// Sends S16LE PCM, blocking until the device has consumed it.
pub unsafe fn audio_write(buf: *const u8, len: u32) -> i64 {
    unsafe { syscall3(901, buf as u64, len as u64, 0) }
}

pub fn audio_close() -> c_int {
    unsafe { syscall3(902, 0, 0, 0) as c_int }
}

/// Non-blocking keypress check: the key, -1 if none is ready, -2 if
/// stdin is a pipe that has been closed (a tile pane shutting down).
pub fn poll_key() -> c_int {
    unsafe { syscall3(903, 0, 0, 0) as c_int }
}

/// Points this process's fd 0/1 at the pipes behind `stdin_fd`/
/// `stdout_fd`, freeing those two fds (a move, not a dup) -- for a
/// freshly forked child right before execve().
pub fn use_as_stdio(stdin_fd: c_int, stdout_fd: c_int) -> c_int {
    unsafe { syscall3(904, stdin_fd as u64, stdout_fd as u64, 0) as c_int }
}

/// Copies a file or a whole directory tree; same `dest` rule as rename().
pub unsafe fn copy_path(src: *const c_char, dest: *const c_char) -> c_int {
    unsafe { syscall3(905, src as u64, dest as u64, 0) as c_int }
}
