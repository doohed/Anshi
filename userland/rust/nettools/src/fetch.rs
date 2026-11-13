//! `fetch [-4|-6] <url> [file]`: an HTTP/1.1 GET over http:// or
//! https://. The body goes to stdout, or into `file` with a one-line
//! summary. Follows up to five redirects (http to https included) and
//! understands chunked replies. IPv6 hosts go in brackets, as in
//! http://[fec0::2]:8000/.

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};

use anshi::net::{self, Family, Ipv6, Socket, SocketAddr};
use anshi::{CString, File, println};

use crate::tls::TlsStream;

const RESOLVE_TIMEOUT_MS: u64 = 5000;
const CONNECT_TIMEOUT_MS: u64 = 10_000;
const IO_TIMEOUT_MS: u64 = 15_000;
const MAX_REDIRECTS: usize = 5;
/// The reply is held in full; a program's heap tops out at 16 MiB.
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

struct Url {
    https: bool,
    /// As written, without brackets.
    host: String,
    port: u16,
    /// Starts with '/'.
    path: String,
}

impl Url {
    fn parse(url: &str) -> Result<Url, String> {
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else {
            (false, url.strip_prefix("http://").unwrap_or(url))
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let default_port = if https { 443 } else { 80 };
        let bad_port = || alloc::format!("bad port in {url}");
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (host, after) = v6.split_once(']').ok_or_else(|| alloc::format!("unclosed [ in {url}"))?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().map_err(|_| bad_port())?,
                None if after.is_empty() => default_port,
                None => return Err(bad_port()),
            };
            (host, port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, p.parse().map_err(|_| bad_port())?),
                None => (authority, default_port),
            }
        };
        if host.is_empty() {
            return Err(alloc::format!("no host in {url}"));
        }
        Ok(Url { https, host: String::from(host), port, path: String::from(path) })
    }

    /// What goes in the Host header and in a rebuilt URL.
    fn authority(&self) -> String {
        let host = if Ipv6::parse(self.host.split('%').next().unwrap_or("")).is_some() {
            alloc::format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == if self.https { 443 } else { 80 } { host } else { alloc::format!("{host}:{}", self.port) }
    }

    fn origin(&self) -> String {
        alloc::format!("{}://{}", if self.https { "https" } else { "http" }, self.authority())
    }
}

/// The connection a request goes over: plain TCP, or TLS on top of it.
enum Stream {
    Plain(Socket),
    Tls(TlsStream),
}

impl Stream {
    fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        match self {
            Stream::Plain(s) => s.write_all(data, IO_TIMEOUT_MS).map_err(|e| alloc::format!("send: {e}")),
            Stream::Tls(t) => t.write_all(data),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        match self {
            Stream::Plain(s) => s.read(buf, IO_TIMEOUT_MS).map_err(|e| alloc::format!("receive: {e}")),
            Stream::Tls(t) => t.read(buf),
        }
    }
}

struct Response {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Undoes Transfer-Encoding: chunked.
fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = find(data, b"\r\n")?;
        let size_text = core::str::from_utf8(&data[..eol]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        data = &data[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(data.get(..size)?);
        data = data.get(size + 2..)?;
    }
}

fn get(url: &Url, family: Family) -> Result<Response, String> {
    let (ip, scope) =
        net::resolve(&url.host, family, RESOLVE_TIMEOUT_MS).map_err(|e| alloc::format!("{}: {e}", url.host))?;
    let to = SocketAddr { ip, port: url.port, scope };
    let sock = net::tcp_connect(&to, CONNECT_TIMEOUT_MS).map_err(|e| alloc::format!("connect to {to}: {e}"))?;
    let mut stream = if url.https {
        // An IP-literal host has no name to check the certificate
        // against; rustls checks the address instead.
        let name = url.host.split('%').next().unwrap_or("");
        Stream::Tls(TlsStream::connect(sock, name)?)
    } else {
        Stream::Plain(sock)
    };

    let request = alloc::format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: anshi-fetch/0.2\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        url.path,
        url.authority()
    );
    stream.write_all(request.as_bytes())?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if crate::interrupted() {
            return Err(String::from("interrupted"));
        }
        match stream.read(&mut buf)? {
            0 => break,
            n => raw.extend_from_slice(&buf[..n]),
        }
        if raw.len() > MAX_RESPONSE {
            return Err(alloc::format!("reply larger than {} MiB", MAX_RESPONSE >> 20));
        }
    }

    let split = find(&raw, b"\r\n\r\n").ok_or("malformed reply (no header end)")?;
    let headers = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = headers
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .filter(|_| headers.starts_with("HTTP/"))
        .ok_or("malformed reply (no status line)")?;
    let mut resp = Response { status, headers, body: raw[split + 4..].to_vec() };
    if resp.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        resp.body = dechunk(&resp.body).ok_or("malformed chunked reply")?;
    } else if let Some(len) = resp.header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        resp.body.truncate(len);
    }
    Ok(resp)
}

#[unsafe(no_mangle)]
pub extern "C" fn fetch_main(argc: c_int, argv: *const *const c_char) -> c_int {
    let (family, args) = crate::family_flag(crate::args(argc, argv));
    let Some(&first) = args.first() else {
        println!("usage: fetch [-4|-6] <http[s]://host[:port]/path> [file]");
        return 2;
    };

    let mut url = String::from(first);
    for _ in 0..=MAX_REDIRECTS {
        let parsed = match Url::parse(&url) {
            Ok(u) => u,
            Err(e) => {
                println!("fetch: {e}");
                return 1;
            }
        };
        let resp = match get(&parsed, family) {
            Ok(r) => r,
            Err(e) => {
                println!("fetch: {e}");
                return 1;
            }
        };
        if matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
            let Some(location) = resp.header("location") else {
                println!("fetch: redirect {} without a Location", resp.status);
                return 1;
            };
            url = if location.starts_with('/') {
                alloc::format!("{}{location}", parsed.origin())
            } else {
                String::from(location)
            };
            println!("fetch: {} -> {url}", resp.status);
            continue;
        }
        return finish(&resp, args.get(1).copied());
    }
    println!("fetch: too many redirects");
    1
}

fn finish(resp: &Response, file: Option<&str>) -> c_int {
    let ok = (200..300).contains(&resp.status);
    match file {
        Some(path) => {
            if !ok {
                println!("fetch: server answered {}", resp.headers.lines().next().unwrap_or(""));
                return 1;
            }
            let saved =
                CString::new(path).ok().and_then(|p| File::create(&p)).is_some_and(|mut f| f.write_all(&resp.body));
            if !saved {
                println!("fetch: cannot write {path}");
                return 1;
            }
            println!("fetch: saved {} bytes to {path}", resp.body.len());
        }
        None => {
            if !ok {
                println!("fetch: server answered {}", resp.headers.lines().next().unwrap_or(""));
            }
            anshi::write_all(1, &resp.body);
            if !resp.body.ends_with(b"\n") && !resp.body.is_empty() {
                anshi::write_all(1, b"\n");
            }
        }
    }
    if ok { 0 } else { 1 }
}
