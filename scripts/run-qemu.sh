#!/usr/bin/env bash
# Boots Anshi.iso in QEMU over UEFI, virtio-gpu as the only display
# device. Does NOT build the ISO -- run scripts/build-iso.sh (or `make`)
# first.
#
#   --pc   Boot it the way a real PC looks instead (docs/real-hardware.md):
#          no virtio devices at all, a plain VGA card whose UEFI GOP
#          framebuffer is the only display, 4 GiB of RAM, 6 CPUs, a USB
#          keyboard on an xHCI controller, and Intel HD Audio. Only what
#          Anshi has a real-hardware driver for works here, so there's
#          no disk yet. Must be the first argument; the rest still go to
#          QEMU.
#
# Requires: qemu-system-x86_64, OVMF firmware.
#   sudo apt-get install -y qemu-system-x86 ovmf
set -euo pipefail
cd "$(dirname "$0")/.."

PC_MODE=0
if [ "${1:-}" = "--pc" ]; then
    PC_MODE=1
    shift
fi

ISO="Anshi.iso"
if [ ! -f "$ISO" ]; then
    echo "error: $ISO not found -- run ./scripts/build-iso.sh (or \`make\`) first" >&2
    exit 1
fi

# Persistent storage backing (M9, virtio-blk) -- a blank disk formats
# itself on first `sync`/auto-save from the shell. Kept out of git
# (see .gitignore) since it's runtime state, not source.
DISK="Anshi-disk.img"
if [ ! -f "$DISK" ]; then
    echo "Creating blank $DISK (16 MiB)..."
    qemu-img create -f raw "$DISK" 16M >/dev/null
fi

find_ovmf_combined() {
    for p in /usr/share/OVMF/OVMF.fd /usr/share/ovmf/OVMF.fd; do
        [ -f "$p" ] && printf '%s\n' "$p" && return 0
    done
    return 1
}

find_ovmf_split() {
    local candidates=(
        "/usr/share/OVMF/OVMF_CODE_4M.fd:/usr/share/OVMF/OVMF_VARS_4M.fd"
        "/usr/share/OVMF/OVMF_CODE.fd:/usr/share/OVMF/OVMF_VARS.fd"
        "/usr/share/edk2/x64/OVMF_CODE.4m.fd:/usr/share/edk2/x64/OVMF_VARS.4m.fd"
        # Homebrew's qemu ships its own edk2 build, so a Mac can run this
        # script directly rather than only inside the container. Note the
        # names don't match: there is no edk2-x86_64-vars.fd, the i386
        # vars file is the correct pair for the x86_64 code blob.
        "/opt/homebrew/share/qemu/edk2-x86_64-code.fd:/opt/homebrew/share/qemu/edk2-i386-vars.fd"
        "/usr/local/share/qemu/edk2-x86_64-code.fd:/usr/local/share/qemu/edk2-i386-vars.fd"
    )
    for pair in "${candidates[@]}"; do
        local code="${pair%%:*}" vars="${pair##*:}"
        if [ -f "$code" ] && [ -f "$vars" ]; then
            printf '%s\n' "$pair"
            return 0
        fi
    done
    return 1
}

fw_args=()
if combined=$(find_ovmf_combined); then
    fw_args=(-bios "$combined")
elif split=$(find_ovmf_split); then
    code="${split%%:*}"
    vars="${split##*:}"
    vars_local="$(pwd)/OVMF_VARS.local.fd"
    [ -f "$vars_local" ] || cp "$vars" "$vars_local"
    fw_args=(
        -drive "if=pflash,format=raw,readonly=on,file=$code"
        -drive "if=pflash,format=raw,file=$vars_local"
    )
else
    echo "error: OVMF UEFI firmware not found. Install it with:" >&2
    echo "  sudo apt-get install -y ovmf" >&2
    exit 1
fi

# Audio backend (M17, virtio-sound) -- auto-picks a real, audible
# backend when one is actually reachable: CoreAudio on macOS,
# PulseAudio/PipeWire via `pactl` then ALSA via `aplay` on Linux, so
# `play` just comes out of your speakers by default. Falls back to
# QEMU's "wav" backend (captures to Anshi-audio-out.wav, gitignored
# like Anshi-disk.img, instead of speakers) only when none is
# reachable -- e.g. a headless CI/sandbox box, or the build container,
# which has no audio setup at all -- so a boot never hard-fails here for
# lack of sound hardware. Set QEMU_AUDIODEV yourself to override, e.g.
# to force the wav capture back on for a deterministic test:
# QEMU_AUDIODEV="wav,id=snd0,path=Anshi-audio-out.wav".
if [ -n "${QEMU_AUDIODEV:-}" ]; then
    : # explicit override wins, nothing to detect
elif [ "$(uname -s)" = "Darwin" ]; then
    # macOS has neither pactl nor aplay, so without this it fell through
    # to the wav fallback below and captured to a file instead of making
    # any sound -- which looks exactly like broken audio.
    QEMU_AUDIODEV="coreaudio,id=snd0"
