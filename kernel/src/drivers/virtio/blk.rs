//! virtio-blk: QEMU's own disk (Anshi-disk.img), where fs/blkfs.rs keeps
//! the filesystem. Missing is a perfectly normal, non-fatal configuration
//! -- the filesystem just stays in memory.

use super::{Buffer, VirtioDevice, Virtqueue};
use crate::drivers::pci;
use crate::sync::Global;
use alloc::boxed::Box;

pub const PCI_VENDOR_ID: u16 = 0x1af4;
pub const PCI_DEVICE_ID: u16 = 0x1042;
pub const SECTOR_SIZE: u64 = 512;

const T_IN: u32 = 0; // Read.
const T_OUT: u32 = 1; // Write.
const S_OK: u8 = 0;

#[repr(C, packed)]
struct ReqHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

/// Layout mandated by the virtio spec ("Block Device") -- only the first
/// field is needed.
#[repr(C, packed)]
struct Config {
    capacity: u64, // In 512-byte sectors.
}

struct Blk {
    requestq: Virtqueue,
    capacity_sectors: u64,
    /// The header and status byte of every request -- on the heap (HHDM
    /// memory, safe to hand the device) and reused, since requests are
    /// synchronous, one at a time.
    header: Box<ReqHeader>,
    status: Box<u8>,
}

static BLK: Global<Option<Blk>> = Global::new(None);

fn blk() -> Option<&'static mut Blk> {
    unsafe { BLK.get() }.as_mut()
}

/// Finds the device (pci_enumerate() must have run), completes the init
/// handshake, and reads its capacity. False if there's no virtio-blk.
pub fn init() -> bool {
    let Some(pci) = pci::find_device(PCI_VENDOR_ID, PCI_DEVICE_ID) else {
        kprintln!("virtio-blk: no block device found -- boot QEMU with -device virtio-blk-pci");
        return false;
    };
    let Some(dev) = VirtioDevice::new(pci).filter(|d| d.negotiate_features(0)) else { return false };
    let Some(requestq) = dev.queue(0) else { return false };
    dev.driver_ok();

    let capacity_sectors = vread!(dev.device_cfg.cast::<Config>(), capacity);
    let header = Box::new(ReqHeader { kind: 0, reserved: 0, sector: 0 });
    unsafe { *BLK.get() = Some(Blk { requestq, capacity_sectors, header, status: Box::new(0) }) };
    kprintln!("virtio-blk: ready, {capacity_sectors} sectors ({} MiB)", capacity_sectors * SECTOR_SIZE / (1024 * 1024));
    true
}

/// Whether init() succeeded -- lets fs/blkfs.rs tell "no disk attached"
/// (expected, not an error) apart from a real I/O failure.
pub fn is_ready() -> bool {
    blk().is_some()
}

pub fn capacity_sectors() -> u64 {
    blk().map_or(0, |b| b.capacity_sectors)
}

/// `data` must be HHDM memory (on the heap, say) -- the device DMAs by
/// physical address.
fn request(kind: u32, sector: u64, data: *mut u8, bytes: u32) -> bool {
    let Some(b) = blk() else { return false };
    *b.header = ReqHeader { kind, reserved: 0, sector };
    *b.status = 0xFF;
    let header: *const ReqHeader = &*b.header;
    let status: *mut u8 = &mut *b.status;
    b.requestq.transfer(&[
        Buffer::to_device(header, size_of::<ReqHeader>()),
        Buffer { addr: data, len: bytes, device_writable: kind == T_IN },
        Buffer::from_device(status, 1),
    ]);
    unsafe { status.read_volatile() == S_OK }
}

pub fn read(sector: u64, buf: *mut u8, count: u32) -> bool {
    request(T_IN, sector, buf, count * SECTOR_SIZE as u32)
}

pub fn write(sector: u64, data: *const u8, count: u32) -> bool {
    request(T_OUT, sector, data.cast_mut(), count * SECTOR_SIZE as u32)
}
