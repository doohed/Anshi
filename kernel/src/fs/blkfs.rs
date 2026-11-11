//! Persists the in-memory VFS tree to and from the virtio-blk disk, in a
//! simple custom format -- not a "real" filesystem (no inode table, no
//! free-space bitmap, no incremental updates): every save serializes the
//! WHOLE tree and overwrites the disk image in one go. Fine at hobby-OS
//! scale; would need a real design to scale up. With no virtio-blk disk,
//! both just report "no disk" and succeed.
//!
//! Sector 0 holds the superblock; the tree follows from sector 1, each
//! node as: type (u8), name length (u8), name, then for a file its size
//! (u32) and bytes, for a directory its child count (u32) and children.

use alloc::vec;
use alloc::vec::Vec;

use super::vfs::{self, Vnode, VnodeType};
use crate::drivers::virtio::blk::{self, SECTOR_SIZE};

/// "ANFS" (Anshi FS), read as a little-endian u32.
const MAGIC: u32 = 0x5346_4e41;
const VERSION: u32 = 1;

fn serialize(out: &mut Vec<u8>, node: &Vnode) {
    out.push(node.kind as u8);
    let name = node.name().as_bytes();
    out.push(name.len() as u8);
    out.extend_from_slice(name);
    if !node.is_dir() {
        out.extend_from_slice(&(node.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&node.data);
        return;
    }
    out.extend_from_slice(&(node.children().count() as u32).to_le_bytes());
    node.children().for_each(|c| serialize(out, c));
}

/// A read cursor over the serialized tree.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        let b = self.buf.get(self.pos..self.pos + len)?;
        self.pos += len;
        Some(b)
    }

    fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }

    fn u32(&mut self) -> Option<u32> {
        self.bytes(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    /// Replays one record as vfs calls under `parent`. None for `parent`
    /// means "this record IS the root" (which vfs::init() already created
    /// -- just consume its record and recurse into its children).
    fn node(&mut self, parent: Option<*mut Vnode>) -> Option<()> {
        let kind = self.u8()?;
        let name_len = self.u8()? as usize;
        if name_len >= vfs::NAME_MAX {
            return None;
        }
        let name = core::str::from_utf8(self.bytes(name_len)?).ok()?;

        if kind == VnodeType::File as u8 {
            let size = self.u32()? as usize;
            let parent = parent?; // A file can't be the top-level record.
            if !vfs::create_file(parent, name) {
                return None;
            }
            if size > 0 && !vfs::write_bytes(parent, name, self.bytes(size)?) {
                return None;
            }
            return Some(());
        }

        let dir = match parent {
            None => vfs::root(),
            Some(p) => {
                if !vfs::mkdir(p, name) {
                    return None;
                }
                vfs::resolve(p, name)?
            }
        };
        for _ in 0..self.u32()? {
            self.node(Some(dir))?;
        }
        Some(())
    }
}

/// Reads the superblock; if it has a valid magic, rebuilds the stored
/// tree under vfs::root(). True on success, including the "blank disk,
/// nothing to load yet" case (not an error -- the fresh empty root just
/// stands as-is); false only on a real I/O error or corrupt data.
pub fn load() -> bool {
    if !blk::is_ready() {
        return true;
    }
    let mut sb = vec![0u8; SECTOR_SIZE as usize];
    if !blk::read(0, sb.as_mut_ptr(), 1) {
        kprintln!("blkfs: failed to read the superblock");
        return false;
    }
    let magic = u32::from_le_bytes(sb[0..4].try_into().unwrap());
    let version = u32::from_le_bytes(sb[4..8].try_into().unwrap());
    let total_size = u64::from_le_bytes(sb[8..16].try_into().unwrap());
    if magic != MAGIC {
        kprintln!("blkfs: no filesystem on disk yet -- will format on first save");
        return true;
    }
    if version != VERSION {
        kprintln!("blkfs: on-disk version {version} unsupported (expected {VERSION}) -- ignoring");
        return true;
    }
    if total_size == 0 {
        kprintln!("blkfs: loaded empty filesystem");
        return true;
    }

    let sectors = total_size.div_ceil(SECTOR_SIZE);
    let mut buf = vec![0u8; (sectors * SECTOR_SIZE) as usize];
    if !blk::read(1, buf.as_mut_ptr(), sectors as u32) {
        kprintln!("blkfs: failed to read filesystem data");
        return false;
    }
    let mut reader = Reader { buf: &buf[..total_size as usize], pos: 0 };
    if reader.node(None).is_none() {
        kprintln!("blkfs: on-disk data is corrupt -- ignoring, starting with an empty filesystem");
        return false;
    }
    kprintln!("blkfs: loaded {total_size} bytes from disk");
    true
}

/// Serializes the entire tree and writes it (plus a fresh superblock) to
/// the disk, overwriting whatever was there. True on success (or with no
/// disk at all -- a silent no-op); false on failure, including "too big
/// for the disk".
pub fn save() -> bool {
    if !blk::is_ready() {
        return true;
    }
    let mut data = Vec::new();
    serialize(&mut data, unsafe { &*vfs::root() });
    let total_size = data.len() as u64;
    let sectors = total_size.div_ceil(SECTOR_SIZE);
    let capacity = blk::capacity_sectors();
    if 1 + sectors > capacity {
        kprintln!("blkfs: filesystem too large for the disk ({} sectors needed, {capacity} available)", 1 + sectors);
        return false;
    }
    data.resize((sectors * SECTOR_SIZE) as usize, 0);

    let mut sb = vec![0u8; SECTOR_SIZE as usize];
    sb[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    sb[4..8].copy_from_slice(&VERSION.to_le_bytes());
    sb[8..16].copy_from_slice(&total_size.to_le_bytes());
    if !blk::write(0, sb.as_ptr(), 1) || (sectors > 0 && !blk::write(1, data.as_ptr(), sectors as u32)) {
        kprintln!("blkfs: write failed");
        return false;
    }
    kprintln!("blkfs: saved {total_size} bytes ({sectors} sectors)");
    true
}
