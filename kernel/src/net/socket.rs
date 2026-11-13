//! The sockets behind a program's socket fds (exec/syscall.rs): TCP and
//! UDP over IPv4 and IPv6, and ICMP/ICMPv6 echo for ping; TCP both ways,
//! connecting out and listening for connections in. Like pipes, nothing
//! here ever blocks -- a call that can't finish yet returns EAGAIN and
//! the program loops in ring 3, yielding in between -- and a socket is
//! shared by reference count across fork().
//!
//! smoltcp keeps sockets per interface, so one socket here may stand for
//! several there ("parts"): a connected TCP socket is one, on the
//! interface its destination routes through; a listening one keeps a
//! backlog of listeners on every interface, any of which a connection
//! can arrive on; a bound UDP socket is bound on every interface.

use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::{self, State};
use smoltcp::socket::{icmp, udp};
use smoltcp::time::Duration;
use smoltcp::wire::{Icmpv4Packet, Icmpv6Packet, IpAddress, IpEndpoint};

use super::{AF_INET, AF_INET6, EAGAIN, Stack, stack};
use crate::drivers::timer;
use crate::random;
use crate::sync::Global;

pub const MAX_SOCKETS: usize = 32;

// Linux's values (userland/rust/anshi/src/sys.rs).
pub const SOCK_STREAM: u64 = 1;
pub const SOCK_DGRAM: u64 = 2;
/// socket()'s type may carry SOCK_NONBLOCK/SOCK_CLOEXEC; every socket here
/// is non-blocking anyway.
const SOCK_TYPE_MASK: u64 = 0xf;
pub const IPPROTO_ICMP: u64 = 1;
pub const IPPROTO_UDP: u64 = 17;
pub const IPPROTO_ICMPV6: u64 = 58;

const TCP_BUFFER: usize = 16 * 1024;
const UDP_BUFFER: usize = 8 * 1024;
const UDP_PACKETS: usize = 8;
const ICMP_BUFFER: usize = 2 * 1024;
const ICMP_PACKETS: usize = 4;
/// A peer that stops answering fails the connection after this long.
const TCP_TIMEOUT_SECS: u64 = 15;
/// How long a closed TCP socket may take to finish its goodbye before
/// it's reset.
const LINGER_MS: u64 = 3000;
/// Connections a listening socket can hold not yet accept()ed, per
/// interface.
const MAX_BACKLOG: usize = 8;
const ICMP_HEADER: usize = 8;

const EPHEMERAL_FIRST: u16 = 49152;
const EPHEMERAL_COUNT: u16 = 16000;

/// An address and port, plus for a link-local IPv6 address the interface
/// it's on (index + 1; 0 = any) -- sockaddr_in/sockaddr_in6, decoded.
#[derive(Clone, Copy)]
pub struct Endpoint {
    pub addr: IpAddress,
    pub port: u16,
    pub scope: u32,
}

pub fn family_of(addr: &IpAddress) -> u16 {
    match addr {
        IpAddress::Ipv4(_) => AF_INET,
        IpAddress::Ipv6(_) => AF_INET6,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Tcp,
    Udp,
    Icmp,
}

#[derive(Clone, Copy)]
struct Part {
    iface: usize,
    handle: SocketHandle,
}

struct Sock {
    kind: Kind,
    family: u16,
    refs: u32,
    parts: Vec<Part>,
    /// Where write() sends (UDP, ICMP); the peer (TCP).
    remote: Option<Endpoint>,
    /// bind()'s port, or the one picked for it.
    local_port: u16,
    /// TCP: connect() was called, or this came from accept().
    started: bool,
    listening: bool,
    /// ICMP: the echo identifier replies are matched by.
    ident: u16,
    /// Closed by its last owner at this uptime; reap() removes it.
    closed_ms: Option<u64>,
}

static SOCKS: Global<[Option<Sock>; MAX_SOCKETS]> = Global::new([const { None }; MAX_SOCKETS]);

fn socks() -> &'static mut [Option<Sock>; MAX_SOCKETS] {
    unsafe { SOCKS.get() }
}

/// An open socket: one a program still holds.
fn sock(id: usize) -> Option<&'static mut Sock> {
    socks().get_mut(id)?.as_mut().filter(|s| s.closed_ms.is_none())
}

/// A local port from the dynamic range.
fn next_port() -> u16 {
    EPHEMERAL_FIRST + (random::u64() % EPHEMERAL_COUNT as u64) as u16
}

fn tcp_socket() -> tcp::Socket<'static> {
    let buffer = || tcp::SocketBuffer::new(vec![0; TCP_BUFFER]);
    let mut t = tcp::Socket::new(buffer(), buffer());
    t.set_timeout(Some(Duration::from_secs(TCP_TIMEOUT_SECS)));
    t
}

