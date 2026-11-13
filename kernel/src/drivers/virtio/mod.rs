//! Generic virtio 1.x ("modern") PCI transport: capability parsing,
//! device status/feature negotiation, and split virtqueues, polling-only
//! (no IRQ/MSI-X). The device drivers (gpu, input, blk, net, snd) build on this
//! instead of touching PCI config space or virtqueue layout directly.

pub mod blk;
pub mod gpu;
pub mod input;
pub mod net;
pub mod rng;
pub mod snd;

use core::sync::atomic::{Ordering, compiler_fence};

use crate::boot::requests::{hhdm_offset, phys_to_virt};
use crate::drivers::pci::PciDevice;
use crate::mm::pmm::{self, PAGE_SIZE};
use crate::mm::vmm;

/// Feature bit; required to drive a device as "modern".
const VIRTIO_F_VERSION_1: u64 = 32;

const STATUS_ACKNOWLEDGE: u8 = 0x01;
const STATUS_DRIVER: u8 = 0x02;
const STATUS_DRIVER_OK: u8 = 0x04;
const STATUS_FEATURES_OK: u8 = 0x08;
const STATUS_FAILED: u8 = 0x80;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

const PCI_CAP_ID_VENDOR: u8 = 0x09;
const PCI_STATUS_CAP_LIST: u16 = 0x10;
const PCI_COMMAND: u8 = 0x04;
const PCI_STATUS: u8 = 0x06;
const PCI_CAP_PTR: u8 = 0x34;
const PCI_COMMAND_MEMORY: u16 = 0x0002;
const PCI_COMMAND_BUS_MASTER: u16 = 0x0004;

const CAP_COMMON_CFG: u8 = 1;
const CAP_NOTIFY_CFG: u8 = 2;
const CAP_ISR_CFG: u8 = 3;
const CAP_DEVICE_CFG: u8 = 4;

/// Layout mandated by the virtio 1.x spec ("Common configuration
/// structure layout"). Every field already falls on its natural alignment
/// in this order, so no packing is needed.
#[repr(C)]
struct CommonCfg {
    device_feature_select: u32,
    device_feature: u32,
    driver_feature_select: u32,
    driver_feature: u32,
    msix_config: u16,
    num_queues: u16,
    device_status: u8,
    config_generation: u8,
    queue_select: u16,
    queue_size: u16,
    queue_msix_vector: u16,
    queue_enable: u16,
    queue_notify_off: u16,
    queue_desc: u64,
    queue_driver: u64,
    queue_device: u64,
}

#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct Avail {
    flags: u16,
    idx: u16,
    ring: [u16; 0],
}

#[repr(C)]
struct UsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct Used {
    flags: u16,
    idx: u16,
    ring: [UsedElem; 0],
}

pub struct VirtioDevice {
    pub pci: PciDevice,
    common: *mut CommonCfg,
    notify_base: *mut u8,
    notify_off_multiplier: u32,
    #[allow(dead_code)]
    isr: *mut u8,
    /// Device-specific configuration (e.g. the block device's capacity).
    pub device_cfg: *mut u8,
}

/// One buffer of a descriptor chain.
pub struct Buffer {
    /// Must be an HHDM pointer -- PMM memory reached at phys + HHDM
    /// offset, or on the heap, or on a kernel stack (which are HHDM memory
    /// too). The device DMAs by physical address, which submit_chain()
    /// gets by subtracting the HHDM offset back out, so the kernel's own
    /// .data/.bss (in the higher-half link region, not the HHDM) will NOT
    /// work here.
    pub addr: *const u8,
    pub len: u32,
    pub device_writable: bool,
}

impl Buffer {
    pub fn to_device<T>(p: *const T, len: usize) -> Buffer {
        Buffer { addr: p.cast(), len: len as u32, device_writable: false }
    }

    pub fn from_device<T>(p: *mut T, len: usize) -> Buffer {
        Buffer { addr: p.cast(), len: len as u32, device_writable: true }
    }
}

pub struct Virtqueue {
    index: u16,
    pub size: u16,
    desc: *mut Desc,
    avail: *mut Avail,
    used: *mut Used,
    free_head: u16,
    num_free: u16,
    last_used_idx: u16,
    notify: *mut u16,
}

/// A BAR's physical base, or 0 for an I/O-space BAR -- modern virtio
/// only uses memory BARs.
fn bar_base(pci: &PciDevice, index: usize) -> u64 {
    let lo = pci.bar[index];
    if lo & 1 != 0 {
        return 0;
    }
    let mut base = (lo & 0xFFFF_FFF0) as u64;
    if (lo >> 1) & 0x3 == 0x2 {
        base |= (pci.bar[index + 1] as u64) << 32; // 64-bit BAR: the high half is the next slot.
    }
    base
}

