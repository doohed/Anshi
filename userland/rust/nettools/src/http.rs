//! HTTP/1.1 client pieces shared by fetch and curl: URLs, connecting
//! (plain TCP or TLS), sending a request, and a reply whose body is read
//! as it arrives -- framed by Content-Length, chunked, or running until
//! the server closes. One request per connection (Connection: close).
//!
//! Errors carry curl's exit code for what went wrong, so curl can return
//! the numbers scripts check for.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use anshi::net::{self, Family, Ipv6, NetError, Socket, SocketAddr};

use crate::tls::TlsStream;

const RESOLVE_TIMEOUT_MS: u64 = 5000;
const CONNECT_TIMEOUT_MS: u64 = 10_000;
const IO_TIMEOUT_MS: u64 = 15_000;
/// Status line and headers together; anything longer is refused.
const MAX_HEAD: usize = 64 * 1024;
const READ_CHUNK: usize = 16 * 1024;

/// What failed, as curl reports it: an exit code and a message.
pub struct Error {
    pub code: i32,
    pub msg: String,
}

impl Error {
    pub fn new(code: i32, msg: impl Into<String>) -> Error {
        Error { code, msg: msg.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

pub const E_UNSUPPORTED: i32 = 1;
pub const E_URL: i32 = 3;
pub const E_RESOLVE: i32 = 6;
pub const E_CONNECT: i32 = 7;
pub const E_WEIRD_REPLY: i32 = 8;
pub const E_PARTIAL: i32 = 18;
pub const E_HTTP: i32 = 22;
pub const E_WRITE: i32 = 23;
pub const E_READ_FILE: i32 = 26;
pub const E_TIMEOUT: i32 = 28;
pub const E_TLS: i32 = 35;
pub const E_REDIRECTS: i32 = 47;
pub const E_EMPTY_REPLY: i32 = 52;
pub const E_SEND: i32 = 55;
pub const E_RECV: i32 = 56;
pub const E_CERT: i32 = 60;

fn io_error(what: &str, e: NetError, default: i32) -> Error {
    let code = if e == NetError::TimedOut { E_TIMEOUT } else { default };
    Error::new(code, alloc::format!("{what}: {e}"))
}

#[derive(Clone)]
pub struct Url {
    pub https: bool,
    /// As written, without brackets.
    pub host: String,
    pub port: u16,
    /// Path and query; starts with '/'.
    pub path: String,
    /// `user:password` from the URL, if it had one.
    pub user: Option<String>,
}

impl Url {
    /// An http:// or https:// URL; with no scheme, http:// is assumed,
    /// as curl does.
    pub fn parse(url: &str) -> Result<Url, Error> {
        let url = url.split('#').next().unwrap_or("");
        let (https, rest) = match url.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("https") => (true, rest),
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => (false, rest),
            Some((scheme, _)) if scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) => {
                return Err(Error::new(E_UNSUPPORTED, alloc::format!("Protocol \"{scheme}\" not supported")));
            }
            _ => (false, url),
        };
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);
        let path = match path {
            "" => String::from("/"),
            p if p.starts_with('?') => alloc::format!("/{p}"),
            p => String::from(p),
        };
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(String::from(u)), h),
            None => (None, authority),
        };
        let default_port = if https { 443 } else { 80 };
        let bad_port = || Error::new(E_URL, alloc::format!("bad port number in {url}"));
        let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
            let (host, after) =
                v6.split_once(']').ok_or_else(|| Error::new(E_URL, alloc::format!("unclosed [ in {url}")))?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().map_err(|_| bad_port())?,
                None if after.is_empty() => default_port,
                None => return Err(bad_port()),
            };
            (host, port)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, "")) => (h, default_port),
                Some((h, p)) => (h, p.parse().map_err(|_| bad_port())?),
                None => (hostport, default_port),
            }
        };
        if host.is_empty() {
            return Err(Error::new(E_URL, alloc::format!("no host name in {url}")));
        }
        Ok(Url { https, host: String::from(host), port, path, user })
    }

    /// What goes in the Host header and in a rebuilt URL.
    pub fn authority(&self) -> String {
        let host = if Ipv6::parse(self.host.split('%').next().unwrap_or("")).is_some() {
            alloc::format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == if self.https { 443 } else { 80 } { host } else { alloc::format!("{host}:{}", self.port) }
    }

    pub fn origin(&self) -> String {
        alloc::format!("{}://{}", if self.https { "https" } else { "http" }, self.authority())
    }

    /// The URL written out in full (without any user:password).
    pub fn full(&self) -> String {
        alloc::format!("{}{}", self.origin(), self.path)
    }

    /// Where a Location header points, read relative to this URL.
    pub fn join(&self, location: &str) -> Result<Url, Error> {
        let first_slash = location.find('/');
        if location.find("://").is_some_and(|s| first_slash.is_none_or(|f| s < f)) {
            return Url::parse(location);
        }
        let joined = if location.starts_with("//") {
            alloc::format!("{}:{location}", if self.https { "https" } else { "http" })
        } else if location.starts_with('/') {
            alloc::format!("{}{location}", self.origin())
        } else if location.starts_with('?') {
            alloc::format!("{}{}{location}", self.origin(), self.path.split('?').next().unwrap_or("/"))
        } else {
            let path = self.path.split('?').next().unwrap_or("/");
            let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
            alloc::format!("{}{dir}{location}", self.origin())
        };
        Url::parse(&joined)
    }

    /// The last path segment, without the query: what `curl -O` saves as.
    pub fn file_name(&self) -> &str {
        let path = self.path.split('?').next().unwrap_or("");
        path.rsplit('/').next().unwrap_or("")
    }
}

