//! Networking on top of the raw socket syscalls (sys.rs): TCP streams
//! out and in, UDP and ping sockets, over IPv4 and IPv6; name lookups;
//! and the interface settings. The kernel never blocks on a socket, so
//! the blocking calls here loop on EAGAIN, yielding the CPU in between,
//! until they finish or time out.

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_int;
use core::fmt;

use crate::{CString, now_ms, sched_yield, sys};

pub use sys::{
    NET_MODE_DHCP, NET_MODE_RENEW, NET_MODE_STATIC, NET_PRIMARY_DNS, NET_PRIMARY_V4, NET_PRIMARY_V6,
    NET_STATE_ACQUIRING, NET_STATE_CONNECTED, NET_STATE_NO_LINK, NetConfig, NetInfo,
};

/// An IPv4 address that prints as a.b.c.d.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    pub fn parse(s: &str) -> Option<Ipv4> {
        let mut out = [0u8; 4];
        let mut parts = s.trim().split('.');
        for b in &mut out {
            let p = parts.next()?;
            if p.is_empty() || p.len() > 3 || !p.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            *b = p.parse().ok()?;
        }
        parts.next().is_none().then_some(Ipv4(out))
    }

    pub fn is_zero(self) -> bool {
        self.0 == [0; 4]
    }
}

impl fmt::Display for Ipv4 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a}.{b}.{c}.{d}")
    }
}

/// An IPv6 address that prints in the short form (RFC 5952): lowercase,
/// no leading zeros, the longest run of zero groups as `::`.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Ipv6(pub [u8; 16]);

impl Ipv6 {
    fn groups(self) -> [u16; 8] {
        core::array::from_fn(|i| u16::from_be_bytes([self.0[2 * i], self.0[2 * i + 1]]))
    }

    pub fn parse(s: &str) -> Option<Ipv6> {
        let s = s.trim();
        let parse_side = |side: &str| -> Option<Vec<u16>> {
            let mut out = Vec::new();
            if side.is_empty() {
                return Some(out);
            }
            let parts: Vec<&str> = side.split(':').collect();
            for (i, p) in parts.iter().enumerate() {
                if i == parts.len() - 1 && p.contains('.') {
                    let v4 = Ipv4::parse(p)?.0;
                    out.push(u16::from_be_bytes([v4[0], v4[1]]));
                    out.push(u16::from_be_bytes([v4[2], v4[3]]));
                } else {
                    if p.is_empty() || p.len() > 4 {
                        return None;
                    }
                    out.push(u16::from_str_radix(p, 16).ok()?);
                }
            }
            Some(out)
        };
        let groups: Vec<u16> = match s.split_once("::") {
            Some((head, tail)) => {
                let (head, tail) = (parse_side(head)?, parse_side(tail)?);
                if head.len() + tail.len() > 7 {
                    return None;
                }
                let mut g = head;
                g.resize(8 - tail.len(), 0);
                g.extend(tail);
                g
            }
            None => parse_side(s)?,
        };
        if groups.len() != 8 {
            return None;
        }
        let mut out = [0u8; 16];
        for (i, g) in groups.iter().enumerate() {
            out[2 * i..2 * i + 2].copy_from_slice(&g.to_be_bytes());
        }
        Some(Ipv6(out))
    }

    pub fn is_zero(self) -> bool {
        self.0 == [0; 16]
    }

    pub fn is_link_local(self) -> bool {
        self.0[0] == 0xfe && self.0[1] & 0xc0 == 0x80
    }
}

impl fmt::Display for Ipv6 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let g = self.groups();
        // The longest run of two or more zero groups, first if tied.
        let (mut best, mut best_len, mut i) = (8, 0, 0);
        while i < 8 {
            let start = i;
            while i < 8 && g[i] == 0 {
                i += 1;
            }
            if i - start > best_len && i - start >= 2 {
                (best, best_len) = (start, i - start);
            }
            i += 1;
        }
        let mut i = 0;
        while i < 8 {
            if i == best {
                f.write_str("::")?;
                i += best_len;
                continue;
            }
            if i > 0 && i != best + best_len {
                f.write_str(":")?;
            }
            write!(f, "{:x}", g[i])?;
            i += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IpAddr {
    V4(Ipv4),
    V6(Ipv6),
}

impl IpAddr {
    /// A literal address: a.b.c.d, or IPv6 in any of its forms.
    pub fn parse(s: &str) -> Option<IpAddr> {
        Ipv4::parse(s).map(IpAddr::V4).or_else(|| Ipv6::parse(s).map(IpAddr::V6))
    }

    pub fn is_v6(self) -> bool {
        matches!(self, IpAddr::V6(_))
    }
}

impl fmt::Display for IpAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            IpAddr::V4(a) => a.fmt(f),
            IpAddr::V6(a) => a.fmt(f),
        }
    }
}

