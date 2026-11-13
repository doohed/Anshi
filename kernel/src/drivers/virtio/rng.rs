//! virtio-rng (QEMU: -device virtio-rng-pci): random bytes from the host,
//! one request at a time. One of random.rs's sources; missing is fine.

use super::{Buffer, VirtioDevice, Virtqueue};
use crate::drivers::pci;
use crate::mm::dma::{self, Dma};
use crate::sync::Global;

const PCI_VENDOR_ID: u16 = 0x1af4;
/// The modern id (disable-legacy=on, as with virtio-blk and virtio-net).
const PCI_DEVICE_ID: u16 = 0x1044;
const MAX_REQUEST: usize = 64;

struct Rng {
    queue: Virtqueue,
    buf: Dma,
}

static RNG: Global<Option<Rng>> = Global::new(None);

pub fn init() -> bool {
    let Some(pci) = pci::find_device(PCI_VENDOR_ID, PCI_DEVICE_ID) else { return false };
    let Some(dev) = VirtioDevice::new(pci).filter(|d| d.negotiate_features(0)) else { return false };
    let Some(queue) = dev.queue(0) else { return false };
    let Some(buf) = dma::alloc(1) else { return false };
    dev.driver_ok();
    unsafe { *RNG.get() = Some(Rng { queue, buf }) };
    kprintln!("virtio-rng: ready");
    true
}

pub fn is_ready() -> bool {
    unsafe { RNG.get() }.is_some()
}

/// Fills as much of `out` as the device gives (it may give less); how
/// many bytes. 0 with no device.
pub fn fill(out: &mut [u8]) -> usize {
    let Some(r) = (unsafe { RNG.get() }).as_mut() else { return 0 };
    let want = out.len().min(MAX_REQUEST);
    let got = (r.queue.transfer(&[Buffer::from_device(r.buf.virt, want)]) as usize).min(want);
    unsafe { core::ptr::copy_nonoverlapping(r.buf.virt, out.as_mut_ptr(), got) };
    got
}
