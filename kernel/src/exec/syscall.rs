//! The syscalls: `int 0x80` with the number in rax and arguments in rdi,
//! rsi and rdx, the result written back to rax.
//!
//! Linux's numbers where Linux has the call -- reusing them costs nothing
//! now and keeps open running real static Linux binaries some day.
//! Anshi-native calls (audio, poll_key, use_as_stdio, copy, the network
//! settings) start at
//! 900, well clear of Linux's table (which tops out in the low 500s), so
//! they read as obviously not-a-real-syscall.

use alloc::vec::Vec;
use core::ffi::{CStr, c_char};

use super::elf::{MAX_ARG_LEN, MAX_ARGS};
use super::process::{self, KERNEL_PARENT_PID, Process, State};
use super::{OpenFile, Usertask, pipe};
use crate::arch::x86_64::idt::InterruptFrame;
use crate::arch::x86_64::{idle_with_interrupts, usermode};
use crate::boot::requests::phys_to_virt;
use crate::console::fbconsole;
use crate::drivers::serial::{self, kout};
use crate::drivers::tty::{ICANON, TCGETS, TCSETS, TIOCGWINSZ, TIOCSWINSZ, Termios, Winsize};
use crate::drivers::virtio::blk;
use crate::drivers::{audio, input, rtc, timer};
use crate::fs::blkfs;
use crate::fs::vfs::{self, NAME_MAX};
use crate::mm::pmm::{self, PAGE_SIZE};
use crate::mm::vmm::{self, PAGE_PRESENT, PAGE_USER, PAGE_WRITABLE};
use crate::net::socket::{self, Endpoint};
use crate::net::{self, NetConfig, NetInfo};
use crate::random;
use smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address};

const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_OPEN: u64 = 2;
const SYS_CLOSE: u64 = 3;
const SYS_LSEEK: u64 = 8;
const SYS_BRK: u64 = 12;
const SYS_IOCTL: u64 = 16;
const SYS_PIPE: u64 = 22;
const SYS_SCHED_YIELD: u64 = 24;
const SYS_GETPID: u64 = 39;
const SYS_SOCKET: u64 = 41;
const SYS_CONNECT: u64 = 42;
const SYS_ACCEPT: u64 = 43;
const SYS_SENDTO: u64 = 44;
const SYS_RECVFROM: u64 = 45;
const SYS_BIND: u64 = 49;
const SYS_LISTEN: u64 = 50;
const SYS_FORK: u64 = 57;
const SYS_EXECVE: u64 = 59;
const SYS_EXIT: u64 = 60;
const SYS_WAIT4: u64 = 61;
const SYS_GETCWD: u64 = 79;
const SYS_CHDIR: u64 = 80;
const SYS_RENAME: u64 = 82;
const SYS_MKDIR: u64 = 83;
const SYS_UNLINK: u64 = 87;
const SYS_SYNC: u64 = 162;
const SYS_GETDENTS: u64 = 217;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_GETRANDOM: u64 = 318;

const SYS_AUDIO_OPEN: u64 = 900;
const SYS_AUDIO_WRITE: u64 = 901;
const SYS_AUDIO_CLOSE: u64 = 902;
const SYS_POLL_KEY: u64 = 903;
/// No Linux equivalent: real Unix composes this from two dup2() calls,
/// but a general dup2() is more surface than the one need -- "make my
/// stdio these two pipes", never arbitrary fd-to-fd redirection.
const SYS_USE_AS_STDIO: u64 = 904;
const SYS_COPY: u64 = 905;
const SYS_NET_INFO: u64 = 906;
const SYS_NET_CONFIG: u64 = 907;
const SYS_RESOLVE: u64 = 908;

/// A task's brk-managed heap never grows past this many bytes past its
/// start -- a simple OOM guard, not a real ulimit.
const HEAP_MAX_BYTES: u64 = 16 * 1024 * 1024;

const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_CREAT: u64 = 0x40;
const O_TRUNC: u64 = 0x200;

const SEEK_SET: u64 = 0;
const SEEK_CUR: u64 = 1;
const SEEK_END: u64 = 2;

/// Linux's real d_type values, for free compat.
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;

/// A deliberately simplified getdents(): one entry per call, not Linux
/// getdents64's batched buffer. d_name is NAME_MAX bytes, NUL-padded.
#[repr(C)]
struct Dirent {
    d_type: u8,
    d_name: [u8; NAME_MAX],
}

/// Linux's struct sockaddr_in: the port and address in network order.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrIn {
    family: u16,
    port: [u8; 2],
    addr: [u8; 4],
    zero: [u8; 8],
}

/// Linux's struct sockaddr_in6.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrIn6 {
    family: u16,
    port: [u8; 2],
    flowinfo: u32,
    addr: [u8; 16],
    scope_id: u32,
}

