# Rust

Anshi is written in Rust, kernel and userland alike. The kernel moved
over from C one subsystem at a time (M29, see the
[roadmap](roadmap.md)); in userland the interactive programs --
[sh](sh.md), [play](play.md), [scarf](scarf.md) and [tile](tile.md) --
each started as C and were rewritten (M23-M26), then the runtime under
them and the self-test programs followed (M28). The one piece of C left
is minimp3, the MP3 decoder `play` vendors.

There's no Rust standard library for Anshi, so everything is `#![no_std]`
on `core` and `alloc`, built for `x86_64-unknown-none`.

## The kernel

One crate, `kernel/`:

```
kernel/
  Cargo.toml          the crate: a staticlib, panic = "abort"
  .cargo/config.toml  target x86_64-unknown-none, static relocations
  rustfmt.toml        its style -- scripts/format.sh applies it
  linker.ld           Limine's higher-half layout
  GNUmakefile         cargo, then the final link
  src/
    main.rs           kmain(): the boot order, self-tests, then /bin/sh
    sync.rs           Global<T>: a subsystem's mutable state
    mmio.rs           vread!/vwrite! and Regs: volatile device access
    arch/x86_64/      GDT, IDT, PIC, LAPIC, FPU, port I/O, entering ring 3;
                        stubs.rs holds all the kernel's assembly
    boot/             the Limine requests
    mm/               physical pages, the heap, page tables, DMA memory
    drivers/          serial, timer, PCI, display, keyboards, audio,
                        disks (NVMe, AHCI, USB), virtio/, usb/
    console/          the framebuffer console, the boot splash, the font
    fs/               the VFS and its on-disk format
    exec/             ELF loading, processes, pipes, syscalls, and the
                        userland binaries embedded in the kernel
    shell.rs          the kernel-resident shell
    util/             crc32
```

**Building.** `kernel/GNUmakefile` runs `cargo build --release` and links
the resulting `libkernel.a` with `ld -T linker.ld -u kmain`: the crate is
a static library rather than a Rust binary so the same linker and script
as always produce the ELF Limine loads (`-u kmain` pulls the crate out of
the archive). Cargo tracks its own dependencies, so it runs every time
and finishes at once when nothing changed. `x86_64-unknown-none` already
is what a kernel wants -- the "kernel" code model (linked in the top 2
GiB), no red zone, and soft-float, so the kernel never touches the
x87/SSE registers that belong to userland -- and `.cargo/config.toml`
adds static relocations, since the kernel is linked at one address.

**Boot.** Limine finds the kernel's requests (`boot/requests.rs`) by
scanning the `.limine_requests` section, so they're `#[used]` statics
placed there with `#[link_section]`, bracketed by the start and end
markers. `kmain` is the ELF entry point, the one `#[unsafe(no_mangle)]`
function. A panic prints its message to the log and halts.

**Assembly** is all in `arch/x86_64/stubs.rs`, as `global_asm!` in AT&T
syntax: the 32 exception stubs, 16 IRQ stubs and the syscall gate, which
push one `InterruptFrame` layout and call into Rust (named with `sym`
operands, so the handlers need no exported symbols), and the ring-3
transition. Single instructions elsewhere (`lgdt`, `in`/`out`, `cr3`) are
inline `asm!`.

**State.** Anshi runs on one CPU and never touches a subsystem's state
from an interrupt handler mid-update, so nothing locks: a subsystem's
state is a `Global<T>` static (`sync.rs`), an `UnsafeCell` handed out
as `&mut`. Two places can't do that, because they re-enter: USB, where
waiting for one transfer processes events that complete others through
callbacks -- devices and controllers are shared `&'static` with their
mutable fields in `Cell`s -- and the process table, which a syscall
reaches from inside a dispatch the scheduler started (and wait()
recurses into the scheduler), so it's only ever borrowed briefly, never
across a dispatch.

**Devices.** Registers and DMA structures are accessed volatile:
`vread!(ptr, field)`/`vwrite!(ptr, field, value)` for one field of a
device-shared struct, `Regs` for a register block by offset. DMA memory
comes from the PMM through `mm::dma`, zeroed, and is reached through the
higher-half direct map. Disks implement `block::BlockDriver`; the block
layer splits and bounds-checks requests and keeps writes locked unless
the kernel command line says `allow-disk-writes`.

**Memory.** `mm/heap.rs` is Rust's global allocator, so kernel code uses
`Box`, `Vec` and `String` freely -- the VFS keeps file contents in a
`Vec<u8>`, for one. Page-granular memory comes straight from `mm::pmm`.

**Logging.** `kprint!`/`kprintln!` go to COM1 and, once it's up, the
framebuffer console. The userland programs the kernel boots with are
embedded with `include_bytes!` (`exec/blobs.rs`) from the `.elf.bin`
files `scripts/build-userland.sh` leaves next to it.

## The userland workspace

Every program lives in one Cargo workspace, `userland/rust/`, with one
lockfile and one `target/`:

```
userland/rust/
  Cargo.toml          workspace: members, shared deps, release profile
  .cargo/config.toml  target = x86_64-unknown-none
  anshi/              the runtime every Rust program links
  tui/                anshi-tui: ratatui on the Anshi console
  sh/                 the shell the system boots into
  play/               the WAV/MP3 player
    c/mp3.c             play's own C: compiles minimp3's implementation
    vendor/minimp3/     minimp3, vendored unmodified (CC0)
  scarf/              the vim-style editor
  tile/               the terminal multiplexer
  selftest/           the self-test payloads, one module each
```

