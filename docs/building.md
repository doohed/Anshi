# Building and running


Needs `qemu-system-x86_64`, `qemu-img`, OVMF firmware, `xorriso`,
`mtools`, and `python3` (`scripts/gen-test-tone.py`/`scripts/disk-put.py`
synthesize/inject audio assets -- see [play.md](play.md) -- and need real
binary struct packing and float math bash doesn't have), plus a C
compiler and `ld` -- `cc` for the one C file left, `play`'s MP3 decoder,
and `ld` to link the kernel and every program:

```sh
sudo apt-get install -y qemu-system-x86 qemu-utils ovmf xorriso mtools python3
```

Everything else is Rust -- the kernel (`kernel/`) and every program (the
`userland/rust/` workspace), see [rust.md](rust.md) -- so the build also
needs a Rust toolchain from [rustup](https://rustup.rs) plus the
bare-metal x86_64 target. The first build downloads userland's crates
(ratatui and its dependencies) from crates.io; the kernel has none:

```sh
rustup target add x86_64-unknown-none
```

Building and running are two separate scripts (`make`/`make run` are thin
wrappers around them, if you'd rather not call them directly):

```sh
./scripts/build-iso.sh   # build userland, kernel/bin/kernel, and assemble Anshi.iso
./scripts/run-qemu.sh    # boot the already-built ISO (UEFI/OVMF, virtio-gpu-pci, serial on stdout)
```

`./scripts/run-qemu.sh --pc` boots it the way a real PC looks instead:
no virtio devices, a plain VGA card whose UEFI GOP framebuffer is the
only display, 4 GiB of RAM, 6 CPUs, a USB keyboard on an xHCI
controller (type into the QEMU window, or the serial console), and
Intel HD Audio with a line-out codec (`play testtone.wav`). It's
where real-hardware drivers get tested first (see
[real-hardware.md](real-hardware.md)).

`run-qemu.sh` does **not** build anything itself — it errors out if
`Anshi.iso` doesn't exist yet, telling you to run `build-iso.sh` first.
It auto-detects the local OVMF firmware layout (combined `OVMF.fd` or split
`OVMF_CODE`/`OVMF_VARS`). It also creates a blank 16 MiB `Anshi-disk.img`
next to the ISO on first run (via `qemu-img`, gitignored) and attaches it
as `virtio-blk-pci` for the M9 persistent filesystem — delete the file to
reset storage back to blank.

## Building on a host that isn't x86_64 Linux

The `apt-get` line above assumes an x86_64 Linux host, where `cc`/`ld`
already are the right toolchain. Everywhere else — an Apple Silicon Mac
being the case this was set up for — `docker/Dockerfile` supplies the
environment instead, and `make docker` builds it and drops you into a
shell with the repo bind-mounted at `/work`:

```sh
make docker                       # or ./scripts/docker-shell.sh
# then, inside:
git submodule update --init --recursive   # first time only (Limine)
./scripts/build-iso.sh
./scripts/run-qemu.sh
```

The container is an environment, not a second build system — the two
scripts above are still the entry points, unchanged. Nothing in the image
is emulated either: it's a native arm64 Debian, pinned to a dated tag so
the toolchain can't drift, because both halves of the problem can be
solved without a foreign-arch userland. clang is a cross-compiler by
construction and will emit x86_64 given `--target=x86_64-unknown-none`
(so no cross-gcc to build or install, and `ld.lld` links the result with
the same `-m elf_x86_64` the GNU `ld` invocation already used), while
`qemu-system-x86_64` emulates the guest CPU in TCG no matter what it's
running on. TCG means no HVF/KVM acceleration — boot takes a second or
two rather than being instant, which for a kernel this size is not worth
caring about.

`CC`/`LD` come from the image's `ENV`, so nothing needs configuring
inside it; `kernel/GNUmakefile` and `scripts/build-userland.sh` both
default to `cc`/`ld` when those aren't set, and the userland script only
adds the `--target` flag when the compiler actually reports itself as
clang, so a native x86_64 gcc build gets nothing extra. The `ovmf` package is worth a note too: it's
`Architecture: all` in Debian (the firmware is x86 machine code, but it's
shipped as data, not as an executable the host has to run), so it
installs on arm64 unchanged and lands in `/usr/share/OVMF/` as the split
`OVMF_CODE_4M.fd`/`OVMF_VARS_4M.fd` pair that `run-qemu.sh`'s existing
auto-detection already looks for first.

The image also carries a Rust toolchain for the kernel and
`userland/rust/`, installed with rustup rather than apt: Debian's `rustc`
ships no prebuilt `core`/`alloc` for `x86_64-unknown-none`. Like clang,
it's a native arm64 toolchain targeting x86_64, so nothing is emulated
there either.

The container runs as root, so files it creates in the bind mount are
root-owned. On Docker Desktop for Mac that's invisible — the VirtioFS
mount maps ownership back to you — but on a Linux host you'll want to
`chown` build output afterwards or run the container with `--user`.

## Code style

The kernel is formatted with rustfmt, set up by `kernel/rustfmt.toml`
(120 columns, `use_small_heuristics = "Max"`). `scripts/format.sh`
applies it; the generated font tables (`console/font.rs`) opt out with
`#[rustfmt::skip]`:

```sh
rustup component add rustfmt
./scripts/format.sh            # reformat in place
./scripts/format.sh --check    # verify only, non-zero exit if anything's off (CI-friendly)
```

## Running under UTM

UTM is a GUI wrapper around the same QEMU, but its defaults differ from
what `run-qemu.sh` passes, and Anshi has no PS/2 or USB HID driver
yet, so UTM's default input devices are invisible to it. In VM Settings:

- **System -> Machine:** `q35`, with **UEFI Boot** enabled. There is no
  legacy BIOS path.
- **Devices -> Display:** `virtio-gpu-pci`. Anything else that leaves a
  UEFI GOP framebuffer (`virtio-vga`, plain VGA) also works, through the
  firmware-framebuffer fallback, but redraws are slower.
- **QEMU -> Arguments:** add `-device` and `virtio-keyboard-pci` as two
  separate entries (each row is one argv token). Without a virtio
  keyboard the shell still starts, but only the serial console can type
  into it.
- **Devices -> New -> Serial.** Needed for the serial console. Without
  one, `serial_init()` notices there's no UART at COM1 and the kernel
  just doesn't use it.