/// The most one getrandom() fills.
const GETRANDOM_MAX: u64 = 1 << 20;

/// Linux's struct timespec.
#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;

/// Coarse user-pointer validation: OS-grade safety needs a fault-recovering
/// copy_from_user(); a bad-but-in-range user pointer still just crashes
/// that one task (the user-fault path in idt.rs), not the kernel, so this
/// only needs to reject pointers that would reach into kernel space.
fn user_ok(ptr: u64, len: u64) -> bool {
    ptr < 0x0000_8000_0000_0000 && ptr.checked_add(len).is_some_and(|end| end < 0x0000_8000_0000_0000)
}

/// `len` bytes of user memory at `ptr`, once it's been range-checked.
fn user_slice<'a>(ptr: u64, len: u64) -> Option<&'a mut [u8]> {
    if !user_ok(ptr, len) {
        return None;
    }
    Some(if len == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len as usize) } })
}

/// A NUL-terminated user string, as UTF-8 (paths and names are; anything
/// else fails the call).
fn user_str<'a>(ptr: u64) -> Option<&'a str> {
    if !user_ok(ptr, 1) {
        return None;
    }
    unsafe { CStr::from_ptr(ptr as *const c_char) }.to_str().ok()
}

fn task() -> Option<&'static mut Usertask> {
    usermode::current_task()
}

/// Real file descriptors are `3 + index` into the task's open_files.
fn open_file(fd: u64) -> Option<&'static mut OpenFile> {
    let f = task()?.open_files.get_mut(fd.checked_sub(3)? as usize)?;
    (!matches!(f, OpenFile::Free)).then_some(f)
}

fn free_slot(t: &Usertask) -> Option<usize> {
    t.open_files.iter().position(|f| matches!(f, OpenFile::Free))
}

fn ok(b: bool) -> i64 {
    if b { 0 } else { -1 }
}

/// Runs a vfs/blkfs call with its kernel-log error reports kept off the
/// screen: the calling program reports failures itself, and the screen
/// may be a tile pane someone else is drawing.
fn quietly<T>(f: impl FnOnce() -> T) -> T {
    serial::mute_console(true);
    let r = f();
    serial::mute_console(false);
    r
}

fn sys_open(path: u64, flags: u64) -> Option<i64> {
    let t = task()?;
    let path = user_str(path)?;
    let access = flags & !(O_CREAT | O_TRUNC);
    if access != O_RDONLY && access != O_WRONLY {
        return None;
    }
    let node = match vfs::resolve(t.cwd, path) {
        Some(n) => n,
        None if flags & O_CREAT != 0 => {
            if !vfs::create_file(t.cwd, path) {
                return None;
            }
            vfs::resolve(t.cwd, path)?
        }
        None => return None,
    };
    let n = unsafe { &mut *node };
    // A directory can only be opened read-only: an open directory fd is
    // what getdents() reads.
    if n.is_dir() && flags != O_RDONLY {
        return None;
    }
    // O_TRUNC drops the existing content before the first write. Without
    // it, write() only ever grows a file, so saving one that got *shorter*
    // would leave the old tail behind -- which an editor does constantly.
    if flags & O_TRUNC != 0 && !n.is_dir() && access == O_WRONLY {
        n.data.clear();
    }
    let i = free_slot(t)?; // Too many open files.
    t.open_files[i] = OpenFile::File { vnode: node, offset: 0, writable: access == O_WRONLY };
    Some(3 + i as i64)
}

fn sys_close(fd: u64) -> Option<i64> {
    let f = open_file(fd)?;
    if let OpenFile::Pipe { pipe, write_end } = *f {
        if write_end { pipe::close_write(pipe) } else { pipe::close_read(pipe) }
    }
    if let OpenFile::Socket { id } = *f {
        socket::close(id);
    }
    *f = OpenFile::Free;
    Some(0)
}

fn sys_chdir(path: u64) -> Option<i64> {
    let t = task()?;
    let node = vfs::resolve(t.cwd, user_str(path)?)?;
    if !unsafe { (*node).is_dir() } {
        return None;
    }
    t.cwd = node;
    Some(0)
}

fn sys_mkdir(path: u64) -> Option<i64> {
    let t = task()?;
    Some(ok(vfs::mkdir(t.cwd, user_str(path)?)))
}

// unlink/rename/copy are the vfs's remove/move/copy -- the same
// operations and refusals as the kernel shell's delete/move/copy. Unlike
// Linux, unlink() removes directories too (recursively), and rename() and
// copy() follow the shell's "a destination that's a directory means
// *into* it" convention.