/// The connection a request goes over: plain TCP, or TLS on top of it.
pub enum Stream {
    Plain(Socket),
    Tls(TlsStream),
}

impl Stream {
    pub fn write_all(&mut self, data: &[u8]) -> Result<(), Error> {
        match self {
            Stream::Plain(s) => s.write_all(data, IO_TIMEOUT_MS).map_err(|e| io_error("send", e, E_SEND)),
            Stream::Tls(t) => t.write_all(data).map_err(|e| Error::new(E_SEND, e)),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        match self {
            Stream::Plain(s) => s.read(buf, IO_TIMEOUT_MS).map_err(|e| io_error("receive", e, E_RECV)),
            Stream::Tls(t) => t.read(buf).map_err(|e| {
                let code = if e.contains("timed out") { E_TIMEOUT } else { E_RECV };
                Error::new(code, e)
            }),
        }
    }
}

/// The address `url` should be reached at.
pub fn resolve(url: &Url, family: Family) -> Result<SocketAddr, Error> {
    let (ip, scope) = net::resolve(&url.host, family, RESOLVE_TIMEOUT_MS).map_err(|e| match e {
        NetError::TimedOut => Error::new(E_RESOLVE, alloc::format!("Could not resolve host: {} (timed out)", url.host)),
        _ => Error::new(E_RESOLVE, alloc::format!("Could not resolve host: {}", url.host)),
    })?;
    Ok(SocketAddr { ip, port: url.port, scope })
}

/// A connection to `to`, with the TLS handshake done for an https URL.
pub fn connect(url: &Url, to: &SocketAddr) -> Result<Stream, Error> {
    let sock = net::tcp_connect(to, CONNECT_TIMEOUT_MS).map_err(|e| {
        let code = if e == NetError::TimedOut { E_TIMEOUT } else { E_CONNECT };
        Error::new(code, alloc::format!("Failed to connect to {} port {}: {e}", url.host, url.port))
    })?;
    if !url.https {
        return Ok(Stream::Plain(sock));
    }
    // An IP-literal host has no name to check the certificate against;
    // rustls checks the address instead.
    let name = url.host.split('%').next().unwrap_or("");
    TlsStream::connect(sock, name).map(Stream::Tls).map_err(|e| {
        let code = if e.starts_with("certificate rejected") { E_CERT } else { E_TLS };
        Error::new(code, e)
    })
}

/// The request line and headers, ending in the blank line. `headers`
/// are sent as given, after Host; Connection: close is added.
pub fn request_head(method: &str, url: &Url, headers: &[(String, String)]) -> String {
    let mut head = alloc::format!("{method} {} HTTP/1.1\r\nHost: {}\r\n", url.path, url.authority());
    for (name, value) in headers {
        head.push_str(name);
        head.push(':');
        if !value.is_empty() {
            head.push(' ');
            head.push_str(value);
        }
        head.push_str("\r\n");
    }
    head.push_str("Connection: close\r\n\r\n");
    head
}

/// How the body after the headers ends.
enum Body {
    Length(u64),
    /// `left` bytes remain in the current chunk; `crlf` once its data is
    /// done and the line break after it is still to come.
    Chunked {
        left: u64,
        crlf: bool,
    },
    /// Everything until the server closes.
    UntilClose,
    Done,
}

pub struct Response {
    pub status: u16,
    /// Status line and headers, without the blank line after them.
    pub head: String,
    stream: Stream,
    /// Received and not yet consumed: `buf[pos..]`.
    buf: Vec<u8>,
    pos: usize,
    body: Body,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl Response {
    /// Reads the reply's status line and headers. `head_request` says the
    /// request was a HEAD, whose reply has no body whatever it claims.
    pub fn read(stream: Stream, head_request: bool) -> Result<Response, Error> {
        let mut r = Response { status: 0, head: String::new(), stream, buf: Vec::new(), pos: 0, body: Body::Done };
        loop {
            let end = loop {
                if let Some(i) = find(&r.buf[r.pos..], b"\r\n\r\n") {
                    break r.pos + i;
                }
                if r.buf.len() - r.pos > MAX_HEAD {
                    return Err(Error::new(E_WEIRD_REPLY, "reply headers too long"));
                }
                if !r.fill()? {
                    return Err(if r.buf.is_empty() {
                        Error::new(E_EMPTY_REPLY, "Empty reply from server")
                    } else {
                        Error::new(E_WEIRD_REPLY, "connection closed in the middle of the headers")
                    });
                }
            };
            r.head = String::from_utf8_lossy(&r.buf[r.pos..end]).into_owned();
            r.pos = end + 4;
            r.status = r
                .head
                .split(' ')
                .nth(1)
                .and_then(|s| s.parse().ok())
                .filter(|_| r.head.starts_with("HTTP/"))
                .ok_or_else(|| Error::new(E_WEIRD_REPLY, "malformed reply (no status line)"))?;
            // 100 Continue and the like come before the real reply.
            if !(100..200).contains(&r.status) || r.status == 101 {
                break;
            }
        }
        r.body = if head_request || r.status == 204 || r.status == 304 {
            Body::Done
        } else if r.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
            Body::Chunked { left: 0, crlf: false }
        } else if let Some(len) = r.header("content-length") {
            Body::Length(len.parse().map_err(|_| Error::new(E_WEIRD_REPLY, "bad Content-Length"))?)
        } else {
            Body::UntilClose
        };
        Ok(r)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    pub fn is_redirect(&self) -> bool {
        matches!(self.status, 301 | 302 | 303 | 307 | 308)
    }

    /// Content-Length, when the body has one.
    pub fn length(&self) -> Option<u64> {
        if let Body::Length(n) = self.body { Some(n) } else { None }
    }

    /// Reads more from the connection; false once it's closed.
    fn fill(&mut self) -> Result<bool, Error> {
        if self.pos > 0 && self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        let have = self.buf.len();
        self.buf.resize(have + READ_CHUNK, 0);
        let got = self.stream.read(&mut self.buf[have..]);
        let n = *got.as_ref().unwrap_or(&0);
        self.buf.truncate(have + n);
        got.map(|n| n > 0)
    }

    /// One line of chunked framing, without its CRLF.
    fn line(&mut self) -> Result<String, Error> {
        loop {
            if let Some(i) = find(&self.buf[self.pos..], b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[self.pos..self.pos + i]).into_owned();
                self.pos += i + 2;
                return Ok(line);
            }
            if !self.fill()? {
                return Err(Error::new(E_PARTIAL, "connection closed in the middle of a chunked reply"));
            }
        }
    }