fn udp_socket() -> udp::Socket<'static> {
    let buffer = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], vec![0; UDP_BUFFER]);
    udp::Socket::new(buffer(), buffer())
}

fn icmp_socket(ident: u16) -> Option<icmp::Socket<'static>> {
    let buffer = || icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; ICMP_PACKETS], vec![0; ICMP_BUFFER]);
    let mut i = icmp::Socket::new(buffer(), buffer());
    i.bind(icmp::Endpoint::Ident(ident)).ok()?;
    Some(i)
}

fn tcp<'a>(s: &'a mut Stack, p: &Part) -> &'a mut tcp::Socket<'static> {
    s.ifaces[p.iface].sockets.get_mut::<tcp::Socket>(p.handle)
}

/// A TCP listener on `port` on interface `iface`.
fn listener(s: &mut Stack, iface: usize, port: u16) -> Option<Part> {
    let mut t = tcp_socket();
    t.listen(port).ok()?;
    Some(Part { iface, handle: s.ifaces[iface].sockets.add(t) })
}

/// socket(): its id, or None for an unsupported kind or a full table.
pub fn open(domain: u64, kind: u64, protocol: u64) -> Option<usize> {
    stack()?;
    let family = match domain as u16 {
        AF_INET => AF_INET,
        AF_INET6 => AF_INET6,
        _ => return None,
    };
    let kind = match (kind & SOCK_TYPE_MASK, protocol, family) {
        (SOCK_STREAM, 0 | 6, _) => Kind::Tcp,
        (SOCK_DGRAM, IPPROTO_ICMP, AF_INET) | (SOCK_DGRAM, IPPROTO_ICMPV6, AF_INET6) => Kind::Icmp,
        (SOCK_DGRAM, 0 | IPPROTO_UDP, _) => Kind::Udp,
        _ => return None,
    };
    let id = socks().iter().position(Option::is_none)?;
    socks()[id] = Some(Sock {
        kind,
        family,
        refs: 1,
        parts: Vec::new(),
        remote: None,
        local_port: 0,
        started: false,
        listening: false,
        ident: next_port(),
        closed_ms: None,
    });
    Some(id)
}

/// fork(): the child's copy of the fd is one more owner.
pub fn dup(id: usize) {
    if let Some(k) = sock(id) {
        k.refs += 1;
    }
}

/// Drops one owner. The last one closes the socket: TCP says goodbye in
/// the background (reap()), the others are gone at once.
pub fn close(id: usize) {
    let (Some(s), Some(k)) = (stack(), sock(id)) else { return };
    k.refs -= 1;
    if k.refs > 0 {
        return;
    }
    if k.kind == Kind::Tcp && !k.parts.is_empty() {
        for p in &k.parts {
            tcp(s, p).close();
        }
        k.closed_ms = Some(timer::uptime_ms());
        s.poll();
    } else {
        for p in &k.parts {
            s.ifaces[p.iface].sockets.remove(p.handle);
        }
        socks()[id] = None;
    }
}

/// Frees closed TCP sockets once their connections have wound down, and
/// resets the ones taking too long. Runs after the interface poll, so a
/// reset queued here goes out on the next one, before the socket is
/// removed.
pub(super) fn reap(s: &mut Stack) {
    let now = timer::uptime_ms();
    for slot in socks().iter_mut() {
        let Some(k) = slot.as_mut() else { continue };
        let Some(closed_ms) = k.closed_ms else { continue };
        k.parts.retain(|p| {
            let t = tcp(s, p);
            if t.state() == State::Closed {
                s.ifaces[p.iface].sockets.remove(p.handle);
                false
            } else {
                if now - closed_ms > LINGER_MS {
                    t.abort();
                }
                true
            }
        });
        if k.parts.is_empty() {
            *slot = None;
        }
    }
}

/// UDP: binds on every interface, to the bind()ed port or a fresh one.
fn ensure_bound(s: &mut Stack, k: &mut Sock) -> Option<()> {
    if !k.parts.is_empty() {
        return Some(());
    }
    if k.local_port == 0 {
        k.local_port = next_port();
    }
    for iface in 0..s.ifaces.len() {
        let mut u = udp_socket();
        u.bind(k.local_port).ok()?;
        k.parts.push(Part { iface, handle: s.ifaces[iface].sockets.add(u) });
    }
    Some(())
}