fn sys_unlink(path: u64) -> Option<i64> {
    let t = task()?;
    let path = user_str(path)?;
    // vfs::remove() checks only the cwd it's given; any other process
    // (another tile pane's shell) inside the target would be left with a
    // freed vnode as its cwd.
    let node = vfs::resolve(t.cwd, path)?;
    if process::cwd_within(node) {
        return None;
    }
    Some(ok(quietly(|| vfs::remove(t.cwd, path, t.cwd))))
}

fn sys_rename(src: u64, dest: u64) -> Option<i64> {
    let t = task()?;
    let (src, dest) = (user_str(src)?, user_str(dest)?);
    Some(ok(quietly(|| vfs::move_node(t.cwd, src, dest))))
}

fn sys_copy(src: u64, dest: u64) -> Option<i64> {
    let t = task()?;
    let (src, dest) = (user_str(src)?, user_str(dest)?);
    Some(ok(quietly(|| vfs::copy(t.cwd, src, dest))))
}

/// Writes the whole filesystem to disk now -- nothing a program writes is
/// persistent until something does this. -1 when there's no disk.
fn sys_sync() -> Option<i64> {
    if !blk::is_ready() {
        return None;
    }
    Some(ok(quietly(blkfs::save)))
}

/// CLOCK_MONOTONIC (time since boot) or CLOCK_REALTIME (wall-clock time,
/// from the RTC read at boot; -1 if that failed), at the tick's 10 ms
/// resolution.
fn sys_clock_gettime(clock: u64, ts: u64) -> Option<i64> {
    if !user_ok(ts, size_of::<Timespec>() as u64) {
        return None;
    }
    let ms = match clock {
        CLOCK_MONOTONIC => timer::uptime_ms(),
        CLOCK_REALTIME => rtc::now_ms()?,
        _ => return None,
    };
    unsafe {
        (ts as *mut Timespec).write(Timespec { tv_sec: (ms / 1000) as i64, tv_nsec: (ms % 1000) as i64 * 1_000_000 })
    };
    Some(0)
}

fn sys_lseek(fd: u64, offset: i64, whence: u64) -> Option<i64> {
    let OpenFile::File { vnode, offset: pos, .. } = open_file(fd)? else { return None };
    let node = unsafe { &**vnode };
    // SEEK_END means nothing for a directory fd (its offset counts
    // children, not bytes -- see getdents()); SEEK_SET(0) is rewinddir().
    let base = match whence {
        SEEK_SET => 0,
        SEEK_CUR => *pos as i64,
        SEEK_END if !node.is_dir() => node.data.len() as i64,
        _ => return None,
    };
    let new = base.checked_add(offset).filter(|&o| o >= 0)?;
    *pos = new as usize;
    Some(new)
}

/// One entry per call: walks the directory `offset` children in, fills
/// `*out`, and advances. 1 for an entry, 0 past the last, -1 for a bad fd
/// or a file.
fn sys_getdents(fd: u64, out: u64) -> Option<i64> {
    let OpenFile::File { vnode, offset, .. } = open_file(fd)? else { return None };
    let dir = unsafe { &**vnode };
    if !dir.is_dir() || !user_ok(out, size_of::<Dirent>() as u64) {
        return None;
    }
    let Some(child) = dir.children().nth(*offset) else { return Some(0) };
    let mut d = Dirent { d_type: if child.is_dir() { DT_DIR } else { DT_REG }, d_name: [0; NAME_MAX] };
    let name = child.name().as_bytes();
    d.d_name[..name.len()].copy_from_slice(name);
    unsafe { (out as *mut Dirent).write(d) };
    *offset += 1;
    Some(1)
}

fn sys_write(fd: u64, buf: u64, len: u64) -> Option<i64> {
    let data = user_slice(buf, len)?;
    if fd == 1 || fd == 2 {
        // use_as_stdio() redirects fd 1/2 to a pipe.
        if let Some(p) = task().and_then(|t| unsafe { t.stdout_pipe.as_mut() }) {
            return Some(p.write(data).map_or(-1, |n| n as i64));
        }
        // One write() from userland becomes exactly one virtio-gpu flush
        // rather than one per character: a full-screen redraw (scarf) is
        // thousands of bytes, and a virtqueue round trip each made the
        // editor unusable. The bytes, and the serial output, are identical
        // either way.
        fbconsole::begin_batch();
        data.iter().for_each(|&b| kout(b));
        fbconsole::end_batch();
        return Some(len as i64);
    }

    match open_file(fd)? {
        &mut OpenFile::Pipe { pipe, write_end } => {
            if !write_end {
                return None;
            }
            Some(unsafe { &mut *pipe }.write(data).map_or(-1, |n| n as i64))
        }
        &mut OpenFile::Socket { id } => socket::send(id, data, None),
        OpenFile::File { vnode, offset, writable } => {
            if !*writable {
                return None;
            }
            let file = &mut unsafe { &mut **vnode }.data;
            // Writing past the end grows the file, zero-filling any gap.
            let end = *offset + data.len();
            if end > file.len() {
                file.resize(end, 0);
            }
            file[*offset..end].copy_from_slice(data);
            *offset = end;
            Some(len as i64)
        }
        OpenFile::Free => None,
    }
}

