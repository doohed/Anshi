//! The userland programs, embedded in the kernel image:
//! scripts/build-userland.sh drops each built ELF at <name>.elf.bin next
//! to this file before the kernel is built, and kmain writes them into
//! /bin at boot -- there's no host-side way to get a file onto the VFS.

/// In the order they're written into /bin (a directory lists newest
/// first): the self-tests, then the real tools.
pub const PROGRAMS: [(&str, &[u8]); 19] = [
    ("hello", include_bytes!("hello.elf.bin")),
    ("crash", include_bytes!("crash.elf.bin")),
    ("malloctest", include_bytes!("malloctest.elf.bin")),
    ("filetest", include_bytes!("filetest.elf.bin")),
    ("dirtest", include_bytes!("dirtest.elf.bin")),
    ("forktest", include_bytes!("forktest.elf.bin")),
    ("forkchild", include_bytes!("forkchild.elf.bin")),
    ("preempttest", include_bytes!("preempttest.elf.bin")),
    ("termtest", include_bytes!("termtest.elf.bin")),
    ("readdirtest", include_bytes!("readdirtest.elf.bin")),
    ("pipetest", include_bytes!("pipetest.elf.bin")),
    ("sh", include_bytes!("sh.elf.bin")),
    ("tile", include_bytes!("tile.elf.bin")),
    ("scarf", include_bytes!("scarf.elf.bin")),
    ("play", include_bytes!("play.elf.bin")),
    ("ping", include_bytes!("ping.elf.bin")),
    ("fetch", include_bytes!("fetch.elf.bin")),
    ("serve", include_bytes!("serve.elf.bin")),
    ("net", include_bytes!("net.elf.bin")),
];

/// Not a program: a synthesized tone (scripts/gen-test-tone.py), so
/// `play testtone.wav` has something to play out of the box.
pub const TESTTONE_WAV: &[u8] = include_bytes!("testtone.wav.bin");