| Crate | Depends on | Draws with |
|---|---|---|
| `sh` | `anshi` | plain escape codes: a shell scrolls, it doesn't own the screen |
| `play` | `anshi`, `anshi-tui`, ratatui | ratatui |
| `scarf` | `anshi`, `anshi-tui`, ratatui | ratatui |
| `tile` | `anshi`, `anshi-tui`, ratatui | ratatui, plus a terminal emulator per pane |
| `selftest` | `anshi` | `println!` |

## How a program is built

Each program is a **static library** for `x86_64-unknown-none`, not a
Rust binary. `scripts/build-userland.sh` builds the whole workspace with

```sh
cargo build --release \
    --manifest-path userland/rust/Cargo.toml \
    --config userland/rust/.cargo/config.toml
```

which works from any directory (plain `cargo build --release` inside
`userland/rust/` does the same). It then links each `lib<name>.a` with
`ld` and `userland/link.ld` (a static ELF at `0x400000`) and embeds the
ELF in the kernel (`/bin/<name>` at boot). The entry point, `_start`,
comes from `anshi` and calls the `main` the program exports with
`#[unsafe(no_mangle)] pub extern "C" fn main(argc, argv)`.

The self-tests share one staticlib, `libselftest.a`: each test module
exports `<name>_main` instead, and the build links the archive once per
test with `--defsym=main=<name>_main`, so `--gc-sections` leaves just
that test in each ELF.

A program's own C (only `play` has any: `c/mp3.c`) is compiled by the
build script, not by Cargo, with the userland `CC`/`CFLAGS`. There is no
C library behind it: `play/c/include/` declares the three `<string.h>`
functions minimp3 calls, and Rust's `compiler_builtins` defines them.

The build needs a Rust toolchain plus the target (see
[building.md](building.md)); the first build downloads ratatui and its
dependencies from crates.io.

**Adding a program** is a new workspace member that depends on
`anshi`, plus one `build_program` line in `build-userland.sh`. A new
self-test is a module in `selftest/` plus its name in `SELFTESTS`, and
the kernel needs the usual `userland_blobs` and `main.c` entries to put
it in `/bin`.

## The `anshi` runtime

`anshi/` holds everything a `no_std` program still needs:

- **The raw syscalls** (`anshi::sys`): one `int $0x80` stub per
  syscall, the only code in userland that enters the kernel (see
  [syscalls.md](syscalls.md)). The self-tests call them directly to
  check exact return values.
- **Safe wrappers over them**: files
  (`File`, closed on drop), raw mode (`RawMode`, restored on drop),
  directories (`read_dir`, `chdir`/`getcwd`, `mkdir`), the file
  operations (`unlink`, `rename`, `copy`, `sync`), processes and pipes
  (`fork`, `exec`, `waitpid`, `pipe`, `use_as_stdio`), the window size,
  the clock, keys (`read_key`, `poll_key`) and audio (`Audio`).
- **Pipe-aware I/O.** Inside a tile pane, stdin and stdout are pipes,
  which never block. `read_key()` and `write_all()` call `sched_yield()`
  and retry instead of spinning, so a program works the same on the
  console and in a pane (see [tile.md](tile.md#sharing-the-cpu)).
- **`_start`** (`start.rs`): reads `argc`/`argv` off the initial stack,
  aligns it, calls `main` and exits with its result.
- **The global allocator** (`heap.rs`): a first-fit free list over the
  `brk` heap, the same design as the kernel's `mm/heap.c`. Payloads are
  16-byte aligned; a type that needs more is over-allocated, with the
  block's real start stashed just below the aligned pointer.
- **`print!`/`println!`**, formatting straight into `write()` on fd 1.
- **The panic handler**, which resets console attributes, shows the
  cursor, prints the panic and exits with code 101.

## Ratatui without std

Ratatui 0.30 supports `no_std` (`default-features = false`; it needs
`alloc`). Its built-in backends (crossterm and the rest) need std, so
`anshi-tui` (`tui/src/backend.rs`) implements
`ratatui::backend::Backend` for the Anshi console. It queues output into
one buffer and writes it with a single `write()` per frame, and emits
only what the console understands (see
[architecture.md](architecture.md#console)):

- CUP, ED and EL, with CUP skipped for consecutive cells on a row;
- SGR for the 16 ANSI colors (foreground and background), bold, dim and
  reverse video. Indexed and RGB colors map to the nearest of the 16;
  other modifiers are dropped;
- printable ASCII plus the console's extra glyphs
  (`anshi_tui::console_has()`), sent as UTF-8. Any other symbol is
  mapped to a stand-in.

Ratatui diffs each frame against the previous one, so only changed cells
are sent. `anshi_tui::init()` clears the screen and hides the cursor
(programs draw their own); `restore()` brings back normal video, a
cleared screen and a visible cursor on exit.

**The shared look** is `anshi_tui::chrome`: round-ended tabs, grey
rules, keycaps, the grey and on-accent colors and the arrow and round
glyphs, all taken from sh's prompt. scarf, play and tile draw their
titles, chips and key hints with it; each picks its own accent colors.

**The bottom row stays empty.** The console wraps the cursor as soon as
a glyph lands in the last column; it has no deferred wrap. Writing the
bottom-right cell would therefore scroll the whole screen up a line, so
the backend reports one row fewer than the console has.

**Soft-float.** `x86_64-unknown-none` is a soft-float target, so any
float math in Rust code runs in software. The programs avoid it on hot
paths: `play` keeps its PCM, volume and spectrum math integer and lays
out with plain `Rect` arithmetic instead of ratatui's float-based
constraint solver. C code (minimp3) is compiled with SSE and uses the
FPU, which is safe because the kernel saves and restores each process's
FPU/SSE state.
