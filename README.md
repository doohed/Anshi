# Anshi

A hobby x86_64 operating system, booted via UEFI through
[Limine](https://github.com/limine-bootloader/limine), with a from-scratch
kernel that talks to hardware exclusively through **virtio** devices (QEMU
is the target platform). Kernel and userland are both written in Rust. This is a rewrite of an earlier 2024 legacy-BIOS/
32-bit prototype — that version is still in git history, but the tree now
targets UEFI + long mode only.

## Documentation

| Document | Contents |
|---|---|
| [docs/architecture.md](docs/architecture.md) | how the system is put together: boot, memory, drivers, processes, syscalls, filesystem, console |
| [docs/building.md](docs/building.md) | building and running -- natively, in the container, and under UTM |
| [docs/sh.md](docs/sh.md) | the shell the system boots into: prompt, suggestions, completion, the language, commands |
| [docs/shell.md](docs/shell.md) | the kernel-resident fallback shell, the serial console, scripted testing |
| [docs/syscalls.md](docs/syscalls.md) | the syscall table, the initial stack layout, terminal handling |
| [docs/scarf.md](docs/scarf.md) | the text editor: keys, design notes, performance |
| [docs/play.md](docs/play.md) | the WAV/MP3 player: screen, keys, MP3 decoding, the spectrum analyzer |
| [docs/tile.md](docs/tile.md) | the terminal multiplexer: panes, keys, how a pane's terminal works |
| [docs/rust.md](docs/rust.md) | the Rust kernel and userland: how each is laid out and built, the runtime, ratatui |
| [docs/roadmap.md](docs/roadmap.md) | milestone history -- what arrived when, and what went wrong building it |
| [docs/net.md](docs/net.md) | networking: IPv4 and IPv6 on every card at boot, the `net` screen, `ping`, `fetch` and `curl` (HTTPS) and `serve`, the drivers and IP stack |
| [docs/real-hardware.md](docs/real-hardware.md) | the plan for booting on a real PC: framebuffer, keyboard, disk, audio drivers |

## Quick start

On x86_64 Linux:

```sh
sudo apt-get install -y qemu-system-x86 qemu-utils ovmf xorriso mtools python3
rustup target add x86_64-unknown-none   # Rust from https://rustup.rs
git submodule update --init --recursive
./scripts/build-iso.sh   # build userland, kernel, and assemble Anshi.iso
./scripts/run-qemu.sh    # boot it (UEFI/OVMF, virtio-gpu-pci, serial on stdout)
```

On anything else -- an Apple Silicon Mac, say -- `make docker` builds a
container with a clang cross-toolchain and Rust, and drops you into it
with the repo mounted at `/work`; the two scripts above are still the
entry points. See [docs/building.md](docs/building.md).

It boots into [sh](docs/sh.md), a shell with a colored two-line prompt,
highlighting as you type, history suggestions and Tab completion -- and a
scripting language: pipes, redirections, variables, loops, functions. The
console doubles as a real serial terminal, so **typing into the terminal
you launched from talks to the system** too. Try:

```
> help
> ls /bin
> scarf .
> play testtone.wav
> tile
> net
> curl -s https://example.com/ | grep -i title
> for f in /bin/p*; do echo "$f"; done
```


## Repo layout

```
Anshi/
├── docs/                  # everything in the table above
├── docker/Dockerfile      # arm64-native cross-build/run environment
├── limine/                # git submodule: prebuilt Limine UEFI bootloader
├── limine.conf            # Limine boot menu, points at the kernel
├── GNUmakefile            # thin wrapper around scripts/*.sh
├── scripts/
│   ├── build-iso.sh         # builds userland + kernel, assembles Anshi.iso
│   ├── build-userland.sh    # builds the ring-3 programs (Rust), embeds them in the kernel
│   ├── run-qemu.sh          # boots an already-built Anshi.iso in QEMU
│   ├── docker-shell.sh      # builds the container and drops into it
│   ├── format.sh            # rustfmt over the kernel (apply or --check)
│   ├── gen-test-tone.py     # synthesizes the testtone.wav fixture for play
│   ├── gen-font.py          # generates the console font (font.rs) from Spleen
│   └── disk-put.py          # writes host files straight onto Anshi-disk.img
├── userland/              # ring-3 programs, all Rust
│   ├── link.ld              # static ELF at 0x400000, entry _start
│   └── rust/                # Cargo workspace for every program (docs/rust.md)
│       ├── anshi/             # runtime: _start, int 0x80 syscalls, allocator, panic handler
│       ├── tui/               # anshi-tui: the ratatui console backend
│       ├── sh/                # the shell the system boots into (docs/sh.md)
│       ├── play/              # the WAV/MP3 player (docs/play.md)
│       ├── scarf/             # the text editor (docs/scarf.md)
│       ├── tile/              # the terminal multiplexer (docs/tile.md)
│       └── selftest/          # hello, crash, and the other self-test payloads
└── kernel/                 # one Rust crate (docs/rust.md#the-kernel)
    ├── Cargo.toml           # a staticlib for x86_64-unknown-none
    ├── GNUmakefile          # cargo, then the link with linker.ld
    ├── linker.ld            # higher-half link script (Limine protocol layout)
    └── src/
        ├── main.rs                    # kmain: init order, self-tests, boots /bin/sh
        ├── sync.rs mmio.rs            # Global<T>; volatile device access
        ├── boot/requests.rs           # Limine boot requests
        ├── arch/x86_64/
        │   ├── gdt.rs idt.rs            # GDT/TSS, IDT, exception + IRQ dispatch
        │   ├── stubs.rs                 # all the assembly: entry stubs, ring-3 transition
        │   ├── usermode.rs              # entering/resuming a program in ring 3
        │   ├── pic.rs lapic.rs          # 8259 remap + LAPIC LINT0/ExtINT passthrough
        │   ├── fpu.rs                   # SSE enable, per-process FPU state
        │   └── io.rs                    # port I/O
        ├── mm/
        │   ├── pmm.rs                   # bitmap physical page allocator
        │   ├── vmm.rs                   # page tables: MMIO, address spaces, fork/free
        │   ├── heap.rs                  # the global allocator, first-fit over the PMM
        │   └── dma.rs                   # zeroed, contiguous memory for devices
        ├── exec/
        │   ├── elf.rs                   # static ELF64 loader + initial stack
        │   ├── process.rs               # process table, scheduler, fork/exec/wait
        │   ├── pipe.rs                  # non-blocking pipes (docs/syscalls.md)
        │   ├── syscall.rs               # int 0x80 dispatch (docs/syscalls.md)
        │   └── blobs.rs                 # the built ring-3 programs, include_bytes!
        ├── drivers/
        │   ├── serial.rs                # COM1 + the kernel log (kprintln!)
        │   ├── timer.rs acpi.rs pit.rs  # the tick: LAPIC timer, PM timer, PIT
        │   ├── pci.rs                   # config-space PCI enumeration
        │   ├── display.rs input.rs      # whichever screen and keyboards there are
        │   ├── keymap.rs tty.rs         # US keymap; termios/winsize (Linux's)
        │   ├── audio.rs hda.rs          # sound: virtio-sound or HD Audio
        │   ├── block.rs nvme.rs ahci.rs # disks, read-only unless unlocked
        │   ├── partition.rs disktest.rs # lsblk's partition report; disktest
        │   ├── usb/                     # xHCI, HID keyboards, mass storage
        │   └── virtio/                  # transport, gpu, input, blk, snd
        ├── console/
        │   ├── fbconsole.rs             # bitmap-font console + ANSI/CSI parser
        │   ├── splash.rs                # boot splash
        │   ├── font.rs                  # the 8x16 font, from scripts/gen-font.py
        │   └── spleen/                  # vendored Spleen 8x16 font (BSD 2-Clause)
        ├── fs/
        │   ├── vfs.rs                   # in-memory directory/file tree
        │   └── blkfs.rs                 # whole-tree persistence over virtio-blk
        ├── shell.rs                     # kernel-resident fallback shell (docs/shell.md)
        └── util/crc32.rs                # the CRC GPT checks
```


`make`/`make run`/`make docker`/`make format` are thin wrappers around
`scripts/*.sh`, which are the actual source of truth for how each step
works and can be run directly.
