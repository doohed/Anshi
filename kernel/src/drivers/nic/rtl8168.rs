//! Realtek RTL8111/8168 (Gigabit) and RTL8125 (2.5 Gigabit) Ethernet --
//! what most PC boards have, the B550M in docs/real-hardware.md among
//! them. Polled, with the "C+" descriptor rings both families share: a
//! ring of receive buffers the card fills and a ring of transmit buffers
//! it drains, each descriptor's OWN bit saying whose turn it is.
//!
//! QEMU emulates neither chip, so unlike every other driver here this one
//! was written against the reference rather than run first: Linux's
//! r8169 (drivers/net/ethernet/realtek/r8169_main.c), whose bring-up
//! order it follows step for step. Each revision needs its own tuning
//! (PCIe PHY values, FIFO sizes, undocumented MAC registers); the two a
//! B550 board is likely to have, RTL8111H and RTL8125B, get Linux's full
//! sequence, the rest of the family the common part only. The boot log
//! names the revision found, and the kernel shell's `nicdebug` dumps the
//! registers that matter when it doesn't work.
//!
//! The Ethernet PHY is left to negotiate on its own: powered up, with
//! autonegotiation restarted. Linux also patches the PHY's firmware on
//! some revisions; that's not done here.

use alloc::boxed::Box;

use super::{MAX_FRAME, Mac, NicDriver};
use crate::drivers::{pci, timer};
use crate::mm::dma::{self, Dma};
use crate::mm::vmm;
use crate::mmio::Regs;

const VENDOR_REALTEK: u16 = 0x10ec;
/// 8168/8111 (and the 8161 variant), 8125, and the 8125 sold as the Killer
/// E3000 (vendor 0x10ec too).
const DEVICE_IDS: [u16; 4] = [0x8168, 0x8161, 0x8125, 0x3000];

// Registers (r8169_main.c's names).
const MAC0: u32 = 0x00;
const MAC4: u32 = 0x04;
const MAR0: u32 = 0x08;
const TX_DESC_LOW: u32 = 0x20;
const TX_DESC_HIGH: u32 = 0x24;
const CHIP_CMD: u32 = 0x37;
const TX_POLL: u32 = 0x38;
const INTR_MASK: u32 = 0x3c;
const INTR_STATUS: u32 = 0x3e;
const TX_CONFIG: u32 = 0x40;
const RX_CONFIG: u32 = 0x44;
const CFG9346: u32 = 0x50;
const CONFIG1: u32 = 0x52;
const CONFIG2: u32 = 0x53;
const CONFIG3: u32 = 0x54;
const CONFIG5: u32 = 0x56;
const PHYAR: u32 = 0x60;
const CSIDR: u32 = 0x64;
const CSIAR: u32 = 0x68;
const PHY_STATUS: u32 = 0x6c;
const ERIDR: u32 = 0x70;
const ERIAR: u32 = 0x74;
const EPHYAR: u32 = 0x80;
const OCPDR: u32 = 0xb0;
const GPHY_OCP: u32 = 0xb8;
const DLLPR: u32 = 0xd0;
const MCU: u32 = 0xd3;
const RX_MAX_SIZE: u32 = 0xda;
const CPLUS_CMD: u32 = 0xe0;
const INTR_MITIGATE: u32 = 0xe2;
const RX_DESC_LOW: u32 = 0xe4;
const RX_DESC_HIGH: u32 = 0xe8;
const MAX_TX_PACKET_SIZE: u32 = 0xec;
const MISC: u32 = 0xf0;
const MISC_1: u32 = 0xf2;
const TX_CONFIG_V2: u32 = 0x60b0;
// RTL8125 only.
const INT_CFG0_8125: u32 = 0x34;
const INTR_MASK_8125: u32 = 0x38;
const INTR_STATUS_8125: u32 = 0x3c;
const INT_CFG1_8125: u32 = 0x7a;
const TX_POLL_8125: u32 = 0x90;
const MAC0_BKP: u32 = 0x19e0;
const RSS_CTRL_8125: u32 = 0x4500;
const Q_NUM_CTRL_8125: u32 = 0x4800;
const EEE_TXIDLE_TIMER_8125: u32 = 0x6048;

const CMD_RESET: u8 = 0x10;
const CMD_RX_ENABLE: u8 = 0x08;
const CMD_TX_ENABLE: u8 = 0x04;
const CMD_STOP_REQ: u8 = 0x80;
const NPQ: u8 = 0x40;
const CFG9346_UNLOCK: u8 = 0xc0;
const CFG9346_LOCK: u8 = 0x00;
const LINK_STATUS: u8 = 0x02;
const PHY_1000: u8 = 0x10;
const PHY_100: u8 = 0x08;
const PHY_10: u8 = 0x04;
const PHY_FULL_DUPLEX: u8 = 0x01;
/// PHYstatus bit 10 on the 8125 (a 16-bit register there).
const PHY_2500_8125: u16 = 0x400;

const ACCEPT_BROADCAST: u32 = 0x08;
const ACCEPT_MULTICAST: u32 = 0x04;
const ACCEPT_MY_PHYS: u32 = 0x02;
const RX_ACCEPT_MASK: u32 = 0x3f;
const RX128_INT_EN: u32 = 1 << 15;
const RX_MULTI_EN: u32 = 1 << 14;
const RX_FIFO_THRESH: u32 = 7 << 13;
const RX_EARLY_OFF: u32 = 1 << 11;
const RX_PAUSE_SLOT_ON: u32 = 1 << 11;
const RX_DMA_BURST: u32 = 7 << 8;
const RX_FETCH_DFLT_8125: u32 = 8 << 27;
const TX_DMA_BURST: u32 = 7 << 8;
const TX_IFG: u32 = 3 << 24;
const TXCFG_AUTO_FIFO: u32 = 1 << 7;
const TXCFG_EMPTY: u32 = 1 << 11;