elif command -v pactl >/dev/null 2>&1 && pactl info >/dev/null 2>&1; then
    QEMU_AUDIODEV="pa,id=snd0"
elif command -v aplay >/dev/null 2>&1 && aplay -l >/dev/null 2>&1; then
    QEMU_AUDIODEV="alsa,id=snd0"
else
    QEMU_AUDIODEV="wav,id=snd0,path=Anshi-audio-out.wav"
fi

# Display resolution -- virtio-gpu's own xres/yres properties, which set
# the mode the device reports to the guest at boot (with --pc, the VGA
# card's, which it reports as its monitor's preferred mode over EDID). Everything downstream
# follows from it with no further configuration: console/fbconsole.rs
# derives its text grid as width/8 x height/8, and TIOCGWINSZ hands that
# to full-screen programs (see docs/scarf.md).
#
# Asked interactively, but ONLY when stdin is a terminal. Scripted runs
# pipe keystrokes into the guest's serial console, and a `read` here
# would eat the first line of them -- so a non-interactive run silently
# takes the default instead of hanging or stealing input. Set ANSHI_RES
# to skip the prompt either way, e.g. ANSHI_RES=1920x1080.
RESOLUTIONS=(
    "1280x800"   # QEMU's own virtio-gpu default
    "1920x1080"
    "1600x900"
    "1366x768"
    "1280x720"
    "1024x768"
)
DEFAULT_RES="1280x800"

if [ -n "${ANSHI_RES:-}" ]; then
    RES="$ANSHI_RES"
elif [ -t 0 ]; then
    echo "Display resolution:"
    for i in "${!RESOLUTIONS[@]}"; do
        mark=""
        [ "${RESOLUTIONS[$i]}" = "$DEFAULT_RES" ] && mark="  (default)"
        printf '  %d) %s%s\n' "$((i + 1))" "${RESOLUTIONS[$i]}" "$mark"
    done
    printf 'Choose [1-%d, or WxH, Enter for default]: ' "${#RESOLUTIONS[@]}"
    # `|| choice=""` matters: read returns non-zero at EOF (Ctrl-D), and
    # `set -e` would otherwise abort the whole script right here, with no
    # message and no VM.
    read -r choice || choice=""
    if [ -z "$choice" ]; then
        RES="$DEFAULT_RES"
    elif [[ "$choice" =~ ^[0-9]+$ ]] && [ "$choice" -ge 1 ] &&
        [ "$choice" -le "${#RESOLUTIONS[@]}" ]; then
        RES="${RESOLUTIONS[$((choice - 1))]}"
    else
        RES="$choice" # typed WxH directly; validated below
    fi
else
    RES="$DEFAULT_RES"
fi

if [[ ! "$RES" =~ ^[0-9]+x[0-9]+$ ]]; then
    echo "error: bad resolution '$RES' -- expected WxH, e.g. 1280x800" >&2
    exit 1
fi
XRES="${RES%x*}"
YRES="${RES#*x}"

# Display backend -- on macOS, Cocoa's window is locked to the guest's
# framebuffer size unless zoom-to-fit is on; with it, the window can be
# resized freely and the guest image is scaled to fill it (the guest's
# own resolution stays $RES -- it's scaling, not a mode change).
# zoom-interpolation smooths that scaling instead of nearest-neighbour.
# Elsewhere QEMU's default display is left alone. Set QEMU_DISPLAY to
# override, e.g. QEMU_DISPLAY="cocoa,zoom-to-fit=off".
display_args=()
if [ -n "${QEMU_DISPLAY:-}" ]; then
    display_args=(-display "$QEMU_DISPLAY")
elif [ "$(uname -s)" = "Darwin" ]; then
    display_args=(-display cocoa,zoom-to-fit=on,zoom-interpolation=on)
fi

if [ "$PC_MODE" = 1 ]; then
    machine_args=(
        -m 4G
        -smp 6
        -device VGA,xres="$XRES",yres="$YRES"
        -device qemu-xhci,id=xhci
        -device usb-kbd,id=usbkbd,bus=xhci.0
        -device intel-hda
        -device hda-output,audiodev=snd0
        -audiodev "$QEMU_AUDIODEV"
    )
else
    machine_args=(
        -m 512M
        -device virtio-gpu-pci,xres="$XRES",yres="$YRES"
        -device virtio-keyboard-pci
        -drive file="$DISK",if=none,id=disk0,format=raw
        -device virtio-blk-pci,drive=disk0,disable-legacy=on
        -device virtio-sound-pci,audiodev=snd0
        -audiodev "$QEMU_AUDIODEV"
    )
fi

exec qemu-system-x86_64 \
    -M q35 \
    -no-shutdown \
    -vga none \
    ${display_args[@]+"${display_args[@]}"} \
    "${fw_args[@]}" \
    -cdrom "$ISO" \
    "${machine_args[@]}" \
    -serial stdio \
    "$@"
