# Real hardware

The plan for booting Anshi on a real PC instead of QEMU. The target
machine is a Ryzen 5 5600G on a MAXSUN B550M board with an RX 5600 XT,
but almost everything here applies to any modern x86_64 UEFI PC.

The kernel was C when most of this was done, and has since moved to Rust
(M29). The finished phases' step-by-step plans below keep the C names
they were written with; the drivers they describe are the same ones,
now `kernel/src/**/*.rs` (`drivers/display.c` is `drivers/display.rs`,
`drivers/usb/xhci.c` is `drivers/usb/mod.rs`, and so on).

## Where things stand

Phase 1 is done: the real PC boots to the shell on its own screen
(third attempt, see step 1d). Everything else still only has a virtio
driver:

| Need | Today | On the real PC |
|---|---|---|
| Screen | virtio-gpu, or the GOP framebuffer (`drivers/display.rs`) | **Done:** the RX 5600 XT's GOP framebuffer, handed over by Limine |
| Keyboard | virtio-input, USB (`drivers/usb/`), or COM1 | **Done:** USB via xHCI (the board has no PS/2 port) |
| Disk | virtio-blk; NVMe, SATA and USB storage read-only (`drivers/block.rs`) | Installed onto a whole dedicated drive -- drivers **work in QEMU, untested on the PC**; installer next |
| Audio | virtio-sound, or HD Audio (`drivers/hda.rs`) | The board's HD Audio analog outputs -- **works in QEMU, untested on the PC**; the GPU's HDMI/DP audio needs a GPU driver |
| Timer | LAPIC timer calibrated against the ACPI PM timer; PIT through the 8259 as a fallback (`drivers/timer.rs`) | The PIT path hung the first real boot; the LAPIC timer needs no routing |
| Other interrupts | 8259 PIC through LAPIC LINT0 (unused: every driver polls) | IO-APIC or MSI-X once drivers use interrupts |
| Debug log | COM1 (`-serial stdio`), skipped when no UART answers | Only if the board has a COM header; otherwise the screen |

Things that already carry over: Limine boots over UEFI, the ISO is a
hybrid image with an EFI boot partition (so it can be written straight to
a USB stick), the PMM sizes itself from the memory map (fine for 16-64
GiB), `vmm::map_mmio()` handles BARs anywhere in the address space,
the virtio transport already reads 64-bit BARs, and `reboot`'s triple fault works
on any x86 CPU.

Out of scope: a real `amdgpu` driver for the RX 5600 XT or the 5600G's
integrated graphics. That needs signed firmware loaded into several
on-chip processors, the display engine and power management; Linux's
version is one of the largest drivers in the kernel. The GOP framebuffer
gives a working screen at the monitor's resolution without it. No mode
switching and no acceleration is the trade-off.

## Ground rules

- **Every driver is built and tested in QEMU first.** QEMU emulates real
  controllers too -- `nvme`, `qemu-xhci` + `usb-kbd`, `intel-hda`, plain
  GOP -- so the real PC only ever runs code that already works.
- **Polling first, interrupts later**, the same as the virtio drivers.
  Polling avoids needing IO-APIC/MSI routing before anything else works.
- **Nothing writes to a real disk until the partition safety checks in
  phase 4 exist.** blkfs today writes from sector 0, which would destroy
  the partition table of whatever drive it's pointed at.
- **The virtio path stays.** Each new driver is a second implementation
  behind the same interface, picked at boot by what PCI enumeration
  finds.

## Phase 0 -- Survey the machine

Before writing drivers, find out exactly what's on the board. Boot a
Linux live USB (any distro) on the PC and save:

```sh
lspci -nnk > lspci.txt        # every PCI device with vendor:device IDs
lsusb -t   > lsusb.txt        # USB tree: which controller the keyboard is on, any hubs
sudo dmesg > dmesg.txt
sudo cat /proc/asound/card*/codec#* > codecs.txt   # HDA codec widget graph
```