const NORMAL_MODE: u16 = 1 << 13;
const INTT_MASK: u16 = 0x3;
const ASPM_EN: u8 = 1 << 0; // Config5
const CLKREQ_EN: u8 = 1 << 7; // Config2
const RDY_TO_L23: u8 = 1 << 1; // Config3
const PFM_EN: u8 = 1 << 6; // DLLPR
const TX_10M_PS_EN: u8 = 1 << 7; // DLLPR
const PFM_D3COLD_EN: u8 = 1 << 6; // MISC_1
const RXDV_GATED_EN: u32 = 1 << 19; // MISC
const NOW_IS_OOB: u8 = 1 << 7; // MCU
const RXTX_EMPTY: u8 = 0x30; // MCU
const LINK_LIST_RDY: u8 = 1 << 1; // MCU

const FLAG: u32 = 0x8000_0000;
const ERIAR_MASK_0001: u32 = 0x1 << 12;
const ERIAR_MASK_0011: u32 = 0x3 << 12;
const ERIAR_MASK_1111: u32 = 0xf << 12;
const CSIAR_BYTE_ENABLE: u32 = 0x0000_f000;

// MII registers of the PHY, and where the 8168g+ map them in OCP space.
const OCP_STD_PHY_BASE: u32 = 0xa400;
const MII_BMCR: u32 = 0;
const BMCR_ANENABLE: u16 = 0x1000;
const BMCR_ANRESTART: u16 = 0x0200;
const BMCR_PDOWN: u16 = 0x0800;
const BMCR_ISOLATE: u16 = 0x0400;

// Descriptors.
const DESC_OWN: u32 = 1 << 31;
const RING_END: u32 = 1 << 30;
const FIRST_FRAG: u32 = 1 << 29;
const LAST_FRAG: u32 = 1 << 28;
const RX_RES: u32 = 1 << 21;
const LEN_MASK: u32 = 0x3fff;

const RX_COUNT: usize = 64;
const TX_COUNT: usize = 16;
const BUF_SIZE: usize = 2048;
const MIN_FRAME: usize = 60;
/// The 8125's registers reach 0x60b0; the 8168's BAR is only 4 KiB, but
/// nothing past 0xff is touched on one.
const MMIO_SIZE: u64 = 0x10000;

/// The frame check sequence the card leaves on every received frame.
const FCS_LEN: usize = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    opts1: u32,
    opts2: u32,
    addr: u64,
}

/// One revision: (mask, XID, Linux's RTL_GIGA_MAC_VER number, name), as
/// r8169_main.c's rtl_chip_infos lists them -- the 8168/8111 and 8125
/// entries.
const CHIPS: [(u16, u16, u8, &str); 37] = [
    (0x7cf, 0x681, 66, "RTL8125BP"),
    (0x7cf, 0x708, 65, "RTL8125CP"),
    (0x7cf, 0x68b, 64, "RTL9151A"),
    (0x7cf, 0x68a, 64, "RTL8125K"),
    (0x7cf, 0x689, 64, "RTL8125D"),
    (0x7cf, 0x688, 64, "RTL8125D"),
    (0x7cf, 0x641, 63, "RTL8125B"),
    (0x7cf, 0x609, 61, "RTL8125A"),
    (0x7cf, 0x54b, 52, "RTL8168fp/8117"),
    (0x7cf, 0x54a, 52, "RTL8168fp/8117"),
    (0x7cf, 0x502, 51, "RTL8168ep/8111ep"),
    (0x7cf, 0x541, 46, "RTL8168h/8111h"),
    (0x7cf, 0x6c0, 46, "RTL8168M"),
    (0x7cf, 0x5c8, 44, "RTL8411b"),
    (0x7cf, 0x509, 42, "RTL8168gu/8111gu"),
    (0x7cf, 0x4c0, 40, "RTL8168g/8111g"),
    (0x7c8, 0x488, 38, "RTL8411"),
    (0x7cf, 0x481, 36, "RTL8168f/8111f"),
    (0x7cf, 0x480, 35, "RTL8168f/8111f"),
    (0x7c8, 0x2c8, 34, "RTL8168evl/8111evl"),
    (0x7cf, 0x2c1, 32, "RTL8168e/8111e"),
    (0x7c8, 0x2c0, 33, "RTL8168e/8111e"),
    (0x7cf, 0x281, 25, "RTL8168d/8111d"),
    (0x7c8, 0x280, 26, "RTL8168d/8111d"),
    (0x7cf, 0x28a, 28, "RTL8168dp/8111dp"),
    (0x7cf, 0x28b, 31, "RTL8168dp/8111dp"),
    (0x7cf, 0x3c9, 23, "RTL8168cp/8111cp"),
    (0x7cf, 0x3c8, 18, "RTL8168cp/8111cp"),
    (0x7c8, 0x3c8, 24, "RTL8168cp/8111cp"),
    (0x7cf, 0x3c0, 19, "RTL8168c/8111c"),
    (0x7cf, 0x3c2, 20, "RTL8168c/8111c"),
    (0x7cf, 0x3c3, 21, "RTL8168c/8111c"),
    (0x7c8, 0x3c0, 22, "RTL8168c/8111c"),
    (0x7c8, 0x380, 17, "RTL8168b/8111b"),
    (0x7c8, 0x300, 11, "RTL8168b/8111b"),
    (0x7c8, 0x180, 12, "RTL8168b/8111b"),
    // Revisions past the table report this, and their real XID in
    // TX_CONFIG_V2 -- newer than anything this driver knows.
    (0x7cf, 0x7c8, 0, "extended"),
];

struct Rtl {
    regs: Regs,
    /// Linux's RTL_GIGA_MAC_VER_xx.
    ver: u8,
    name: &'static str,
    /// The PCI function number, which CSI accesses carry.
    func: u8,
    /// CPlusCmd's mode bits as the firmware left them, read at probe.
    cp_cmd: u16,
    rx_ring: *mut Desc,
    tx_ring: *mut Desc,
    rx_bufs: Dma,
    tx_bufs: Dma,
    rx_ring_phys: u64,
    tx_ring_phys: u64,
    rx_next: usize,
    tx_next: usize,
    rx_dropped: u64,
}

fn pages(bytes: usize) -> u64 {
    bytes.div_ceil(4096) as u64
}