/// fd 0 in canonical mode (the default, ICANON) is line-buffered: polls
/// every keyboard, echoes as it goes, stops at Enter or `len` bytes. Raw
/// mode (ICANON cleared with TCSETS) has no echo and no backspace
/// handling: it waits for at least one byte (VMIN=1), then takes whatever
/// else is ready up to `len` without waiting more (VTIME=0) -- the one
/// combination raw-mode programs use. Other fds read from the open file
/// at its offset, or from a pipe.
fn sys_read(fd: u64, buf: u64, len: u64) -> Option<i64> {
    let out = user_slice(buf, len)?;
    if fd != 0 {
        return match open_file(fd)? {
            &mut OpenFile::Pipe { pipe, write_end } => {
                if write_end {
                    return None;
                }
                Some(unsafe { &mut *pipe }.read(out).map_or(-1, |n| n as i64))
            }
            &mut OpenFile::Socket { id } => socket::recv(id, out).map(|(n, _)| n),
            OpenFile::File { vnode, offset, .. } => {
                let node = unsafe { &**vnode };
                if node.is_dir() {
                    return None;
                }
                let rest = node.data.get(*offset..).unwrap_or(&[]);
                let n = out.len().min(rest.len());
                out[..n].copy_from_slice(&rest[..n]);
                *offset += n;
                Some(n as i64)
            }
            OpenFile::Free => None,
        };
    }

    let t = task();
    // use_as_stdio() redirects fd 0 to a pipe -- non-blocking, unlike the
    // keyboard path below (see pipe.rs on why pipes never block in a
    // syscall). A program that wants a blocking read loops in its own
    // ring-3 code, as anshi's read_key() does.
    if let Some(p) = t.as_ref().and_then(|t| unsafe { t.stdin_pipe.as_mut() }) {
        return Some(p.read(out).map_or(-1, |n| n as i64));
    }

    let raw = t.is_some_and(|t| t.termios.c_lflag & ICANON == 0);
    let mut n = 0;
    while n < out.len() {
        let Some(c) = input::poll_char() else {
            if raw && n > 0 {
                break; // VMIN=1/VTIME=0: at least one byte, don't wait for more.
            }
            // `int 0x80` is an interrupt gate, so this runs with interrupts
            // off -- a read waiting seconds for a key would stop the clock
            // for seconds (the uptime freezes, a held USB key never starts
            // repeating). Letting them in is safe: the tick never preempts
            // ring 0. The network keeps moving meanwhile: a shell sitting at
            // its prompt is where this loop spends its life.
            net::poll();
            idle_with_interrupts();
            continue;
        };
        if raw {
            out[n] = c;
            n += 1;
            continue;
        }
        match c {
            b'\n' | b'\r' => {
                kprint!("\n");
                break;
            }
            b'\x08' | 0x7F => {
                if n > 0 {
                    n -= 1;
                    kprint!("\x08 \x08");
                }
            }
            _ => {
                out[n] = c;
                n += 1;
                kout(c);
            }
        }
    }
    Some(n as i64)
}

