#!/usr/bin/env bash
# Builds every ring-3 program -- all Rust, in one Cargo workspace,
# userland/rust/ (see docs/rust.md): the interactive programs (sh, play,
# scarf, tile) and the small self-test payloads (hello, crash,
# malloctest, ...) that prove the ring-3 pipeline end to end. Drops their
# built ELF binaries where kernel/src/exec/blobs.rs expects to find them
# (include_bytes!) so they get embedded directly into the kernel image --
# see kmain (kernel/src/main.rs) for how they land in /bin at boot.
#
# This is a genuinely separate build (its own toolchain flags, its own
# linker script) from the kernel itself -- these are ring-3 userland
# programs, not kernel code -- run before `make -C kernel` since the
# kernel embeds its output.
set -euo pipefail
cd "$(dirname "$0")/.."

# Same CC/LD override as kernel/GNUmakefile, for the same reason: native
# cc/ld on an x86_64 host, clang/ld.lld when docker/Dockerfile's ENV says
# so. The only C left in userland is play's MP3 decoder (minimp3, see
# userland/rust/play/c/mp3.c); clang defaults to the host triple, so it
# needs telling that it's building bare-metal x86_64; gcc gets nothing
# extra.
CC="${CC:-cc}"
LD="${LD:-ld}"
# Unlike the kernel, userland may use x87/SSE: the kernel saves/restores
# each process's FPU registers around every dispatch (kernel/src/arch/
# x86_64/fpu.rs) -- which minimp3 needs, being float-based. play/c/include
# holds the minimal <string.h>/<stdlib.h> it includes; there's no libc.
CFLAGS=(-g -O2 -ffreestanding -fno-stack-protector -fno-stack-check -fno-pic -fno-pie
    -ffunction-sections -fdata-sections -m64 -march=x86-64 -mabi=sysv -mno-red-zone
    -I userland/rust/play/c/include)
if "$CC" --version 2>/dev/null | grep -qi clang; then
    CFLAGS+=(--target=x86_64-unknown-none)
fi
# The programs come only as archives, which a linker searches just for
# symbols something already needs -- -u makes that `_start` (in anshi).
LDFLAGS=(-m elf_x86_64 -nostdlib -static --gc-sections -T userland/link.ld -u _start)

# Every program is a no_std staticlib exporting a `main`; the `anshi`
# runtime crate in each provides `_start`, which calls it. --config names
# the workspace's own config explicitly (target x86_64-unknown-none),
# since cargo otherwise only finds .cargo/config.toml relative to the
# current directory.
cargo build --release --quiet \
    --manifest-path userland/rust/Cargo.toml \
    --config userland/rust/.cargo/config.toml
RUST_OUT=userland/rust/target/x86_64-unknown-none/release

# Links one program from its staticlib plus any C sources of its own,
# with any extra linker flags given after `--`.
build_program() {
    local name="$1" lib="$2"
    shift 2
    local objs=("$RUST_OUT/lib${lib}.a") ldextra=()
    while (($#)); do
        if [[ "$1" == -- ]]; then
            shift
            ldextra=("$@")
            break
        fi
        local obj="${1%.*}.o"
        "$CC" "${CFLAGS[@]}" -c "$1" -o "$obj"
        objs=("$obj" "${objs[@]}")
        shift
    done
    "$LD" "${LDFLAGS[@]}" ${ldextra[@]+"${ldextra[@]}"} "${objs[@]}" -o "userland/${name}.elf"
    cp "userland/${name}.elf" "kernel/src/exec/${name}.elf.bin"
}

build_program sh sh
build_program play play userland/rust/play/c/mp3.c
build_program scarf scarf
build_program tile tile

# The self-tests are one staticlib, each test exporting `<name>_main`:
# pointing `main` at one and letting --gc-sections drop the rest gives
# each its own small ELF (userland/rust/selftest/src/lib.rs).
SELFTESTS=(hello crash malloctest filetest dirtest forktest forkchild preempttest termtest readdirtest pipetest)
for t in "${SELFTESTS[@]}"; do
    build_program "$t" selftest -- "--defsym=main=${t}_main"
done

# Not a userland ELF -- a synthesized WAV fixture for `play` (see the
# script itself for why), dropped at the same kernel/src/exec/*.bin
# location blobs.rs expects.
python3 scripts/gen-test-tone.py # relative to the repo root, where the cd above put us

echo "Built userland/{sh,play,scarf,tile,$(IFS=,; echo "${SELFTESTS[*]}")}.elf"