impl Rtl {
    fn is_8125(&self) -> bool {
        self.ver >= 61
    }

    /// 8168evl and later 8168s: the ones with the auto FIFO and ERI MAC.
    fn is_8168evl_up(&self) -> bool {
        (34..=52).contains(&self.ver) && self.ver != 39
    }

    /// 8168g and later, the 8125s: OCP-mapped PHY, the RX DV gate.
    fn is_8168g_up(&self) -> bool {
        self.ver >= 40
    }

    // --- Timing ---

    /// About `us` microseconds: one MMIO read each, which takes at least
    /// that long on a real PCIe bus.
    fn udelay(&self, us: u32) {
        for _ in 0..us {
            self.regs.rd8(CHIP_CMD);
        }
    }

    /// Polls `cond` until it's `want` or about `timeout_us` passes (the
    /// tick is 10 ms, so short timeouts round up).
    fn wait(&self, want: bool, timeout_us: u64, cond: impl Fn(&Rtl) -> bool) -> bool {
        let deadline = timer::uptime_ms() + timeout_us / 1000 + 20;
        loop {
            if cond(self) == want {
                return true;
            }
            if timer::uptime_ms() > deadline {
                return false;
            }
            self.udelay(10);
        }
    }

    // --- Indirect register spaces ---

    /// ERI: extended GMAC registers, through ERIAR/ERIDR.
    fn eri_write(&self, addr: u32, mask: u32, val: u32) {
        self.regs.wr32(ERIDR, val);
        self.regs.wr32(ERIAR, FLAG | mask | addr);
        self.wait(false, 10_000, |r| r.regs.rd32(ERIAR) & FLAG != 0);
    }

    fn eri_read(&self, addr: u32) -> u32 {
        self.regs.wr32(ERIAR, ERIAR_MASK_1111 | addr);
        if self.wait(true, 10_000, |r| r.regs.rd32(ERIAR) & FLAG != 0) { self.regs.rd32(ERIDR) } else { !0 }
    }

    fn eri_modify(&self, addr: u32, set: u32, clear: u32) {
        let v = self.eri_read(addr);
        self.eri_write(addr, ERIAR_MASK_1111, (v & !clear) | set);
    }

    /// MAC OCP: the 8168g+/8125 MAC's own register space.
    fn mac_ocp_write(&self, reg: u32, data: u16) {
        self.regs.wr32(OCPDR, FLAG | (reg << 15) | data as u32);
    }

    fn mac_ocp_read(&self, reg: u32) -> u16 {
        self.regs.wr32(OCPDR, reg << 15);
        self.regs.rd32(OCPDR) as u16
    }

    fn mac_ocp_modify(&self, reg: u32, clear: u16, set: u16) {
        let v = self.mac_ocp_read(reg);
        self.mac_ocp_write(reg, (v & !clear) | set);
    }

    /// PHY OCP: the 8168g+/8125 PHY's registers, standard MII ones at
    /// OCP_STD_PHY_BASE + 2 * reg.
    fn phy_ocp_write(&self, reg: u32, data: u16) {
        self.regs.wr32(GPHY_OCP, FLAG | (reg << 15) | data as u32);
        self.wait(false, 250, |r| r.regs.rd32(GPHY_OCP) & FLAG != 0);
    }

    fn phy_ocp_read(&self, reg: u32) -> Option<u16> {
        self.regs.wr32(GPHY_OCP, reg << 15);
        self.wait(true, 250, |r| r.regs.rd32(GPHY_OCP) & FLAG != 0).then(|| self.regs.rd32(GPHY_OCP) as u16)
    }

    /// A standard MII register of the PHY.
    fn mii_write(&self, reg: u32, data: u16) {
        if self.is_8168g_up() {
            self.phy_ocp_write(OCP_STD_PHY_BASE + reg * 2, data);
        } else {
            self.regs.wr32(PHYAR, FLAG | (reg & 0x1f) << 16 | data as u32);
            self.wait(false, 2000, |r| r.regs.rd32(PHYAR) & FLAG != 0);
            self.udelay(20);
        }
    }

    fn mii_read(&self, reg: u32) -> Option<u16> {
        if self.is_8168g_up() {
            return self.phy_ocp_read(OCP_STD_PHY_BASE + reg * 2);
        }
        self.regs.wr32(PHYAR, (reg & 0x1f) << 16);
        let ok = self.wait(true, 2000, |r| r.regs.rd32(PHYAR) & FLAG != 0);
        self.udelay(20);
        ok.then(|| self.regs.rd32(PHYAR) as u16)
    }

    /// EPHY: the PCIe PHY. The register number is masked to 5 bits as
    /// Linux does, so the 8125's second-lane entries (0x4x, 0x6x) land
    /// on the first lane's -- with the same values, so harmlessly.
    fn ephy_write(&self, reg: u32, value: u16) {
        self.regs.wr32(EPHYAR, FLAG | value as u32 | (reg & 0x1f) << 16);
        self.wait(false, 1000, |r| r.regs.rd32(EPHYAR) & FLAG != 0);
        self.udelay(10);
    }

    fn ephy_read(&self, reg: u32) -> u16 {
        self.regs.wr32(EPHYAR, (reg & 0x1f) << 16);
        if self.wait(true, 1000, |r| r.regs.rd32(EPHYAR) & FLAG != 0) { self.regs.rd32(EPHYAR) as u16 } else { !0 }
    }

    /// (register, bits to clear, bits to set) for each.
    fn ephy_init(&self, table: &[(u32, u16, u16)]) {
        for &(reg, clear, set) in table {
            let v = (self.ephy_read(reg) & !clear) | set;
            self.ephy_write(reg, v);
        }
    }

