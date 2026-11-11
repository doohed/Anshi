# Syscalls

Entry is `int 0x80` -- vector `0x80` is the one deliberately `DPL=3`
gate in the IDT, and the only way into the kernel from ring 3. There is
no `SYSCALL`/`SYSRET` fast path yet. `exec/syscall.rs` dispatches on the
number in `rax`; arguments follow the ordinary System V register order
(`rdi`, `rsi`, `rdx`).

**The numbers are Linux's real ones.** Reusing them costs nothing now
and avoids a renumbering exercise if a Linux-binary-compatibility layer
is ever attempted. The same reasoning applies to the flag and struct
values below -- `O_CREAT` is `0x40`, `TCGETS` is `0x5401`, `struct
termios` has Linux's layout, and so on.

## Table

| № | Name | Signature | Notes |
|---|---|---|---|
| 0 | `read` | `read(fd, buf, len)` | fd 0 is the console; `ICANON` selects line vs raw mode |
| 1 | `write` | `write(fd, buf, len)` | fd 1/2 go to the console, one flush per call |
| 2 | `open` | `open(path, flags)` | `O_RDONLY`/`O_WRONLY`, plus `O_CREAT` and `O_TRUNC` |
| 3 | `close` | `close(fd)` | |
| 8 | `lseek` | `lseek(fd, offset, whence)` | `SEEK_END` rejected on directory fds |
| 12 | `brk` | `brk(new_end)` | grows a page at a time, capped at 4 MiB, never shrinks |
| 16 | `ioctl` | `ioctl(fd, req, argp)` | `TCGETS`, `TCSETS`, `TIOCGWINSZ`, `TIOCSWINSZ` only |
| 39 | `getpid` | `getpid()` | |
| 57 | `fork` | `fork()` | full page-by-page address-space copy, no copy-on-write |
| 59 | `execve` | `execve(path, argv)` | replaces the image in place; same pid, open files, cwd, termios |
| 60 | `exit` | `exit(status)` | |
| 61 | `wait4` | `waitpid(pid, status)` | simplified: no options, no `WNOHANG` |
| 82 | `rename` | `rename(src, dest)` | move/rename; a `dest` that's an existing directory means *into* it |
| 87 | `unlink` | `unlink(path)` | removes a file, **or a directory and everything in it**; refuses if any process's cwd is inside |
| 162 | `sync` | `sync()` | writes the whole filesystem to disk; -1 with no disk |
| 228 | `clock_gettime` | `clock_gettime(clock, timespec*)` | `CLOCK_MONOTONIC` (1) only: time since boot, 10 ms resolution |
| 22 | `pipe` | `pipe(pipefd[2])` | `pipefd[0]`=read end, `pipefd[1]`=write end; both never block (see below) |
| 24 | `sched_yield` | `sched_yield()` | gives up the rest of the time slice; what a loop polling an empty pipe calls instead of spinning |
| 79 | `getcwd` | `getcwd(buf, size)` | absolute path of the task's cwd |
| 80 | `chdir` | `chdir(path)` | also the only reliable "is this a directory?" test |
| 83 | `mkdir` | `mkdir(path)` | |
| 217 | `getdents` | `getdents(fd, dirent*)` | **one entry per call**, not Linux's batched-buffer ABI |

Everything resolves paths against the calling task's cwd, so relative
paths behave as they do in the shell.

## Anshi-native syscalls (900+)

Linux does audio via `/dev/snd/*` + `ioctl`, and Anshi has no devfs, so
unlike every syscall above there's no real Linux number to reuse for
audio. These four are Anshi-only, picked well clear of Linux's real
x86_64 table (which tops out in the low 500s) so they read as obviously
not-a-real-syscall:

| № | Name | Signature | Notes |
|---|---|---|---|
| 900 | `audio_open` | `audio_open(rate_hz, channels)` | `rate_hz` ∈ {44100, 48000}, `channels` ∈ {1, 2} only |
| 901 | `audio_write` | `audio_write(buf, len)` | S16LE PCM; blocks until the device has consumed it |
| 902 | `audio_close` | `audio_close()` | |
| 903 | `poll_key` | `poll_key()` | non-blocking; -1 if no key is ready. With a piped stdin (a tile pane) it reads that pipe instead of the keyboard, and returns -2 once the pipe is closed |
| 905 | `copy` | `copy_path(src, dest)` (`anshi::sys` name) | copies a file or a whole directory tree; same `dest` rule as `rename` |
| 904 | `use_as_stdio` | `use_as_stdio(stdin_fd, stdout_fd)` | points this task's fd 0/1 at the pipes behind two existing fds |

`poll_key` is what makes an interactive audio player possible without
threads or `poll`/`select`: it's the exact same non-blocking
`input::poll_char()` the kernel shell's own `read_line()` already polls
with, exposed as a syscall so a playback
loop can check for a control key without ever blocking the loop that
feeds the audio device — see [play.md](play.md).

`use_as_stdio` is the redirection half of running an independent
process in a tile pane (M19-M21, see [tile.md](tile.md)): real Unix
composes this from two `dup2()` calls, but this project deliberately
doesn't implement general `dup2()` — the only real need is "make my
stdio these two pipes," called by a freshly `fork()`'d child right
before `execve()`, which is narrower surface than arbitrary fd→fd
redirection. It's a *move*, not a dup: the two source fds are freed as
part of the call, so there's still exactly one live reference to each
pipe end afterward.