/// TCP: starts connecting, then reports progress each time it's called
/// again -- EAGAIN while the handshake runs, 0 once connected, None if it
/// failed or there's no route. UDP and ICMP: sets where write() sends; 0
/// at once.
pub fn connect(id: usize, to: Endpoint) -> Option<i64> {
    let s = stack()?;
    s.poll();
    let k = sock(id)?;
    if family_of(&to.addr) != k.family || k.listening {
        return None;
    }
    match k.kind {
        Kind::Tcp => {
            if !k.started {
                let iface = s.route(&to.addr, to.scope)?;
                let local = if k.local_port != 0 { k.local_port } else { next_port() };
                let i = &mut s.ifaces[iface];
                let mut t = tcp_socket();
                t.connect(i.iface.context(), IpEndpoint::new(to.addr, to.port), local).ok()?;
                k.parts.push(Part { iface, handle: i.sockets.add(t) });
                k.started = true;
                k.remote = Some(to);
                s.poll(); // The SYN goes out now, not at the next tick.
                return Some(EAGAIN);
            }
            let t = tcp(s, k.parts.first()?);
            match t.state() {
                State::SynSent | State::SynReceived => Some(EAGAIN),
                _ if t.may_send() => Some(0),
                _ => None,
            }
        }
        Kind::Udp => {
            ensure_bound(s, k)?;
            k.remote = Some(to);
            Some(0)
        }
        Kind::Icmp => {
            k.remote = Some(to);
            Some(0)
        }
    }
}

/// The local port, on every interface (the address is ignored: a socket
/// here is always bound to all of them). TCP: for listen(), or the port
/// connect() comes from. UDP: where datagrams arrive.
pub fn bind(id: usize, at: Endpoint) -> Option<i64> {
    let s = stack()?;
    let k = sock(id)?;
    if k.started || k.listening || !k.parts.is_empty() || family_of(&at.addr) != k.family {
        return None;
    }
    match k.kind {
        Kind::Tcp => k.local_port = at.port,
        Kind::Udp => {
            k.local_port = at.port;
            ensure_bound(s, k)?;
        }
        Kind::Icmp => return None,
    }
    Some(0)
}

/// TCP: starts accepting connections on the bound port (a fresh one if
/// none was bound), holding up to `backlog` not yet accept()ed.
pub fn listen(id: usize, backlog: u64) -> Option<i64> {
    let s = stack()?;
    let k = sock(id)?;
    if k.kind != Kind::Tcp || k.started {
        return None;
    }
    if k.listening {
        return Some(0);
    }
    if k.local_port == 0 {
        k.local_port = next_port();
    }
    let backlog = (backlog as usize).clamp(1, MAX_BACKLOG);
    for iface in 0..s.ifaces.len() {
        for _ in 0..backlog {
            let part = listener(s, iface, k.local_port)?;
            k.parts.push(part);
        }
    }
    k.listening = true;
    Some(0)
}

/// A connection that has arrived on a listening socket: its new socket's
/// id and the peer. EAGAIN when none has yet.
pub fn accept(id: usize) -> Option<Result<(usize, Endpoint), i64>> {
    let s = stack()?;
    s.poll();
    let k = sock(id)?;
    if !k.listening {
        return None;
    }
    let port = k.local_port;
    let mut ready = None;
    for (n, p) in k.parts.iter().enumerate() {
        let t = tcp(s, p);
        match t.state() {
            State::Listen | State::SynReceived => {}
            // A handshake that failed (reset, timed out): listen again.
            State::Closed => {
                let _ = t.listen(port);
            }
            _ => {
                ready = Some(n);
                break;
            }
        }
    }
    let Some(n) = ready else { return Some(Err(EAGAIN)) };
    let slot = socks().iter().position(Option::is_none)?;
    let part = k.parts.remove(n);
    if let Some(fresh) = listener(s, part.iface, port) {
        k.parts.push(fresh);
    }
    let t = tcp(s, &part);
    let remote = t.remote_endpoint()?;
    let peer = Endpoint { addr: remote.addr, port: remote.port, scope: part.iface as u32 + 1 };
    socks()[slot] = Some(Sock {
        kind: Kind::Tcp,
        family: family_of(&remote.addr),
        refs: 1,
        parts: vec![part],
        remote: Some(peer),
        local_port: port,
        started: true,
        listening: false,
        ident: 0,
        closed_ms: None,
    });
    Some(Ok((slot, peer)))
}