    /// CSI: the chip's own view of its PCI config space, reaching the
    /// extended space (past 0xff) that legacy config access can't.
    fn csi_modify(&self, addr: u32, clear: u32, set: u32) {
        let func = (self.func as u32) << 16;
        self.regs.wr32(CSIAR, (addr & 0xfff) | func | CSIAR_BYTE_ENABLE);
        if !self.wait(true, 1000, |r| r.regs.rd32(CSIAR) & FLAG != 0) {
            return;
        }
        let v = (self.regs.rd32(CSIDR) & !clear) | set;
        self.regs.wr32(CSIDR, v);
        self.regs.wr32(CSIAR, FLAG | (addr & 0xfff) | CSIAR_BYTE_ENABLE | func);
        self.wait(false, 1000, |r| r.regs.rd32(CSIAR) & FLAG != 0);
    }

    // --- Bring-up, in r8169's order ---

    fn unlock(&self) {
        self.regs.wr8(CFG9346, CFG9346_UNLOCK);
    }

    fn lock(&self) {
        self.regs.wr8(CFG9346, CFG9346_LOCK);
    }

    fn mask_and_ack_irqs(&self) {
        if self.is_8125() {
            self.regs.wr32(INTR_MASK_8125, 0);
            self.regs.wr32(INTR_STATUS_8125, !0);
        } else {
            self.regs.wr16(INTR_MASK, 0);
            self.regs.wr16(INTR_STATUS, 0xffff);
        }
        self.regs.rd8(CHIP_CMD);
    }

    fn init_rxcfg(&self) -> u32 {
        let v = match self.ver {
            ..=17 => RX_FIFO_THRESH | RX_DMA_BURST,
            18..=24 | 34..=36 | 38 => RX128_INT_EN | RX_MULTI_EN | RX_DMA_BURST,
            40..=52 => RX128_INT_EN | RX_MULTI_EN | RX_DMA_BURST | RX_EARLY_OFF,
            61 => RX_FETCH_DFLT_8125 | RX_DMA_BURST,
            63.. => RX_FETCH_DFLT_8125 | RX_DMA_BURST | RX_PAUSE_SLOT_ON,
            _ => RX128_INT_EN | RX_DMA_BURST,
        };
        self.regs.wr32(RX_CONFIG, v);
        v
    }

    fn wait_ll_share_fifo_ready(&self) {
        self.wait(true, 4200, |r| r.regs.rd8(MCU) & LINK_LIST_RDY != 0);
    }

    fn wait_txrx_fifo_empty(&self) {
        match self.ver {
            40..=52 => {
                self.wait(true, 4200, |r| r.regs.rd32(TX_CONFIG) & TXCFG_EMPTY != 0);
                self.wait(true, 4200, |r| r.regs.rd8(MCU) & RXTX_EMPTY == RXTX_EMPTY);
            }
            61 => {
                self.wait(true, 4200, |r| r.regs.rd8(MCU) & RXTX_EMPTY == RXTX_EMPTY);
            }
            63.. => {
                self.regs.wr8(CHIP_CMD, self.regs.rd8(CHIP_CMD) | CMD_STOP_REQ);
                self.wait(true, 4200, |r| r.regs.rd8(MCU) & RXTX_EMPTY == RXTX_EMPTY);
                self.wait(true, 4200, |r| r.regs.rd16(INTR_MITIGATE) & 0x0103 == 0x0103);
            }
            _ => {}
        }
    }

    /// The RX DV gate holds received data back from the MAC; the 8168g+
    /// come out of reset with it closed, and receive nothing until it
    /// opens.
    fn enable_rxdvgate(&self) {
        self.regs.wr32(MISC, self.regs.rd32(MISC) | RXDV_GATED_EN);
        timer::sleep_ms(2);
        self.wait_txrx_fifo_empty();
    }

    fn disable_rxdvgate(&self) {
        self.regs.wr32(MISC, self.regs.rd32(MISC) & !RXDV_GATED_EN);
    }

    /// Once, at probe: takes the 8168g+/8125 out of the firmware's
    /// out-of-band (management) mode.
    fn hw_initialize(&self) {
        if !self.is_8168g_up() {
            return;
        }
        self.enable_rxdvgate();
        self.regs.wr8(CHIP_CMD, self.regs.rd8(CHIP_CMD) & !(CMD_TX_ENABLE | CMD_RX_ENABLE));
        timer::sleep_ms(1);
        self.regs.wr8(MCU, self.regs.rd8(MCU) & !NOW_IS_OOB);
        self.mac_ocp_modify(0xe8de, 1 << 14, 0);
        self.wait_ll_share_fifo_ready();
        if self.is_8125() {
            self.mac_ocp_write(0xc0aa, 0x07d0);
            self.mac_ocp_write(0xc0a6, 0x0150);
            self.mac_ocp_write(0xc01e, 0x5555);
        } else {
            self.mac_ocp_modify(0xe8de, 0, 1 << 15);
        }
        self.wait_ll_share_fifo_ready();
    }

    /// The station address: where the firmware left it for this
    /// revision, else the MAC0 registers.
    fn read_mac(&self) -> [u8; 6] {
        let mut mac = [0u8; 6];
        if self.is_8168evl_up() && self.ver != 34 {
            mac[..4].copy_from_slice(&self.eri_read(0xe0).to_le_bytes());
            mac[4..].copy_from_slice(&(self.eri_read(0xe4) as u16).to_le_bytes());
        } else if self.is_8125() {
            mac[..4].copy_from_slice(&self.regs.rd32(MAC0_BKP).to_le_bytes());
            mac[4..].copy_from_slice(&self.regs.rd16(MAC0_BKP + 4).to_le_bytes());
        }
        if mac == [0; 6] || mac == [0xff; 6] {
            mac[..4].copy_from_slice(&self.regs.rd32(MAC0).to_le_bytes());
            mac[4..].copy_from_slice(&self.regs.rd16(MAC4).to_le_bytes());
        }
        mac
    }

