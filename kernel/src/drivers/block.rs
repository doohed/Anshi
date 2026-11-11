//! Every disk the storage drivers find -- NVMe, SATA (AHCI), USB mass
//! storage -- behind one interface, addressed in the disk's own logical
//! sectors (512 or 4096 bytes).
//!
//! Writes are locked: write() refuses unless the kernel was booted with
//! `allow-disk-writes` on its command line (limine.conf `cmdline:`), which
//! only test configurations pass. The live USB image never does, so
//! nothing it runs can change a disk. virtio-blk, QEMU's own disk image
//! (Anshi-disk.img), isn't registered here and isn't affected.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::boot::requests;
use crate::sync::Global;

pub const MAX_DEVICES: usize = 16;

/// What a storage driver provides. `buf` is any kernel memory -- drivers
/// bounce through their own DMA memory -- exactly `count` sectors long,
/// and `count` is at most the device's `max_sectors`.
pub trait BlockDriver {
    fn read(&mut self, lba: u64, count: u32, buf: &mut [u8]) -> bool;
    fn write(&mut self, lba: u64, count: u32, buf: &[u8]) -> bool;
}

pub struct BlockDevice {
    /// "nvme0", "sata0", "usb0".
    pub name: String,
    /// As the drive reports it, trimmed.
    pub model: String,
    pub sector_size: u32,
    pub sector_count: u64,
    /// Most a single driver call may move.
    pub max_sectors: u32,
    driver: Box<dyn BlockDriver>,
}

struct Registry {
    devices: Vec<&'static mut BlockDevice>,
    writes_allowed: bool,
}

static REGISTRY: Global<Registry> = Global::new(Registry { devices: Vec::new(), writes_allowed: false });

fn registry() -> &'static mut Registry {
    unsafe { REGISTRY.get() }
}

/// Reads `allow-disk-writes` from the command line; called once at boot.
pub fn init() {
    registry().writes_allowed = requests::option("allow-disk-writes");
    if writes_allowed() {
        kprintln!("block: disk writes ALLOWED (allow-disk-writes on the command line)");
    }
}

pub fn writes_allowed() -> bool {
    registry().writes_allowed
}

/// Adds a disk. Its index, or None if the table is full.
pub fn register(
    name: String,
    model: String,
    sector_size: u32,
    sector_count: u64,
    max_sectors: u32,
    driver: Box<dyn BlockDriver>,
) -> Option<usize> {
    let r = registry();
    if r.devices.len() == MAX_DEVICES {
        return None;
    }
    let dev = BlockDevice { name, model, sector_size, sector_count, max_sectors, driver };
    r.devices.push(Box::leak(Box::new(dev)));
    Some(r.devices.len() - 1)
}

pub fn count() -> usize {
    registry().devices.len()
}

pub fn get(index: usize) -> Option<&'static mut BlockDevice> {
    registry().devices.get_mut(index).map(|d| unsafe { &mut *(*d as *mut BlockDevice) })
}

pub fn find(name: &str) -> Option<&'static mut BlockDevice> {
    (0..count()).filter_map(get).find(|d| d.name == name)
}

impl BlockDevice {
    fn in_range(&self, lba: u64, count: u64) -> bool {
        count > 0 && lba < self.sector_count && count <= self.sector_count - lba
    }

    /// Bounds-checked, split into driver-sized pieces. `buf` holds exactly
    /// `count` sectors.
    pub fn read(&mut self, mut lba: u64, count: u64, buf: &mut [u8]) -> bool {
        if !self.in_range(lba, count) {
            return false;
        }
        let sector = self.sector_size as usize;
        for chunk in buf[..count as usize * sector].chunks_mut(self.max_sectors as usize * sector) {
            let n = (chunk.len() / sector) as u32;
            if !self.driver.read(lba, n, chunk) {
                return false;
            }
            lba += n as u64;
        }
        true
    }

    pub fn write(&mut self, mut lba: u64, count: u64, buf: &[u8]) -> bool {
        if !writes_allowed() {
            kprintln!("block: refusing to write to {} -- disk writes are locked", self.name);
            return false;
        }
        if !self.in_range(lba, count) {
            return false;
        }
        let sector = self.sector_size as usize;
        for chunk in buf[..count as usize * sector].chunks(self.max_sectors as usize * sector) {
            let n = (chunk.len() / sector) as u32;
            if !self.driver.write(lba, n, chunk) {
                return false;
            }
            lba += n as u64;
        }
        true
    }
}
