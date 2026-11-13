//! The IP stack: smoltcp (ARP/NDP, IPv4, IPv6, ICMP, UDP, TCP, DHCP, DNS)
//! on top of every network card drivers/nic found. Each card is its own
//! interface, with its own addresses, routes and sockets, and comes up by
//! itself: DHCP for IPv4 at boot and again whenever its link comes back
//! (unless /etc/net.conf gives it a static address), and IPv6 always --
//! a link-local address from the MAC, and a global one from the router's
//! advertisements (SLAAC).
//!
//! Outgoing traffic picks its interface by route (route()): one whose
//! subnet holds the destination, else the first with a default route for
//! that IP version. DNS goes through the first interface that has a
//! server.
//!
//! Polled, like the drivers: poll() moves frames between the cards and
//! the stack and runs every protocol timer. It's called from kernel
//! context only -- each pass of the scheduler, the keyboard wait, the
//! socket syscalls -- and never from the tick handler (sync.rs), so the
//! 10 ms tick is what bounds how late a frame is noticed.

pub mod socket;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::dhcpv4;
use smoltcp::socket::dns::{self, GetQueryResultError};
use smoltcp::time::Instant;
use smoltcp::wire::{DnsQueryType, EthernetAddress, IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};

use crate::drivers::nic::{self, MAX_FRAME, Mac};
use crate::drivers::{serial, timer};
use crate::fs::vfs;
use crate::random;
use crate::sync::Global;

/// What a non-blocking call returns for "not yet, ask again": Linux's
/// EAGAIN, negated the way a raw syscall returns it.
pub const EAGAIN: i64 = -11;

pub const AF_INET: u16 = 2;
pub const AF_INET6: u16 = 10;

/// Where the settings are kept: a `[ethN]` line starts that interface's
/// section (lines before any section are eth0's), then `mode=dhcp`, or
/// `mode=static` with `address=a.b.c.d/len`, `gateway=` and `dns=`.
const CONF_PATH: &str = "/etc/net.conf";
/// How long boot waits for DHCP, so addresses are usually there by the
/// first prompt -- a real card can need a second just to settle its link.
/// Boot only waits until every linked interface has its lease; past
/// this, they still arrive, just later.
const BOOT_WAIT_MS: u64 = 1500;
/// A name lookup nobody came back for is dropped after this long.
const QUERY_MAX_AGE_MS: u64 = 15_000;
const MAX_QUERIES: usize = 4;
/// IPv6 addresses reported per interface.
pub const MAX_V6: usize = 3;

pub const MODE_DHCP: u8 = 0;
pub const MODE_STATIC: u8 = 1;
/// net_config() only: ask the DHCP server again.
pub const MODE_RENEW: u8 = 2;

pub const STATE_NO_LINK: u8 = 0;
pub const STATE_ACQUIRING: u8 = 1;
pub const STATE_CONNECTED: u8 = 2;

/// What net_info() reports about one interface -- the layout userland
/// reads (userland/rust/anshi/src/sys.rs). Addresses are in network
/// order, as written: 10.0.2.15 is [10, 0, 2, 15].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NetInfo {
    pub name: [u8; 16],
    pub model: [u8; 16],
    pub mac: [u8; 6],
    pub link: u8,
    pub mode: u8,
    pub state: u8,
    pub prefix: u8,
    /// How many of `v6` are filled in.
    pub v6_count: u8,
    /// Bit 0: IPv4 traffic to other networks leaves here; bit 1: IPv6
    /// traffic does; bit 2: name lookups go through here.
    pub primary: u8,
    pub address: [u8; 4],
    pub gateway: [u8; 4],
    pub dns: [u8; 4],
    pub v6: [[u8; 16]; MAX_V6],
    pub v6_prefix: [u8; MAX_V6],
    pub reserved: [u8; 5],
    pub v6_gateway: [u8; 16],
    /// Milliseconds since the IPv4 address was configured.
    pub connected_ms: u64,
    pub rx_packets: u64,
    pub rx_bytes: u64,
    pub tx_packets: u64,
    pub tx_bytes: u64,
}

