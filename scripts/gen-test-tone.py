#!/usr/bin/env python3
"""Synthesizes a short 440 Hz sine wave as a 44.1kHz mono S16LE WAV file
and writes it to kernel/src/exec/testtone.wav.bin, where
kernel/src/exec/blobs.rs embeds it (include_bytes!) directly into the
kernel image -- same reasoning userland ELF payloads get embedded that
way (see scripts/build-userland.sh): there's no host-side way to get a
file onto Anshi's VFS otherwise, so `play testtone.wav` needs something
to play built in.

Run from scripts/build-userland.sh, before the kernel build, same
ordering .elf.bin payloads already need.
"""
import math
import os
import struct

SAMPLE_RATE = 44100
DURATION_S = 2.0
FREQ_HZ = 440.0
AMPLITUDE = 12000  # comfortably below the int16 ceiling

repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
out_path = os.path.join(repo_root, "kernel", "src", "exec", "testtone.wav.bin")

num_samples = int(SAMPLE_RATE * DURATION_S)
samples = bytearray()
for i in range(num_samples):
    v = int(AMPLITUDE * math.sin(2.0 * math.pi * FREQ_HZ * i / SAMPLE_RATE))
    samples += struct.pack("<h", v)

data_bytes = bytes(samples)
byte_rate = SAMPLE_RATE * 1 * 2
block_align = 1 * 2

with open(out_path, "wb") as f:
    f.write(b"RIFF")
    f.write(struct.pack("<I", 36 + len(data_bytes)))
    f.write(b"WAVE")
    f.write(b"fmt ")
    f.write(struct.pack("<IHHIIHH", 16, 1, 1, SAMPLE_RATE, byte_rate, block_align, 16))
    f.write(b"data")
    f.write(struct.pack("<I", len(data_bytes)))
    f.write(data_bytes)

print(f"Wrote {out_path} ({len(data_bytes)} bytes of PCM, {DURATION_S}s @ {SAMPLE_RATE}Hz)")