    /// What the receive filter matches (rtl_rar_set).
    fn set_mac(&self, mac: [u8; 6]) {
        self.unlock();
        self.regs.wr32(MAC4, u16::from_le_bytes([mac[4], mac[5]]) as u32);
        self.regs.rd8(CHIP_CMD);
        self.regs.wr32(MAC0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
        self.regs.rd8(CHIP_CMD);
        self.lock();
    }

    /// Powers the PHY up and has it negotiate whatever both ends can do.
    fn init_phy(&self) {
        if matches!(self.ver, 28 | 31) {
            // The 8168dp reach their PHY through yet another interface,
            // not ported: the firmware's PHY setup stands.
            kprintln!("rtl8168: {}: PHY left as the firmware set it up", self.name);
            return;
        }
        let bmcr = self.mii_read(MII_BMCR).unwrap_or(0);
        let bmcr = (bmcr & !(BMCR_PDOWN | BMCR_ISOLATE)) | BMCR_ANENABLE | BMCR_ANRESTART;
        if self.ver == 40 {
            // rtl8168g_phy_suspend_quirk: a powered-up PHY on the 8168g
            // needs these MAC bits set too.
            self.eri_modify(0x1a8, 0xfc00_0000, 0);
        }
        self.mii_write(MII_BMCR, bmcr);
    }

    /// rtl8169_cleanup + rtl_hw_reset: stop, drain, reset.
    fn reset(&self) {
        self.mask_and_ack_irqs();
        self.regs.wr32(RX_CONFIG, self.regs.rd32(RX_CONFIG) & !RX_ACCEPT_MASK);
        match self.ver {
            34..=38 => {
                self.regs.wr8(CHIP_CMD, self.regs.rd8(CHIP_CMD) | CMD_STOP_REQ);
                self.wait(true, 66_600, |r| r.regs.rd32(TX_CONFIG) & TXCFG_EMPTY != 0);
            }
            40.. => {
                self.enable_rxdvgate();
                timer::sleep_ms(2);
            }
            _ => {
                self.regs.wr8(CHIP_CMD, self.regs.rd8(CHIP_CMD) | CMD_STOP_REQ);
                self.udelay(100);
            }
        }
        self.regs.wr8(CHIP_CMD, CMD_RESET);
        self.wait(false, 10_000, |r| r.regs.rd8(CHIP_CMD) & CMD_RESET != 0);
    }

    /// rtl_hw_aspm_clkreq_enable(false): no link power saving -- Linux
    /// keeps it off too on most boards, since it hangs some.
    fn disable_aspm_clkreq(&self) {
        if self.ver < 32 {
            return;
        }
        self.regs.wr8(CONFIG5, self.regs.rd8(CONFIG5) & !ASPM_EN);
        self.regs.wr8(CONFIG2, self.regs.rd8(CONFIG2) & !CLKREQ_EN);
        if matches!(self.ver, 46..=48 | 52 | 61..) {
            self.mac_ocp_modify(0xe092, 0x00ff, 0);
        }
    }

    fn set_def_aspm_entry_latency(&self) {
        // L0 7 us, L1 16 us, in config space at 0x070f -- past what
        // legacy config access reaches, so through CSI.
        self.csi_modify(0x070c, 0xff00_0000, 0x27 << 24);
    }

    fn hw_start_8168h_1(&self) {
        self.ephy_init(&[
            (0x1e, 0x0800, 0x0001),
            (0x1d, 0x0000, 0x0800),
            (0x05, 0xffff, 0x2089),
            (0x06, 0xffff, 0x5881),
            (0x04, 0xffff, 0x854a),
            (0x01, 0xffff, 0x068b),
        ]);
        // FIFO sizes, and the pause-frame thresholds.
        self.eri_write(0xc8, ERIAR_MASK_1111, (0x08 << 16) | 0x02);
        self.eri_write(0xe8, ERIAR_MASK_1111, (0x10 << 16) | 0x06);
        self.eri_write(0xcc, ERIAR_MASK_0001, 0x38);
        self.eri_write(0xd0, ERIAR_MASK_0001, 0x48);
        self.set_def_aspm_entry_latency();
        // Reset the packet filter.
        self.eri_modify(0xdc, 0, 1);
        self.eri_modify(0xdc, 1, 0);
        self.eri_modify(0xdc, 0x001c, 0);
        self.eri_write(0x5f0, ERIAR_MASK_0011, 0x4f87);
        self.disable_rxdvgate();
        self.eri_write(0xc0, ERIAR_MASK_0011, 0);
        self.eri_write(0xb8, ERIAR_MASK_0011, 0);
        self.regs.wr8(DLLPR, self.regs.rd8(DLLPR) & !PFM_EN);
        self.regs.wr8(MISC_1, self.regs.rd8(MISC_1) & !PFM_D3COLD_EN);
        self.regs.wr8(DLLPR, self.regs.rd8(DLLPR) & !TX_10M_PS_EN);
        self.eri_modify(0x1b0, 0, 1 << 12);
        self.regs.wr8(CONFIG3, self.regs.rd8(CONFIG3) & !RDY_TO_L23);
        // The PHY's 1 ms counter, calibrated from its sawtooth count
        // (PHY page 0x0c42, register 0x13).
        let saw = self.phy_ocp_read((0x0c42 << 4) + (0x13 - 0x10) * 2).unwrap_or(0) & 0x3fff;
        if saw > 0 {
            self.mac_ocp_modify(0xd412, 0x0fff, (16_000_000 / saw as u32) as u16 & 0x0fff);
        }
        self.mac_ocp_modify(0xe056, 0x00f0, 0x0000);
        self.mac_ocp_modify(0xe052, 0x6000, 0x8008);
        self.mac_ocp_modify(0xe0d6, 0x01ff, 0x017f);
        self.mac_ocp_modify(0xd420, 0x0fff, 0x047f);
        self.mac_ocp_write(0xe63e, 0x0001);
        self.mac_ocp_write(0xe63e, 0x0000);
        self.mac_ocp_write(0xc094, 0x0000);
        self.mac_ocp_write(0xc09e, 0x0000);
    }

    fn hw_start_8125_common(&self) {
        self.regs.wr8(CONFIG3, self.regs.rd8(CONFIG3) & !RDY_TO_L23);
        self.regs.wr16(0x382, 0x221b);
        self.regs.wr32(RSS_CTRL_8125, 0);
        self.regs.wr16(Q_NUM_CTRL_8125, 0);
        self.mac_ocp_modify(0xd40a, 0x0010, 0x0000); // UPS off
        self.regs.wr8(CONFIG1, self.regs.rd8(CONFIG1) & !0x10);
        self.mac_ocp_write(0xc140, 0xffff);
        self.mac_ocp_write(0xc142, 0xffff);
        self.mac_ocp_modify(0xd3e2, 0x0fff, 0x03a9);
        self.mac_ocp_modify(0xd3e4, 0x00ff, 0x0000);
        self.mac_ocp_modify(0xe860, 0x0000, 0x0080);
        self.mac_ocp_modify(0xeb58, 0x0001, 0x0000); // the old TX descriptor format
        if self.ver == 63 {
            self.mac_ocp_modify(0xe614, 0x0700, 0x0200);
            self.mac_ocp_modify(0xe63e, 0x0c30, 0x0000);
        } else {
            self.mac_ocp_modify(0xe614, 0x0700, 0x0300);
            self.mac_ocp_modify(0xe63e, 0x0c30, 0x0020);
        }
        self.mac_ocp_modify(0xc0b4, 0x0000, 0x000c);
        self.mac_ocp_modify(0xeb6a, 0x00ff, 0x0033);
        self.mac_ocp_modify(0xeb50, 0x03e0, 0x0040);
        self.mac_ocp_modify(0xe056, 0x00f0, 0x0000);
        self.mac_ocp_modify(0xe040, 0x1000, 0x0000);
        self.mac_ocp_modify(0xea1c, 0x0003, 0x0001);
        self.mac_ocp_modify(0xea1c, 0x0004, 0x0000);
        self.mac_ocp_modify(0xe0c0, 0x4f0f, 0x4403);
        self.mac_ocp_modify(0xe052, 0x0080, 0x0068);
        self.mac_ocp_modify(0xd430, 0x0fff, 0x047f);
        self.mac_ocp_modify(0xea1c, 0x0004, 0x0000);
        self.mac_ocp_modify(0xeb54, 0x0000, 0x0001);
        self.udelay(1);
        self.mac_ocp_modify(0xeb54, 0x0001, 0x0000);
        self.regs.wr16(0x1880, self.regs.rd16(0x1880) & !0x0030);
        self.mac_ocp_write(0xe098, 0xc302);
        self.wait(false, 10_000, |r| r.mac_ocp_read(0xe00e) & (1 << 13) != 0);
        self.disable_rxdvgate();
    }

    /// rtl_hw_start_8125: per-revision tuning, then the common part.
    fn hw_start_8125(&self) {
        self.regs.wr8(INT_CFG0_8125, 0);
        // Interrupt coalescing off.
        let end = if self.ver == 63 { 0xa80 } else { 0xb00 };
        for reg in (0xa00..end).step_by(4) {
            self.regs.wr32(reg, 0);
        }
        if self.ver == 63 {
            self.regs.wr16(INT_CFG1_8125, 0);
        }
        self.mac_ocp_modify(0xea84, 0, 0x3); // extended tally counter
        self.set_def_aspm_entry_latency();
        match self.ver {
            61 => self.ephy_init(&[
                (0x04, 0xffff, 0xd000),
                (0x0a, 0xffff, 0x8653),
                (0x23, 0xffff, 0xab66),
                (0x20, 0xffff, 0x9455),
                (0x21, 0xffff, 0x99ff),
                (0x29, 0xffff, 0xfe04),
                (0x44, 0xffff, 0xd000),
                (0x4a, 0xffff, 0x8653),
                (0x63, 0xffff, 0xab66),
                (0x60, 0xffff, 0x9455),
                (0x61, 0xffff, 0x99ff),
                (0x69, 0xffff, 0xfe04),
            ]),
            63 => self.ephy_init(&[
                (0x0b, 0xffff, 0xa908),
                (0x1e, 0xffff, 0x20eb),
                (0x4b, 0xffff, 0xa908),
                (0x5e, 0xffff, 0x20eb),
                (0x22, 0x0030, 0x0020),
                (0x62, 0x0030, 0x0020),
            ]),
            _ => {}
        }
        self.hw_start_8125_common();
    }

    /// rtl_hw_start_8168: the per-revision tuning there is (only the
    /// 8111H's is ported; others get the common steps), then the parts
    /// every 8168 shares.
    fn hw_start_8168(&self) {
        self.regs.wr8(MAX_TX_PACKET_SIZE, if self.is_8168evl_up() { 0x27 } else { 0x3f });
        match self.ver {
            46 | 48 => self.hw_start_8168h_1(),
            40.. => self.disable_rxdvgate(),
            _ => {}
        }
        self.regs.wr16(INTR_MITIGATE, 0);
    }

    /// rtl_hw_start: everything up to and including enabling RX and TX.
    fn hw_start(&self) {
        self.unlock();
        self.disable_aspm_clkreq();
        // Without RX checksum offload or VLAN stripping.
        self.regs.wr16(CPLUS_CMD, self.cp_cmd);
        // EEE: idle after an MTU-sized frame's time.
        let lpi = (1500 + 14 + 0x20) as u16;
        match self.ver {
            46 | 48 => self.mac_ocp_write(0xe048, lpi),
            61.. => self.regs.wr16(EEE_TXIDLE_TIMER_8125, lpi),
            _ => {}
        }
        if self.is_8125() {
            self.hw_start_8125();
        } else {
            self.hw_start_8168();
        }
        // Which events end the link's L1 power state.
        match self.ver {
            34..=36 => self.eri_modify(0xd4, 0x1f00, 0),
            37..=38 => self.eri_modify(0xd4, 0x0c00, 0),
            40.. => self.mac_ocp_modify(0xc0ac, 0, 0x1f80),
            _ => {}
        }
        self.regs.wr16(RX_MAX_SIZE, BUF_SIZE as u16);
        // The high halves first: some boards need that order.
        self.regs.wr32(TX_DESC_HIGH, (self.tx_ring_phys >> 32) as u32);
        self.regs.wr32(TX_DESC_LOW, self.tx_ring_phys as u32);
        self.regs.wr32(RX_DESC_HIGH, (self.rx_ring_phys >> 32) as u32);
        self.regs.wr32(RX_DESC_LOW, self.rx_ring_phys as u32);
        if self.is_8125() {
            self.regs.wr32(RSS_CTRL_8125, 0);
        }
        self.lock();
        self.regs.rd8(CHIP_CMD);

        self.regs.wr8(CHIP_CMD, CMD_TX_ENABLE | CMD_RX_ENABLE);
        let rxcfg = self.init_rxcfg();
        let auto_fifo = if self.is_8168evl_up() { TXCFG_AUTO_FIFO } else { 0 };
        self.regs.wr32(TX_CONFIG, TX_DMA_BURST | TX_IFG | auto_fifo);
        // Every multicast (IPv6 runs on it), broadcast, and our address.
        self.regs.wr32(MAR0 + 4, !0);
        self.regs.wr32(MAR0, !0);
        self.regs.wr32(RX_CONFIG, (rxcfg & !RX_ACCEPT_MASK) | ACCEPT_BROADCAST | ACCEPT_MULTICAST | ACCEPT_MY_PHYS);
        self.mask_and_ack_irqs();
    }

    fn fill_rings(&mut self) {
        for i in 0..RX_COUNT {
            let d = unsafe { self.rx_ring.add(i) };
            vwrite!(d, addr, self.rx_bufs.phys + (i * BUF_SIZE) as u64);
            vwrite!(d, opts2, 0);
            let end = if i == RX_COUNT - 1 { RING_END } else { 0 };
            vwrite!(d, opts1, DESC_OWN | end | BUF_SIZE as u32);
        }
        for i in 0..TX_COUNT {
            let d = unsafe { self.tx_ring.add(i) };
            vwrite!(d, addr, self.tx_bufs.phys + (i * BUF_SIZE) as u64);
            vwrite!(d, opts2, 0);
            vwrite!(d, opts1, if i == TX_COUNT - 1 { RING_END } else { 0 });
        }
        self.rx_next = 0;
        self.tx_next = 0;
    }

    fn speed(&self) -> &'static str {
        let s = self.regs.rd8(PHY_STATUS);
        if s & LINK_STATUS == 0 {
            return "down";
        }
        if self.is_8125() && self.regs.rd16(PHY_STATUS) & PHY_2500_8125 != 0 {
            return "up, 2.5 Gb/s";
        }
        match (s & (PHY_1000 | PHY_100 | PHY_10), s & PHY_FULL_DUPLEX != 0) {
            (PHY_1000, _) => "up, 1 Gb/s",
            (PHY_100, true) => "up, 100 Mb/s full duplex",
            (PHY_100, false) => "up, 100 Mb/s half duplex",
            (PHY_10, _) => "up, 10 Mb/s",
            _ => "up",
        }
    }
}