    /// Takes up to `want` buffered body bytes into `out`, reading from
    /// the connection first if none are buffered. 0 only at its close.
    fn take(&mut self, out: &mut [u8], want: u64) -> Result<usize, Error> {
        if self.pos == self.buf.len() && !self.fill()? {
            return Ok(0);
        }
        let n = out.len().min(self.buf.len() - self.pos).min(want.min(usize::MAX as u64) as usize);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }

    /// Body bytes into `out`; 0 once the body is complete.
    pub fn read_body(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            match self.body {
                Body::Done => return Ok(0),
                Body::Length(0) => self.body = Body::Done,
                Body::Length(left) => {
                    let n = self.take(out, left)?;
                    if n == 0 {
                        return Err(Error::new(
                            E_PARTIAL,
                            alloc::format!("transfer closed with {left} bytes remaining"),
                        ));
                    }
                    self.body = Body::Length(left - n as u64);
                    return Ok(n);
                }
                Body::UntilClose => {
                    let n = self.take(out, u64::MAX)?;
                    if n == 0 {
                        self.body = Body::Done;
                    }
                    return Ok(n);
                }
                Body::Chunked { left: 0, crlf } => {
                    if crlf {
                        self.line()?;
                    }
                    let line = self.line()?;
                    let size = u64::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                        .map_err(|_| Error::new(E_WEIRD_REPLY, "malformed chunked reply"))?;
                    if size == 0 {
                        // Trailer fields, if any, up to the blank line.
                        while !self.line().unwrap_or_default().is_empty() {}
                        self.body = Body::Done;
                    } else {
                        self.body = Body::Chunked { left: size, crlf: true };
                    }
                }
                Body::Chunked { left, .. } => {
                    let n = self.take(out, left)?;
                    if n == 0 {
                        return Err(Error::new(E_PARTIAL, "connection closed in the middle of a chunked reply"));
                    }
                    self.body = Body::Chunked { left: left - n as u64, crlf: true };
                    return Ok(n);
                }
            }
        }
    }

    /// The whole body, refusing one larger than `limit` bytes.
    pub fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, Error> {
        let mut body = Vec::new();
        let mut buf = alloc::vec![0u8; READ_CHUNK];
        loop {
            if crate::interrupted() {
                return Err(Error::new(E_RECV, "interrupted"));
            }
            match self.read_body(&mut buf)? {
                0 => return Ok(body),
                n => body.extend_from_slice(&buf[..n]),
            }
            if body.len() > limit {
                return Err(Error::new(E_RECV, alloc::format!("reply larger than {} MiB", limit >> 20)));
            }
        }
    }
}