/// An address and port. `scope` picks the interface (index + 1) for a
/// link-local IPv6 address; 0 otherwise.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SocketAddr {
    pub ip: IpAddr,
    pub port: u16,
    pub scope: u32,
}

impl SocketAddr {
    pub fn new(ip: IpAddr, port: u16) -> SocketAddr {
        SocketAddr { ip, port, scope: 0 }
    }

    /// As a sockaddr_in or sockaddr_in6: the bytes and how many are used.
    fn encode(&self) -> ([u8; 28], usize) {
        let mut out = [0u8; 28];
        match self.ip {
            IpAddr::V4(a) => {
                let s =
                    sys::SockAddrIn { family: sys::AF_INET, port: self.port.to_be_bytes(), addr: a.0, zero: [0; 8] };
                unsafe { (out.as_mut_ptr() as *mut sys::SockAddrIn).write_unaligned(s) };
                (out, size_of::<sys::SockAddrIn>())
            }
            IpAddr::V6(a) => {
                let s = sys::SockAddrIn6 {
                    family: sys::AF_INET6,
                    port: self.port.to_be_bytes(),
                    flowinfo: 0,
                    addr: a.0,
                    scope_id: self.scope,
                };
                unsafe { (out.as_mut_ptr() as *mut sys::SockAddrIn6).write_unaligned(s) };
                (out, size_of::<sys::SockAddrIn6>())
            }
        }
    }

    fn decode(b: &[u8; 28]) -> Option<SocketAddr> {
        match u16::from_ne_bytes([b[0], b[1]]) {
            sys::AF_INET => {
                let s = unsafe { (b.as_ptr() as *const sys::SockAddrIn).read_unaligned() };
                Some(SocketAddr { ip: IpAddr::V4(Ipv4(s.addr)), port: u16::from_be_bytes(s.port), scope: 0 })
            }
            sys::AF_INET6 => {
                let s = unsafe { (b.as_ptr() as *const sys::SockAddrIn6).read_unaligned() };
                Some(SocketAddr { ip: IpAddr::V6(Ipv6(s.addr)), port: u16::from_be_bytes(s.port), scope: s.scope_id })
            }
            _ => None,
        }
    }
}

impl fmt::Display for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.ip {
            IpAddr::V4(a) => write!(f, "{a}:{}", self.port),
            IpAddr::V6(a) if self.scope > 0 && a.is_link_local() => {
                write!(f, "[{a}%eth{}]:{}", self.scope - 1, self.port)
            }
            IpAddr::V6(a) => write!(f, "[{a}]:{}", self.port),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    /// The call failed outright: no network or route, refused, reset,
    /// bad input.
    Failed,
    TimedOut,
    /// The name has no address (or no DNS server is known).
    NotFound,
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            NetError::Failed => "failed",
            NetError::TimedOut => "timed out",
            NetError::NotFound => "name not found",
        })
    }
}

/// Calls `f` until it stops returning EAGAIN or `timeout_ms` passes.
fn retry(timeout_ms: u64, mut f: impl FnMut() -> i64) -> Result<i64, NetError> {
    let deadline = now_ms() + timeout_ms;
    loop {
        match f() {
            sys::EAGAIN if now_ms() >= deadline => return Err(NetError::TimedOut),
            sys::EAGAIN => sched_yield(),
            n if n < 0 => return Err(NetError::Failed),
            n => return Ok(n),
        }
    }
}

/// Which IP version a lookup may answer with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// IPv4 if the name has an address of that kind, else IPv6.
    Any,
    V4,
    V6,
}