/// `new_end == 0` queries the current break without changing it (the
/// glibc/musl convention). Never shrinks -- once a page is mapped for the
/// heap it stays mapped. Grows a page at a time up to HEAP_MAX_BYTES past
/// the heap's start.
fn sys_brk(new_end: u64) -> Option<i64> {
    let t = task()?;
    if new_end == 0 || new_end <= t.heap_end {
        return Some(t.heap_end as i64);
    }
    if new_end - t.heap_start > HEAP_MAX_BYTES {
        return None;
    }
    while t.heap_end < new_end {
        let phys = pmm::alloc_page();
        if phys == 0 {
            return None;
        }
        unsafe { core::ptr::write_bytes(phys_to_virt::<u8>(phys), 0, PAGE_SIZE as usize) };
        vmm::map(&t.aspace, t.heap_end, phys, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        t.heap_end += PAGE_SIZE;
    }
    Some(t.heap_end as i64)
}

/// Terminal requests on the console (fd 0/1/2, which share the task's
/// termios); anything else fails, like a real ENOTTY.
fn sys_ioctl(fd: u64, request: u64, argp: u64) -> Option<i64> {
    let t = task()?;
    if fd > 2 {
        return None;
    }
    // The size to check depends on the request: a winsize is 8 bytes, a
    // termios 36 -- one fixed size would reject a valid winsize pointer
    // near the end of a mapped page.
    let size = match request {
        TCGETS | TCSETS => size_of::<Termios>(),
        TIOCGWINSZ | TIOCSWINSZ => size_of::<Winsize>(),
        _ => return None,
    };
    if !user_ok(argp, size as u64) {
        return None;
    }
    unsafe {
        match request {
            TCGETS => (argp as *mut Termios).write_unaligned(t.termios),
            TCSETS => t.termios = (argp as *const Termios).read_unaligned(),
            TIOCSWINSZ => t.winsize = (argp as *const Winsize).read_unaligned(),
            _ => {
                // A TIOCSWINSZ override (a tile pane) wins over the console.
                // With no framebuffer console the serial line is the only
                // terminal, and nothing says how big the far end is -- the
                // classic 80x24 then, so a full-screen program still has a
                // usable (if possibly wrong) size.
                let ws = if t.winsize.ws_col != 0 && t.winsize.ws_row != 0 {
                    t.winsize
                } else {
                    let (cols, rows) = fbconsole::size().unwrap_or((80, 24));
                    Winsize { ws_row: rows as u16, ws_col: cols as u16, ws_xpixel: 0, ws_ypixel: 0 }
                };
                (argp as *mut Winsize).write_unaligned(ws);
            }
        }
    }
    Some(0)
}

/// pipefd[0] = read end, pipefd[1] = write end -- Linux pipe()'s exact
/// convention. Both ends land in the calling task's open files.
fn sys_pipe(pipefd: u64) -> Option<i64> {
    let t = task()?;
    if !user_ok(pipefd, 8) {
        return None;
    }
    let mut free = t.open_files.iter().enumerate().filter(|(_, f)| matches!(f, OpenFile::Free)).map(|(i, _)| i);
    let (r, w) = (free.next()?, free.next()?);
    let p = pipe::create();
    t.open_files[r] = OpenFile::Pipe { pipe: p, write_end: false };
    t.open_files[w] = OpenFile::Pipe { pipe: p, write_end: true };
    unsafe { (pipefd as *mut [i32; 2]).write_unaligned([3 + r as i32, 3 + w as i32]) };
    Some(0)
}

/// Points the task's fd 0/1 at the pipes behind `stdin_fd`/`stdout_fd`
/// and frees those two entries -- a *move*, not a dup, so each pipe end
/// keeps exactly one reference. For a freshly fork()'d child right before
/// execve().
fn sys_use_as_stdio(stdin_fd: u64, stdout_fd: u64) -> Option<i64> {
    let t = task()?;
    let (OpenFile::Pipe { pipe: input, write_end: false }, OpenFile::Pipe { pipe: output, write_end: true }) =
        (*open_file(stdin_fd)?, *open_file(stdout_fd)?)
    else {
        return None;
    };
    t.stdin_pipe = input;
    t.stdout_pipe = output;
    *open_file(stdin_fd)? = OpenFile::Free;
    *open_file(stdout_fd)? = OpenFile::Free;
    Some(0)
}

/// socket(domain, type, protocol): AF_INET or AF_INET6, with
/// SOCK_STREAM (TCP), SOCK_DGRAM (UDP), or SOCK_DGRAM + IPPROTO_ICMP /
/// IPPROTO_ICMPV6 (ping). Every socket is non-blocking: see
/// net/socket.rs for what each call returns.
fn sys_socket(domain: u64, kind: u64, protocol: u64) -> Option<i64> {
    let t = task()?;
    let i = free_slot(t)?;
    let id = socket::open(domain, kind, protocol)?;
    t.open_files[i] = OpenFile::Socket { id };
    Some(3 + i as i64)
}

fn socket_id(fd: u64) -> Option<usize> {
    match open_file(fd)? {
        &mut OpenFile::Socket { id } => Some(id),
        _ => None,
    }
}

/// Decodes a user sockaddr_in or sockaddr_in6.
fn read_sockaddr(addr: u64, len: u64) -> Option<Endpoint> {
    if len < 2 || !user_ok(addr, len) {
        return None;
    }
    let family = unsafe { (addr as *const u16).read_unaligned() };
    match family {
        net::AF_INET if len >= size_of::<SockAddrIn>() as u64 => {
            let a = unsafe { (addr as *const SockAddrIn).read_unaligned() };
            let ip = IpAddress::Ipv4(Ipv4Address::from(a.addr));
            Some(Endpoint { addr: ip, port: u16::from_be_bytes(a.port), scope: 0 })
        }
        net::AF_INET6 if len >= size_of::<SockAddrIn6>() as u64 => {
            let a = unsafe { (addr as *const SockAddrIn6).read_unaligned() };
            let ip = IpAddress::Ipv6(Ipv6Address::from(a.addr));
            Some(Endpoint { addr: ip, port: u16::from_be_bytes(a.port), scope: a.scope_id })
        }
        _ => None,
    }
}

/// Writes `ep` to a user sockaddr buffer of `*len_ptr` bytes (truncated
/// if short, as Linux does) and sets `*len_ptr` to its full size. Both
/// pointers 0: nothing is wanted.
fn write_sockaddr(ep: &Endpoint, addr: u64, len_ptr: u64) -> Option<()> {
    if addr == 0 || len_ptr == 0 {
        return Some(());
    }
    if !user_ok(len_ptr, 4) {
        return None;
    }
    let room = unsafe { (len_ptr as *const u32).read_unaligned() } as usize;
    let mut bytes = [0u8; size_of::<SockAddrIn6>()];
    let full = match ep.addr {
        IpAddress::Ipv4(v4) => {
            let a = SockAddrIn { family: net::AF_INET, port: ep.port.to_be_bytes(), addr: v4.octets(), zero: [0; 8] };
            unsafe { (bytes.as_mut_ptr() as *mut SockAddrIn).write_unaligned(a) };
            size_of::<SockAddrIn>()
        }
        IpAddress::Ipv6(v6) => {
            let a = SockAddrIn6 {
                family: net::AF_INET6,
                port: ep.port.to_be_bytes(),
                flowinfo: 0,
                addr: v6.octets(),
                scope_id: if net::is_link_local(&v6) { ep.scope } else { 0 },
            };
            unsafe { (bytes.as_mut_ptr() as *mut SockAddrIn6).write_unaligned(a) };
            size_of::<SockAddrIn6>()
        }
    };
    let n = room.min(full);
    user_slice(addr, n as u64)?.copy_from_slice(&bytes[..n]);
    unsafe { (len_ptr as *mut u32).write_unaligned(full as u32) };
    Some(())
}

fn sys_connect(fd: u64, addr: u64, len: u64) -> Option<i64> {
    socket::connect(socket_id(fd)?, read_sockaddr(addr, len)?)
}

fn sys_bind(fd: u64, addr: u64, len: u64) -> Option<i64> {
    socket::bind(socket_id(fd)?, read_sockaddr(addr, len)?)
}

fn sys_listen(fd: u64, backlog: u64) -> Option<i64> {
    socket::listen(socket_id(fd)?, backlog)
}

/// accept(fd, addr, addrlen): the new connection's fd, with the peer's
/// address written out; -11 while none has arrived.
fn sys_accept(fd: u64, addr: u64, len_ptr: u64) -> Option<i64> {
    let id = socket_id(fd)?;
    let t = task()?;
    let i = free_slot(t)?; // Checked first: a connection taken can't be put back.
    match socket::accept(id)? {
        Err(e) => Some(e),
        Ok((new, peer)) => {
            t.open_files[i] = OpenFile::Socket { id: new };
            write_sockaddr(&peer, addr, len_ptr);
            Some(3 + i as i64)
        }
    }
}

/// sendto(fd, buf, len, flags, addr, addrlen); `addr` 0 means the
/// connect()ed destination. Flags are ignored.
fn sys_sendto(fd: u64, buf: u64, len: u64, addr: u64, addr_len: u64) -> Option<i64> {
    let id = socket_id(fd)?;
    let data = user_slice(buf, len)?;
    let to = if addr == 0 { None } else { Some(read_sockaddr(addr, addr_len)?) };
    socket::send(id, data, to)
}

/// recvfrom(fd, buf, len, flags, addr, addrlen): like read(), and says
/// who sent it. Flags are ignored.
fn sys_recvfrom(fd: u64, buf: u64, len: u64, addr: u64, len_ptr: u64) -> Option<i64> {
    let id = socket_id(fd)?;
    let out = user_slice(buf, len)?;
    let (n, from) = socket::recv(id, out)?;
    if n >= 0
        && let Some(from) = from
    {
        write_sockaddr(&from, addr, len_ptr)?;
    }
    Some(n)
}

/// getrandom(buf, len, flags): fills the buffer from the kernel's
/// generator (random.rs) -- never blocks, never short. Flags are ignored.
fn sys_getrandom(buf: u64, len: u64) -> Option<i64> {
    let out = user_slice(buf, len.min(GETRANDOM_MAX))?;
    random::fill(out);
    Some(out.len() as i64)
}

/// Fills in a NetInfo for interface `index`; -1 past the last one, which
/// is how a program counts them.
fn sys_net_info(index: u64, out: u64) -> Option<i64> {
    if !user_ok(out, size_of::<NetInfo>() as u64) {
        return None;
    }
    net::poll();
    let info = net::info(index as usize)?;
    unsafe { (out as *mut NetInfo).write_unaligned(info) };
    Some(0)
}

fn sys_net_config(index: u64, config: u64) -> Option<i64> {
    if !user_ok(config, size_of::<NetConfig>() as u64) {
        return None;
    }
    let config = unsafe { (config as *const NetConfig).read_unaligned() };
    Some(ok(net::configure(index as usize, &config)))
}

/// resolve(name, out[16], family): looks `name` up for AF_INET or
/// AF_INET6, writing the address to `out` (4 or 16 bytes) and returning
/// the family; -11 while the answer is on its way (call again with the
/// same name and family), -1 if there isn't one.
fn sys_resolve(name: u64, out: u64, family: u64) -> Option<i64> {
    let name = user_str(name)?;
    if !user_ok(out, 16) {
        return None;
    }
    match net::resolve(name, family as u16)? {
        Ok((fam, addr)) => {
            unsafe { (out as *mut [u8; 16]).write_unaligned(addr) };
            Some(fam as i64)
        }
        Err(e) => Some(e),
    }
}

/// Non-blocking single-key poll -- what lets a player check for a control
/// key without blocking the loop that feeds the audio device. A process
/// whose stdin is a pipe (a tile pane) gets its keys from that pipe, not
/// the keyboard, or it would steal keystrokes meant for tile -- plus -2
/// once the pipe is closed for good (tile quitting), which a program
/// can't otherwise tell apart from "no key yet".
fn sys_poll_key() -> Option<i64> {
    if let Some(p) = task().and_then(|t| unsafe { t.stdin_pipe.as_mut() }) {
        let mut key = [0u8; 1];
        return Some(match p.read(&mut key) {
            Some(1) => key[0] as i64,
            None => -2,
            _ => -1,
        });
    }
    Some(input::poll_char().map_or(-1, i64::from))
}

/// The task's working directory as an absolute path.
fn sys_getcwd(buf: u64, size: u64) -> Option<i64> {
    let t = task()?;
    let out = user_slice(buf, size).filter(|o| !o.is_empty())?;
    let path = vfs::path(t.cwd);
    let n = path.len().min(out.len() - 1);
    out[..n].copy_from_slice(&path.as_bytes()[..n]);
    out[n] = 0;
    Some(0)
}

/// argv lives in the calling task's address space, which exec() is about
/// to replace -- so the strings are copied out first.
fn copy_user_argv(argv: u64) -> Option<Vec<Vec<u8>>> {
    let mut args = Vec::new();
    if argv == 0 {
        return Some(args);
    }
    while args.len() < MAX_ARGS {
        let slot = argv + args.len() as u64 * 8;
        if !user_ok(slot, 8) {
            return None;
        }
        let ptr = unsafe { (slot as *const u64).read_unaligned() };
        if ptr == 0 {
            break;
        }
        if !user_ok(ptr, 1) {
            return None;
        }
        let s = unsafe { CStr::from_ptr(ptr as *const c_char) }.to_bytes();
        args.push(s[..s.len().min(MAX_ARG_LEN - 1)].to_vec());
    }
    Some(args)
}

/// Resolves `path` against the caller's cwd and replaces the caller with
/// it. Never returns on success -- return_to_kernel() abandons the old
/// program's execution entirely, like exit().
fn sys_execve(path: u64, argv: u64) -> Option<i64> {
    let me = process::current()?;
    let path = user_str(path)?;
    let node = vfs::resolve(me.task.cwd, path)?;
    let node = unsafe { &*node };
    if node.is_dir() {
        return None;
    }
    let loaded = {
        let args = copy_user_argv(argv)?;
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_slice()).collect();
        process::exec(me, &node.data, &argv)
    }; // The copies are dropped here: return_to_kernel() would leak them.
    if !loaded {
        return None;
    }
    usermode::return_to_kernel(0)
}

