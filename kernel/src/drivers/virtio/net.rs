//! virtio-net: QEMU's network card (-device virtio-net-pci). Whole
//! Ethernet frames, each behind the 12-byte virtio_net_hdr the spec puts
//! in front of every packet -- all zeroes here, since no offload is
//! negotiated. Registered with drivers/nic as an ethN; a missing device
//! is a normal configuration, the system just has no network.

use alloc::boxed::Box;

use super::{Buffer, VirtioDevice, Virtqueue};
use crate::drivers::nic::{self, MAX_FRAME, Mac, NicDriver};
use crate::drivers::pci;
use crate::mm::dma;
use crate::mm::pmm::PAGE_SIZE;

const PCI_VENDOR_ID: u16 = 0x1af4;
/// The modern id. QEMU's virtio-net-pci shows the transitional one
/// (0x1000) unless booted with disable-legacy=on, like virtio-blk.
const PCI_DEVICE_ID: u16 = 0x1041;

/// The device has a MAC address in its config space.
const F_MAC: u64 = 1 << 5;
/// The config space has a status field (link up/down).
const F_STATUS: u64 = 1 << 16;

// Device config layout ("Network Device", device configuration layout).
const CFG_MAC: usize = 0;
const CFG_STATUS: usize = 6;
const STATUS_LINK_UP: u16 = 1;

const RXQ: u16 = 0;
const TXQ: u16 = 1;

/// virtio_net_hdr under VIRTIO_F_VERSION_1: always 12 bytes (num_buffers
/// is present whether or not mergeable buffers were negotiated).
const HDR_LEN: usize = 12;
/// Room for the header and a full frame.
const BUF_SIZE: usize = 2048;
const RX_BUFFERS: u16 = 64;

struct VirtioNet {
    dev: VirtioDevice,
    rxq: Virtqueue,
    txq: Virtqueue,
    /// RX_BUFFERS buffers of BUF_SIZE, buffer i behind descriptor i.
    rx: *mut u8,
    /// One buffer: transmits are synchronous, one at a time.
    tx: *mut u8,
    has_status: bool,
}

impl VirtioNet {
    fn post(&mut self, index: u16) {
        let buf = unsafe { self.rx.add(index as usize * BUF_SIZE) };
        self.rxq.submit_chain(&[Buffer::from_device(buf, BUF_SIZE)]);
    }
}

impl NicDriver for VirtioNet {
    fn link_up(&mut self) -> bool {
        !self.has_status
            || unsafe { self.dev.device_cfg.add(CFG_STATUS).cast::<u16>().read_volatile() } & STATUS_LINK_UP != 0
    }

    fn transmit(&mut self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME {
            return false;
        }
        unsafe {
            core::ptr::write_bytes(self.tx, 0, HDR_LEN);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), self.tx.add(HDR_LEN), frame.len());
        }
        self.txq.transfer(&[Buffer::to_device(self.tx, HDR_LEN + frame.len())]);
        true
    }

    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let (id, len) = self.rxq.try_wait()?;
            let n = (len as usize).saturating_sub(HDR_LEN);
            let fits = n > 0 && n <= buf.len() && n <= BUF_SIZE - HDR_LEN;
            if fits {
                let src = unsafe { self.rx.add(id as usize * BUF_SIZE + HDR_LEN) };
                unsafe { core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), n) };
            }
            self.post(id); // The same slot the device just filled -- repost immediately.
            if fits {
                return Some(n);
            }
        }
    }
}

/// Finds every virtio-net device (pci::enumerate() must have run) and
/// registers it. How many came up.
pub fn init() -> usize {
    let mut found = 0;
    for i in 0.. {
        let Some(pci) = pci::find_device_nth(PCI_VENDOR_ID, PCI_DEVICE_ID, i) else { break };
        let Some(dev) = VirtioDevice::new(pci) else { continue };
        let Some(features) = dev.negotiate(F_MAC | F_STATUS) else { continue };
        if features & F_MAC == 0 || dev.device_cfg.is_null() {
            kprintln!("virtio-net: device has no MAC address");
            continue;
        }
        let (Some(rxq), Some(txq)) = (dev.queue(RXQ), dev.queue(TXQ)) else { continue };
        let count = RX_BUFFERS.min(rxq.size);
        let rx_pages = (count as usize * BUF_SIZE).div_ceil(PAGE_SIZE as usize) as u64;
        let (Some(rx), Some(tx)) = (dma::alloc(rx_pages), dma::alloc(1)) else {
            kprintln!("virtio-net: out of memory");
            continue;
        };
        dev.driver_ok();

        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = unsafe { dev.device_cfg.add(CFG_MAC + i).read_volatile() };
        }
        let mut net = VirtioNet { dev, rxq, txq, rx: rx.virt, tx: tx.virt, has_status: features & F_STATUS != 0 };
        (0..count).for_each(|i| net.post(i));
        let link = if net.link_up() { "up" } else { "down" };
        if nic::register("virtio-net", mac, Box::new(net)).is_some() {
            kprintln!("virtio-net: ready, mac {}, link {link}", Mac(mac));
            found += 1;
        }
    }
    found
}
