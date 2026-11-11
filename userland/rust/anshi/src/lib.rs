//! The runtime every Anshi Rust program links against. There is no Rust
//! std for Anshi, so this provides what a `no_std` program still needs:
//!
//! - the raw syscalls (sys.rs) and safe wrappers over them here -- the
//!   only code in userland that talks to the kernel;
//! - `_start` (start.rs), which calls the program's `main`;
//! - the global allocator (heap.rs), on the brk-managed heap;
//! - `print!`/`println!` and the panic handler.
//!
//! A program just depends on this crate; registering the entry point,
//! allocator and panic handler happens here, once. See ../Cargo.toml for
//! how programs are built and linked.

#![no_std]

extern crate alloc;

mod heap;
mod start;
pub mod sys;

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{CStr, c_char, c_int};
use core::fmt;

pub use alloc::ffi::CString;
pub use sys::{SEEK_CUR, SEEK_END, SEEK_SET};

use sys::{O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY, Termios};

/// Writes all of `bytes` to `fd`. A pipe (stdout inside a tile pane)
/// never blocks: when it's full the write returns 0, so this yields to
/// let the reader drain it and tries again. Gives up on an error (-1,
/// e.g. nobody left to read).
pub fn write_all(fd: c_int, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let n = unsafe { sys::write(fd, bytes.as_ptr(), bytes.len()) };
        if n < 0 {
            return;
        }
        if n == 0 {
            sched_yield();
            continue;
        }
        bytes = &bytes[n as usize..];
    }
}

/// Hands the rest of this time slice to the next runnable process --
/// for a loop that would otherwise spin on an empty pipe.
pub fn sched_yield() {
    sys::sched_yield();
}

/// One non-blocking read from a pipe (or file) fd: `Some(n)` bytes, with
/// `Some(0)` meaning "nothing yet"; `None` once the other end is closed
/// and drained (or on error).
pub fn read_fd(fd: c_int, buf: &mut [u8]) -> Option<usize> {
    let n = unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) };
    if n < 0 { None } else { Some(n as usize) }
}

pub fn close(fd: c_int) {
    sys::close(fd);
}

pub fn exit(code: i32) -> ! {
    sys::exit(code)
}

/// Blocking read of one byte from stdin -- in raw mode (RawMode), one
/// keypress. None on EOF or error.
///
/// On the console this blocks in the kernel. Inside a tile pane stdin is
/// a pipe, which never blocks -- an empty read returns 0 -- so this
/// yields and retries until a key arrives. None only once stdin is
/// closed for good.
pub fn read_key() -> Option<u8> {
    let mut b = 0u8;
    loop {
        match unsafe { sys::read(0, &raw mut b, 1) } {
            1 => return Some(b),
            0 => sched_yield(),
            _ => return None,
        }
    }
}

pub enum KeyPoll {
    Key(u8),
    /// Nothing pressed yet.
    Empty,
    /// stdin is a pipe that's been closed (a tile pane shutting down):
    /// no key will ever come, so the program should quit.
    Closed,
}

/// Non-blocking keypress check.
pub fn poll_key() -> KeyPoll {
    match sys::poll_key() {
        -2 => KeyPoll::Closed,
        k if k < 0 => KeyPoll::Empty,
        k => KeyPoll::Key(k as u8),
    }
}

