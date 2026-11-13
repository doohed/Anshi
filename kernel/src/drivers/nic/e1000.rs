//! Intel 8254x/82574 Gigabit Ethernet ("e1000"): the card QEMU, UTM,
//! VirtualBox and VMware all emulate by default, and a real PC part --
//! the first network driver here that isn't virtio. Polled, with legacy
//! descriptors: a ring of receive buffers the card fills and a ring of
//! transmit buffers it drains, each descriptor's DD bit saying when the
//! card is done with it. No interrupts, no offloads.
//!
//! Register and descriptor layouts are from Intel's "PCI/PCI-X Family of
//! Gigabit Ethernet Controllers Software Developer's Manual" (8254x);
//! the 82574 keeps the same ones for everything used here.

use alloc::boxed::Box;

use super::{MAX_FRAME, Mac, NicDriver};
use crate::drivers::{pci, timer};
use crate::mm::dma::{self, Dma};
use crate::mm::vmm;
use crate::mmio::Regs;

const VENDOR_INTEL: u16 = 0x8086;
/// 82540EM (QEMU's `e1000`), 82545EM (VMware), 82574L (QEMU's `e1000e`).
const DEVICE_IDS: [u16; 3] = [0x100e, 0x100f, 0x10d3];

const CTRL: u32 = 0x0000;
const STATUS: u32 = 0x0008;
const IMC: u32 = 0x00d8;
const RCTL: u32 = 0x0100;
const TCTL: u32 = 0x0400;
const TIPG: u32 = 0x0410;
const RDBAL: u32 = 0x2800;
const RDBAH: u32 = 0x2804;
const RDLEN: u32 = 0x2808;
const RDH: u32 = 0x2810;
const RDT: u32 = 0x2818;
const TDBAL: u32 = 0x3800;
const TDBAH: u32 = 0x3804;
const TDLEN: u32 = 0x3808;
const TDH: u32 = 0x3810;
const TDT: u32 = 0x3818;
const MTA: u32 = 0x5200;
const RAL0: u32 = 0x5400;
const RAH0: u32 = 0x5404;

const CTRL_ASDE: u32 = 1 << 5;
const CTRL_SLU: u32 = 1 << 6;
const CTRL_RST: u32 = 1 << 26;
const STATUS_LU: u32 = 1 << 1;
const RAH_AV: u32 = 1 << 31;

const RCTL_EN: u32 = 1 << 1;
/// Accept every multicast frame. IPv6 runs on multicast -- router
/// advertisements, neighbor discovery -- and smoltcp filters what it
/// doesn't want, so the card's own multicast table isn't worth managing.
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15; // Accept broadcast -- DHCP answers and ARP need it.
const RCTL_SECRC: u32 = 1 << 26; // Strip the FCS.
// BSIZE 00 with BSEX 0: 2048-byte receive buffers.

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT: u32 = 0x10 << 4;
const TCTL_COLD: u32 = 0x40 << 12;
/// The manual's recommended inter-packet gap for copper.
const TIPG_VALUE: u32 = 0x0060_200a;

const DESC_DD: u8 = 1 << 0;
const RX_EOP: u8 = 1 << 1;
const TX_EOP: u8 = 1 << 0;
const TX_IFCS: u8 = 1 << 1;
const TX_RS: u8 = 1 << 3;

/// Both rings: a multiple of 8, so each ring is a multiple of 128 bytes.
const RX_COUNT: usize = 32;
const TX_COUNT: usize = 16;
const BUF_SIZE: usize = 2048;
const RESET_TIMEOUT_MS: u64 = 100;
const MMIO_SIZE: u64 = 0x20000;

#[repr(C)]
#[derive(Clone, Copy)]
struct RxDesc {
    addr: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TxDesc {
    addr: u64,
    length: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

struct E1000 {
    regs: Regs,
    rx_ring: *mut RxDesc,
    tx_ring: *mut TxDesc,
    rx_bufs: Dma,
    tx_bufs: Dma,
    rx_next: usize,
    tx_next: usize,
}

fn pages(bytes: usize) -> u64 {
    bytes.div_ceil(4096) as u64
}

impl NicDriver for E1000 {
    fn link_up(&mut self) -> bool {
        self.regs.rd32(STATUS) & STATUS_LU != 0
    }

    fn transmit(&mut self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME {
            return false;
        }
        let i = self.tx_next;
        let d = unsafe { self.tx_ring.add(i) };
        // Every slot starts out done (init()); one the card hasn't sent
        // yet means the whole ring is queued -- drop rather than wait.
        if vread!(d, status) & DESC_DD == 0 {
            return false;
        }
        let buf = unsafe { self.tx_bufs.virt.add(i * BUF_SIZE) };
        unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), buf, frame.len()) };
        vwrite!(d, length, frame.len() as u16);
        vwrite!(d, cmd, TX_EOP | TX_IFCS | TX_RS);
        vwrite!(d, status, 0);
        self.tx_next = (i + 1) % TX_COUNT;
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.regs.wr32(TDT, self.tx_next as u32);
        true
    }

    fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        loop {
            let i = self.rx_next;
            let d = unsafe { self.rx_ring.add(i) };
            let status = vread!(d, status);
            if status & DESC_DD == 0 {
                return None;
            }
            let len = vread!(d, length) as usize;
            // A frame split across buffers (larger than BUF_SIZE) or one
            // with an error is dropped; neither happens at a 1500 MTU.
            let good = status & RX_EOP != 0 && vread!(d, errors) == 0 && len <= out.len() && len <= BUF_SIZE;
            if good {
                let src = unsafe { self.rx_bufs.virt.add(i * BUF_SIZE) };
                unsafe { core::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), len) };
            }
            // Hand the slot back: the tail is the last one the card may fill.
            vwrite!(d, status, 0);
            self.rx_next = (i + 1) % RX_COUNT;
            self.regs.wr32(RDT, i as u32);
            if good {
                return Some(len);
            }
        }
    }
}