impl VirtioDevice {
    /// Locates and maps a device's virtio-pci capabilities via the PCI
    /// capability list, enables Memory Space + Bus Master, and resets the
    /// device (spec init step 1).
    pub fn new(pci: &PciDevice) -> Option<VirtioDevice> {
        let mut dev = VirtioDevice {
            pci: *pci,
            common: core::ptr::null_mut(),
            notify_base: core::ptr::null_mut(),
            notify_off_multiplier: 0,
            isr: core::ptr::null_mut(),
            device_cfg: core::ptr::null_mut(),
        };
        if pci.read16(PCI_STATUS) & PCI_STATUS_CAP_LIST == 0 {
            kprintln!("virtio: device has no PCI capability list");
            return None;
        }
        pci.write16(PCI_COMMAND, pci.read16(PCI_COMMAND) | PCI_COMMAND_MEMORY | PCI_COMMAND_BUS_MASTER);

        let mut cap = pci.read8(PCI_CAP_PTR) & 0xFC;
        while cap != 0 {
            if pci.read8(cap) == PCI_CAP_ID_VENDOR {
                let cfg_type = pci.read8(cap + 3);
                let bar = pci.read8(cap + 4) as usize;
                let offset = pci.read32(cap + 8) as u64;
                let length = pci.read32(cap + 12) as u64;
                // BARs (especially 64-bit ones) commonly live way outside
                // what Limine's HHDM covers -- QEMU/OVMF placed this one at
                // physical ~0xc000000000 in testing -- so each capability's
                // region is mapped explicitly.
                let region = vmm::map_mmio(bar_base(pci, bar) + offset, length);
                match cfg_type {
                    CAP_COMMON_CFG => dev.common = region.cast(),
                    CAP_NOTIFY_CFG => {
                        dev.notify_base = region;
                        dev.notify_off_multiplier = pci.read32(cap + 16);
                    }
                    CAP_ISR_CFG => dev.isr = region,
                    CAP_DEVICE_CFG => dev.device_cfg = region,
                    _ => {}
                }
            }
            cap = pci.read8(cap + 1) & 0xFC;
        }
        if dev.common.is_null() || dev.notify_base.is_null() {
            kprintln!("virtio: missing required common-cfg or notify-cfg capability");
            return None;
        }

        // Spec init step 1: reset, then wait for the device to observe it.
        vwrite!(dev.common, device_status, 0);
        while vread!(dev.common, device_status) != 0 {
            core::hint::spin_loop();
        }
        Some(dev)
    }

    fn set_status(&self, bits: u8) {
        vwrite!(self.common, device_status, vread!(self.common, device_status) | bits);
    }

    /// Spec init steps 2-5: ACKNOWLEDGE, DRIVER, feature negotiation
    /// (VIRTIO_F_VERSION_1 is always requested in addition to `wanted`),
    /// FEATURES_OK. False if the device doesn't speak VERSION_1 or rejects
    /// the subset.
    pub fn negotiate_features(&self, wanted: u64) -> bool {
        self.negotiate(wanted).is_some()
    }

    /// negotiate_features(), returning the features both sides agreed on
    /// -- for a driver whose behavior depends on which of `wanted` the
    /// device actually offers.
    pub fn negotiate(&self, wanted: u64) -> Option<u64> {
        let cfg = self.common;
        self.set_status(STATUS_ACKNOWLEDGE);
        self.set_status(STATUS_DRIVER);

        vwrite!(cfg, device_feature_select, 0);
        let mut offered = vread!(cfg, device_feature) as u64;
        vwrite!(cfg, device_feature_select, 1);
        offered |= (vread!(cfg, device_feature) as u64) << 32;

        let negotiated = (wanted | 1 << VIRTIO_F_VERSION_1) & offered;
        if negotiated & 1 << VIRTIO_F_VERSION_1 == 0 {
            kprintln!("virtio: device does not offer VIRTIO_F_VERSION_1");
            return None;
        }
        vwrite!(cfg, driver_feature_select, 0);
        vwrite!(cfg, driver_feature, negotiated as u32);
        vwrite!(cfg, driver_feature_select, 1);
        vwrite!(cfg, driver_feature, (negotiated >> 32) as u32);

        self.set_status(STATUS_FEATURES_OK);
        if vread!(cfg, device_status) & STATUS_FEATURES_OK == 0 {
            kprintln!("virtio: device rejected our feature subset");
            vwrite!(cfg, device_status, STATUS_FAILED);
            return None;
        }
        Some(negotiated)
    }

    /// Sets DRIVER_OK (spec init step 8) -- once every queue needed is set
    /// up.
    pub fn driver_ok(&self) {
        self.set_status(STATUS_DRIVER_OK);
    }