pub const PRIMARY_V4: u8 = 1;
pub const PRIMARY_V6: u8 = 2;
pub const PRIMARY_DNS: u8 = 4;

/// What net_config() takes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NetConfig {
    pub mode: u8,
    pub prefix: u8,
    pub reserved: [u8; 2],
    pub address: [u8; 4],
    pub gateway: [u8; 4],
    pub dns: [u8; 4],
}

const _: () = assert!(size_of::<NetInfo>() == 168);
const _: () = assert!(size_of::<NetConfig>() == 16);

/// smoltcp's view of a card: one frame at a time through two buffers that
/// live as long as the stack.
struct Phy {
    dev: usize,
    rx: Box<[u8; MAX_FRAME]>,
    tx: Box<[u8; MAX_FRAME]>,
}

pub struct Rx<'a>(&'a [u8]);

pub struct Tx<'a> {
    dev: usize,
    buf: &'a mut [u8; MAX_FRAME],
}

impl RxToken for Rx<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let frame = &mut self.buf[..len.min(MAX_FRAME)];
        let r = f(frame);
        if let Some(dev) = nic::get(self.dev) {
            dev.transmit(frame);
        }
        r
    }
}

impl Device for Phy {
    type RxToken<'a> = Rx<'a>;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx<'_>, Tx<'_>)> {
        let n = nic::get(self.dev)?.receive(&mut self.rx[..])?;
        Some((Rx(&self.rx[..n]), Tx { dev: self.dev, buf: &mut self.tx }))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        Some(Tx { dev: self.dev, buf: &mut self.tx })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MAX_FRAME;
        caps
    }
}

/// One interface: a card, its smoltcp state, and the sockets that use it.
pub struct Iface {
    phy: Phy,
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    /// None in static mode: a DHCP socket left in the set keeps asking.
    dhcp: Option<SocketHandle>,
    dns: SocketHandle,
    mode: u8,
    link: bool,
    address: Option<Ipv4Cidr>,
    gateway: Option<Ipv4Address>,
    dns_server: Option<Ipv4Address>,
    configured_ms: u64,
    /// The global IPv6 addresses last logged, to log changes once.
    logged_v6: Vec<Ipv6Address>,
}

struct Query {
    name: String,
    family: u16,
    iface: usize,
    handle: dns::QueryHandle,
    started_ms: u64,
}

pub struct Stack {
    pub ifaces: Vec<Iface>,
    queries: Vec<Query>,
}

static STACK: Global<Option<Stack>> = Global::new(None);
/// After boot, status lines go to the serial log only: the screen may
/// belong to a full-screen program by then.
static QUIET: Global<bool> = Global::new(false);

pub fn stack() -> Option<&'static mut Stack> {
    unsafe { STACK.get() }.as_mut()
}

pub fn now() -> Instant {
    Instant::from_millis(timer::uptime_ms() as i64)
}

fn log(args: core::fmt::Arguments) {
    if unsafe { *QUIET.get() } {
        let _ = serial::Serial.write_fmt(format_args!("{args}\n"));
    } else {
        kprintln!("{args}");
    }
}

fn name(index: usize) -> String {
    nic::get(index).map_or_else(|| alloc::format!("if{index}"), |d| d.name.clone())
}

/// fe80::/64 with the MAC as the interface id (modified EUI-64).
fn link_local(mac: [u8; 6]) -> Ipv6Cidr {
    let id = [mac[0] ^ 2, mac[1], mac[2], 0xff, 0xfe, mac[3], mac[4], mac[5]];
    let mut b = [0u8; 16];
    b[0] = 0xfe;
    b[1] = 0x80;
    b[8..].copy_from_slice(&id);
    Ipv6Cidr::new(Ipv6Address::from(b), 64)
}