Also check the board manual for a **PS/2 port** and a **COM (serial)
header**. Both make the first boots far easier. Then in the firmware
setup: **Secure Boot off** (Limine isn't signed), **CSM off** (UEFI only).

These files decide phase 3 (PS/2 or xHCI first), phase 4 (NVMe or AHCI)
and phase 6 (which codec and pins).

## Phase 1 -- Device interfaces and the GOP framebuffer

**Status: done, on the real PC too.** 1a-1c verified in `run-qemu.sh
--pc` (1920x1080 GOP, 4 GiB, no virtio devices, with and without a
serial port): splash, scrolling log, `sh`, and `scarf` full-screen. 1d
took three boots on the real PC, below. One fix beyond the plan: `serial::init()` now
checks a UART is really at COM1, because a missing one reads as an
endless stream of phantom `0xFF` key presses -- which the shell, now
running without a keyboard, would otherwise have received.

**First real boot (1d), attempt 1: hung on the splash.** The logo and
caption appeared, then nothing -- the first `timer_sleep_ms()` in the
splash animation never returned. The timer was the PIT's IRQ0 through
the 8259 and LAPIC LINT0, which QEMU wires up and this board evidently
doesn't (or the firmware left the LAPIC in x2APIC mode, where the old
MMIO setup is silently ignored -- no serial port, so no way to tell
which). Phase 5's timer item was pulled forward to fix it:
`drivers/timer.rs` now runs the Local APIC timer, in xAPIC or x2APIC
mode, calibrated against the ACPI PM timer (`drivers/acpi.rs`) or PIT
channel 2 polled, and checks after `sti` that ticks really arrive --
falling back to the PIT, and failing that to busy-wait sleeps, rather
than hanging. Each path was forced and verified in QEMU, and the boot
log now shows a `Timer:` line on screen saying which one is in use.

**Attempt 2: past the splash, then `vfs: out of memory` and a page
fault.** The timer worked (LAPIC, xAPIC, ARAT). The PMM returns 0 for
"out of memory", but never reserved physical page 0 -- and this PC's
firmware reports the first 640 KiB, from address 0, as usable RAM,
which QEMU's never does. So the heap's first page-run allocation got
page 0, read it as a failure, and the filesystem root was never
created. `pmm::init()` now always reserves page 0; page-fault dumps also
print `cr2` (the faulting address), and a missing VFS root stops boot
with a message instead of a fault.

**Attempt 3: boots to the `sh` prompt.** No keyboard yet (phase 3).

The goal: boot on the PC and see the kernel log and the shell on screen.

**1a. Split the virtio assumptions out of the generic code.**

- Replace `struct virtio_gpu_fb` with a generic `struct framebuffer`
  (`pixels`, `width`, `height`, `pitch` in pixels) in
  `drivers/display.h`, flushed through `display_flush_rect()`. `fbconsole.c` and `splash.c` index with
  `y * pitch + x` instead of `y * width + x` -- a GOP framebuffer's
  pitch is often wider than its visible width.
- Add a `drivers/input.c` with `input_poll_char()` that polls every
  registered keyboard source (virtio-input now, PS/2 and USB later) and
  COM1. `shell.c`'s `read_line()` and the two polling sites in
  `syscall.c` call it instead of `virtio_input_poll_char()` +
  `serial_poll_char()`.
- In `kmain`, stop gating the VFS, `/bin` and the shell on
  `virtio_input_init()` succeeding. A machine with no keyboard driver
  yet should still boot to the shell (useful for watching boot logs).

**1b. Fall back to the GOP framebuffer.** When there's no virtio-gpu,
build the `struct framebuffer` from Limine's `framebuffer_request`
(already requested in `boot/requests.c`):

- Limine maps the framebuffer into the HHDM, so `address` is directly
  usable. Check `bpp == 32` and the red/green/blue mask shifts (16/8/0
  is the BGRX layout fbconsole already writes); refuse anything else.
- Draw into a **shadow buffer in RAM** and have `flush` copy it to the
  framebuffer. Reading video memory over PCIe is extremely slow, and
  fbconsole scrolls by reading back what's on screen. Copying only the
  rows that changed since the last flush keeps it fast at 1440p/4K.
- **Scale the font.** 8x8 glyphs at 1920x1080 is 240 columns of tiny
  text. Draw each glyph 2x (or 3x on 4K) when the framebuffer is wide.

**1c. A QEMU profile that looks like the PC.** Add an option to
`run-qemu.sh` (e.g. `--pc`) that drops every virtio device and uses
`-vga std` instead. It grows a device per phase: `-device nvme`,
`-device qemu-xhci -device usb-kbd`, `-device intel-hda -device
hda-output`, `-smp 6 -m 4G`. Boot-verify each phase there with a
screendump, the same way the roadmap milestones were verified.

**1d. First real boot.** Write `Anshi.iso` to a USB stick (`dd`, or
balenaEtcher), boot it from the firmware boot menu, and photograph the
screen. Expected: splash, kernel log, the PCI device list, and a shell
with no keyboard yet. Compare the PCI list against `lspci.txt`.

## Phase 2 -- Debugging without a serial port

If the board has no COM header, a crash on real hardware is otherwise
invisible.

