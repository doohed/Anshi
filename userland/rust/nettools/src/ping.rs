//! `ping [-4|-6] <host> [count]`: sends ICMP (or ICMPv6) echo requests a
//! second apart and reports each reply's round trip. Ctrl-C or q stops
//! early. Times are at the kernel tick's 10 ms resolution.

use alloc::vec::Vec;
use core::ffi::{c_char, c_int};

use anshi::net::{self, IpAddr};
use anshi::println;

const DEFAULT_COUNT: u32 = 4;
const INTERVAL_MS: u64 = 1000;
const RESOLVE_TIMEOUT_MS: u64 = 5000;
const PAYLOAD: usize = 32;
const ECHO_REQUEST: u8 = 8;
const ECHO_REPLY: u8 = 0;
const ECHO_REQUEST_V6: u8 = 128;
const ECHO_REPLY_V6: u8 = 129;

#[unsafe(no_mangle)]
pub extern "C" fn ping_main(argc: c_int, argv: *const *const c_char) -> c_int {
    let (family, args) = crate::family_flag(crate::args(argc, argv));
    let Some(&host) = args.first() else {
        println!("usage: ping [-4|-6] <host> [count]");
        return 2;
    };
    let count = match args.get(1).map(|c| c.parse::<u32>()) {
        None => DEFAULT_COUNT,
        Some(Ok(n)) if n > 0 => n,
        _ => {
            println!("ping: bad count");
            return 2;
        }
    };

    let (ip, scope) = match net::resolve(host, family, RESOLVE_TIMEOUT_MS) {
        Ok(r) => r,
        Err(e) => {
            println!("ping: {host}: {e}");
            return 1;
        }
    };
    let mut sock = match net::icmp_connect(ip, scope) {
        Ok(s) => s,
        Err(_) => {
            println!("ping: no network");
            return 1;
        }
    };
    let (request, reply) = if ip.is_v6() { (ECHO_REQUEST_V6, ECHO_REPLY_V6) } else { (ECHO_REQUEST, ECHO_REPLY) };
    println!("PING {host} ({ip}): {PAYLOAD} data bytes");

    // Replies are matched to any packet still outstanding, not just the
    // last one: the first can easily come back late (smoltcp waits a
    // second between ARP requests, so a ping right after a DNS lookup
    // waits for the gateway's), and a late reply still counts, as with
    // the system ping.
    let mut sent_at: Vec<u64> = Vec::new();
    let mut answered: Vec<bool> = Vec::new();
    let mut received = 0u32;
    let (mut min, mut max, mut total) = (u64::MAX, 0u64, 0u64);
    'outer: for seq in 0..count as u16 {
        let mut packet = [0u8; 8 + PAYLOAD];
        packet[0] = request;
        packet[6..8].copy_from_slice(&seq.to_be_bytes());
        for (i, b) in packet[8..].iter_mut().enumerate() {
            *b = b'a' + (i % 26) as u8;
        }
        let start = anshi::now_ms();
        if sock.write_all(&packet, INTERVAL_MS).is_err() {
            println!("ping: send failed");
            break;
        }
        sent_at.push(start);
        answered.push(false);

        // Wait out the interval -- after the last packet, until it's
        // answered or the interval is up.
        let last = seq as u32 + 1 == count;
        while anshi::now_ms() - start < INTERVAL_MS && !(last && answered[seq as usize]) {
            if crate::interrupted() {
                break 'outer;
            }
            let mut buf = [0u8; 128];
            match sock.try_read(&mut buf) {
                Ok(Some(n)) if n >= 8 && buf[0] == reply => {
                    let s = u16::from_be_bytes([buf[6], buf[7]]) as usize;
                    if s < answered.len() && !answered[s] {
                        let rtt = anshi::now_ms() - sent_at[s];
                        println!("{n} bytes from {ip}: icmp_seq={s} time={rtt} ms");
                        answered[s] = true;
                        received += 1;
                        (min, max, total) = (min.min(rtt), max.max(rtt), total + rtt);
                    }
                }
                Ok(_) => anshi::sched_yield(),
                Err(_) => break 'outer,
            }
        }
        if !answered[seq as usize] {
            println!("request timeout for icmp_seq {seq}");
        }
    }
    let sent = sent_at.len() as u32;

    report(host, ip, sent, received, min, max, total)
}

fn report(host: &str, _ip: IpAddr, sent: u32, received: u32, min: u64, max: u64, total: u64) -> c_int {
    println!("--- {host} ping statistics ---");
    let loss = if sent == 0 { 0 } else { (sent - received) * 100 / sent };
    println!("{sent} packets transmitted, {received} received, {loss}% packet loss");
    if received > 0 {
        println!("round-trip min/avg/max = {min}/{}/{max} ms", total / received as u64);
        0
    } else {
        1
    }
}