pub fn is_link_local(a: &Ipv6Address) -> bool {
    a.octets()[0] == 0xfe && a.octets()[1] & 0xc0 == 0x80
}

impl Iface {
    fn index(&self) -> usize {
        self.phy.dev
    }

    fn apply(&mut self, address: Ipv4Cidr, gateway: Option<Ipv4Address>, dns_server: Option<Ipv4Address>) {
        // Only the IPv4 entry is replaced: SLAAC owns the IPv6 ones.
        self.iface.update_ip_addrs(|addrs| {
            addrs.retain(|a| !matches!(a, IpCidr::Ipv4(_)));
            let _ = addrs.push(IpCidr::Ipv4(address));
        });
        self.iface.routes_mut().remove_default_ipv4_route();
        if let Some(gw) = gateway {
            let _ = self.iface.routes_mut().add_default_ipv4_route(gw);
        }
        let servers: Vec<IpAddress> = dns_server.iter().map(|&s| IpAddress::Ipv4(s)).collect();
        self.sockets.get_mut::<dns::Socket>(self.dns).update_servers(&servers);
        self.address = Some(address);
        self.gateway = gateway;
        self.dns_server = dns_server;
        self.configured_ms = timer::uptime_ms();

        let mut line = String::new();
        let _ = write!(line, "net: {} {address}", name(self.index()));
        if let Some(gw) = gateway {
            let _ = write!(line, " via {gw}");
        }
        if let Some(s) = dns_server {
            let _ = write!(line, ", dns {s}");
        }
        let how = if self.mode == MODE_STATIC { "static" } else { "dhcp" };
        log(format_args!("{line} ({how})"));
    }

    fn clear(&mut self) {
        if self.address.take().is_some() {
            log(format_args!("net: {} lost its IPv4 address", name(self.index())));
        }
        self.iface.update_ip_addrs(|addrs| addrs.retain(|a| !matches!(a, IpCidr::Ipv4(_))));
        self.iface.routes_mut().remove_default_ipv4_route();
        self.sockets.get_mut::<dns::Socket>(self.dns).update_servers(&[]);
        self.gateway = None;
        self.dns_server = None;
    }

    fn set_dhcp(&mut self) {
        self.mode = MODE_DHCP;
        self.clear();
        match self.dhcp {
            Some(h) => self.sockets.get_mut::<dhcpv4::Socket>(h).reset(),
            None => self.dhcp = Some(self.sockets.add(dhcpv4::Socket::new())),
        }
    }

    fn set_static(&mut self, address: Ipv4Cidr, gateway: Option<Ipv4Address>, dns_server: Option<Ipv4Address>) {
        self.mode = MODE_STATIC;
        if let Some(h) = self.dhcp.take() {
            self.sockets.remove(h);
        }
        self.apply(address, gateway, dns_server);
    }

    fn poll(&mut self) {
        self.iface.poll(now(), &mut self.phy, &mut self.sockets);

        let link = nic::get(self.phy.dev).is_some_and(|d| d.link_up());
        if link != self.link {
            self.link = link;
            log(format_args!("net: {} link {}", name(self.index()), if link { "up" } else { "down" }));
            // A cable coming back may be a different network: ask again.
            if self.mode == MODE_DHCP {
                self.set_dhcp();
            }
        }

        if let Some(h) = self.dhcp {
            let event = match self.sockets.get_mut::<dhcpv4::Socket>(h).poll() {
                Some(dhcpv4::Event::Configured(c)) => Some(Some((c.address, c.router, c.dns_servers.first().copied()))),
                Some(dhcpv4::Event::Deconfigured) => Some(None),
                None => None,
            };
            match event {
                Some(Some((address, router, dns_server))) => self.apply(address, router, dns_server),
                Some(None) => self.clear(),
                None => {}
            }
        }

        let global = self.global_v6();
        if global != self.logged_v6 {
            for a in global.iter().filter(|a| !self.logged_v6.contains(a)) {
                log(format_args!("net: {} {a} (slaac)", name(self.index())));
            }
            self.logged_v6 = global;
        }
    }