/// waitpid(pid, status) -- no options, no WNOHANG. Blocks by running
/// other processes (the target among them) in an ordinary *recursive*
/// call into the scheduler until it exits.
fn sys_wait(pid: u64, status: u64) -> Option<i64> {
    let me = process::current()?;
    let pid = pid as i64 as i32;
    let target = process::find_child(me.pid, (pid >= 0).then_some(pid))?.pid;
    let code = process::run_until(Some(target));
    if status != 0 && user_ok(status, 4) {
        unsafe { (status as *mut i32).write_unaligned(code) };
    }
    Some(target as i64)
}

/// "process N exited with code X" -- on the console only for a program
/// the kernel shell launched itself, since that's the only way it reports
/// exit statuses. Any other process's parent gets the status from wait()
/// and shows it its own way (sh's prompt), and may be drawn by someone
/// else (a tile pane), where kernel output on the framebuffer would land
/// on top of it. Those exits go to the serial log only.
fn log_exit(me: Option<&Process>, code: i32) {
    let pid = me.map_or(-1, |p| p.pid);
    if me.is_none_or(|p| p.parent_pid == KERNEL_PARENT_PID && p.task.stdout_pipe.is_null()) {
        kprintln!("\nprocess {pid} exited with code {code}");
        return;
    }
    let _ = core::fmt::Write::write_fmt(&mut serial::Serial, format_args!("\nprocess {pid} exited with code {code}\n"));
}

