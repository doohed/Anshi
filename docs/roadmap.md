# Roadmap

Built as a sequence of milestones, all **boot-verified**: booted in
QEMU (`-M q35 -vga none -device virtio-gpu-pci`, OVMF) with a
screendump confirming the console text and a pixel swatch actually
render through the virtio-gpu queue.


Built as a sequence of milestones, all now **boot-verified**: booted in
QEMU (`-M q35 -vga none -device virtio-gpu-pci`, OVMF) with a screendump
confirming the console text and a pixel swatch actually render through the
virtio-gpu queue.

- [x] **M0 — Boot to serial.** Limine hands off to `kmain` in 64-bit long
      mode; kernel reads the HHDM offset, memory map, boot framebuffer info,
      and RSDP pointer from Limine and logs them over COM1.
- [x] **M1 — GDT/TSS/IDT.** Own 64-bit GDT and TSS, full IDT with exception
      handlers that dump CPU state over serial (and, once up, the
      framebuffer console) instead of triple-faulting silently. `kmain`
      deliberately triggers a `#DE` (divide-by-zero) after setup to prove
      the dispatch path end-to-end.
- [x] **M2 — Physical memory manager.** Bitmap page allocator built from
      Limine's memory map, plus a contiguous-run allocator
      (`pmm_alloc_pages`) for DMA buffers like the GPU framebuffer. Self-
      tests itself (alloc/free a batch, check for dupes and for the free
      count returning to baseline) on every boot.
- [x] **M3 — PCI enumeration.** Brute-force config-space scan to find
      devices, specifically the virtio-gpu-pci device QEMU exposes.
- [x] **M4 — Generic virtio-pci transport.** Capability parsing, split
      virtqueues, feature negotiation — reusable by any future virtio driver
      (blk, net, ...). Needed `mm/vmm.c` (see Architecture) once testing
      showed the common-cfg BAR living way outside the HHDM.
- [x] **M5 — virtio-gpu driver.** 2D resource/scanout/transfer/flush over
      the control virtqueue; a pixel swatch proves direct framebuffer
      writes go through the driver (not Limine's boot-services
      framebuffer, which is never touched), and a small bitmap-font text
      console (`font8x8_basic`, public domain) sits on top and mirrors
      everything `kprintf` logs. `console/splash.c` shows a Fedora/
      Windows-style boot splash first (centered ASCII logo, looping
      `.`/`..`/`...` indicator) before the console takes over — paced with
      `pit_sleep_ms()` (see M8) as of that milestone, a plain busy-wait
      spin before it.
