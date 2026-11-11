# The shell

`kernel/src/shell.rs` is the kernel-resident command shell, reading lines from
either input path (see [architecture.md](architecture.md#console)). The
system boots into the userland shell, [sh](sh.md), instead; this one
takes over if `sh` exits (`sh` starts it again), and is the only place
the kernel diagnostics (`meminfo`, `lspci`, `crash`, `reboot`, ...) live.
Command *names* are matched case-insensitively; arguments keep whatever
case you typed.

## Builtins

Deliberately not named like standard Unix -- `create dir <name>` rather
than `mkdir`, `create file <name>` rather than `touch`, and the same
verb-first pattern for `delete`/`copy`/`move`:

| Command | Purpose |
|---|---|
| `help` | list commands |
| `echo <text>` | print text back |
| `clear` | clear the screen |
| `uname` | kernel/arch info |
| `meminfo` | physical allocator stats |
| `lspci` | PCI devices found at boot |
| `uptime` | time since interrupts were enabled |
| `crash` | deliberately trigger a `#DE`, to exercise the handler |
| `reboot` / `halt` | what they say |
| `create dir\|file <name>` | make a directory or empty file |
| `delete dir\|file <path>` | remove one (directories recursively) |
| `copy dir\|file <src> <dest>` | duplicate |
| `move dir\|file <src> <dest>` | rename/relocate |
| `cd [path]` / `pwd` / `ls [path]` | navigate and list |
| `cat <file>` / `write <file> <text...>` | read and overwrite |
| `sync` | flush the filesystem to disk now |
| `run <path> [args...]` | execute a static ELF64 program in ring 3 |

`vfs::remove()` refuses to delete the current directory or an ancestor of
it -- the shell's `cwd` pointer would otherwise dangle.

## Running programs

Programs live in `/bin` and can be run by bare name from any directory:

```
Anshi:/> hello
Anshi:/> cd docs
Anshi:/docs> scarf .
```

A name containing `/` is taken as a path (relative to the current
directory, or absolute). A bare name is looked up in the current
directory first, then in `/bin` -- one hardcoded search directory, since
there is no environment to hold a real `$PATH`.

**Builtins always win a name collision.** A file dropped in `/bin` must
never be able to shadow `ls` or `cd`.

A launched process **inherits the shell's current directory**, so a
relative argument like `.` means what it looks like it means. It does
not inherit anything else -- no environment, no open files.

## Using the shell interactively

`./scripts/run-qemu.sh` defaults to `-display none` (no graphical window
at all — `virtio-gpu`/`virtio-keyboard` still work, there's just nothing
on screen and no keyboard focus to give them). That's deliberate: the
shell doubles as a real **serial console**. `drivers/serial.rs`'s
`poll_char()` reads bytes straight off COM1, and the kernel shell's
`read_line()` polls it alongside the keyboards (`input::poll_char()`) —
whichever has a byte ready wins. Since
`run-qemu.sh` passes `-serial stdio`, that means **typing directly into
the terminal you ran the script from talks to the shell**, no graphical
window, VNC, or anything else required. This is the normal way to use
Anshi in this repo.

If you do want a graphical window instead (to exercise the
virtio-gpu/virtio-input path specifically, e.g. while working on those
drivers), add a real display backend yourself, e.g.
`./scripts/run-qemu.sh -display gtk` (or `sdl`) — click into that window
for keyboard focus; the terminal you launched it from goes back to being
just the serial log in that case.

## Testing keyboard input headlessly (scripted, non-interactive)

For driving the shell from a script without occupying a terminal (as
opposed to a person just typing, which the serial console above already
covers): QEMU's HMP `sendkey` monitor command does **not** reach
`virtio-keyboard-pci` in this setup — it targets the legacy PS/2 keyboard
that q35's i8042 controller provides by default (`info qtree` shows both
`ps2-kbd` and `virtio-keyboard-pci` present simultaneously), and Anshi has
no PS/2 driver on purpose (virtio-only). Use QMP's `input-send-event`
instead, with `device` set to the **display** device's id (not the
keyboard's — that crashes this QEMU build), e.g.
`-device virtio-gpu-pci,id=gpu0` and `{"execute": "input-send-event",
"arguments": {"device": "gpu0", "events": [...]}}` for each key down/up.
(Simplest of all for scripting, though: just write to the QEMU process's
stdin like a person would type, since `-serial stdio` is listening —
no QMP needed if the serial console path above is good enough for what
you're testing.)

One more gotcha specific to the QMP/virtio-input path: punctuation keys
use QEMU's `QKeyCode` names, not the literal character -- `.` is `"dot"`,
`/` is `"slash"`, `,` is `"comma"`, etc. Sending the raw character for
these silently fails (check the JSON response for an `"error"` key; it's
easy to miss otherwise) and the keystroke is just dropped, which reads
exactly like a kernel bug until you notice `main.c` arrived as `mainc`.

Scripting the serial console via a FIFO (`qemu-system-x86_64 ... -serial
stdio < some.fifo`) has its own gotcha: each `printf ... > some.fifo`
from a separate shell invocation opens and immediately closes the write
end. Every close-with-no-other-writer-open briefly signals EOF to
whatever's reading the pipe, and QEMU's stdio backend treats that as "the
user hit Ctrl-D" and stops polling stdin for the rest of the process's
life -- keystrokes sent afterwards vanish with no error. Keep a single
write descriptor open for the whole session instead (`exec 4<>some.fifo`
once, `printf ... >&4` for every subsequent command, from the *same*
shell process) and it behaves like a real terminal.

`virtio-blk-pci` is also worth calling out: unlike `virtio-gpu-pci`/
`virtio-keyboard-pci` (which QEMU only ever exposes as modern virtio
1.x devices), `virtio-blk-pci` defaults to the legacy/transitional PCI
device id (`0x1001`) unless you pass `disable-legacy=on`.
`drivers/virtio/blk.rs` only looks for the modern id (`0x1042`, see its
`PCI_DEVICE_ID`), so without that flag `blk::init()` reports "no block
device found" even though `-device virtio-blk-pci` is right there on the
command line. `scripts/run-qemu.sh` already passes it; add it yourself
if you ever invoke `qemu-system-x86_64` directly.