/// Called from the syscall gate (arch/x86_64/stubs.rs) with the caller's
/// saved registers.
pub extern "C" fn syscall_dispatch(frame: *mut InterruptFrame) {
    let f = unsafe { &mut *frame };
    let (a1, a2, a3) = (f.rdi, f.rsi, f.rdx);
    // The Linux ABI's 4th-6th arguments, for the calls that take them.
    let (_a4, a5, a6) = (f.r10, f.r8, f.r9);
    let result = match f.rax {
        SYS_READ => sys_read(a1, a2, a3),
        SYS_WRITE => sys_write(a1, a2, a3),
        SYS_OPEN => sys_open(a1, a2),
        SYS_CLOSE => sys_close(a1),
        SYS_IOCTL => sys_ioctl(a1, a2, a3),
        SYS_LSEEK => sys_lseek(a1, a2 as i64, a3),
        SYS_CHDIR => sys_chdir(a1),
        SYS_MKDIR => sys_mkdir(a1),
        SYS_UNLINK => sys_unlink(a1),
        SYS_RENAME => sys_rename(a1, a2),
        SYS_COPY => sys_copy(a1, a2),
        SYS_SYNC => sys_sync(),
        SYS_CLOCK_GETTIME => sys_clock_gettime(a1, a2),
        SYS_GETDENTS => sys_getdents(a1, a2),
        SYS_BRK => sys_brk(a1),
        SYS_GETPID => process::current().map(|p| p.pid as i64),
        SYS_PIPE => sys_pipe(a1),
        SYS_USE_AS_STDIO => sys_use_as_stdio(a1, a2),
        SYS_AUDIO_OPEN => Some(ok(audio::open(a1 as u32, a2 as u8))),
        SYS_AUDIO_WRITE => {
            // `a1` is a user pointer; the audio drivers copy out of it.
            user_slice(a1, a2 as u32 as u64).map(|pcm| audio::write(pcm).map_or(-1, |n| n as i64))
        }
        SYS_AUDIO_CLOSE => Some(ok(audio::close())),
        SYS_POLL_KEY => sys_poll_key(),
        SYS_SOCKET => sys_socket(a1, a2, a3),
        SYS_CONNECT => sys_connect(a1, a2, a3),
        SYS_BIND => sys_bind(a1, a2, a3),
        SYS_LISTEN => sys_listen(a1, a2),
        SYS_ACCEPT => sys_accept(a1, a2, a3),
        SYS_SENDTO => sys_sendto(a1, a2, a3, a5, a6),
        SYS_RECVFROM => sys_recvfrom(a1, a2, a3, a5, a6),
        SYS_GETRANDOM => sys_getrandom(a1, a2),
        SYS_NET_INFO => sys_net_info(a1, a2),
        SYS_NET_CONFIG => sys_net_config(a1, a2),
        SYS_RESOLVE => sys_resolve(a1, a2, a3),
        SYS_FORK => process::current().and_then(|me| process::fork(me, f)).map(i64::from),
        SYS_GETCWD => sys_getcwd(a1, a2),
        SYS_EXECVE => sys_execve(a1, a2), // Only returns on failure.
        SYS_WAIT4 => sys_wait(a1, a2),
        SYS_SCHED_YIELD => {
            // Gives the CPU to the next runnable process now instead of at
            // the next tick -- what a ring-3 loop polling an empty pipe (sh,
            // tile, a program in a pane) does instead of burning its whole
            // time slice. Exactly the preemption path in idt.rs, with this
            // syscall's own frame as the resume point: the process comes
            // back as if the syscall had returned 0.
            f.rax = 0;
            if let Some(me) = process::current() {
                me.saved_rsp = frame as u64;
                me.state = State::Runnable;
                usermode::return_to_kernel(0);
            }
            return;
        }
        SYS_EXIT => {
            let code = a1 as i32;
            let me = process::current();
            log_exit(me.as_deref(), code);
            if let Some(me) = me {
                process::close_stdio_pipes(me);
                process::close_sockets(me);
                me.state = State::Zombie;
                me.exit_status = code;
            }
            usermode::return_to_kernel(code);
        }
        _ => None,
    };
    f.rax = result.unwrap_or(-1) as u64;
}

// The layouts userland expects (userland/rust/anshi/src/sys.rs).
const _: () = assert!(size_of::<Dirent>() == 1 + NAME_MAX);
const _: () = assert!(size_of::<Termios>() == 36);
