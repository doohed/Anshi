# Build/run environment for Anshi on a non-x86_64 host (an Apple Silicon
# Mac, specifically). Nothing in here is emulated: the image is native to
# the host architecture, because the two things that care about x86_64 can
# both be told about it without a foreign-arch userland. clang targets
# x86_64 from any host given --target (so no separate cross-gcc), and
# qemu-system-x86_64 emulates the guest CPU in TCG regardless of what it's
# running on. TCG means no KVM/HVF acceleration, which is fine for a kernel
# this size -- the whole thing is a few hundred KiB and boots in a second.
#
# Pinned to a dated Debian tag rather than a floating one so the toolchain
# doesn't drift underneath the build.
FROM debian:trixie-20260803-slim

# The package set is derived from what scripts/*.sh actually invoke:
#
#   build-iso.sh       -> make, cargo (the kernel is Rust), $LD (lld, see
#                         ENV below) to link it, xorriso, and git for the
#                         limine submodule
#   build-userland.sh  -> cargo again (userland is Rust too), $CC for the
#                         one C file left -- play's MP3 decoder, minimp3 --
#                         and $LD, plus python3 (gen-test-tone.py
#                         synthesizes a WAV fixture -- real binary struct
#                         packing and float math, not a fit for bash -- see
#                         docs/play.md). Rust comes from rustup below, not
#                         apt: Debian's rustc ships no prebuilt core/alloc
#                         for the x86_64-unknown-none target.
#   run-qemu.sh        -> qemu-system-x86_64, qemu-img, OVMF firmware
#   format.sh          -> rustfmt
#
# disk-put.py (docs/play.md) is also python3 but isn't part of the build
# -- it writes straight onto Anshi-disk.img from the host, run by hand.
#
# mtools is the README's documented dependency for the ISO step; xorriso
# builds the EFI boot partition itself here, but it's kept so the
# documented install line and this image stay the same set. nasm is *not*
# installed: nothing in the tree is .asm -- the kernel's assembly is
# inline in its Rust (global_asm!).
#
# ovmf is Architecture: all in Debian (the firmware blobs are just x86
# binaries in a data package), so it installs on arm64 unchanged and lands
# in /usr/share/OVMF/, which run-qemu.sh's split-firmware auto-detection
# already looks in.
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang \
        lld \
        make \
        git \
        ca-certificates \
        xorriso \
        mtools \
        qemu-system-x86 \
        qemu-utils \
        ovmf \
        python3 \
        curl \
    && rm -rf /var/lib/apt/lists/*

# Rust for the kernel and userland/rust/: a minimal stable toolchain plus
# the bare-metal x86_64 target, and rustfmt for format.sh. On an arm64 host
# this is still a native toolchain -- x86_64-unknown-none is a cross
# target, like clang's --target.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path \
    && rustup target add x86_64-unknown-none \
    && rustup component add rustfmt \
    && chmod -R a+w "$CARGO_HOME"

# Self-configuring toolchain: kernel/GNUmakefile and build-userland.sh take
# CC/LD from the environment (defaulting to cc/ld for a native x86_64 host
# build), and build-userland.sh adds --target=x86_64-unknown-none only when
# CC is actually clang. Setting these here is all it takes to make the
# unmodified scripts cross-compile.
ENV CC=clang \
    LD=ld.lld

WORKDIR /work

CMD ["bash"]