/// A literal address with an optional `%eth0` / `%1` scope.
fn literal(host: &str) -> Option<(IpAddr, u32)> {
    let (addr, scope) = match host.split_once('%') {
        Some((a, s)) => {
            let n: u32 = s.strip_prefix("eth").unwrap_or(s).parse().ok()?;
            (a, n + if s.starts_with("eth") { 1 } else { 0 })
        }
        None => (host, 0),
    };
    Some((IpAddr::parse(addr)?, scope))
}

fn lookup(host: &str, family: u16, timeout_ms: u64) -> Result<IpAddr, NetError> {
    let name = CString::new(host).map_err(|_| NetError::NotFound)?;
    let mut out = [0u8; 16];
    match retry(timeout_ms, || unsafe { sys::resolve(name.as_ptr(), &mut out, family) }) {
        Ok(f) if f as u16 == sys::AF_INET => Ok(IpAddr::V4(Ipv4([out[0], out[1], out[2], out[3]]))),
        Ok(_) => Ok(IpAddr::V6(Ipv6(out))),
        Err(NetError::Failed) => Err(NetError::NotFound),
        Err(e) => Err(e),
    }
}

/// Looks `host` up -- or reads it, when it's a literal address (with an
/// optional `%eth0` scope for a link-local one). The address and scope.
pub fn resolve(host: &str, family: Family, timeout_ms: u64) -> Result<(IpAddr, u32), NetError> {
    if let Some((ip, scope)) = literal(host) {
        let ok = match family {
            Family::Any => true,
            Family::V4 => !ip.is_v6(),
            Family::V6 => ip.is_v6(),
        };
        return if ok { Ok((ip, scope)) } else { Err(NetError::NotFound) };
    }
    let ip = match family {
        Family::V4 => lookup(host, sys::AF_INET, timeout_ms)?,
        Family::V6 => lookup(host, sys::AF_INET6, timeout_ms)?,
        Family::Any => match lookup(host, sys::AF_INET, timeout_ms) {
            Err(NetError::NotFound) => lookup(host, sys::AF_INET6, timeout_ms)?,
            other => other?,
        },
    };
    Ok((ip, 0))
}

/// An open socket fd; closed on drop.
pub struct Socket(c_int);

impl Socket {
    fn open(v6: bool, kind: c_int, protocol: c_int) -> Result<Socket, NetError> {
        let domain = if v6 { sys::AF_INET6 } else { sys::AF_INET };
        let fd = sys::socket(domain as c_int, kind, protocol);
        if fd < 0 { Err(NetError::Failed) } else { Ok(Socket(fd)) }
    }

    fn connect(&self, to: &SocketAddr, timeout_ms: u64) -> Result<(), NetError> {
        let (addr, len) = to.encode();
        retry(timeout_ms, || unsafe { sys::connect(self.0, addr.as_ptr(), len) }).map(|_| ())
    }

    fn bind(&self, at: &SocketAddr) -> Result<(), NetError> {
        let (addr, len) = at.encode();
        if unsafe { sys::bind(self.0, addr.as_ptr(), len) } < 0 { Err(NetError::Failed) } else { Ok(()) }
    }

    /// One non-blocking read: Ok(Some(n)) bytes (0 = the peer closed),
    /// Ok(None) when nothing has arrived yet.
    pub fn try_read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, NetError> {
        match unsafe { sys::read(self.0, buf.as_mut_ptr(), buf.len()) } {
            sys::EAGAIN => Ok(None),
            n if n < 0 => Err(NetError::Failed),
            n => Ok(Some(n as usize)),
        }
    }

