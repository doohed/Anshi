# play

An interactive WAV/MP3 player with a [ratatui](https://ratatui.rs) UI,
written in Rust (`userland/rust/play/`). It plays a playlist of files over the
`virtio-sound` driver (`kernel/src/drivers/virtio/snd.rs`, M17) -- or
Intel HD Audio on a real PC (`kernel/src/drivers/hda.rs`).

```
> play testtone.wav                # the built-in test fixture
> play song1.mp3 song2.wav song3.mp3
```

## Screen

A "play" tab and a grey rule across the top with the track counter at
its end; the file name and format; a spectrum analyzer with peak
markers; a progress bar with elapsed and total time; a status chip and a
volume gauge; and keycaps along the bottom. It shares its look with
[scarf](scarf.md#screen) and [tile](tile.md) (`anshi_tui::chrome`, see
[rust.md](rust.md#ratatui-without-std)).

The accent is green while playing and yellow while paused: the tab, the
status chip and the progress bar's fill all follow it. The spectrum's
bars are colored like a level meter, green, then yellow, then red near
the top, and each peak marker takes the color of the row it floats in.
Volume is cyan. Bars and fills are colored spaces, so they draw as solid
blocks.

The content is a column capped at 100 columns, centred horizontally and
vertically between the header and footer. The spectrum gets at most one
row per level (20), so the screen doesn't stretch edge to edge on a large
framebuffer. The spectrum's 20 bars always span exactly the column's
width, lined up with the progress and status rows below; columns that
don't divide evenly are spread across the gaps. Text lines are separated
by a blank row, so the column reads as separate items. Everything adapts down to small terminals: the spectrum shrinks
first (minimum 3 rows), and labels that wouldn't fit are dropped.

## Keys

| Key | Action |
|---|---|
| `space` | pause / resume |
| `q` | stop and quit |
| `n` | skip to the next track |
| `+` / `-` | volume up/down by 10%, clamped 0-200% |

Controls only work over a **real terminal** — the same caveat
the `termtest` self-test's own comment already gives for raw mode: a
scripted, non-interactive input stream never produces the actual
keystrokes `poll_key()` is polling for.

## Format support

PCM, 16-bit signed little-endian, mono or stereo, 44100 Hz or 48000 Hz
only. That is not an arbitrary restriction — it is exactly what
`virtio_snd_open()` accepts (see `docs/architecture.md`); anything else
prints an error naming the actual unsupported field (format/bit depth/
channel count/rate) and moves on to the next track. The WAV parser
(`src/source.rs`) walks RIFF chunks looking for `fmt `/`data` rather than assuming
a fixed layout, since some WAV files carry a `LIST`/`fact` chunk in
between.

## MP3

`play` also plays MP3s (MPEG-1/2 layer I-III), decoded by
[minimp3](https://github.com/lieff/minimp3) -- a CC0 single-header
decoder vendored unmodified in `userland/rust/play/vendor/minimp3/` and
compiled in `userland/rust/play/c/mp3.c`, called from Rust over FFI
(`src/source.rs` mirrors its `mp3dec_t` struct field for field). The
format is sniffed from the file's first
bytes (`RIFF` vs. an ID3v2 tag or MPEG frame sync), never the extension,
so an MP3 named `.wav` plays too. The decoded rate still has to be
44100 or 48000 Hz; a mono or stereo MP3 at any bitrate is fine.

The file is streamed through a 16 KiB input buffer (a whole song won't
fit under the 4 MiB brk cap), a leading ID3v2 tag is skipped outright
(cover art can be hundreds of KiB), and the decoded frames feed the
exact same volume/spectrum/`audio_write()` path WAV playback uses. The
duration comes from a Xing/Info header when there is one (VBR files),
otherwise from file size / bitrate (assumes CBR).

This needed two kernel changes first, since minimp3 is float-based:

- **Per-process FPU/SSE state** (`kernel/src/arch/x86_64/fpu.rs`).
  `fpu::init()` enables SSE (CR0/CR4), and `dispatch()` in
  `arch/x86_64/usermode.rs` eagerly `FXSAVE`s/`FXRSTOR`s each process's
  registers around every dispatch -- including the nested one `wait()`
  does. The kernel itself builds soft-float, so it never touches those
  registers; userland's C (minimp3) builds with SSE on.
- **A 64 KiB user stack** (was 16 KiB): `mp3dec_decode_frame()` alone
  keeps a ~16 KiB scratch struct on the stack.

Volume and the spectrum are integer-only (see below for why that matters
on the Rust side).

## Design notes

`play` is a member of the Rust userland workspace; see [rust.md](rust.md)
for how Rust programs are built and linked, the shared `anshi` runtime,
and the ratatui backend it draws through. Its own sources:

```
userland/rust/play/
  src/              the player (lib.rs, source.rs, spectrum.rs, ui.rs)
  c/mp3.c           play's own C: compiles minimp3's implementation
  c/include/        the <string.h>/<stdlib.h> minimp3 includes
  vendor/minimp3/   minimp3, vendored unmodified (CC0)
```

`build-userland.sh` links `libplay.a` with `c/mp3.c`. There's no C
library: `c/include/` declares the `memcpy`/`memmove`/`memset` minimp3
calls, which Rust's `compiler_builtins` provides.

The screen (`play/src/ui.rs`) draws through ratatui's `Frame`/`Buffer`,
`Line` and `Span`. The spectrum and the meters are small custom
widgets, because the stock `BarChart`/`Gauge` assume block glyphs or
a color palette `play` doesn't use. Geometry is plain `Rect` arithmetic
rather than ratatui's constraint-solver `Layout`: the crate is
soft-float, and the layout depends only on the terminal size.

**Soft-float.** `x86_64-unknown-none` is a soft-float target, so the
little float math ratatui does runs in software. `play` avoids ratatui's
float-based layout solver entirely (see above). The hot paths
(PCM copy, volume and the Goertzel filters) stay integer. minimp3 is C
compiled with SSE and does use the FPU, which is safe because the kernel
saves and restores each process's FPU/SSE state.

**Redraw rate.** The screen redraws after every 4 KiB audio chunk, about
21 ms of 48 kHz stereo. Ratatui diffs each frame against the previous one
and only sends changed cells. Tested under TCG on an Apple Silicon host,
playback keeps up in real time. The guest's output, captured with QEMU's
`wav` audiodev, correlates 0.9999 with the host's own decode of the same
MP3, with no dropouts.

## Spectrum analyzer

A cava-style per-band level meter (`src/spectrum.rs`) needs some kind of
DFT, but the crate is soft-float, so a per-sample float FFT would be
expensive. The **Goertzel algorithm** avoids that. It computes a single
DFT bin's magnitude as a plain 2nd-order IIR recurrence,
```
s[n] = x[n] + coeff*s[n-1] - s[n-2]
power = s[n-1]^2 + s[n-2]^2 - coeff*s[n-1]*s[n-2]
```
with exactly one constant (`coeff = 2*cos(2*pi*f/fs)`) per frequency
band. That constant depends only on the target frequency and the sample
rate, so it's precomputed on the host as a Q15 fixed-point integer. There
are two 20-entry tables (one per supported sample rate), log-spaced from
60 Hz to 7000 Hz. The recurrence runs in `i64`, which leaves plenty of
headroom for ~2048 samples of `i16` input. The accumulator is bounded,
not exponentially unstable, because Goertzel's poles sit exactly on the
unit circle.

Turning raw power into a bar height needs a log scale. The power's bit
length is an exact integer stand-in for log2, and each bit is ~3 dB:
`level = bitlen(power) - 25`, clamped to 0-20, which spans 60 dB. The
floor of 25 was calibrated by simulating this exact recurrence against
real audio: quiet bands land under bit length ~25, and a present tone or
music at about 27-47. Most music sits in the lower half of that range,
which is why the top is cut at 45. Bars jump straight up but fall at
most 3 levels per chunk, a cheap "gravity" that keeps them from
flickering. A peak marker per band holds the recent maximum for 15 chunks
(~0.3 s), then falls one level per chunk.

## Adding your own audio

Two ways to get a file onto the VFS, since Anshi has no network and no
host-mountable filesystem:

**Small fixtures, baked into the kernel image.** What `testtone.wav`
does: drop the file at `kernel/src/exec/<name>.wav.bin`, add an
`include_bytes!` for it in `exec/blobs.rs`, and a `vfs::write_bytes()`
call in `kmain` (`main.rs`). Fine for a small built-in demo asset; costs
a kernel rebuild every time, and the bytes end up resident twice at
runtime (once in the kernel image itself, once in the VFS heap copy
`vfs::write_bytes()` makes) — a bad fit for anything beyond a few hundred KB.

**Anything bigger: `scripts/disk-put.py`, straight onto the disk.**
`blkfs` (`kernel/src/fs/blkfs.rs`) is Anshi's own on-disk format — not a
real filesystem, just a flat recursive dump of the VFS tree (a 512-byte
superblock, then each node as type/name/size/data) — simple enough to
speak directly from the host, so this script is a faithful
reimplementation of `blkfs.rs`'s own `serialize()` and `Reader`. It reads whatever's already on `Anshi-disk.img`
(if anything), adds/replaces a file at VFS root, and writes it back,
growing the image file as needed:
```
scripts/disk-put.py Anshi-disk.img yourfile.wav
```
No kernel rebuild, no doubled memory cost, and it survives reboots the
same way anything `sync`'d from inside the VM would. The constraints
from "Format support" and "MP3" above still apply — the script doesn't
validate or convert audio, it just moves bytes.

A quirk found using this on a real-world file: some WAV writers never
patch the true length back into the `RIFF`/`data` chunk-size fields
(`0xFFFFFFFF`, a "streamed, size unknown at write time" placeholder) —
the parser clamps the declared `data` chunk size to what's
actually left in the file rather than trusting the header, so the
progress bar shows the real duration instead of a nonsensical
multi-hour one.

## What it needed from the kernel

| Capability | Why |
|---|---|
| the virtio-sound driver (`drivers/virtio/snd.rs`) | no audio driver of any kind existed before M17 |
| `audio_open`/`audio_write`/`audio_close` syscalls | Linux does audio via `/dev/snd/*` + `ioctl`; Anshi has no devfs, so there was no existing syscall shape to reuse |
| `poll_key` syscall | a playback loop has to check for a control key *without* blocking — `read()`'s raw-mode "wait for one byte" behavior would stall audio output; this is the kernel shell's own non-blocking keyboard poll (`input::poll_char()`), exposed as a syscall |
| `testtone.wav` boot fixture (`scripts/gen-test-tone.py`) | there is no host-side way to get an arbitrary file onto the VFS (same reason `filetest.txt` is a boot-time fixture), so `play testtone.wav` needs something built in to play |

## Verifying it actually plays something

`scripts/run-qemu.sh` defaults `-audiodev` to QEMU's `wav` backend,
capturing whatever the guest sends to `Anshi-audio-out.wav` on the
host (gitignored, like `Anshi-disk.img`) — this works on any host with
no real audio hardware or PulseAudio/ALSA setup required, so `play` is
boot-verifiable the same deterministic way virtio-gpu is verified by
screendump. Inspect the captured file afterward (e.g. Python's `wave`
module) to confirm non-silent samples of roughly the right
duration/frequency came out. To actually *hear* it on a machine with
working audio, override the backend: `QEMU_AUDIODEV="pa,id=snd0"`
(PulseAudio), `"alsa,id=snd0"` (ALSA), or `"coreaudio,id=snd0"`
(macOS).

## Not supported

Seeking, a persistent playlist file, per-track metadata display beyond
the filename (ID3 tags are skipped, not read), and formats other than PCM
WAV and MP3. Sample rates other than 44100/48000 Hz would need
resampling.
