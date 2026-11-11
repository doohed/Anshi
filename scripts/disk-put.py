#!/usr/bin/env python3
"""Writes a host file directly onto an Anshi disk image (blkfs format),
without going through a kernel rebuild or embedding it in the image.

Why this exists: the only host->guest path Anshi otherwise has is baking
a file into the kernel image at build time (see scripts/gen-test-tone.py
and kernel/src/exec/blobs.rs) -- fine for a small fixture, bad
for anything sizeable, since it doubles the memory cost (resident kernel
image + a duplicate VFS heap copy) and bloats/slows every kernel rebuild.
blkfs (kernel/src/fs/blkfs.rs) is Anshi's own on-disk format -- not a
real filesystem, just a flat recursive dump of the VFS tree -- but it's
simple enough to speak directly from the host. This script is a faithful
byte-for-byte reimplementation of blkfs.rs's serialize() and its
Reader, read-modify-write: it loads whatever's already on the
image (if anything), adds/replaces files at VFS root, and writes it back,
growing the image file if needed.

Usage:
    scripts/disk-put.py Anshi-disk.img Falling.wav
    scripts/disk-put.py Anshi-disk.img Falling.wav:song.wav other.wav

Each argument after the image path is HOST_FILE[:VFS_NAME] -- VFS_NAME
defaults to the host file's basename. Files land at VFS root (same place
testtone.wav lives), same as every current boot fixture.
"""
import os
import struct
import sys

SECTOR_SIZE = 512
MAGIC = 0x53464E41  # "ANFS" on disk, little-endian u32 -- see blkfs.rs.
VERSION = 1
VNODE_DIR = 0
VNODE_FILE = 1
NAME_MAX = 64  # kernel/src/fs/vfs.rs's NAME_MAX -- names must fit under this.


class Node:
    def __init__(self, name, is_dir):
        self.name = name
        self.is_dir = is_dir
        self.children = []  # list[Node], dirs only, in serialization order
        self.data = b""  # files only


def serialize(node, out):
    out.append(bytes([VNODE_DIR if node.is_dir else VNODE_FILE]))
    name_bytes = node.name.encode("ascii")
    out.append(bytes([len(name_bytes)]))
    out.append(name_bytes)
    if not node.is_dir:
        out.append(struct.pack("<I", len(node.data)))
        out.append(node.data)
        return
    out.append(struct.pack("<I", len(node.children)))
    for c in node.children:
        serialize(c, out)


class Reader:
    def __init__(self, buf):
        self.buf = buf
        self.pos = 0

    def bytes(self, n):
        b = self.buf[self.pos : self.pos + n]
        if len(b) != n:
            raise ValueError("truncated blkfs data")
        self.pos += n
        return b

    def u8(self):
        return self.bytes(1)[0]

    def u32(self):
        return struct.unpack("<I", self.bytes(4))[0]


def deserialize(r):
    node_type = r.u8()
    name_len = r.u8()
    name = r.bytes(name_len).decode("ascii", errors="replace")
    if node_type == VNODE_FILE:
        size = r.u32()
        data = r.bytes(size)
        node = Node(name, is_dir=False)
        node.data = data
        return node
    node = Node(name, is_dir=True)
    child_count = r.u32()
    for _ in range(child_count):
        node.children.append(deserialize(r))
    return node


def load_root(path):
    """Returns a root Node -- empty if the image doesn't exist, is too
    small, or doesn't carry blkfs's magic (a fresh/unformatted disk,
    exactly like blkfs_load()'s own "no filesystem on disk yet" path)."""
    if not os.path.exists(path):
        return Node("", is_dir=True)
    with open(path, "rb") as f:
        sb = f.read(SECTOR_SIZE)
        if len(sb) < 16:
            return Node("", is_dir=True)
        magic, version, total_size = struct.unpack_from("<IIQ", sb, 0)
        if magic != MAGIC:
            return Node("", is_dir=True)
        if version != VERSION:
            print(f"disk-put: on-disk version {version} unsupported (expected {VERSION}) "
                  "-- starting fresh", file=sys.stderr)
            return Node("", is_dir=True)
        if total_size == 0:
            return Node("", is_dir=True)
        f.seek(SECTOR_SIZE)
        data = f.read(total_size)
        if len(data) != total_size:
            print("disk-put: on-disk data is truncated -- starting fresh", file=sys.stderr)
            return Node("", is_dir=True)
    try:
        return deserialize(Reader(data))
    except ValueError as e:
        print(f"disk-put: on-disk data is corrupt ({e}) -- starting fresh", file=sys.stderr)
        return Node("", is_dir=True)


def save_root(path, root, min_bytes):
    out = []
    serialize(root, out)
    tree_data = b"".join(out)

    sb = struct.pack("<IIQ", MAGIC, VERSION, len(tree_data))
    sb += b"\x00" * (SECTOR_SIZE - len(sb))

    needed = SECTOR_SIZE + len(tree_data)
    # Never shrink an existing image, and always leave some slack for
    # future in-VM `sync`s to grow into without immediately running out
    # of room -- round up to the next 4 MiB past whatever's needed.
    image_size = max(needed, min_bytes)
    slack = 4 * 1024 * 1024
    image_size = ((image_size + slack - 1) // slack) * slack

    with open(path, "wb") as f:
        f.write(sb)
        f.write(tree_data)
        f.truncate(image_size)

    return image_size, len(tree_data)


def main(argv):
    if len(argv) < 3:
        print(__doc__)
        return 1

    disk_path = argv[1]
    min_bytes = os.path.getsize(disk_path) if os.path.exists(disk_path) else 0

    root = load_root(disk_path)
    by_name = {c.name: c for c in root.children}

    for spec in argv[2:]:
        if ":" in spec:
            host_path, vfs_name = spec.split(":", 1)
        else:
            host_path, vfs_name = spec, os.path.basename(spec)
        if len(vfs_name.encode("ascii")) >= NAME_MAX:
            print(f"disk-put: name '{vfs_name}' is too long (>= {NAME_MAX} bytes)", file=sys.stderr)
            return 1

        with open(host_path, "rb") as f:
            content = f.read()

        node = Node(vfs_name, is_dir=False)
        node.data = content
        by_name[vfs_name] = node
        print(f"disk-put: staged {host_path} -> /{vfs_name} ({len(content)} bytes)")

    root.children = list(by_name.values())
    image_size, tree_bytes = save_root(disk_path, root, min_bytes)
    print(f"disk-put: wrote {tree_bytes} bytes of tree data, image now {image_size} bytes "
          f"({image_size // (1024*1024)} MiB)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
