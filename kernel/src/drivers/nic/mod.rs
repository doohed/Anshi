//! Every network card the drivers find -- virtio-net under QEMU
//! (drivers/virtio/net.rs), Intel e1000 (e1000.rs) -- behind one interface: whole Ethernet frames
//! in and out, no checksum or segmentation offload. Polled, like every
//! other driver here: receive() is asked whether a frame has arrived, by
//! the IP stack's poll (net/mod.rs).

pub mod e1000;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::Global;

pub const MAX_DEVICES: usize = 4;
/// The largest Ethernet frame without its FCS: 14-byte header + 1500 MTU.
pub const MAX_FRAME: usize = 1514;

/// What a network driver provides.
pub trait NicDriver {
    /// Whether the cable is in (or the access point is associated).
    fn link_up(&mut self) -> bool;
    /// Sends one frame, at most MAX_FRAME bytes. False if it was dropped.
    fn transmit(&mut self, frame: &[u8]) -> bool;
    /// Non-blocking: copies the next received frame into `buf` (at least
    /// MAX_FRAME bytes) and returns its length, or None when there's none.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize>;
}

pub struct NetDevice {
    /// "eth0", "eth1".
    pub name: String,
    /// "virtio-net".
    pub model: &'static str,
    pub mac: [u8; 6],
    pub rx_packets: u64,
    pub rx_bytes: u64,
    pub tx_packets: u64,
    pub tx_bytes: u64,
    driver: Box<dyn NicDriver>,
}

static DEVICES: Global<Vec<&'static mut NetDevice>> = Global::new(Vec::new());

fn devices() -> &'static mut Vec<&'static mut NetDevice> {
    unsafe { DEVICES.get() }
}

/// Adds a card, named ethN in the order found. Its index, or None if the
/// table is full.
pub fn register(model: &'static str, mac: [u8; 6], driver: Box<dyn NicDriver>) -> Option<usize> {
    let d = devices();
    if d.len() == MAX_DEVICES {
        return None;
    }
    let name = alloc::format!("eth{}", d.len());
    let dev = NetDevice { name, model, mac, rx_packets: 0, rx_bytes: 0, tx_packets: 0, tx_bytes: 0, driver };
    d.push(Box::leak(Box::new(dev)));
    Some(d.len() - 1)
}

pub fn count() -> usize {
    devices().len()
}

pub fn get(index: usize) -> Option<&'static mut NetDevice> {
    devices().get_mut(index).map(|d| unsafe { &mut *(*d as *mut NetDevice) })
}

impl NetDevice {
    pub fn link_up(&mut self) -> bool {
        self.driver.link_up()
    }

    pub fn transmit(&mut self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME || !self.driver.transmit(frame) {
            return false;
        }
        self.tx_packets += 1;
        self.tx_bytes += frame.len() as u64;
        true
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let n = self.driver.receive(buf)?;
        self.rx_packets += 1;
        self.rx_bytes += n as u64;
        Some(n)
    }
}

/// "52:54:00:12:34:56".
pub struct Mac(pub [u8; 6]);

impl core::fmt::Display for Mac {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}