impl NicDriver for Rtl {
    fn link_up(&mut self) -> bool {
        self.regs.rd8(PHY_STATUS) & LINK_STATUS != 0
    }

    fn transmit(&mut self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME {
            return false;
        }
        let i = self.tx_next;
        let d = unsafe { self.tx_ring.add(i) };
        // The card still has this slot: the whole ring is queued.
        if vread!(d, opts1) & DESC_OWN != 0 {
            return false;
        }
        // Short frames padded with zeros, not whatever the buffer held.
        let len = frame.len().max(MIN_FRAME);
        let buf = unsafe { self.tx_bufs.virt.add(i * BUF_SIZE) };
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buf, frame.len());
            core::ptr::write_bytes(buf.add(frame.len()), 0, len - frame.len());
        }
        let end = if i == TX_COUNT - 1 { RING_END } else { 0 };
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        vwrite!(d, opts1, DESC_OWN | end | FIRST_FRAG | LAST_FRAG | len as u32);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.tx_next = (i + 1) % TX_COUNT;
        if self.is_8125() {
            self.regs.wr16(TX_POLL_8125, 1);
        } else {
            self.regs.wr8(TX_POLL, NPQ);
        }
        true
    }

    fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        loop {
            let i = self.rx_next;
            let d = unsafe { self.rx_ring.add(i) };
            let status = vread!(d, opts1);
            if status & DESC_OWN != 0 {
                return None;
            }
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            let len = ((status & LEN_MASK) as usize).saturating_sub(FCS_LEN);
            // One buffer per frame at this MTU: a frame spread over
            // several, or one with an error, is dropped.
            let whole = status & (FIRST_FRAG | LAST_FRAG) == FIRST_FRAG | LAST_FRAG;
            let good = whole && status & RX_RES == 0 && len > 0 && len <= out.len() && len <= BUF_SIZE;
            if good {
                let src = unsafe { self.rx_bufs.virt.add(i * BUF_SIZE) };
                unsafe { core::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), len) };
            } else {
                self.rx_dropped += 1;
            }
            // Hand the slot back.
            let end = if i == RX_COUNT - 1 { RING_END } else { 0 };
            vwrite!(d, opts2, 0);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            vwrite!(d, opts1, DESC_OWN | end | BUF_SIZE as u32);
            self.rx_next = (i + 1) % RX_COUNT;
            if good {
                return Some(len);
            }
        }
    }

    fn debug(&mut self) {
        let r = &self.regs;
        kprintln!("  {} (VER_{}), link {}", self.name, self.ver, self.speed());
        kprintln!(
            "  ChipCmd {:02x}  RxConfig {:08x}  TxConfig {:08x}  CPlusCmd {:04x}  MISC {:08x}  MCU {:02x}",
            r.rd8(CHIP_CMD),
            r.rd32(RX_CONFIG),
            r.rd32(TX_CONFIG),
            r.rd16(CPLUS_CMD),
            r.rd32(MISC),
            r.rd8(MCU)
        );
        let isr = if self.is_8125() { r.rd32(INTR_STATUS_8125) } else { r.rd16(INTR_STATUS) as u32 };
        kprintln!(
            "  PHYstatus {:04x}  IntrStatus {isr:08x}  Config1-5 {:02x} {:02x} {:02x} {:02x} {:02x}",
            r.rd16(PHY_STATUS),
            r.rd8(CONFIG1),
            r.rd8(CONFIG2),
            r.rd8(CONFIG3),
            r.rd8(0x55),
            r.rd8(CONFIG5)
        );
        kprintln!(
            "  PHY BMCR {:04x} BMSR {:04x}",
            self.mii_read(0).unwrap_or(0xffff),
            self.mii_read(1).unwrap_or(0xffff)
        );
        let owned = |ring: *mut Desc, n: usize| {
            (0..n).filter(|&k| vread!(unsafe { ring.add(k) }, opts1) & DESC_OWN != 0).count()
        };
        kprintln!(
            "  rx next {} ({} of {} with the card), {} dropped; tx next {} ({} of {} with the card)",
            self.rx_next,
            owned(self.rx_ring, RX_COUNT),
            RX_COUNT,
            self.rx_dropped,
            self.tx_next,
            owned(self.tx_ring, TX_COUNT),
            TX_COUNT
        );
    }
}

