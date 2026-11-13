//! The network commands, one crate like the self-tests: each module
//! exports `<name>_main`, and scripts/build-userland.sh links the
//! staticlib once per tool with `main` pointed at it.
//!
//! - `ping [-4|-6] <host> [count]`: ICMP/ICMPv6 echo through a ping
//!   socket.
//! - `fetch [-4|-6] <url> [file]`: an HTTP/1.1 GET over http:// or
//!   https://, printed or saved.
//! - `serve [port] [dir]`: an HTTP file server.

#![no_std]

extern crate alloc;

mod fetch;
mod ping;
mod serve;
mod tls;

use alloc::vec::Vec;
use core::ffi::{CStr, c_char, c_int};

/// argv[1..] as strings.
fn args<'a>(argc: c_int, argv: *const *const c_char) -> Vec<&'a str> {
    (1..argc.max(1) as usize).map(|i| unsafe { CStr::from_ptr(*argv.add(i)) }.to_str().unwrap_or("")).collect()
}

/// Takes `-4`/`-6` out of `args`: which IP version a name may resolve to.
fn family_flag(args: Vec<&str>) -> (anshi::net::Family, Vec<&str>) {
    use anshi::net::Family;
    let mut family = Family::Any;
    let mut rest = Vec::new();
    for a in args {
        match a {
            "-4" => family = Family::V4,
            "-6" => family = Family::V6,
            _ => rest.push(a),
        }
    }
    (family, rest)
}

/// Ctrl-C or q pressed since the last check -- how a long-running tool
/// is stopped, there being no signals.
fn interrupted() -> bool {
    matches!(anshi::poll_key(), anshi::KeyPoll::Key(3 | b'q') | anshi::KeyPoll::Closed)
}