## Pipes (M19)

`pipe()` creates a `Pipe` (`kernel/src/exec/pipe.rs`) — a small,
fixed-size (4096 byte) in-kernel ring buffer, deliberately unrelated to
a `Vnode`: a pipe has no tree identity and needs bounded,
ref-counted open/closed state a vnode's whole-buffer-regrow write model
doesn't fit.

**Every pipe operation is non-blocking**, full stop — `read()` returns
`>0` (bytes), `0` (empty but a writer's still connected), or `-1` (EOF,
every write-end reference gone); `write()` copies as much as fits and
returns a short count rather than blocking on a full buffer. This isn't
a simplification of convenience: `irq_handler()`'s preemption check
(`arch/x86_64/idt.rs`) only fires when the timer interrupts *ring-3*
code, never ring-0, so a busy-spin inside a syscall handler waiting on
*another process* to produce data would never yield the CPU to that
process — a real deadlock, not a style choice. A program that wants
blocking-looking behavior over a pipe loops in its own ring-3 code
instead (see `anshi::read_key()` in `userland/rust/anshi/`, which
yields with `sched_yield()` between empty reads), the same shape
`poll_key()`-based loops already use elsewhere.

References are counted, not tracked as a single "open" boolean — `fork()`
copies the *entire* `open_files` table (same as every other
fd), so after a `fork()` both parent and child hold independent table
entries referencing the same pipe, and closing one's copy must not
silently close the other's. `process::fork()` bumps the relevant
refcount for every pipe-backed entry it copies to keep this correct.
There's no `O_CLOEXEC`/close-on-exec either — a child that inherits fds
it doesn't know about (e.g. tile spawning a *second* pane while the
first one's pipes are still open) must close them explicitly, or it
silently holds a phantom reference that keeps the first pipe from ever
reaching zero refs. Found exactly this way, as a real hang.

## The initial stack

`elf_load()` builds a System V AMD64 process-initialization stack. At
entry `RSP` points at `argc`:

```
RSP ->  argc
        argv[0]
        ...
        argv[argc-1]
        NULL              <- argv terminator
        NULL              <- envp[0]; there is no environment yet
        0, 0              <- auxv AT_NULL
        ... argument strings ...
```

`_start` (`userland/rust/anshi/src/start.rs`) reads `argc` into
`%rdi` and `argv` into `%rsi`, aligns the stack to 16 bytes, and calls
the program's `main(argc, argv)`, then exits with its return value.

It is written through the HHDM rather than through the new address
space: the stack pages come from a single `pmm_alloc_pages()` run, so
they are physically contiguous and reachable without switching `CR3`.

Arguments are capped at 16 of 128 bytes each, bounds-checked against the
64 KiB user stack.

## Terminal handling

`struct usertask` carries a `termios`, defaulted to `ICANON|ECHO` and
explicitly preserved across `exec()` -- real Unix semantics, since
terminal settings belong to the terminal, not the program. `fork()`
inherits it for free via the existing shallow copy.

`read()` on fd 0 branches on `ICANON`. Raw mode does no echo and no
backspace handling, and returns as soon as one byte is available
(`VMIN=1`, `VTIME=0` -- the one concrete combination implemented).

`TIOCGWINSZ` answers from the framebuffer console's glyph grid, or
80x24 on a boot with no screen at all. Note that it reports the
*framebuffer* geometry even when you are on the serial console, where
the real terminal may be a different size -- it is the only size the
kernel knows.

**Unless the process has its own window size.** `TIOCSWINSZ` stores a
size in the calling process's `struct usertask`. `TIOCGWINSZ` then
reports that size instead, and so does every process forked or exec'd
from it, because the field is inherited like `termios`. On Linux the
window size lives on the pty; Anshi has no ptys, so it lives on the
process. This is how [tile](tile.md) tells a pane's programs the pane's
size: the pane's child sets it once before exec'ing `sh`, and
everything `sh` runs inherits it.

**File operations are the kernel shell's.** `unlink`, `rename` and
`copy` call the same `fs/vfs.rs` functions as the kernel shell's
`delete`, `move` and `copy`, with the same refusals. Those functions
report errors to the kernel log; on a program's behalf that output is
kept off the screen (serial only, via `serial::mute_console()`), and the
syscall just returns -1 for the program to report. Nothing a program
writes is on disk until something calls `sync`.

**Exit messages.** The kernel's "process N exited with code X" line
appears on the console only for a program the kernel shell launched
itself, because that's the only way the kernel shell reports exit
statuses. Every other process's parent gets the status from `wait()` and
shows it its own way (sh's prompt), and may not even be drawing on the
console directly (a tile pane), so those exits go to the serial log
only.

## Not implemented

No `mmap`, no signals, no `poll`/`select`, no general `dup`/`dup2` (M19
added `pipe()` and the narrower `use_as_stdio()` instead -- see above),
no pty (a per-process window size stands in for the one thing tile
needed from one), no `O_NONBLOCK` on anything but pipes (which are unconditionally
non-blocking), no `stat`/`fstat`, no `envp`, no TLS
or `arch_prctl`. A real terminal multiplexer and a userland shell turned
out *not* to need most of that list after all -- see
[tile.md](tile.md) -- but a musl port still does.