/// The first memory BAR (Realtek puts I/O space in BAR0 and the
/// registers in BAR2, 64-bit).
fn mmio_bar(dev: &pci::PciDevice) -> Option<u64> {
    let mut i = 0;
    while i < 6 {
        let lo = dev.bar[i];
        if lo & 1 != 0 {
            i += 1; // I/O space
            continue;
        }
        let is_64 = lo & 0x6 == 0x4;
        let mut base = (lo & !0xf) as u64;
        if is_64 && i < 5 {
            base |= (dev.bar[i + 1] as u64) << 32;
        }
        if base != 0 {
            return Some(base);
        }
        i += if is_64 { 2 } else { 1 };
    }
    None
}

/// The PCIe link's own power saving (ASPM, clock requests) off, as Linux
/// does for these chips: some boards hang the device with it on.
fn disable_pcie_aspm(dev: &pci::PciDevice) {
    let mut cap = dev.read8(0x34) & 0xfc;
    let mut guard = 0;
    while cap != 0 && guard < 48 {
        if dev.read8(cap) == 0x10 {
            let lnkctl = cap + 0x10;
            dev.write16(lnkctl, dev.read16(lnkctl) & !(0x0003 | 0x0100));
            return;
        }
        cap = dev.read8(cap + 1) & 0xfc;
        guard += 1;
    }
}

