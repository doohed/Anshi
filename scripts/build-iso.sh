#!/usr/bin/env bash
# Builds kernel/bin/kernel and assembles a UEFI-only Anshi.iso.
#
# Requires: cargo and ld (kernel build), xorriso (ISO assembly).
#   sudo apt-get install -y xorriso
#
# This is the single source of truth for how the ISO gets built -- the
# root GNUmakefile's `iso`/`all` targets just call this script, so there's
# one place to change if the layout (Limine files, boot path, ...) does.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE_NAME="Anshi.iso"

if ! command -v xorriso >/dev/null 2>&1; then
    echo "error: xorriso not found. Install it with:" >&2
    echo "  sudo apt-get install -y xorriso" >&2
    exit 1
fi

./scripts/build-userland.sh
make -C kernel

# Stage the kernel, Limine's boot config, and Limine's prebuilt UEFI
# El Torito image + BOOTX64.EFI, then let xorriso build the hybrid ISO
# 9660 image. No BIOS boot catalog entry / limine-bios-install step,
# since legacy BIOS boot is intentionally not supported.
rm -rf iso_root
mkdir -p iso_root/boot/limine
cp kernel/bin/kernel iso_root/boot/
cp limine.conf iso_root/boot/limine/
cp limine/limine-uefi-cd.bin iso_root/boot/limine/
mkdir -p iso_root/EFI/BOOT
cp limine/BOOTX64.EFI iso_root/EFI/BOOT/

xorriso -as mkisofs -R -r -J \
    -hfsplus -apm-block-size 2048 \
    --efi-boot boot/limine/limine-uefi-cd.bin \
    -efi-boot-part --efi-boot-image --protective-msdos-label \
    iso_root -o "$IMAGE_NAME"

rm -rf iso_root

echo "Built $IMAGE_NAME"