    pub fn v6_addrs(&self) -> impl Iterator<Item = Ipv6Cidr> + '_ {
        self.iface.ip_addrs().iter().filter_map(|a| match a {
            IpCidr::Ipv6(c) => Some(*c),
            _ => None,
        })
    }

    fn global_v6(&self) -> Vec<Ipv6Address> {
        self.v6_addrs().map(|c| c.address()).filter(|a| !is_link_local(a)).collect()
    }

    pub fn v6_gateway(&self) -> Option<Ipv6Address> {
        self.iface.routes().get_default_ipv6_route().and_then(|r| match r.via_router {
            IpAddress::Ipv6(a) => Some(a),
            _ => None,
        })
    }

    fn state(&self) -> u8 {
        if !self.link {
            STATE_NO_LINK
        } else if self.address.is_some() || !self.global_v6().is_empty() {
            STATE_CONNECTED
        } else {
            STATE_ACQUIRING
        }
    }

    /// Whether `dst` is on one of this interface's own networks.
    fn on_link(&self, dst: &IpAddress) -> bool {
        self.iface.ip_addrs().iter().any(|c| c.contains_addr(dst))
    }
}

impl Stack {
    /// The interface traffic to `dst` leaves through: a linked one whose
    /// network holds it, else the first linked one with a default route
    /// for its IP version. `scope` (an interface index + 1, from
    /// sockaddr_in6's scope id) picks the interface for a link-local
    /// IPv6 destination, which every interface's network "holds".
    pub fn route(&self, dst: &IpAddress, scope: u32) -> Option<usize> {
        let linked = || self.ifaces.iter().filter(|i| i.link);
        if let IpAddress::Ipv6(a) = dst
            && is_link_local(a)
        {
            if scope > 0 {
                return self.ifaces.get(scope as usize - 1).filter(|i| i.link).map(|i| i.index());
            }
            return linked().next().map(|i| i.index());
        }
        if let Some(i) = linked().find(|i| i.on_link(dst)) {
            return Some(i.index());
        }
        linked()
            .find(|i| match dst {
                IpAddress::Ipv4(_) => i.gateway.is_some(),
                IpAddress::Ipv6(_) => i.v6_gateway().is_some(),
            })
            .map(|i| i.index())
    }

    /// Where name lookups go: the first interface that has a DNS server.
    fn dns_iface(&self) -> Option<usize> {
        self.ifaces.iter().find(|i| i.link && i.dns_server.is_some()).map(|i| i.index())
    }

    pub fn poll(&mut self) {
        for i in &mut self.ifaces {
            i.poll();
        }
        socket::reap(self);

        // Lookups whose asker went away (a program killed mid-resolve).
        let cutoff = timer::uptime_ms().saturating_sub(QUERY_MAX_AGE_MS);
        let ifaces = &mut self.ifaces;
        self.queries.retain(|q| {
            let keep = q.started_ms >= cutoff;
            if !keep && let Some(i) = ifaces.get_mut(q.iface) {
                let dns = i.sockets.get_mut::<dns::Socket>(i.dns);
                // Reading the result frees a finished query's slot;
                // cancelling frees a pending one.
                if let Err(GetQueryResultError::Pending) = dns.get_query_result(q.handle) {
                    dns.cancel_query(q.handle);
                }
            }
            keep
        });
    }
}

fn parse_ip(s: &str) -> Option<Ipv4Address> {
    s.trim().parse().ok()
}

type Static = (Ipv4Cidr, Option<Ipv4Address>, Option<Ipv4Address>);