fn bring_up(dev: &pci::PciDevice) -> Option<(Rtl, [u8; 6])> {
    // Memory space + bus master, legacy INTx off (polled).
    dev.write16(0x04, dev.read16(0x04) | 0x0406);
    disable_pcie_aspm(dev);
    let Some(base) = mmio_bar(dev) else {
        kprintln!("rtl8168: no memory BAR");
        return None;
    };
    let regs = Regs(vmm::map_mmio(base, MMIO_SIZE));

    let txconfig = regs.rd32(TX_CONFIG);
    if txconfig == !0 {
        kprintln!("rtl8168: the card doesn't answer (TxConfig reads all ones)");
        return None;
    }
    let mut xid = ((txconfig >> 20) & 0xfcf) as u16;
    let mut chip = CHIPS.iter().find(|&&(mask, val, _, _)| xid & mask == val);
    if chip.is_some_and(|c| c.2 == 0) {
        xid = regs.rd32(TX_CONFIG_V2) as u16;
        chip = None;
    }
    let Some(&(_, _, ver, name)) = chip else {
        kprintln!("rtl8168: unknown revision, XID {xid:03x} -- not driven");
        return None;
    };
    if ver < 17 {
        kprintln!("rtl8168: {name} (XID {xid:03x}) is too old for this driver");
        return None;
    }

    let rx_ring = dma::alloc(pages(RX_COUNT * size_of::<Desc>()))?;
    let tx_ring = dma::alloc(pages(TX_COUNT * size_of::<Desc>()))?;
    let rx_bufs = dma::alloc(pages(RX_COUNT * BUF_SIZE))?;
    let tx_bufs = dma::alloc(pages(TX_COUNT * BUF_SIZE))?;
    let mut nic = Rtl {
        regs,
        ver,
        name,
        func: dev.func,
        cp_cmd: regs.rd16(CPLUS_CMD) & (NORMAL_MODE | INTT_MASK),
        rx_ring: rx_ring.as_ptr(),
        tx_ring: tx_ring.as_ptr(),
        rx_bufs,
        tx_bufs,
        rx_ring_phys: rx_ring.phys,
        tx_ring_phys: tx_ring.phys,
        rx_next: 0,
        tx_next: 0,
        rx_dropped: 0,
    };

    // rtl_init_one, then rtl8169_up / rtl_reset_work.
    nic.init_rxcfg();
    nic.mask_and_ack_irqs();
    nic.hw_initialize();
    let mac = nic.read_mac();
    if mac == [0; 6] || mac == [0xff; 6] || mac[0] & 1 != 0 {
        kprintln!("rtl8168: {name}: no valid MAC address");
        return None;
    }
    nic.set_mac(mac);
    nic.init_phy();
    nic.reset();
    nic.fill_rings();
    nic.hw_start();
    let tuned = matches!(ver, 46 | 48 | 61 | 63..);
    kprintln!(
        "rtl8168: {name} (XID {xid:03x}, VER_{ver}){}",
        if tuned { "" } else { " -- this revision gets only the common setup, not its own tuning" }
    );
    Some((nic, mac))
}

/// Finds every supported Realtek card (pci::enumerate() must have run)
/// and registers it. How many came up.
pub fn init() -> usize {
    let mut found = 0;
    for &id in &DEVICE_IDS {
        for n in 0.. {
            let Some(dev) = pci::find_device_nth(VENDOR_REALTEK, id, n) else { break };
            let Some((nic, mac)) = bring_up(dev) else { continue };
            let (name, link) = (nic.name, nic.speed());
            if super::register(name, mac, Box::new(nic)).is_some() {
                kprintln!("rtl8168: ready, mac {}, link {link}", Mac(mac));
                found += 1;
            }
        }
    }
    found
}