    /// Waits up to `timeout_ms` for data: bytes read, 0 once the peer
    /// has closed.
    pub fn read(&mut self, buf: &mut [u8], timeout_ms: u64) -> Result<usize, NetError> {
        let fd = self.0;
        retry(timeout_ms, || unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) }).map(|n| n as usize)
    }

    /// Sends all of `data`, waiting for buffer room as needed.
    pub fn write_all(&mut self, mut data: &[u8], timeout_ms: u64) -> Result<(), NetError> {
        while !data.is_empty() {
            let n = retry(timeout_ms, || unsafe { sys::write(self.0, data.as_ptr(), data.len()) })?;
            data = &data[n as usize..];
        }
        Ok(())
    }

    /// One datagram to `to` (UDP, ping).
    pub fn send_to(&mut self, data: &[u8], to: &SocketAddr, timeout_ms: u64) -> Result<(), NetError> {
        let (addr, len) = to.encode();
        retry(timeout_ms, || unsafe { sys::sendto(self.0, data.as_ptr(), data.len(), addr.as_ptr(), len) }).map(|_| ())
    }

    /// One datagram, if one has arrived, and who sent it.
    pub fn try_recv_from(&mut self, buf: &mut [u8]) -> Result<Option<(usize, SocketAddr)>, NetError> {
        let mut addr = [0u8; 28];
        let mut len = addr.len() as u32;
        match unsafe { sys::recvfrom(self.0, buf.as_mut_ptr(), buf.len(), addr.as_mut_ptr(), &mut len) } {
            sys::EAGAIN => Ok(None),
            n if n < 0 => Err(NetError::Failed),
            n => Ok(Some((n as usize, SocketAddr::decode(&addr).ok_or(NetError::Failed)?))),
        }
    }

    /// A listening socket's next connection, if one has arrived: its
    /// socket and the peer.
    pub fn try_accept(&mut self) -> Result<Option<(Socket, SocketAddr)>, NetError> {
        let mut addr = [0u8; 28];
        let mut len = addr.len() as u32;
        match unsafe { sys::accept(self.0, addr.as_mut_ptr(), &mut len) } {
            sys::EAGAIN => Ok(None),
            n if n < 0 => Err(NetError::Failed),
            fd => Ok(Some((Socket(fd as c_int), SocketAddr::decode(&addr).ok_or(NetError::Failed)?))),
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        sys::close(self.0);
    }
}

/// A connected TCP stream.
pub fn tcp_connect(to: &SocketAddr, timeout_ms: u64) -> Result<Socket, NetError> {
    let s = Socket::open(to.ip.is_v6(), sys::SOCK_STREAM, 0)?;
    s.connect(to, timeout_ms)?;
    Ok(s)
}

/// A TCP socket accepting connections on `port`, over IPv4 and IPv6 and
/// on every interface, holding up to `backlog` not yet accepted.
pub fn tcp_listen(port: u16, backlog: u32) -> Result<Socket, NetError> {
    let s = Socket::open(true, sys::SOCK_STREAM, 0)?;
    s.bind(&SocketAddr::new(IpAddr::V6(Ipv6::default()), port))?;
    if sys::listen(s.0, backlog as c_int) < 0 {
        return Err(NetError::Failed);
    }
    Ok(s)
}

/// A UDP socket bound to `port` on every interface (0: any free port),
/// for IPv6 peers if `v6`, else IPv4.
pub fn udp_bind(port: u16, v6: bool) -> Result<Socket, NetError> {
    let s = Socket::open(v6, sys::SOCK_DGRAM, 0)?;
    let any = if v6 { IpAddr::V6(Ipv6::default()) } else { IpAddr::V4(Ipv4::default()) };
    s.bind(&SocketAddr::new(any, port))?;
    Ok(s)
}

/// A UDP socket whose writes go to `to`.
pub fn udp_connect(to: &SocketAddr) -> Result<Socket, NetError> {
    let s = Socket::open(to.ip.is_v6(), sys::SOCK_DGRAM, 0)?;
    s.connect(to, 0)?;
    Ok(s)
}

/// A ping socket (ICMP, or ICMPv6 for an IPv6 address): write an echo
/// request (the kernel fills in the identifier and checksum), read back
/// whole messages.
pub fn icmp_connect(ip: IpAddr, scope: u32) -> Result<Socket, NetError> {
    let protocol = if ip.is_v6() { sys::IPPROTO_ICMPV6 } else { sys::IPPROTO_ICMP };
    let s = Socket::open(ip.is_v6(), sys::SOCK_DGRAM, protocol)?;
    s.connect(&SocketAddr { ip, port: 0, scope }, 0)?;
    Ok(s)
}

/// Every interface the kernel knows, in order.
pub fn interfaces() -> Vec<NetInfo> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut info: NetInfo = unsafe { core::mem::zeroed() };
        if unsafe { sys::net_info(i, &mut info) } != 0 {
            break;
        }
        out.push(info);
    }
    out
}

/// Applies `config` to interface `index`.
pub fn configure(index: u32, config: &NetConfig) -> bool {
    unsafe { sys::net_config(index, config) == 0 }
}

/// A NUL-padded name field as a String.
pub fn field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into()
}