/// The static settings /etc/net.conf gives each interface, by index.
fn read_conf() -> Vec<(usize, Static)> {
    let mut out = Vec::new();
    let Some(node) = vfs::resolve(vfs::root(), CONF_PATH) else { return out };
    let Ok(text) = core::str::from_utf8(unsafe { &(*node).data }) else { return out };

    let mut sections: Vec<(usize, Vec<(&str, &str)>)> = vec![(0, Vec::new())];
    for line in text.lines().map(str::trim) {
        if let Some(n) = line.strip_prefix("[eth").and_then(|r| r.strip_suffix(']')).and_then(|n| n.parse().ok()) {
            sections.push((n, Vec::new()));
        } else if let Some((k, v)) = line.split_once('=') {
            sections.last_mut().unwrap().1.push((k.trim(), v.trim()));
        }
    }
    for (index, pairs) in sections {
        let get = |key: &str| pairs.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| *v);
        if get("mode") != Some("static") {
            continue;
        }
        let address = get("address").and_then(|a| {
            let (ip, len) = a.split_once('/')?;
            let len: u8 = len.trim().parse().ok().filter(|&l| l <= 32)?;
            Some(Ipv4Cidr::new(parse_ip(ip)?, len))
        });
        let Some(address) = address else {
            kprintln!("net: {CONF_PATH}: eth{index} is static but has no valid address -- using DHCP");
            continue;
        };
        out.push((index, (address, get("gateway").and_then(parse_ip), get("dns").and_then(parse_ip))));
    }
    out
}

/// Brings up every card and starts connecting. The filesystem must be
/// loaded (the settings are a file). False when there's no card.
pub fn init() -> bool {
    if nic::count() == 0 {
        return false;
    }
    let conf = read_conf();
    let mut ifaces = Vec::new();
    for index in 0..nic::count() {
        let dev = nic::get(index).unwrap();
        let mac = dev.mac;
        let mut phy = Phy { dev: index, rx: Box::new([0; MAX_FRAME]), tx: Box::new([0; MAX_FRAME]) };
        let mut config = Config::new(EthernetAddress(mac).into());
        // Seeds TCP sequence numbers and DHCP/DNS transaction ids.
        config.random_seed = random::u64();
        config.slaac = true;
        let mut iface = Interface::new(config, &mut phy, now());
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::Ipv6(link_local(mac)));
        });
        let mut sockets = SocketSet::new(vec![]);
        let dns = sockets.add(dns::Socket::new(&[], vec![]));
        let mut i = Iface {
            phy,
            iface,
            sockets,
            dhcp: None,
            dns,
            mode: MODE_DHCP,
            link: dev.link_up(),
            address: None,
            gateway: None,
            dns_server: None,
            configured_ms: 0,
            logged_v6: Vec::new(),
        };
        match conf.iter().find(|(n, _)| *n == index) {
            Some(&(_, (address, gateway, dns_server))) => i.set_static(address, gateway, dns_server),
            None => i.set_dhcp(),
        }
        if !i.link {
            kprintln!("net: {} has no link", dev.name);
        }
        ifaces.push(i);
    }
    unsafe { *STACK.get() = Some(Stack { ifaces, queries: Vec::new() }) };

    let s = stack().unwrap();
    let waiting = |s: &Stack| s.ifaces.iter().any(|i| i.mode == MODE_DHCP && i.link && i.address.is_none());
    let deadline = timer::uptime_ms() + BOOT_WAIT_MS;
    while waiting(s) && timer::uptime_ms() < deadline {
        s.poll();
        core::hint::spin_loop();
    }
    for i in s.ifaces.iter().filter(|i| i.mode == MODE_DHCP && i.link && i.address.is_none()) {
        kprintln!("net: {} waiting for a DHCP answer", name(i.index()));
    }
    unsafe { *QUIET.get() = true };
    true
}

/// Moves frames and runs timers on every interface. Cheap when nothing
/// is happening; safe with no stack.
pub fn poll() {
    if let Some(s) = stack() {
        s.poll();
    }
}