    /// Selects queue `index`, allocates one PMM page each for its
    /// descriptor table, avail ring and used ring (which caps the queue at
    /// 256 descriptors -- comfortably more than any driver here uses), and
    /// enables it.
    pub fn queue(&self, index: u16) -> Option<Virtqueue> {
        let cfg = self.common;
        vwrite!(cfg, queue_select, index);
        let max = vread!(cfg, queue_size);
        if max == 0 {
            kprintln!("virtio: queue {index} not available");
            return None;
        }
        let size = max.min((PAGE_SIZE as usize / size_of::<Desc>()) as u16);

        let (desc_phys, avail_phys, used_phys) = (pmm::alloc_page(), pmm::alloc_page(), pmm::alloc_page());
        if desc_phys == 0 || avail_phys == 0 || used_phys == 0 {
            kprintln!("virtio: out of memory setting up queue {index}");
            return None;
        }
        for phys in [desc_phys, avail_phys, used_phys] {
            unsafe { core::ptr::write_bytes(phys_to_virt::<u8>(phys), 0, PAGE_SIZE as usize) };
        }
        let desc: *mut Desc = phys_to_virt(desc_phys);
        for i in 0..size {
            vwrite!(desc.add(i as usize), next, i + 1);
        }

        vwrite!(cfg, queue_size, size);
        vwrite!(cfg, queue_desc, desc_phys);
        vwrite!(cfg, queue_driver, avail_phys);
        vwrite!(cfg, queue_device, used_phys);
        vwrite!(cfg, queue_enable, 1);
        let notify_off = vread!(cfg, queue_notify_off) as usize;

        Some(Virtqueue {
            index,
            size,
            desc,
            avail: phys_to_virt(avail_phys),
            used: phys_to_virt(used_phys),
            free_head: 0,
            num_free: size,
            last_used_idx: 0,
            notify: unsafe { self.notify_base.add(notify_off * self.notify_off_multiplier as usize).cast() },
        })
    }
}

impl Virtqueue {
    fn desc(&self, i: u16) -> *mut Desc {
        unsafe { self.desc.add(i as usize) }
    }

    fn free_chain(&mut self, head: u16) {
        let mut i = head;
        loop {
            let d = self.desc(i);
            let has_next = vread!(d, flags) & DESC_F_NEXT != 0;
            let next = vread!(d, next);
            vwrite!(d, next, self.free_head);
            self.free_head = i;
            self.num_free += 1;
            if !has_next {
                break;
            }
            i = next;
        }
    }

    /// Chains up to 8 buffers into one descriptor list, publishes it on the
    /// avail ring, and kicks the device.
    pub fn submit_chain(&mut self, buffers: &[Buffer]) {
        let count = buffers.len();
        if count == 0 || count > 8 || (self.num_free as usize) < count {
            kprintln!("virtio: submit_chain rejected (count={count}, free={})", self.num_free);
            return;
        }
        let mut indices = [0u16; 8];
        let mut cur = self.free_head;
        for slot in indices.iter_mut().take(count) {
            *slot = cur;
            cur = vread!(self.desc(cur), next); // Next free slot -- read before .next is repurposed.
        }
        self.free_head = cur;
        self.num_free -= count as u16;

        let hhdm = hhdm_offset();
        for (i, b) in buffers.iter().enumerate() {
            let d = self.desc(indices[i]);
            vwrite!(d, addr, b.addr as u64 - hhdm);
            vwrite!(d, len, b.len);
            let mut flags = if b.device_writable { DESC_F_WRITE } else { 0 };
            if i + 1 < count {
                flags |= DESC_F_NEXT;
                vwrite!(d, next, indices[i + 1]);
            } else {
                vwrite!(d, next, 0);
            }
            vwrite!(d, flags, flags);
        }

        let idx = vread!(self.avail, idx);
        unsafe {
            (&raw mut (*self.avail).ring).cast::<u16>().add((idx % self.size) as usize).write_volatile(indices[0])
        };
        compiler_fence(Ordering::SeqCst); // The ring entry must be visible before idx moves.
        vwrite!(self.avail, idx, idx.wrapping_add(1));
        compiler_fence(Ordering::SeqCst);
        unsafe { self.notify.write_volatile(self.index) };
    }

    /// Non-blocking: if the used ring has advanced, reclaims the completed
    /// chain and returns (head descriptor index, bytes the device wrote).
    /// The head index matters to a caller that keeps a separate buffer per
    /// descriptor slot, like the input driver's event queue.
    pub fn try_wait(&mut self) -> Option<(u16, u32)> {
        if vread!(self.used, idx) == self.last_used_idx {
            return None;
        }
        let elem =
            unsafe { (&raw const (*self.used).ring).cast::<UsedElem>().add((self.last_used_idx % self.size) as usize) };
        let id = vread!(elem, id) as u16;
        let len = vread!(elem, len);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        self.free_chain(id);
        Some((id, len))
    }

    /// Polls until the device advances the used ring; the byte length it
    /// wrote.
    pub fn wait(&mut self) -> u32 {
        loop {
            if let Some((_, len)) = self.try_wait() {
                return len;
            }
            core::hint::spin_loop();
        }
    }

    /// submit_chain() then wait(): one synchronous request.
    pub fn transfer(&mut self, buffers: &[Buffer]) -> u32 {
        self.submit_chain(buffers);
        self.wait()
    }
}