fn bring_up(dev: &pci::PciDevice) -> Option<(E1000, [u8; 6])> {
    // Memory space + bus master, legacy INTx off (polled).
    dev.write16(0x04, dev.read16(0x04) | 0x0406);
    let mut base = (dev.bar[0] & !0xF) as u64;
    if dev.bar[0] & 0x6 == 0x4 {
        base |= (dev.bar[1] as u64) << 32;
    }
    if base == 0 || dev.bar[0] & 1 != 0 {
        kprintln!("e1000: BAR0 is not a memory BAR");
        return None;
    }
    let regs = Regs(vmm::map_mmio(base, MMIO_SIZE));

    regs.wr32(IMC, 0xffff_ffff);
    regs.wr32(CTRL, regs.rd32(CTRL) | CTRL_RST);
    timer::sleep_ms(1); // The manual: wait at least 1 us before reading back.
    let deadline = timer::uptime_ms() + RESET_TIMEOUT_MS;
    while regs.rd32(CTRL) & CTRL_RST != 0 {
        if timer::uptime_ms() > deadline {
            kprintln!("e1000: reset timed out");
            return None;
        }
        core::hint::spin_loop();
    }
    regs.wr32(IMC, 0xffff_ffff); // Reset turned them back on.
    regs.wr32(CTRL, regs.rd32(CTRL) | CTRL_SLU | CTRL_ASDE);

    // The card loads its MAC address from its EEPROM into receive
    // address 0 at reset.
    let (ral, rah) = (regs.rd32(RAL0), regs.rd32(RAH0));
    if rah & RAH_AV == 0 {
        kprintln!("e1000: no MAC address loaded");
        return None;
    }
    let mac = [ral as u8, (ral >> 8) as u8, (ral >> 16) as u8, (ral >> 24) as u8, rah as u8, (rah >> 8) as u8];
    for i in 0..128 {
        regs.wr32(MTA + i * 4, 0);
    }

    let rx_ring = dma::alloc(pages(RX_COUNT * size_of::<RxDesc>()))?;
    let tx_ring = dma::alloc(pages(TX_COUNT * size_of::<TxDesc>()))?;
    let rx_bufs = dma::alloc(pages(RX_COUNT * BUF_SIZE))?;
    let tx_bufs = dma::alloc(pages(TX_COUNT * BUF_SIZE))?;
    let rx: *mut RxDesc = rx_ring.as_ptr();
    let tx: *mut TxDesc = tx_ring.as_ptr();
    for i in 0..RX_COUNT {
        let d = unsafe { rx.add(i) };
        vwrite!(d, addr, rx_bufs.phys + (i * BUF_SIZE) as u64);
        vwrite!(d, status, 0);
    }
    for i in 0..TX_COUNT {
        let d = unsafe { tx.add(i) };
        vwrite!(d, addr, tx_bufs.phys + (i * BUF_SIZE) as u64);
        vwrite!(d, status, DESC_DD);
    }

    regs.wr32(RDBAL, rx_ring.phys as u32);
    regs.wr32(RDBAH, (rx_ring.phys >> 32) as u32);
    regs.wr32(RDLEN, (RX_COUNT * size_of::<RxDesc>()) as u32);
    regs.wr32(RDH, 0);
    regs.wr32(RDT, RX_COUNT as u32 - 1);
    regs.wr32(RCTL, RCTL_EN | RCTL_MPE | RCTL_BAM | RCTL_SECRC);

    regs.wr32(TDBAL, tx_ring.phys as u32);
    regs.wr32(TDBAH, (tx_ring.phys >> 32) as u32);
    regs.wr32(TDLEN, (TX_COUNT * size_of::<TxDesc>()) as u32);
    regs.wr32(TDH, 0);
    regs.wr32(TDT, 0);
    regs.wr32(TIPG, TIPG_VALUE);
    regs.wr32(TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);

    Some((E1000 { regs, rx_ring: rx, tx_ring: tx, rx_bufs, tx_bufs, rx_next: 0, tx_next: 0 }, mac))
}

/// Finds every supported Intel card (pci::enumerate() must have run) and
/// registers it. How many came up.
pub fn init() -> usize {
    let mut found = 0;
    for &id in &DEVICE_IDS {
        for n in 0.. {
            let Some(dev) = pci::find_device_nth(VENDOR_INTEL, id, n) else { break };
            let Some((mut nic, mac)) = bring_up(dev) else { continue };
            let link = if nic.link_up() { "up" } else { "down" };
            if super::register("e1000", mac, Box::new(nic)).is_some() {
                kprintln!("e1000: {id:04x} ready, mac {}, link {link}", Mac(mac));
                found += 1;
            }
        }
    }
    found
}