/// Interface `index`, or None past the last card.
pub fn info(index: usize) -> Option<NetInfo> {
    let dev = nic::get(index)?;
    let mut info = NetInfo {
        name: [0; 16],
        model: [0; 16],
        mac: dev.mac,
        link: dev.link_up() as u8,
        mode: MODE_DHCP,
        state: STATE_NO_LINK,
        prefix: 0,
        v6_count: 0,
        primary: 0,
        address: [0; 4],
        gateway: [0; 4],
        dns: [0; 4],
        v6: [[0; 16]; MAX_V6],
        v6_prefix: [0; MAX_V6],
        reserved: [0; 5],
        v6_gateway: [0; 16],
        connected_ms: 0,
        rx_packets: dev.rx_packets,
        rx_bytes: dev.rx_bytes,
        tx_packets: dev.tx_packets,
        tx_bytes: dev.tx_bytes,
    };
    let n = dev.name.len().min(15);
    info.name[..n].copy_from_slice(&dev.name.as_bytes()[..n]);
    let n = dev.model.len().min(15);
    info.model[..n].copy_from_slice(&dev.model.as_bytes()[..n]);

    let s = stack()?;
    let Some(i) = s.ifaces.get(index) else { return Some(info) };
    info.mode = i.mode;
    info.state = i.state();
    if let Some(a) = i.address {
        info.address = a.address().octets();
        info.prefix = a.prefix_len();
        info.connected_ms = timer::uptime_ms() - i.configured_ms;
    }
    info.gateway = i.gateway.map_or([0; 4], |g| g.octets());
    info.dns = i.dns_server.map_or([0; 4], |d| d.octets());
    // Global addresses first, then the link-local one.
    let mut v6: Vec<Ipv6Cidr> = i.v6_addrs().collect();
    v6.sort_by_key(|c| is_link_local(&c.address()));
    for (k, c) in v6.iter().take(MAX_V6).enumerate() {
        info.v6[k] = c.address().octets();
        info.v6_prefix[k] = c.prefix_len();
        info.v6_count += 1;
    }
    info.v6_gateway = i.v6_gateway().map_or([0; 16], |g| g.octets());
    if s.route(&IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8)), 0) == Some(index) {
        info.primary |= PRIMARY_V4;
    }
    if s.route(&IpAddress::Ipv6(Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)), 0) == Some(index) {
        info.primary |= PRIMARY_V6;
    }
    if s.dns_iface() == Some(index) {
        info.primary |= PRIMARY_DNS;
    }
    Some(info)
}

/// Switches an interface between DHCP and a static IPv4 address, or asks
/// the DHCP server again. Takes effect at once; saving it is the
/// caller's job (the `net` program writes /etc/net.conf).
pub fn configure(index: usize, c: &NetConfig) -> bool {
    let Some(s) = stack() else { return false };
    let Some(i) = s.ifaces.get_mut(index) else { return false };
    let ip = |b: [u8; 4]| (b != [0; 4]).then(|| Ipv4Address::from(b));
    match c.mode {
        MODE_DHCP => i.set_dhcp(),
        MODE_RENEW if i.mode == MODE_DHCP => i.set_dhcp(),
        MODE_STATIC => {
            let Some(address) = ip(c.address).filter(|_| c.prefix <= 32) else { return false };
            i.set_static(Ipv4Cidr::new(address, c.prefix), ip(c.gateway), ip(c.dns));
        }
        _ => return false,
    }
    s.poll();
    true
}

/// A resolved address: (AF_INET or AF_INET6, the address in the first 4
/// or all 16 bytes).
pub type Resolved = (u16, [u8; 16]);

fn resolved(a: &IpAddress) -> Resolved {
    let mut b = [0u8; 16];
    match a {
        IpAddress::Ipv4(v4) => {
            b[..4].copy_from_slice(&v4.octets());
            (AF_INET, b)
        }
        IpAddress::Ipv6(v6) => (AF_INET6, v6.octets()),
    }
}