- Bring the framebuffer console up **before** PCI enumeration (the GOP
  path doesn't need PCI), so early panics land on screen.
- Keep a kernel log ring buffer from the first log line, replayed onto
  the screen once fbconsole is up, and add a `dmesg` builtin.
- Make the exception handler's register dump fit on one screen, and stop
  there instead of scrolling it away.

## Phase 3 -- Keyboard

**Status: 3b done, and typing works on the real PC** (first boot with
it). Built against `run-qemu.sh --pc`, which now has a `qemu-xhci`
controller with a `usb-kbd`. 3a was skipped:
the board has no PS/2 port. Verified by typing through QMP: plain and
shifted characters, arrows and Delete through `sh`'s line editor,
Backspace, Ctrl-U, key repeat (19 characters from a 1.2 s hold), unplug
and replug (hot-plug), and a keyboard behind a hub reported as
unsupported instead of failing silently. On the PC, the boot log lists
each xHCI controller and each device found (`usb: port N: vendor:product,
speed, class`), which is what to photograph if the keyboard doesn't work.

Key repeat turned up a latent bug: a blocking `read()` spun in the
kernel with interrupts off (`int 0x80` is an interrupt gate), so the
tick -- and with it the uptime -- stopped while a program waited for a
key. The read loops now let interrupts in while they wait.

**3a. PS/2 (i8042)**, about 150 lines: init the controller, enable the
keyboard port, register IRQ1 with `irq_register()` (the PIC path already
works), translate scan code set 1 to the same bytes and escape sequences
virtio-input produces. Works with a PS/2 keyboard, and sometimes with a
USB keyboard when the firmware's "USB legacy support" emulates one --
convenient for early boots, not something to rely on.

**3b. xHCI + USB HID keyboard**, the biggest driver in this plan (a few
thousand lines):

1. Find every controller with class `0C.03` prog-if `30`. The 5600G and
   the B550 chipset each have their own, so drive all of them.
2. **BIOS handoff** through the USB Legacy Support extended capability.
   Skipping this leaves the firmware's SMM code fighting the driver.
3. Reset the controller; set up the device context base array, the
   command ring, and one event ring (polled).
4. For each connected port: reset it, Enable Slot, Address Device, read
   the device and configuration descriptors.
5. For a HID keyboard: Set Configuration, Set Protocol (boot protocol),
   then poll its interrupt-IN endpoint for 8-byte boot reports and turn
   key-down transitions into bytes, plus key repeat driven by the timer
   tick. HID usages are converted to Linux key codes so the virtio
   keyboard and this one share one keymap (`drivers/keymap.rs`).

First version supports a keyboard plugged **directly** into a rear port,
including after boot. USB hubs (including ones inside monitors, and some
front-panel wiring) come after, as a follow-up: a hub is logged as
unsupported and skipped.

## Phase 4 -- Disk, and an installer

The goal changed: instead of a partition carved out of an existing
drive, the live USB gets an **installer** that puts Anshi on a whole
dedicated drive -- a fresh GPT with an EFI system partition (Limine and
the kernel) and an Anshi data partition (blkfs) -- which then boots on
its own with persistent files. There's no spare drive yet, so the
storage drivers came first, tested on the PC read-only.

**4a. Storage drivers, read-only on the PC -- done in QEMU.**

- `drivers/block.rs`: every disk behind one interface, bounds-checked,
  in the disk's own sector size. **Writes are locked**: a block write
  refuses unless the kernel command line (limine.conf `cmdline:`) has
  `allow-disk-writes`, which only test ISOs carry -- `Anshi.iso` never
  does. virtio-blk (`Anshi-disk.img`) isn't part of this layer.
- `drivers/nvme.rs` (nvme0, ...), `drivers/ahci.rs` (sata0, ...; CD/DVD
  drives skipped) and `drivers/usb/storage.rs` (usb0, ...; Bulk-Only
  Transport + SCSI, on new bulk transfers in `drivers/usb/mod.rs`).
  Bringing any of them up only resets controllers and asks questions;
  a drive's data is only touched by reads and (unlocked) writes.
- `drivers/partition.rs`: GPT (both CRCs checked) or MBR, each partition
  with its type, size, name and filesystem (NTFS, FAT, exFAT, ext,
  BitLocker, btrfs, XFS, LUKS, ISO 9660, blkfs). Printed at boot and by
  the kernel shell's `lsblk`.
- `disktest <disk>` (kernel shell: `exit` from `sh`) reads the first MiB
  twice and compares, then times 64 MiB of reads. `disktest <disk> -w`
  (only when unlocked) saves a 1 MiB region, writes a pattern, reads it
  back, then restores and verifies the original.

Verified in QEMU on a 256 MiB GPT test image (FAT32 EFI partition plus
NTFS- and ext4-signed ones) on each of NVMe, SATA and USB: the right
partitions, types, sizes, names and filesystems with both GPT CRCs ok,
identical double reads, and with writes unlocked the write check passed
and left each image byte-for-byte unchanged. A copy of the ISO as a USB
stick shows up as the hybrid ISO it is. On the PC, the boot log's
`nvme`/`ahci`/`usb-storage` lines and partition listings (and
`disktest`) are the test.

**4b. The installer -- next.**

1. GPT writing (protective MBR, both headers and arrays, CRCs).
2. A FAT32 formatter: the EFI system partition with `EFI/BOOT/BOOTX64.EFI`
   (Limine, embedded in the kernel like `testtone.wav`), `limine.conf`
   and the kernel -- which Limine hands the kernel its own copy of, so
   nothing has to be read back from the USB stick.
3. blkfs on the data partition through the block layer, found at boot by
   its partition GUID: the installed `limine.conf` passes `root=<GUID>`,
   the live USB passes nothing and stays in RAM.
4. An `install` program: pick a disk (model, size, current partitions),
   type its name to confirm, progress -- the one place the write lock is
   lifted, for that one disk.

Testable end to end in QEMU: install to an NVMe image, then boot QEMU
from that image alone and check files survive a reboot.

## Phase 5 -- Platform

Not strictly required for the phases above, but they make the system
behave correctly rather than by luck:

- **ACPI tables:** `drivers/acpi.rs` already walks the XSDT/RSDT for the
  FADT's PM timer; still to read: MADT for the IO-APIC and the CPU list,
  the FADT's reset register.
- **IO-APIC** instead of the 8259 through LINT0, or **MSI-X** per device
  once drivers move from polling to interrupts.
- **Timer:** done early, see phase 1d above -- the LAPIC timer, which
  SMP will need per CPU anyway.
- **SMP** (using all 6 cores of the 5600G) is a project of its own --
  the scheduler and every shared kernel structure assume one CPU.

## Phase 6 -- Audio

**Status: done in QEMU** (`run-qemu.sh --pc` now has `intel-hda` +
`hda-output`), waiting on the real PC. `play testtone.wav` captured
through QEMU's wav backend matches the source sample for sample on both
channels (mono is played on both), twice in a row, in 2.0 s for the
2-second tone -- so the pacing is real time. Analog outputs only: the
RX 5600 XT's HDMI/DP audio controller is found but skipped, since the
GPU's display engine has to be told to carry audio. On the PC the boot
log lists each controller, the codec (`hda: codec N: vendor:device`),
and each routed output (`hda: line out (pin 0x14) <- DAC 0x2`).

Two things learned building it: the controller only delivers RINTCNT
responses until the driver acknowledges them in RIRBSTS (an interrupt
handler's job elsewhere), and the response flag behind that only rises
with the RIRB interrupt enable set -- so it's set, while the global
interrupt enable stays off. A codec that stops answering is given up on
after one timeout, so broken audio hardware can't stall the boot.

An Intel HD Audio driver behind an `audio_open/write/close` interface
that `play`'s syscalls already match (S16LE, 44.1/48 kHz, stereo):

1. Find controllers with class `04.03`. The board's analog audio and the
   RX 5600 XT's HDMI/DP audio are separate controllers.
2. Reset, set up the CORB/RIRB command rings, enumerate codecs.
3. Walk the codec's widget graph from an output pin (line out or
   headphones on the board; the display pin on the GPU) back to a DAC,
   set amplifier gains and unmute along the path.
4. One output stream with a buffer descriptor list. Pace `write` with
   the stream's position register, which is what makes playback run in
   real time, as `virtio_snd_write()` does today.

Roughly 1000-1500 lines. `codecs.txt` from phase 0 shows the codec's
exact widget layout, which makes step 3 much easier to get right.

## Later

- Network: the board's Ethernet controller (check `lspci.txt`; usually a
  Realtek RTL8111-family part).
- USB hubs, USB mass storage (boot stick as a writable disk), mouse.
- ACPI power-off (needs an AML interpreter -- large).

## Order and size

| Phase | Result on the PC | Size |
|---|---|---|
| 0 | Device list, firmware set up | an evening |
| 1 | Boots to a readable screen | small |
| 2 | Crashes are visible | small |
| 3a | Typing works (PS/2 or legacy emulation) | small |
| 3b | Typing works with any USB keyboard | large |
| 4 | Files persist on the NVMe | medium |
| 5 | Correct interrupt routing, ACPI reset | medium |
| 6 | `play` works through speakers or HDMI | medium |