/// Terminal size in (columns, rows), 80x24 if the ioctl fails.
pub fn window_size() -> (u16, u16) {
    let mut ws = sys::Winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    let ok = unsafe { sys::ioctl(0, sys::TIOCGWINSZ, (&raw mut ws).cast()) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 { (ws.ws_col, ws.ws_row) } else { (80, 24) }
}

/// Sets this process's window size -- what window_size() (TIOCGWINSZ)
/// will report from now on, here and in every process forked or exec'd
/// from it, instead of the physical console's. tile uses it to tell a
/// pane's programs how big the pane is (kernel/src/drivers/tty.rs).
pub fn set_window_size(cols: u16, rows: u16) -> bool {
    let mut ws = sys::Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { sys::ioctl(0, sys::TIOCSWINSZ, (&raw mut ws).cast()) == 0 }
}

/// Both ends of a new pipe: (read end, write end). Pipes never block --
/// see read_fd()/write_all().
pub fn pipe() -> Option<(c_int, c_int)> {
    let mut fds = [0 as c_int; 2];
    if unsafe { sys::pipe(fds.as_mut_ptr()) } == 0 { Some((fds[0], fds[1])) } else { None }
}

/// fork(): the child's pid in the parent, 0 in the child, None on
/// failure. The child gets a copy of every open fd -- there's no
/// close-on-exec, so it must close whatever it shouldn't keep.
pub fn fork() -> Option<i32> {
    let pid = sys::fork();
    if pid < 0 { None } else { Some(pid) }
}

/// Points this process's stdin/stdout at the given pipe fds (freeing
/// those two fd numbers) -- Anshi's narrow stand-in for dup2(), meant
/// for a freshly forked child right before exec().
pub fn use_as_stdio(stdin_fd: c_int, stdout_fd: c_int) -> bool {
    sys::use_as_stdio(stdin_fd, stdout_fd) == 0
}

/// Replaces this process with the program at `path`. Only returns if
/// that failed.
pub fn exec(path: &CStr, args: &[&CStr]) {
    let mut argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(core::ptr::null());
    unsafe { sys::execve(path.as_ptr(), argv.as_ptr()) };
}

/// Waits for child `pid` to exit; its exit status.
pub fn waitpid(pid: i32) -> i32 {
    let mut status: c_int = -1;
    unsafe { sys::waitpid(pid, &mut status) };
    status
}

/// Puts the console in raw mode (no line buffering, no echo); restores
/// the original settings when dropped.
pub struct RawMode(Termios);

impl RawMode {
    pub fn enable() -> Option<RawMode> {
        let mut orig = Termios { c_iflag: 0, c_oflag: 0, c_cflag: 0, c_lflag: 0, c_line: 0, c_cc: [0; 19] };
        if unsafe { sys::ioctl(0, sys::TCGETS, (&raw mut orig).cast()) } != 0 {
            return None;
        }
        let mut raw = orig;
        raw.c_lflag &= !(sys::ICANON | sys::ECHO);
        raw.c_cc[sys::VMIN] = 1;
        raw.c_cc[sys::VTIME] = 0;
        if unsafe { sys::ioctl(0, sys::TCSETS, (&raw mut raw).cast()) } != 0 {
            return None;
        }
        Some(RawMode(orig))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { sys::ioctl(0, sys::TCSETS, (&raw mut self.0).cast()) };
    }
}

/// Changes the working directory. Fails on anything that isn't a
/// directory -- which makes it the reliable "is this a directory?" test,
/// since open() succeeds on directories too.
pub fn chdir(path: &CStr) -> bool {
    unsafe { sys::chdir(path.as_ptr()) == 0 }
}

/// The working directory as an absolute, canonical path.
pub fn getcwd() -> Option<String> {
    let mut buf = [0 as c_char; 256];
    if unsafe { sys::getcwd(buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    let s = unsafe { CStr::from_ptr(buf.as_ptr()) };
    s.to_str().ok().map(String::from)
}

// ---------- paths as &str ----------
//
// The filesystem calls below take Rust strings and do the NUL
// termination themselves; a path with an interior NUL simply fails.

fn cpath(path: &str) -> Option<CString> {
    CString::new(path).ok()
}

/// Whether `path` names anything (a file or a directory).
pub fn exists(path: &str) -> bool {
    cpath(path).and_then(|p| File::open(&p)).is_some()
}

/// Whether `path` is a directory -- without chdir()ing to find out:
/// getdents() succeeds on a directory fd and fails on a file's.
pub fn is_dir(path: &str) -> bool {
    let Some(p) = cpath(path) else { return false };
    let Some(f) = File::open(&p) else { return false };
    let mut e = sys::Dirent { d_type: 0, d_name: [0; 64] };
    unsafe { sys::getdents(f.0, &mut e) >= 0 }
}

pub fn mkdir(path: &str) -> bool {
    cpath(path).is_some_and(|p| unsafe { sys::mkdir(p.as_ptr()) == 0 })
}

/// Creates an empty file, or empties an existing one.
pub fn create_file(path: &str) -> bool {
    cpath(path).and_then(|p| File::create(&p)).is_some()
}

/// Removes a file, or a directory and everything in it.
pub fn unlink(path: &str) -> bool {
    cpath(path).is_some_and(|p| unsafe { sys::unlink(p.as_ptr()) == 0 })
}

/// Moves/renames; a `dest` that's an existing directory means into it.
pub fn rename(src: &str, dest: &str) -> bool {
    match (cpath(src), cpath(dest)) {
        (Some(s), Some(d)) => unsafe { sys::rename(s.as_ptr(), d.as_ptr()) == 0 },
        _ => false,
    }
}

/// Copies a file or a whole directory tree; a `dest` that's an existing
/// directory means into it.
pub fn copy(src: &str, dest: &str) -> bool {
    match (cpath(src), cpath(dest)) {
        (Some(s), Some(d)) => unsafe { sys::copy_path(s.as_ptr(), d.as_ptr()) == 0 },
        _ => false,
    }
}

/// Writes the whole filesystem to disk. False when there's no disk.
pub fn sync() -> bool {
    sys::sync() == 0
}

/// Milliseconds since boot (10 ms resolution).
pub fn now_ms() -> u64 {
    let mut ts = sys::Timespec { tv_sec: 0, tv_nsec: 0 };
    if unsafe { sys::clock_gettime(sys::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Lists a directory's entries, in the VFS's own order. None if it
/// can't be opened.
pub fn read_dir(path: &CStr) -> Option<Vec<DirEntry>> {
    let dir = File::open(path)?;
    let mut entries = Vec::new();
    loop {
        let mut e = sys::Dirent { d_type: 0, d_name: [0; 64] };
        match unsafe { sys::getdents(dir.0, &mut e) } {
            1 => {}
            0 => return Some(entries),
            _ => return None, // not a directory
        }
        let name = CStr::from_bytes_until_nul(&e.d_name).map_or(&e.d_name[..], |n| n.to_bytes());
        entries.push(DirEntry { name: String::from_utf8_lossy(name).into(), is_dir: e.d_type == sys::DT_DIR });
    }
}

/// An open file descriptor, closed on drop.
pub struct File(c_int);

impl File {
    pub fn open(path: &CStr) -> Option<File> {
        let fd = unsafe { sys::open(path.as_ptr(), O_RDONLY) };
        if fd < 0 { None } else { Some(File(fd)) }
    }

    /// Opens for writing, creating the file or truncating it to empty.
    /// O_TRUNC matters: the kernel's write path only ever extends a file,
    /// so without it a shorter save would leave the old tail behind.
    pub fn create(path: &CStr) -> Option<File> {
        let fd = unsafe { sys::open(path.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC) };
        if fd < 0 { None } else { Some(File(fd)) }
    }

    /// Reads up to `buf.len()` bytes; 0 at end of file or on error.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let n = unsafe { sys::read(self.0, buf.as_mut_ptr(), buf.len()) };
        if n <= 0 { 0 } else { n as usize }
    }

    /// Writes all of `bytes`; false if the write fell short.
    pub fn write_all(&mut self, mut bytes: &[u8]) -> bool {
        while !bytes.is_empty() {
            let n = unsafe { sys::write(self.0, bytes.as_ptr(), bytes.len()) };
            if n <= 0 {
                return false;
            }
            bytes = &bytes[n as usize..];
        }
        true
    }

    /// Fills as much of `buf` as the file has left; returns the count.
    pub fn read_full(&mut self, buf: &mut [u8]) -> usize {
        let mut got = 0;
        while got < buf.len() {
            let n = self.read(&mut buf[got..]);
            if n == 0 {
                break;
            }
            got += n;
        }
        got
    }

    pub fn seek(&mut self, offset: i64, whence: c_int) -> i64 {
        sys::lseek(self.0, offset, whence)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        sys::close(self.0);
    }
}

/// The single virtio-sound playback stream; closed on drop.
pub struct Audio;

impl Audio {
    pub fn open(rate: u32, channels: u32) -> Option<Audio> {
        if sys::audio_open(rate, channels) == 0 { Some(Audio) } else { None }
    }

    /// Sends S16LE PCM, blocking until the device has consumed it.
    pub fn write(&mut self, pcm: &[u8]) -> bool {
        unsafe { sys::audio_write(pcm.as_ptr(), pcm.len() as u32) >= 0 }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        sys::audio_close();
    }
}

/// `core::fmt` sink straight to a file descriptor -- `write!` without
/// allocating, which the panic handler relies on.
pub struct FdWriter(pub c_int);

impl fmt::Write for FdWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_all(self.0, s.as_bytes());
        Ok(())
    }
}

/// `print!` to stdout, formatted straight into write() calls.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        let _ = core::fmt::Write::write_fmt(&mut $crate::FdWriter(1), format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        $crate::print!($($arg)*);
        $crate::print!("\n");
    }};
}

/// Puts the console back in a usable state before reporting: plain
/// attributes, cursor shown, on a fresh line -- a panic mid-frame in a
/// full-screen program would otherwise leave reverse video on and the
/// cursor hidden. The program's screen itself is left as it was.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    write_all(1, b"\x1b[0m\x1b[?25h\r\n");
    let _ = writeln!(FdWriter(1), "panic: {info}");
    exit(101)
}