/// TCP: the next bytes of the stream; 0 once the peer has closed and
/// everything was read. UDP: one datagram's payload. ICMP: one message,
/// header included. With who sent it (UDP, ICMP) or the peer (TCP).
/// EAGAIN when nothing has arrived yet.
pub fn recv(id: usize, buf: &mut [u8]) -> Option<(i64, Option<Endpoint>)> {
    let s = stack()?;
    s.poll();
    let k = sock(id)?;
    match k.kind {
        Kind::Tcp => {
            if k.listening {
                return None;
            }
            let Some(p) = k.parts.first() else { return None };
            let t = tcp(s, p);
            let n = if t.can_recv() {
                t.recv_slice(buf).ok()? as i64
            } else if matches!(t.state(), State::SynSent | State::SynReceived) || t.may_recv() {
                EAGAIN
            } else {
                0
            };
            Some((n, k.remote))
        }
        Kind::Udp => {
            if k.parts.is_empty() {
                return None;
            }
            for p in &k.parts {
                let u = s.ifaces[p.iface].sockets.get_mut::<udp::Socket>(p.handle);
                match u.recv_slice(buf) {
                    Ok((n, meta)) => {
                        let from =
                            Endpoint { addr: meta.endpoint.addr, port: meta.endpoint.port, scope: p.iface as u32 + 1 };
                        return Some((n as i64, Some(from)));
                    }
                    Err(udp::RecvError::Exhausted) => {}
                    Err(udp::RecvError::Truncated) => return None,
                }
            }
            Some((EAGAIN, None))
        }
        Kind::Icmp => {
            for p in &k.parts {
                let i = s.ifaces[p.iface].sockets.get_mut::<icmp::Socket>(p.handle);
                match i.recv_slice(buf) {
                    Ok((n, addr)) => {
                        return Some((n as i64, Some(Endpoint { addr, port: 0, scope: p.iface as u32 + 1 })));
                    }
                    Err(icmp::RecvError::Exhausted) => {}
                    Err(icmp::RecvError::Truncated) => return None,
                }
            }
            Some((EAGAIN, None))
        }
    }
}

/// TCP: queues as much of `data` as fits and returns how much (`to` is
/// ignored). UDP: one datagram, to `to` or the connect()ed address. ICMP:
/// one echo request -- the caller writes the whole message, the
/// identifier and checksum are filled in here, as Linux's ping sockets
/// do. EAGAIN when the send buffer is full or the connection isn't up
/// yet; None when it can't be sent at all (no route, for one).
pub fn send(id: usize, data: &[u8], to: Option<Endpoint>) -> Option<i64> {
    let s = stack()?;
    let k = sock(id)?;
    let result = match k.kind {
        Kind::Tcp => {
            let t = tcp(s, k.parts.first().filter(|_| !k.listening)?);
            if t.may_send() {
                match t.send_slice(data) {
                    Ok(0) if !data.is_empty() => Some(EAGAIN),
                    Ok(n) => Some(n as i64),
                    Err(_) => None,
                }
            } else if matches!(t.state(), State::SynSent | State::SynReceived) {
                Some(EAGAIN)
            } else {
                None
            }
        }
        Kind::Udp => {
            let to = to.or(k.remote).filter(|e| family_of(&e.addr) == k.family)?;
            ensure_bound(s, k)?;
            let iface = s.route(&to.addr, to.scope)?;
            let p = k.parts.iter().find(|p| p.iface == iface)?;
            let u = s.ifaces[iface].sockets.get_mut::<udp::Socket>(p.handle);
            match u.send_slice(data, IpEndpoint::new(to.addr, to.port)) {
                Ok(()) => Some(data.len() as i64),
                Err(udp::SendError::BufferFull) => Some(EAGAIN),
                Err(udp::SendError::Unaddressable) => None,
            }
        }
        Kind::Icmp => {
            let to = to.or(k.remote).filter(|e| family_of(&e.addr) == k.family)?;
            if data.len() < ICMP_HEADER {
                return None;
            }
            let iface = s.route(&to.addr, to.scope)?;
            let p = match k.parts.iter().find(|p| p.iface == iface) {
                Some(p) => *p,
                None => {
                    let p = Part { iface, handle: s.ifaces[iface].sockets.add(icmp_socket(k.ident)?) };
                    k.parts.push(p);
                    p
                }
            };
            let i = &mut s.ifaces[iface];
            let src6 = match to.addr {
                IpAddress::Ipv6(dst) => Some((i.iface.get_source_address_ipv6(&dst), dst)),
                IpAddress::Ipv4(_) => None,
            };
            match i.sockets.get_mut::<icmp::Socket>(p.handle).send(data.len(), to.addr) {
                Ok(payload) => {
                    payload.copy_from_slice(data);
                    match src6 {
                        None => {
                            let mut packet = Icmpv4Packet::new_unchecked(payload);
                            packet.set_echo_ident(k.ident);
                            packet.fill_checksum();
                        }
                        Some((src, dst)) => {
                            let mut packet = Icmpv6Packet::new_unchecked(payload);
                            packet.set_echo_ident(k.ident);
                            packet.fill_checksum(&src, &dst);
                        }
                    }
                    Some(data.len() as i64)
                }
                Err(icmp::SendError::BufferFull) => Some(EAGAIN),
                Err(icmp::SendError::Unaddressable) => None,
            }
        }
    };
    s.poll(); // Out now, not at the next tick.
    result
}
