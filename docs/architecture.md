# Architecture

How Anshi is put together, subsystem by subsystem. The
[roadmap](roadmap.md) covers *why* each piece arrived when it did and
what went wrong building it; this is the shape of the result.

## Boot and core


- **Bootloader:** Limine (vendored as a git submodule pinned to the
  `v11.x-binary` release branch), booting the kernel over UEFI only — no
  legacy BIOS path.
- **Kernel:** a 64-bit ELF, linked to the top of the address space
  (`0xffffffff80000000`) per the Limine boot protocol, loaded directly by
  Limine (no separate `kernel_entry` stub needed — `kmain` is the ELF entry
  point). It's one `#![no_std]` Rust crate, `kernel/`, built for
  `x86_64-unknown-none` -- the "kernel" code model, no red zone, and
  soft-float, since the x87/SSE registers belong to userland -- as a
  static library that `kernel/GNUmakefile` links with `linker.ld`. The
  little assembly it needs (interrupt entry stubs, the ring-3
  transition) is `global_asm!` in `arch/x86_64/stubs.rs`. See
  [rust.md](rust.md#the-kernel).
- **Memory:** ordinary RAM access rides on Limine's higher-half direct map
  (HHDM) — `virtual = physical + hhdm_offset`. PCI BAR MMIO does **not**
  live in the HHDM (Limine only maps memory the memory map describes;
  QEMU/OVMF routinely place 64-bit BARs far above actual RAM — virtio-gpu's
  landed at physical `0xc000000000` in testing here, way past a 256 MiB
  guest's real memory), so the page-table walker (`mm/vmm.rs`) maps
  those: it walks/extends the page tables Limine already built,
  allocating any missing levels via the PMM, to map a given physical MMIO
  range into a dedicated slice of virtual address space. The same walker
  builds each process's address space (below). Physical pages come from
  a bitmap allocator (`mm/pmm.rs`); everything else -- `Box`, `Vec`,
  `String` -- from a first-fit heap on top of it (`mm/heap.rs`), Rust's
  global allocator. No demand paging.
- **Drivers:** virtio, plus what a real PC needs (in progress, see
  [real-hardware.md](real-hardware.md)). The kernel enumerates PCI itself
  and speaks the virtio 1.x ("modern") PCI transport directly — no legacy
  virtio. Each kind of device has a real-hardware path next to its
  virtio one: the firmware's GOP framebuffer when there's no virtio-gpu
  (`drivers/display.rs`), USB keyboards on xHCI (`drivers/usb/`),
  Intel HD Audio (`drivers/hda.rs`) behind the same `audio` interface
  as virtio-sound (`drivers/audio.rs`; playback only, see
  [play.md](play.md)), and NVMe, SATA (AHCI) and USB disks behind one
  block layer (`drivers/block.rs`, a `BlockDriver` trait) whose writes
  are locked unless the kernel command line says `allow-disk-writes`.
  Network cards -- virtio-net, and Intel e1000 for everything else --
  sit behind `drivers/nic` (a `NicDriver` trait), each one an active
  interface of the smoltcp IP stack in `net/` (IPv4 and IPv6); see
  [net.md](net.md). `drivers/rtc.rs` reads the CMOS clock once at boot,
  for wall-clock time; `random.rs` is a ChaCha20 generator seeded from
  RDSEED/RDRAND, virtio-rng and timing jitter.
  Everything is polled -- no device interrupts.
- **Debugging:** all kernel logging goes out over the COM1 serial port
  (`kprintln!`), independent of the display, so anything after boot is
  debuggable via `-serial stdio` even before the framebuffer driver works.
  `serial::init()` checks the UART is really there (a loopback self-test)
  and goes quiet if not, as on many real PCs. A kernel panic prints its
  message the same way and halts.
- **Timer:** `drivers/timer.rs` ticks at 100 Hz from the Local APIC timer
  (xAPIC or x2APIC), calibrated against the ACPI PM timer that
  `drivers/acpi.rs` finds in the FADT. The PIT through the 8259 is only
  the fallback: a real board needn't route it to the CPU at all. Boot
  checks that ticks really arrive and logs a `Timer:` line saying which
  source won.

## Processes and userspace

- **Ring 3:** `arch/x86_64/usermode.rs` (on the assembly in
  `stubs.rs`) is the entry/exit primitive -- entering a program is a
  hand-rolled one-shot context switch (an asm-level "setjmp" of the
  kernel's registers, then a manually-built `iretq` frame into ring 3),
  and `return_to_kernel()` restores it via a bare `ret`. `TSS.RSP0` is
  what the CPU uses to find a valid ring-0 stack on any ring 3 -> ring 0
  transition, and it is per-process state, not global.
- **Process table:** `exec/process.rs` holds a fixed 16-slot `Process`
  table (pid, parent pid, state, exit status), each wrapping the
  `Usertask` the usermode layer needs.
- **Scheduling:** plain round-robin. `wait()` blocks by *recursively*
  calling into the scheduler (`process::run_until()`) rather than
  through a suspend/resume mechanism, so there is no distinct "blocked"
  state at all. Preemption is real: `idt.rs`'s `irq_handler()` notices
  the timer interrupting ring-3 code and hands the CPU to a different
  runnable process, reusing the same resume primitive `fork()` already
  needed.
- **Address spaces:** `vmm::new_address_space()` copies the current
  address space's upper 256 PML4 entries -- the whole higher half:
  kernel code, the HHDM, MMIO -- so kernel mappings are automatically
  present and supervisor-only in every task with no further
  bookkeeping. `vmm::clone_user_pages()` (for `fork()`, a full
  page-by-page copy, no copy-on-write) and `vmm::free_user_pages()` (at
  exit/exec) are its counterparts.
- **ELF loading:** `exec/elf.rs` is a from-scratch static, non-PIE
  ELF64 loader. `PT_LOAD` segments each get their own
  `pmm::alloc_pages()` run, mapped `PAGE_USER`, file bytes copied in and
  the remainder zeroed for bss. It also builds the System V process
  initialization stack -- see [syscalls.md](syscalls.md#the-initial-stack).
- **Pipes (M19):** `exec/pipe.rs` is a small fixed-size ring buffer, not
  vnode-based -- a pipe has no tree identity and needs ref-counted
  open/closed state. Every operation is non-blocking by design: a
  busy-spin inside a syscall handler can't yield the CPU to the *other*
  process that would need to run to produce the data being waited for,
  since preemption (above) only fires on a ring-3 interruption, never
  ring-0. `pipe()` + the narrower `use_as_stdio()` (not general
  `dup2()`) are what let tile (`userland/rust/tile/`) run independent `sh`
  processes in tiled panes -- see [syscalls.md](syscalls.md#pipes-m19)
  and [tile.md](tile.md).
- **Userland:** every program is `no_std` Rust in one Cargo workspace,
  `userland/rust/`: the interactive programs (`sh`, `play`, `scarf`,
  `tile`, `net`), the network commands (`ping`, `fetch` with HTTPS,
  `serve`) and the self-test payloads. Each links the `anshi` runtime,
  which provides `_start`, the allocator and the `int 0x80` syscall stubs
  -- see [rust.md](rust.md).

## Syscalls

Vector `0x80` is the one deliberately `DPL=3` gate in the IDT, and the
only way into the kernel from ring 3 (no `SYSCALL`/`SYSRET` fast path
yet). `exec/syscall.rs` dispatches on the number in `rax`. Linux's real
syscall numbers are reused throughout, which costs nothing now and
avoids a renumbering exercise if a Linux-binary-compatibility layer is
ever attempted. Full table: [syscalls.md](syscalls.md).

## Filesystem

- **`fs/vfs.rs`** is a real tree of directory/file `Vnode`s
  (parent/children/sibling links, absolute or cwd-relative resolution
  with `.` and `..`) living entirely in the kernel heap.
- **`fs/blkfs.rs`** persists it -- deliberately not a "real" filesystem
  (no inode table, no free-space bitmap): it recursively serializes the
  *entire* tree to a flat buffer (superblock at sector 0, tree data from
  sector 1), writes it in one shot, and replays it back through the
  vfs's own `mkdir`/`create_file`/`write_bytes` on load. Correct and
  sufficient at hobby-OS scale; not designed to scale to a large
  filesystem.
- Programs live in **`/bin`**, written there at boot from copies
  embedded in the kernel image with `include_bytes!`
  (`exec/blobs.rs`) -- there is no host-side way to get a file onto the
  VFS otherwise. Because they're rewritten on every boot, `blkfs` doesn't
  save them (nor `/testtone.wav`); anything else in `/bin` is saved.

## Console

Two independent output paths, both fed by the kernel log
(`kprintln!`) and by programs' writes to stdout:

- **Serial (COM1).** The primary debug console, and a real interactive
  terminal -- `run-qemu.sh` passes `-serial stdio`, so the host
  terminal *is* the console.
- **Framebuffer.** `console/fbconsole.rs` draws an 8x16 bitmap font over
  whatever `drivers/display.rs` found -- virtio-gpu, or the firmware's GOP
  framebuffer, drawn in a RAM copy and copied to the screen a changed
  rectangle at a time (reading video memory back is slow). The text grid
  sits inside a 16-pixel margin. Each font pixel (and the margin) is
  scaled 2x from 1600 pixels wide and 3x from 3200, so text stays
  readable on a 1080p or 4K monitor: 156x48 cells at 1280x800, 116x31 at
  1920x1080. It understands a subset
  of ANSI/CSI escape sequences: cursor addressing, erase, and SGR for the 16 ANSI
  colors (foreground and background, from its own palette), bold (drawn
  as the bright color), dim (blended toward the background) and reverse
  video -- enough for full-screen programs like [scarf](scarf.md) and
  the colored prompt of [sh](sh.md). Anything outside that subset is
  swallowed, not printed.

  The font is [Spleen](https://github.com/fcambus/spleen) 8x16 (BSD
  2-Clause, vendored in `console/spleen/`), made for terminals: each
  glyph has space above and below it in its cell, so lines of text don't
  touch. Text is UTF-8: ASCII plus 33 extra glyphs -- box drawing
  (`─ │ ┌ ╭ ├ ┼` ...), the arrow and round segment ends a powerline-style
  prompt uses (U+E0B0-U+E0B7), and `❯ ❮ ✔ ✘ · … ● █ ▌ ▐`. Any other
  character draws as `?`. `scripts/gen-font.py` generates
  `console/font.rs` from Spleen, plus `#`/`.` pictures of the few
  glyphs Spleen lacks; to add a glyph, add it there (and to
  `anshi_tui::console_has()`) and rerun the script. tile's per-pane
  terminal and the ratatui backend pass the same set through. A
  redraw costs one flush; on virtio-gpu that currently transfers the
  *entire* framebuffer; see [scarf.md](scarf.md#performance).

Input is equally dual: the kernel shell's `read_line()` and the
read/poll syscalls call `input::poll_char()` (`drivers/input.rs`), which
polls the
virtio keyboard (needs a graphical window with focus), USB keyboards
(`drivers/usb/`: an xHCI driver plus the HID boot-protocol keyboard,
polled, with hot-plug but no hubs yet) and COM1 on every iteration,
whichever has a byte ready. Both keyboards translate through one keymap
(`drivers/keymap.rs`, US QWERTY): keys with no single byte -- arrows,
Home/End, Delete, Page Up/Down -- come out as the escape sequences a
VT100-style terminal sends (`ESC [ A` for Up, `ESC [ 3 ~` for Delete,
...), so a program sees the same bytes from every input.

The kernel boots into the userland shell, `/bin/sh` ([sh.md](sh.md)),
and falls back to the kernel-resident shell (`shell.rs`) if it ever
exits.