/// Non-blocking name lookup for an IPv4 (`family` AF_INET) or IPv6
/// (AF_INET6) address: the address, EAGAIN while the answer is on its
/// way (call again with the same name and family), or None when it can't
/// be resolved. A literal address of that family resolves to itself.
pub fn resolve(name: &str, family: u16) -> Option<Result<Resolved, i64>> {
    let literal: Option<IpAddress> = name.trim().parse().ok();
    if let Some(a) = literal {
        let fam = resolved(&a).0;
        return (fam == family).then(|| Ok(resolved(&a)));
    }
    let kind = match family {
        AF_INET => DnsQueryType::A,
        AF_INET6 => DnsQueryType::Aaaa,
        _ => return None,
    };
    let s = stack()?;
    s.poll();
    if let Some(k) = s.queries.iter().position(|q| q.name == name && q.family == family) {
        let q = &s.queries[k];
        let i = &mut s.ifaces[q.iface];
        let dns = i.sockets.get_mut::<dns::Socket>(i.dns);
        return match dns.get_query_result(q.handle) {
            Err(GetQueryResultError::Pending) => Some(Err(EAGAIN)),
            Ok(addrs) => {
                s.queries.remove(k);
                addrs.iter().map(resolved).find(|r| r.0 == family).map(Ok)
            }
            Err(GetQueryResultError::Failed) => {
                s.queries.remove(k);
                None
            }
        };
    }
    if s.queries.len() == MAX_QUERIES {
        return None;
    }
    let index = s.dns_iface()?;
    let i = &mut s.ifaces[index];
    let dns = i.sockets.get_mut::<dns::Socket>(i.dns);
    let handle = dns.start_query(i.iface.context(), name, kind).ok()?;
    s.queries.push(Query { name: String::from(name), family, iface: index, handle, started_ms: timer::uptime_ms() });
    s.poll(); // The question goes out now, not at the next tick.
    Some(Err(EAGAIN))
}

/// The kernel shell's `netinfo`.
pub fn print_info() {
    if nic::count() == 0 {
        kprintln!("no network device");
    }
    for info in (0..nic::count()).filter_map(info) {
        let text =
            |b: &[u8]| String::from_utf8_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]).into_owned();
        let mut roles = Vec::new();
        if info.primary & PRIMARY_V4 != 0 {
            roles.push("default IPv4");
        }
        if info.primary & PRIMARY_V6 != 0 {
            roles.push("default IPv6");
        }
        if info.primary & PRIMARY_DNS != 0 {
            roles.push("DNS");
        }
        let roles = if roles.is_empty() { String::new() } else { alloc::format!(" [{}]", roles.join(", ")) };
        kprintln!(
            "{}: {}, mac {}, link {}{roles}",
            text(&info.name),
            text(&info.model),
            Mac(info.mac),
            if info.link != 0 { "up" } else { "down" }
        );
        if info.address != [0; 4] {
            let how = if info.mode == MODE_STATIC { "static" } else { "dhcp" };
            kprintln!(
                "  inet  {}/{} via {}, dns {} ({how})",
                Ipv4Address::from(info.address),
                info.prefix,
                Ipv4Address::from(info.gateway),
                Ipv4Address::from(info.dns)
            );
        } else if info.state != STATE_NO_LINK && info.mode == MODE_DHCP {
            kprintln!("  inet  no address yet (waiting for DHCP)");
        }
        for k in 0..info.v6_count as usize {
            kprintln!("  inet6 {}/{}", Ipv6Address::from(info.v6[k]), info.v6_prefix[k]);
        }
        if info.v6_gateway != [0; 16] {
            kprintln!("  inet6 router {}", Ipv6Address::from(info.v6_gateway));
        }
        kprintln!(
            "  rx {} packets, {} bytes; tx {} packets, {} bytes",
            info.rx_packets,
            info.rx_bytes,
            info.tx_packets,
            info.tx_bytes
        );
    }
}