- [x] **M6 — virtio-input keyboard + interactive shell.** QEMU exposes
      keyboard/mouse/tablet under the identical PCI id, so
      `virtio_input_init()` walks every match (`pci_find_device_nth()`)
      and reads each one's `VIRTIO_INPUT_CFG_ID_NAME` to find the one
      actually named "Keyboard". `shell/shell.c` reads lines from it
      (polling, with backspace editing) and dispatches to a small builtin
      table (`help`, `echo`, `clear`, `uname`, `meminfo`, `lspci`,
      `crash`, `reboot`, `halt`) — command *names* are matched case-
      insensitively, arguments keep whatever case you typed. The `crash`
      builtin is the `#DE` self-test that used to run automatically at
      the end of boot — now on demand, since the exception handler halts
      forever and we want an interactive prompt afterward instead.
      `read_line()` also polls `serial_poll_char()` (`drivers/serial.c`)
      alongside virtio-input, so the shell works as a real serial console
      too — added after virtio-input alone turned out to need a
      graphical window with keyboard focus to receive anything at all,
      which a headless `-display none` run (this repo's default) simply
      doesn't have. See "Using the shell interactively" below.
- [x] **M7 — In-memory filesystem + directory/file management.** Two new
      pieces underneath the shell: `mm/heap.c` (`kmalloc`/`kfree`, a
      first-fit allocator carving variable-sized blocks out of pages from
      `pmm_alloc_pages()` — nothing above the PMM could allocate anything
      but whole pages before this), and `fs/vfs.c`, a real tree of
      directory/file `vnode`s (parent/children/sibling links, absolute or
      cwd-relative path resolution with `.`/`..`) that lives entirely in
      that heap and resets on reboot — no block device or on-disk format
      yet (that's the natural next step, once this UX layer was proven).
      New shell builtins, deliberately not named like standard Unix
      (`create dir <name>` instead of `mkdir`, `create file <name>`
      instead of `touch`, same verb-first pattern for `delete`/`copy`/
      `move`): `create`, `delete`, `copy`, `move` (each `dir`/`file` +
      path, `copy`/`move` also take a destination), plus `cd`, `pwd`,
      `ls`, `cat`, `write` to navigate and inspect. The prompt now shows
      the current directory (`Anshi:/some/path>`). `vfs_remove()`
      refuses to delete the current directory or an ancestor of it (the
      shell's `cwd` pointer would otherwise dangle) — verified by trying
      to delete `.`, `..`, and an absolute ancestor path while `cwd` sat
      inside the target, all correctly refused.
- [x] **M8 — Hardware interrupts + a PIT timer.** Everything before this
      milestone polled; there was no clock, no `sleep()`, nothing a future
      scheduler could tick against. `arch/x86_64/pic.c` remaps the 8259
      pair (IRQ0-15 -> vectors 32-47, clear of the CPU exception range),
      `arch/x86_64/irq.S` + `idt.c` add a generic 16-line IRQ dispatch
      (`irq_register()`) alongside the existing exception handlers, and
      `drivers/pit.c` programs channel 0 for a 100 Hz tick
      (`pit_ticks()`/`pit_uptime_ms()`/`pit_sleep_ms()`, the last now
      pacing the M5 splash instead of a guessed spin count; new `uptime`
      shell builtin).
      **The actual discovery this milestone was about:** a fully-correct
      PIC remap + PIT program + unmask + `sti` still left every timer
      interrupt permanently stuck pending in the 8259's IRR, never
      delivered — confirmed via QEMU's `info pic` (`irr=01`, `isr=00`,
      CPU confirmed halted with IF=1). On chipsets that default to
      APIC-only routing (QEMU's q35 included), the legacy PIC's INTR line
      only reaches the CPU if the Local APIC's LINT0 pin is explicitly
      configured for ExtINT delivery and unmasked, and the LAPIC itself
      software-enabled via its Spurious Interrupt Vector Register — none
      of which firmware guarantees once it's done with its own boot phase.
      `pic_remap()` now also does this (`lapic_enable_extint_passthrough()`
      in `pic.c`), which is why it has to run after `pmm_init()` (it maps
      the LAPIC's MMIO page via `vmm_map_mmio()`) rather than right after
      `idt_init()` like the rest of CPU bring-up. Also caught while wiring
      up the `uptime` command: `kprintf` has no field-width support
      (`%03lu` silently consumed zero arguments instead of erroring,
      desyncing every `va_arg` read after it in that call) — its own doc
      comment already said so, but it's a sharp edge worth remembering.

- [x] **M9 — Persistent storage (virtio-blk).** The M7 filesystem was
      100% in-RAM and reset every reboot. `drivers/virtio/virtio_blk.c`
      is a new virtio-blk-pci driver (device type 2, PCI id `0x1042`),
      same shape as the gpu/input drivers: built entirely on the
      existing generic transport in `virtio.c`, one request virtqueue,
      each I/O a 3-descriptor chain (header / data / status).
      `fs/blkfs.c` is the persistence layer on top of it — not a "real"
      filesystem (no inode table, no free-space bitmap): it recursively
      serializes the *entire* in-memory `vnode` tree to a flat buffer
      (superblock at sector 0, tree data from sector 1) and writes it in
      one shot, replaying it back through `vfs.c`'s own
      `vfs_mkdir`/`vfs_create_file`/`vfs_write_bytes` on load — the same
      pragmatic "correct and sufficient at hobby-OS scale, not designed
      to scale to a large filesystem" trade-off this project has made
      elsewhere (splash pacing, the shell's line editor). New
      `vfs_write_bytes()` in `vfs.c` (a `vfs_write_file()` refactor)
      takes an explicit length instead of `strlen()`-ing, since restored
      file content isn't NUL-terminated text. The shell auto-saves after
      every successful `create`/`delete`/`copy`/`move`/`write`, so
      persistence needs no extra step; a new `sync` builtin flushes
      on demand (e.g. right before `halt`/`reboot`). Booting without
      `-device virtio-blk-pci` attached still works exactly as in M7 —
      in-memory only, `sync` reports there's nothing to persist to.

- [x] **M10 — Ring-3 userspace (one program at a time).** Everything
      before this milestone ran as one privileged kernel blob; nothing
      could execute code the kernel didn't ship. `mm/vmm.c` grew a real
      per-address-space API (`vmm_new_address_space()`/`vmm_map()`/
      `vmm_switch()`, plus a `PAGE_USER` flag) on top of its existing
      4-level page-table walker — a fresh address space copies the
      current one's upper 256 PML4 entries (the whole higher half:
      kernel code, the HHDM, MMIO) so kernel/HHDM mappings are
      automatically present, supervisor-only, in every task without any
      further bookkeeping. `arch/x86_64/gdt.c`'s ring-3 GDT descriptors
      (built since early on but never used) are now exposed as
      `SEL_UCODE`/`SEL_UDATA`, and `tss_set_kernel_stack()` wires up
      `TSS.RSP0` — previously never set at all, which is what the CPU
      needs to find a valid ring-0 stack on any ring 3 -> ring 0
      transition. New `arch/x86_64/usermode.c/.h/.S` is the actual
      ring-3 entry/exit primitive: `enter_usermode()` is a hand-rolled
      one-shot context switch (asm-level "setjmp" of the kernel's own
      registers/`rsp`, then a manually-built `iretq` frame into ring 3);
      `return_to_kernel()` (called from the exit syscall, and from a
      user-mode fault -- see below) restores it via a bare `ret`. New
      vector `0x80` (the one deliberately `DPL=3` gate in the IDT) is
      the syscall entry point, dispatching in new `exec/syscall.c` to
      `exit`/`write`/`read` (Linux's own syscall numbers, reused for
      free in case a real Linux-binary-compatibility layer is ever
      attempted). `exec/elf.c` is a from-scratch static, non-PIE ELF64
      loader: `PT_LOAD` segments get their own `pmm_alloc_pages()` run,
      mapped `PAGE_USER`, file bytes copied in and the rest zeroed for
      bss. New shell builtin `run <path>` ties it together. A hand-
      rolled 3-syscall "libc" + two tiny test programs live in the new
      `userland/` directory (`hello.c`, proving `write`/`read`/`exit`
      round-trip; `crash.c`, deliberately faulting through a null
      pointer) — built by `scripts/build-userland.sh` and embedded
      directly into the kernel image via `.incbin` (see
      `exec/userland_blobs.S`) as self-test fixtures, the same idea as
      the PMM/VMM self-tests above but landing on the VFS instead of
      just printing a result.
      **The actual discovery this milestone was about:** `idt.c`'s
      `struct interrupt_frame` had every general-purpose register field
      declared in the *same* order the assembly stubs `push` them in —
      but since `push` stores at the stack pointer *after* decrementing
      it, the last register pushed ends up at the lowest address (offset
      0), not the first. The struct needed the reverse order to actually
      alias the pushed registers correctly (the doc comment even said
      "reversed" — just not what the code did). This bug predates M10
      by nine milestones and was completely invisible the whole time,
      since the only consumer (`isr_handler`'s fatal-exception dump) only
      ever *printed* these values after an unrecoverable halt — nobody
      was cross-checking a frozen crash dump against what the registers
      "should" say. It became fatal the moment `syscall_dispatch()`
      started reading `frame->rax`/`rdi`/`rsi`/`rdx` for their actual,
      load-bearing meaning (the syscall number and arguments): every
      syscall silently read the wrong register, always fell through to
      the "unknown syscall" case, and returned without doing anything —
      which looked exactly like a hang, since the userland test payload
      never even checks `write()`/`exit()`'s return values.
      Diagnosed by querying real CPU state via QEMU's monitor (`info
      registers`) rather than trusting kernel-side debug prints, which
      is what actually revealed `RAX`/`RDI` held plausible values while
      `struct interrupt_frame`'s fields didn't reflect them.

- [x] **M11 — a real userland libc.** M10 proved ring-3 execution but
      the only thing that could run was a hand-written `crt0.S` + a
      3-function stub — no dynamic memory, no `printf`, no way to touch a
      real file beyond fd 0/1/2. A genuine `musl` port isn't realistic
      yet (it expects a fully Linux-compatible syscall surface — real
      `mmap`, `clone`, `futex`, `arch_prctl` for TLS — and a TLS ABI
      Anshi has none of), so this milestone is a small, from-scratch
      libc instead: enough for a hand-written C program to allocate
      memory, format output, and read/write a real VFS file.
      `struct usertask` (`arch/x86_64/usermode.h`) gained a brk-managed
      heap range (`heap_start`/`heap_end`) and a small fixed-size open-
      file table (`MAX_OPEN_FILES=8`), plus a `usermode_current_task()`
      accessor so `exec/syscall.c` can reach the running task's state —
      previously nothing did, since M10 never needed mutable per-task
      state beyond what it took to launch a task once.
      Four new syscalls, all Linux-numbered like M10's original three:
      `brk` (grows the heap a page at a time via `pmm_alloc_page()` +
      `vmm_map()`, capped at 4 MiB, never shrinks — same "no giving
      memory back" pragmatism as `mm/heap.c`'s own `kmalloc`/`kfree`),
      and `open`/`close`/`lseek` (`O_RDONLY`/`O_WRONLY` only, no
      `O_CREAT` — a task can only work with files that already exist).
      `read`/`write` gained an `fd >= 3` path through the new file table,
      writing straight into `vnode->data` (growing it via a fresh
      `kmalloc`+`memcpy`+`kfree` when a write extends past the current
      end — the same operation `fs/vfs.c`'s own `vfs_write_bytes()` does
      for a whole-file overwrite, just done here directly since this
      needs to write at an arbitrary offset, matching `fs/blkfs.c`'s
      existing precedent of touching vnode fields directly).
      The libc itself lives in `userland/libc/` (`string.c`, `stdio.c`,
      `malloc.c`) plus `userland/syscalls.c` (raw `int 0x80` wrappers)
      behind a shared `userland/libc.h`. `malloc()`/`free()` is a
      from-scratch free-list allocator *deliberately mirroring*
      `mm/heap.c`'s own design (first-fit, forward-only coalescing,
      grows on demand — here via `brk()` instead of `pmm_alloc_pages()`)
      rather than reusing its code, since userland can't call kernel
      functions. `printf()` re-implements `kprintf`'s exact minimal
      format-spec support over `write(1, ...)`. Two new self-test
      payloads prove it: `malloctest.bin` (several `malloc()`s of
      different sizes, a pattern-fill/verify pass, a `free()`+reuse
      check) and `filetest.bin` (`open`/`read`/`printf`s a real VFS
      fixture file, then `lseek`s and `write`s to patch and extend it —
      verified by `cat`ing the file from the shell before and after and
      confirming the *shell* sees the change the ring-3 task made,
      proving the syscall reaches the real VFS, not a copy).

- [x] **M12 — `O_CREAT`, `mkdir`, and a per-task working directory.**
      M11's file I/O only worked against files that already existed,
      always resolved against the VFS root. `struct usertask`
      (`arch/x86_64/usermode.h`) gained a `cwd` field (always starts at
      `/` — a task doesn't inherit the launching shell's `cwd`), and two
      new Linux-numbered syscalls: `chdir` (80) and `mkdir` (83). `open`
      now accepts `O_CREAT` (`0x40`, Linux's real value) — if the path
      doesn't resolve, it creates the file via `vfs_create_file()` first
      — and resolves every path against `task->cwd` instead of always
      the root, so relative paths work the same way they already did in
      the shell's own `cwd` global. New test payload `dirtest.bin`
      proves all three together: `mkdir`s a directory, `chdir`s into it,
      `open`s a brand-new file there by a *relative* name with
      `O_CREAT`, writes and reads it back — verified by `ls`/`cat`ing it
      from the shell afterward and seeing the real thing the task
      created, not a private copy.

- [x] **M13 — a scheduler, `fork`/`exec`/`wait`, and real preemption.**
      Everything before this milestone was strictly one process at a
      time: `run` blocked the shell synchronously on a single static
      task. This is the biggest architectural jump since M10 itself,
      shipped in two halves.
      **Phase A — process table + cooperative multitasking.** New
      `exec/process.c/.h`: a fixed 16-slot `struct process` table
      (pid/parent pid/state/exit status, wrapping the same `struct
      usertask` M10 already had), and three new Linux-numbered syscalls
      — `fork` (57, full page-by-page address-space copy via new
      `vmm_clone_user_pages()`, no copy-on-write), `execve` (59,
      simplified single-arg `exec(path)`, replaces a process's image in
      place, same pid/open files/cwd), `wait4`/`getpid` (61/39,
      simplified `waitpid(pid, status)`). `wait()` blocks via an
      ordinary *recursive* call into the scheduler (`scheduler_run_until()`
      runs other processes — including the target, once picked — until
      it exits) rather than a suspend/resume mechanism; no distinct
      "blocked" state needed at all in Phase A. `shell.c`'s `run` spawns
      a process and calls `scheduler_run_until(-1)`, blocking until every
      process that launch (transitively, via `fork()`) spawned has
      exited — there's no init process to reparent orphans to yet, so
      the convention is that a process which forks a child waits for it
      before exiting.
      **Phase B — real preemption.** `idt.c`'s `irq_handler()` now
      recognizes the timer (IRQ0) interrupting ring-3 code and hands the
      CPU to a different runnable process instead of just returning —
      reusing the *same* resume primitive Phase A's `fork()` already
      needed (`arch_resume_process()`, added to `usermode.S`): a
      preempted process's full register state is already sitting on its
      own kernel stack exactly where the interrupt frame points, so
      "pausing" it costs nothing more than remembering that address.
      `isr.S`/`syscall.S`/`irq.S`'s previously-duplicated pop-registers-
      then-`iretq` tail is now one shared `common_return_from_interrupt`
      label all three (plus `arch_resume_process`) jump into. New test
      payload `preempttest.bin` proves it unambiguously: two forked
      children each spin a tight busy loop with *no* voluntary yield
      point at all, printing their own tag — the output interleaves
      (`AAABBBBBAAAABBBB...`) instead of running as two clean sequential
      blocks, which is only possible if the timer is genuinely
      interrupting one mid-flight and resuming the other.
      **Two real bugs found building this, both the same species:**
      state that's fine for exactly one task in flight breaks the moment
      a second one can be "in progress" underneath it. `wait()`'s
      recursive scheduler call dispatches a *different* process while
      the waiting one is still mid-syscall (not done, just not
      currently running) — this broke `TSS.RSP0` (left pointing at
      whichever process most recently ran, so the *outer* process's next
      syscall built its interrupt frame on the wrong kernel stack) and
      the kernel-side dispatch-resume bookkeeping (a single shared save
      slot, overwritten by the nested dispatch, stranding the outer
      process's real resume point) — both fixed by making the relevant
      state per-process (`kernel_resume` moved into `struct usertask`)
      or explicitly saved/restored around nested calls (`TSS.RSP0`,
      `usermode.c`'s `current_task`), the same `outer_current` pattern
      `process.c`'s own scheduler loop already used for itself. Caught
      by the exact symptom that pattern predicts: things worked
      perfectly for the *first* nested dispatch and broke on whatever
      ran after the recursion unwound.

- [x] **M14 — free process memory on exit/exec.** M13 made the M10-era
      "never free a process's address space or kernel stack" shortcut a
      real problem: `fork()`/`exec()`/process exit now happen
      constantly, not once per kernel lifetime. New `vmm_free_user_pages()`
      (`mm/vmm.c`) is the missing counterpart to `vmm_clone_user_pages()`
      — the same 4-level user-half walk, freeing each leaf page and
      every intermediate PT/PD/PDPT table page (never touching the
      shared higher half) back to the PMM, then the PML4 itself.
      Reap time (`exec/process.c`'s `scheduler_run_until()`, both the
      specific-pid and the auto-reap-on-empty paths) frees a zombie's
      address space and kernel stack immediately — by then `dispatch()`
      has already switched CR3/`TSS.RSP0` away, so it's provably safe.
      `exec()` is the one genuinely tricky spot: `process_exec()` runs
      *while still executing on the old kernel stack* (CR3 hasn't
      switched away yet either), so freeing either right there would
      mean freeing memory the CPU is currently using — it stashes them
      in two new `pending_free_as`/`pending_free_kstack` fields on
      `struct process` instead, freed by `process_free_pending()` at the
      start of this same process's *next* dispatch, the first point
      that's guaranteed safe. `process_fork()`'s own two OOM failure
      paths got the same treatment, so a failed `fork()` doesn't leak
      its partial clone either. Verified by running `forktest.bin` 15
      times in a row and watching `meminfo`: free pages drop once (heap
      warm-up) then stay *exactly flat* for every subsequent run — the
      actual signature of bounded memory use, not just a slower leak.

- [x] **M15 — `termios` raw mode, and `opendir`/`readdir`.** Two
      prerequisites for eventually running real terminal programs
      (`vim`'s raw keystroke-at-a-time input, directory-aware tools).
      New `drivers/tty.h` defines Linux's real `struct termios` layout
      and `ICANON`/`ECHO`/`VMIN`/`VTIME` flag values (same free-future-
      compat reasoning M10-M14 already used for syscall numbers); `struct
      usertask` gains a `termios` field, defaulted by `elf_load()` to
      today's actual behavior (`ICANON|ECHO`) and explicitly preserved
      across `exec()` — real Unix semantics: terminal settings belong to
      the terminal, not the program, `fork()` inherits it for free via
      the existing shallow copy. New syscall `ioctl` (16, `TCGETS`/
      `TCSETS=0x5401/0x5402`) gets/sets it; `sys_read_impl()`'s fd-0 path
      now branches on `ICANON` — raw mode does no echo/backspace
      handling and returns as soon as one byte is available (`VMIN=1`,
      `VTIME=0` — the one concrete combination implemented). Separately,
      `sys_open_impl()` now allows opening a directory read-only
      (`open_file.offset` reused as "index of the next child"), and new
      syscall `getdents` (217) walks `vnode->children` one entry per call
      — a deliberate simplification of real `getdents64`'s batched-
      buffer ABI. `userland/libc/termios.c` (`tcgetattr`/`tcsetattr`) and
      `libc/dirent.c` (`opendir`/`readdir`/`closedir`/`rewinddir`, a
      `DIR *` wrapping an fd, `readdir()` reusing a `static` buffer per
      real `readdir()`'s "valid until next call" contract) present the
      usual POSIX-shaped API over both. New test payloads:
      `readdirtest.bin` (lists `/`, verified against the shell's own
      `ls` — same 11 entries, same names) and `termtest.bin` (reads back
      the default `c_lflag` as `0xa` = `ICANON|ECHO`, switches to raw
      mode, and reads three interactively-typed keystrokes one at a
      time with no Enter needed and no kernel-side echo — only the
      program's own `got byte:` prints appear — before restoring the
      original settings).

- [x] **M16 -- a text editor, and the plumbing it needed.** The first
      Anshi program that draws a full screen rather than scrolling log
      lines past: `userland/scarf.c`, a modal editor with vim
      keybindings and a file-explorer sidebar (see
      [scarf.md](scarf.md) for keys and design notes). Nothing about
      it was possible before, and the interesting part is the list of
      things that had to exist first.
      **Kernel side.** `console/fbconsole.c` gained an ANSI/CSI parser
      -- cursor addressing (`ESC[r;cH`), erase display/line, and SGR
      reverse video; before this, escape sequences were drawn as
      literal glyphs, so cursor positioning had nowhere to land.
      `exec/syscall.c` gained `TIOCGWINSZ` (nothing could ask how big
      the terminal was, though the kernel knew: `fbconsole.c` computes
      `cols = fb->width / 8`) and `O_TRUNC` (the write path only ever
      *grew* a file, so saving a shortened buffer left the previous
      version's tail behind -- which an editor does constantly).
      `drivers/virtio/virtio_input.c` gained Escape, the shifted symbol
      row, and Ctrl: all three were unreachable in a graphical window,
      which meant no way to leave insert mode, no way to type `:` for
      `:w`/`:q`, and no `Ctrl-b`. Over serial none of this ever
      mattered, because the terminal produces those bytes itself --
      the gap only existed on the virtio-input path.
      **Then `argv`, which is the real milestone.** `elf_load()` now
      builds a genuine System V process-initialization stack (`argc`,
      the `argv[]` array, its NULL, an empty `envp`, an `AT_NULL`
      auxv, and the string data), written through the HHDM since the
      stack pages are one contiguous `pmm_alloc_pages()` run. `crt0.S`
      reads them off `%rsp` and calls `main(argc, argv)`; programs
      declaring `main(void)` ignore the registers, which is why the
      ABI change didn't require touching all eleven existing payloads.
      `execve` now matches Linux's real `execve(path, argv)`, and
      threads through `process_spawn`/`process_exec`.
      Alongside it, two decisions from earlier milestones were
      reversed on purpose: a task now **inherits its launcher's cwd**
      (M12 deliberately started every task at `/`, which made `scarf .`
      meaningless), and programs moved to **`/bin`**, which `shell.c`
      searches for bare command names -- so `scarf .` works from any
      directory rather than needing `run /bin/scarf .`. New `getcwd`
      (79) resolves the inherited cwd to an absolute path. Builtins
      still win a name collision, so a file in `/bin` can never shadow
      `ls` or `cd`.
      **Three bugs worth remembering, all found by looking rather than
      reasoning.** First: a line that exactly fills the terminal width
      auto-wraps, and the explicit `\r\n` after it then advanced a
      *second* time -- every frame drifted down one row until the
      screen scrolled, leaving a duplicate status line. Caught by
      screendumping the framebuffer over QMP, not from the serial log,
      where it was invisible. Fixed by positioning every row explicitly
      with `ESC[row;colH`. Second: `.incbin` dependencies were not
      tracked -- `kernel/GNUmakefile` built `userland_blobs.S.o` from
      `%.S GNUmakefile` only, so editing a userland program and
      rebuilding silently kept the *previous* binary in the kernel
      image. That one cost two rounds of debugging a program that had
      already been fixed. Third: `opendir()` cannot detect a directory,
      because it is just `open(path, O_RDONLY)` and opening a regular
      file read-only succeeds; `chdir()` is the test that actually
      checks the type.
      Window splits (`Ctrl-w s`/`v`, a proper binary layout tree) were
      built, verified working, and then **removed**: tiled panes can't
      use `ESC[K` to clear a line without erasing the pane beside them,
      so every cell had to be padded, at roughly 20 KB of output per
      keystroke. The sidebar layout keeps one editor pane against the
      right edge specifically so it can clear with `ESC[K`.

- [x] **M17 -- `virtio-sound` driver + audio syscalls.** No audio path
      existed at all before this: no sound driver, no audio syscalls.
      New `kernel/src/drivers/virtio/virtio_snd.c/.h`, same shape as
      `virtio_blk.c` and built entirely on the existing generic
      transport in `virtio.c` -- only the control and tx virtqueues get
      wired up (event/rx go untouched, the same "only wire up the
      queues you use" precedent `virtio_gpu.c` already set by skipping
      its cursor queue), since this driver only ever plays audio. Init
      queries `PCM_INFO` on stream 0 and checks its direction is output
      before trusting it, rather than blindly assuming stream layout.
      Playback is `SET_PARAMS` + `PREPARE` + `START` on open, one
      3-descriptor tx chain (xfer header / data / status -- the same
      shape `virtio_blk.c`'s own request chain already uses) per
      4 KiB chunk on write, `STOP` + `RELEASE` on close. Only 16-bit
      signed PCM at 44100/48000 Hz, mono or stereo, is supported -- the
      one concrete combination this milestone needed.
      Four new syscalls (`docs/syscalls.md`), the first ones in the
      project with **no Linux equivalent** to reuse a number from
      (Linux does audio via `/dev/snd/*` + `ioctl`, and Anshi has no
      devfs): `audio_open`/`audio_write`/`audio_close` proxy directly
      to the driver, and `poll_key` -- a non-blocking keypress check --
      is `shell.c`'s own `read_line()` polling pattern
      (`virtio_input_poll_char()` then `serial_poll_char()`) lifted
      into a syscall, since a playback loop has to check for a control
      key without ever blocking the loop that feeds the audio device.
      **The actual discovery this milestone was about:** the kernel
      builds with `-mno-sse -mno-80387` everywhere and has no
      FPU/SSE context-switch support at all, so real MP3 decoding
      (float-heavy Huffman/IMDCT/synthesis-filterbank work) isn't
      reachable yet -- see M18.

- [x] **M18 -- `play`, an interactive WAV/PCM CLI player.** New
      `userland/play.c`: a small RIFF/WAVE chunk walker (not a library,
      just enough to find `fmt `/`data`, tolerating a `LIST`/`fact`
      chunk in between), a playback loop that reads 4 KiB chunks,
      applies **integer-only** volume scaling (`sample * volume / 100`
      -- deliberately no floating point, see M17's discovery above),
      and calls the new `audio_write()` syscall, polling `poll_key()`
      once per chunk for `space`/`q`/`n`/`+`/`-` controls. Raw terminal
      mode (`tcgetattr`/`tcsetattr` dropping `ICANON|ECHO`) is the same
      pattern `userland/termtest.c` already used. New
      `scripts/gen-test-tone.py` synthesizes a short sine-wave WAV at
      build time, embedded the same `.incbin` way ELF payloads are --
      there is still no host-side way to get an arbitrary file onto the
      VFS otherwise -- so `play testtone.wav` works with zero
      provisioning. `scripts/run-qemu.sh` now always passes
      `-device virtio-sound-pci`, defaulting `-audiodev` to QEMU's
      `wav` backend (captures to `Anshi-audio-out.wav` on the host) so
      this is boot-verifiable on any machine, real audio hardware or
      not -- the audio equivalent of M5's pixel-swatch screendump
      check. See [play.md](play.md).

- [x] **M19 -- pipes.** The user wanted real tiling terminals --
      independent processes side by side, not `scarf`'s reverted
      split-views-of-one-process approach. That needed a way to
      redirect a child's stdin/stdout away from the physical console,
      which didn't exist: `sys_write_impl`/`sys_read_impl` hardcoded fd
      0/1/2 straight to the keyboard/screen, bypassing the per-task file
      table entirely. New `kernel/src/exec/pipe.c/.h`: a small
      fixed-size ring buffer, deliberately **not** vnode-based (a pipe
      has no tree identity, and needs ref-counted open/closed state a
      vnode's whole-buffer-regrow write model doesn't fit) and
      deliberately **non-blocking on every operation, no exceptions** --
      `irq_handler()`'s preemption check (`arch/x86_64/idt.c`) only
      fires when the timer interrupts ring-3 code, never ring-0, so a
      busy-spin inside a syscall handler waiting on *another process* to
      produce data would never yield the CPU to that process. A real
      deadlock, not a style choice -- confirmed by reading `idt.c`
      directly before writing a line of the rest of this. Two new
      syscalls: `pipe()` (Linux's real syscall 22, real signature) and
      `use_as_stdio()` (Anshi-native, 904 -- narrower than general
      `dup2()`, which this project deliberately doesn't implement, since
      "make my stdio these two pipes before exec()" is the only actual
      need). See [syscalls.md](syscalls.md#pipes-m19).
      **Two real bugs, both found by testing:** a plain "is this end
      open" boolean broke the moment `fork()`'s existing whole-table
      shallow copy gave two processes independent references to the
      same pipe -- one process's `close()` of its own copy closed the
      *shared* object's end out from under the other, caught by a
      self-test (`pipetest.bin`) silently capturing zero bytes from an
      `exec()`'d child. Fixed with real reference counts,
      `process_fork()` bumping them for every pipe-backed entry it
      copies. See [tile.md](tile.md#two-real-bugs-both-found-by-testing-not-review)
      for the second one (no close-on-exec), found one milestone later
      as a genuine hang.

- [x] **M20 -- `sh`, a userland shell.** `kernel/src/shell/shell.c` is
      kernel-resident -- nothing to `exec()` into a pane. New
      `userland/sh.c` ports a deliberately narrow subset of it (`cd`,
      `pwd`, `ls`, `cat`, `write`, `create`, `echo`, `clear`, `help`,
      running programs via the same `fork()`/`execve()`/`waitpid()`
      composition `forktest.c` already proved) rewritten against
      syscalls instead of kernel-internal `vfs_*` calls -- `delete`/
      `copy`/`move`/`sync` and every kernel-debug builtin are explicitly
      out, needing syscalls (`unlink`/`rename`/an explicit sync) that
      don't exist. Its `read_line()` reads one byte at a time in a loop
      that treats a `0` return as "no data yet" -- identical whether fd
      0 is the real console (never actually returns 0, blocks
      internally) or a pipe (does, by M19's design) -- one code path,
      not two. See [tile.md](tile.md).

- [x] **M21 -- `tile`, a fixed-grid multiplexer.** New `userland/tile.c`:
      spawns N `sh` panes (`pipe()` x2, `fork()`, child closes what it
      doesn't need and `use_as_stdio()`s the rest before `execve()`,
      same pattern M19's own design settled on), tiled in a fixed 1/2/2x2
      grid -- not `scarf`'s reverted dynamic tree. Each pane's virtual
      terminal is scrollback-only, no ANSI parsing, since `sh` never
      emits cursor-addressing escapes -- running `scarf`/`play` as a
      pane is explicit future work, needing a real per-pane ANSI
      interpreter. `Ctrl-b` prefixes pane-switch/quit commands, tmux's
      own convention. **The second real bug this session found by
      testing, not review:** spawning a *second* pane's child inherits
      the *first* pane's pipes too (the same whole-table `fork()` copy
      M19 already had to account for once), and since it never
      references them by name it never closes them -- a phantom
      reference that kept the first pipe's refcount from ever reaching
      zero. Surfaced as a genuine hang (`Ctrl-b q` closing pane 1's
      stdin and waiting forever in `waitpid()`), diagnosed with
      temporary `kprintf` tracing in the kernel (serial output is
      unaffected by whatever's piped where, unlike the userland debug
      prints tried first, which vanished into an unread pipe). Fixed by
      having each new pane's child explicitly close every earlier
      pane's fds first -- real Unix has this exact problem, which is
      what close-on-exec exists to solve, and this project doesn't have
      it. See [tile.md](tile.md).

- [x] **M22 -- MP3 playback.** Per-process x87/SSE state: new
      `arch/x86_64/fpu.c` enables SSE at boot, and `dispatch()` eagerly
      `FXSAVE`s/`FXRSTOR`s each process's 512-byte state (in `struct
      usertask`) around every dispatch, including `wait()`'s nested one;
      `fork()` snapshots the parent's live registers and `exec()` loads a
      clean state. The kernel still builds `-mno-sse`; userland no longer
      does. `play` vendors minimp3 (CC0) and sniffs WAV vs. MP3 from the
      file's bytes. User stacks grew 16 -> 64 KiB, since minimp3 keeps a
      ~16 KiB scratch struct on the stack. Verified end to end with
      QEMU's `wav` audiodev: the guest's decoded output correlates 0.9999
      with the host's own decode of the same file, with no dropouts under
      TCG. See [play.md](play.md#mp3).

- [x] **M23 -- `play` rewritten in Rust + ratatui.** Rust userland now
      lives in one Cargo workspace, `userland/rust/`: `anshi/` is the
      shared runtime (FFI wrappers over the C libc, the global allocator
      on `malloc()`, the panic handler), and `play/` is a `#![no_std]`
      staticlib for `x86_64-unknown-none` that also holds its own C
      (`c/mp3.c`) and vendored minimp3 (`vendor/`). `build-userland.sh`
      builds the workspace and links each program with `crt0.o` and the
      C libc. Ratatui 0.30 runs without std; `play/src/backend.rs` (since
      moved to the shared `tui/` crate, M24) is a
      custom `Backend` that emits only the console's ANSI subset and
      ASCII symbols. **The console quirk this found:** writing the
      bottom-right cell wraps the cursor immediately and scrolls the
      screen, so the backend reports one row fewer than the console has.
      Verified the same way as M22: the guest's decoded output
      correlates 0.9999 with the reference, with no dropouts under TCG.
      See [play.md](play.md#design-notes).

- [x] **M24 -- `scarf` rewritten in Rust + ratatui.** `userland/rust/scarf/`
      ports the C editor with identical keys and behavior, split into
      buffer/sidebar/editor/ui modules. The ratatui backend moved out of
      `play` into a shared crate, `anshi-tui` (`userland/rust/tui/`),
      and the `anshi` runtime gained file writing, `chdir`/`getcwd`
      and directory listing. Ratatui's frame diffing replaces the C
      version's hand-kept sidebar-dirty tracking. New look in `play`'s
      style: solid focused-pane titles, a solid divider, a status bar
      with a mode chip, line numbers, and double-spaced lines on tall
      consoles (the 8x8 font has no leading). See [scarf.md](scarf.md).

- [x] **M25 -- `tile` rewritten in Rust, with a terminal per pane.**
      `userland/rust/tile/`: each pane's output goes through a terminal
      emulator (`vt.rs`) that matches the console's ANSI subset, and the
      panes are composed into one ratatui frame, so `scarf` and `play`
      run inside panes. Kernel: `TIOCSWINSZ` gives a process (and
      everything it runs) its own window size; `poll_key()` reads a piped
      stdin, returning -2 once it's closed; a `sched_yield()` syscall
      (24) lets loops polling empty pipes give up the CPU instead of
      spinning; and a piped process's exit line goes to serial only.
      `sh` loses its "no full-screen programs in a pane" guard and yields
      while waiting for input. **The two bugs this fixed in the C
      version:** pane output reached the console byte for byte (so
      `clear` in a pane wiped the screen), and the right column's last
      row wrote the screen's bottom-right cell, scrolling the whole
      display up a line on every redraw. Verified with `play` and
      `scarf` running side by side: the captured audio still matches
      the reference with no dropouts. See [tile.md](tile.md).

- [x] **M26 -- a new `sh`, and the system boots into it.**
      `userland/rust/sh/` replaces the C `sh`: a two-line prompt (colored
      blocks for the system, the working directory, the last command's
      duration and status), the command line highlighted as it's typed,
      grey suggestions from history, Tab completion of commands and paths
      with a selectable list, and history search on Up/Down. It has
      every file command (`ls`, `cat`, `mkdir`, `touch`, `rm`, `cp`,
      `mv`, `sync`, plus the kernel shell's spellings), so the kernel
      boots straight into it, falling back to the kernel shell if it
      exits. Kernel: `unlink`/`rename`/`copy`/`sync` syscalls over the
      kernel shell's own VFS operations (their `kprintf` errors kept
      off-screen), `clock_gettime` (monotonic), the 16 ANSI colors plus
      bold and dim in `fbconsole.c`, escape sequences for arrows/Home/
      End/Delete from the virtio keyboard, and the "process exited"
      line only for programs the kernel shell launched. The ratatui
      backend and tile's VT carry colors through; scarf learned the
      arrow keys. See [sh.md](sh.md).

- [x] **M27 -- prompt styles and `configure`.** `sh`'s prompt is drawn
      from settings (`/.sh_prompt`) chosen in a step-by-step wizard with
      live previews: Lean, Classic or Rainbow; one line or two; a frame;
      a dotted or solid connection; sharp, round or flat segment ends;
      glyphs or plain text; system name; spacing. For the arrows, rounds
      and icons, the console now decodes UTF-8 and has ~30 extra 8x8
      glyphs (`kernel/src/console/font8x8_ext.h`, generated from
      pictures by `scripts/gen-font-ext.py`), passed through by tile's
      VT and the ratatui backend. **The bug this found:** the ELF loader
      mapped a fresh zeroed page for every segment, so when two
      segments shared a page (rustc's `.got` right after `.rodata`) the
      later one wiped the end of the earlier one -- here, the integer
      formatting table, so every number the prompt printed came out as
      NUL bytes. `elf_load()` now writes a later segment's bytes into a
      page an earlier one already mapped. Also fixed:
      `build-userland.sh` failing when run from inside `scripts/`.

- [x] **M28 -- userland is all Rust.** `crt0.S`, the hand-written C
      libc and the C self-tests are gone. The `anshi` crate now makes
      the `int 0x80` syscalls itself (`sys.rs`), provides `_start`
      (`start.rs`) and allocates from the `brk` heap with its own
      first-fit free list (`heap.rs`, the kernel heap's design). The
      self-tests are one `selftest` crate; the build links it once per
      test with `--defsym=main=<name>_main`, so each test is still its
      own small ELF in `/bin`. The only C left in userland is minimp3:
      it does its float math with SSE, where a Rust decoder on the
      soft-float `x86_64-unknown-none` target couldn't, and it needs
      only the `memcpy`/`memmove`/`memset` that Rust's
      `compiler_builtins` already defines. See [rust.md](rust.md).

- [x] **M29 -- the kernel is Rust.** Moved over one subsystem at a time,
      booting after every step: the Rust crate (`kernel/`) started as a
      static library linked into the C kernel, each ported module
      exporting the C functions its `.c` file used to, so the C that was
      left called it unchanged. In order: the boot requests, serial and
      memory management; the GDT, PIC, LAPIC, FPU, timer and PCI; the
      virtio drivers and HD Audio; storage and USB; the console; the
      VFS; and last, together, the IDT, ring-3 entry, processes,
      syscalls, kernel shell and `kmain`, after which the C-facing
      shims came off. The kernel's assembly is now `global_asm!`
      (`arch/x86_64/stubs.rs`), the embedded userland is
      `include_bytes!` (`exec/blobs.rs`), `scripts/gen-font.py` writes
      Rust, and rustfmt replaced clang-format. Some designs changed
      where Rust made it natural: disks are a `BlockDriver` trait, USB
      devices are shared references with their state in `Cell`s (a
      transfer's callback re-enters the driver), and file contents are
      a `Vec<u8>`. **What the port turned up:** the C heap's 24-byte
      block header meant a `kmalloc()` payload was only guaranteed 8-byte
      alignment, whatever `HEAP_ALIGN` said (the Rust heap's header is 32
      bytes, so payloads really are 16-byte aligned); and the virtio-input
      device name includes its terminating NUL in its length, which the
      C hid by stopping at the NUL when printing. See
      [rust.md](rust.md#the-kernel).
- [x] **M30 -- networking.** The system gets online by itself: a
      virtio-net driver (`drivers/virtio/net.rs`) and an Intel e1000/
      e1000e one (`drivers/nic/e1000.rs`, the first non-virtio network
      card) behind a `NicDriver` trait, the smoltcp IP stack (`net/`,
      the kernel's first external crate) with DHCP at boot and again on
      every link change, and static addressing from `/etc/net.conf`.
      Sockets are fds (`socket`/`connect`/`bind` with Linux's numbers,
      plus `net_info`/`net_config`/`resolve` at 906-908), non-blocking
      like pipes and refcounted across `fork()`. Userland gets
      `anshi::net`, `ping`, `fetch` (plain HTTP) and `net`, a settings
      screen with a connection test. Verified end to end in QEMU on
      virtio-net, e1000 and e1000e: DHCP lease at boot, ping to the
      gateway and to example.com, `fetch` from a host HTTP server and
      from the internet, static/DHCP switching saved to disk, `net` in a
      `tile` pane, and a boot with no card. **What testing turned up:**
      q35 adds an e1000e of its own when the command line names no
      network, which the new driver promptly brought online --
      `ANSHI_NET=off` now passes `-nic none`; and on the e1000 the lease
      took about a second, past the first 400 ms boot wait, which is now
      1.5 s. See [net.md](net.md).
- [x] **M31 -- HTTPS, IPv6, servers, every card active.** `fetch`
      speaks TLS 1.2/1.3 through rustls with RustCrypto (pure Rust, no
      std), checking certificates against Mozilla's roots, for the host
      name, at the time from a new CMOS RTC driver; the kernel gained a
      ChaCha20 generator seeded from RDSEED/RDRAND, a new virtio-rng
      driver and timing jitter, behind `getrandom()`. IPv6: link-local
      plus SLAAC addresses, ICMPv6 ping, `sockaddr_in6` with scopes, AAAA
      lookups, `[addr]` URLs. Servers: `listen`/`accept` (a backlog of
      smoltcp listeners per interface), `sendto`/`recvfrom` (the first
      six-argument syscalls, `r10`/`r8`/`r9`), automatic echo replies,
      and `serve`, an HTTP file server. Every card is now its own active
      interface with its own DHCP and `/etc/net.conf` section, traffic
      routed by subnet then default route, and `net` configures each.
      Also: the user heap cap went from 4 to 16 MiB, and `blkfs` no
      longer saves the programs the kernel rewrites into `/bin` at every
      boot (8.7 MB instead of 10.8 MB on the dev disk). Verified in QEMU:
      HTTPS to example.com and an http-to-https redirect from github.com;
      expired, wrong-host and self-signed certificates refused; ping and
      fetch over IPv6, link-local with a scope; `serve` reached from the
      host through a port forward, `..` refused; two cards on two
      networks, each routing its own traffic and configured separately.
      **What testing turned up:** IPv6 didn't work at all on the e1000:
      its receive filter passed broadcast but no multicast, and IPv6
      runs on multicast (router advertisements, neighbor discovery) --
      it now takes all multicast. And `--pc` mode's QEMU CPU had no
      RDRAND, so TLS keys would have come from timing jitter alone; it
      now has RDRAND/RDSEED, as the real PC does. See [net.md](net.md).

**Explicitly out of scope for now:** making virtio interrupt-driven
(their PCI interrupt routing is a separate concern from the ISA IRQ0-15
path above), APIC/IOAPIC beyond the minimal LINT0 passthrough above (no
I/O APIC redirection table use, no SMP), a real on-disk filesystem
format (ext2/FAT/UFS — M9's format above is a whole-tree dump, not an
incremental one), copy-on-write `fork()`, `envp` for `exec()`,
dynamic linking/shared libraries, TLS, W^X/NX enforcement, the
`SYSCALL`/`SYSRET` MSR fast path (`int 0x80` only for now), signals,
`O_EXCL`/`rmdir`/`unlink` from userland,
an `init` process/orphan reparenting, priority scheduling (Phase B is
plain round-robin), an actual musl (or similar) port capable of
building arbitrary third-party C source — userland has no C library at
all since M28, only the Rust `anshi` runtime — using FPU/SSE inside the kernel itself (M22
gave userland per-process FPU state; kernel code stays soft-float), and
`XSAVE`/AVX state beyond what `FXSAVE` covers; general `dup2()`
and close-on-exec (M19-M21 use a narrower `use_as_stdio()` instead, and
tile's spawn logic closes inherited fds by hand — see
[tile.md](tile.md#two-real-bugs-both-found-by-testing-not-review)). These are natural next milestones from
here.
