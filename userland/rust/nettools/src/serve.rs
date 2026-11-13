//! `serve [port] [dir]`: a small HTTP/1.0 file server -- the network
//! stack's listening side. Serves `dir` (default: the current directory)
//! on `port` (default 80), over IPv4 and IPv6 and on every interface:
//! files as they are, directories as their index.html or a generated
//! listing. Logs one line per request; q or Ctrl-C stops it.
//!
//! One request at a time, each on a connection of its own (no
//! keep-alive); the kernel holds a few more connections in its backlog
//! meanwhile. Paths can't climb out of `dir`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};

use anshi::net::{self, Socket, SocketAddr};
use anshi::{CString, File, println};

const DEFAULT_PORT: u16 = 80;
const BACKLOG: u32 = 4;
const REQUEST_MAX: usize = 8 * 1024;
const REQUEST_TIMEOUT_MS: u64 = 5000;
const SEND_TIMEOUT_MS: u64 = 15_000;
/// Files are read whole into memory; a program's heap stops at 16 MiB.
const FILE_MAX: usize = 8 * 1024 * 1024;

#[unsafe(no_mangle)]
pub extern "C" fn serve_main(argc: c_int, argv: *const *const c_char) -> c_int {
    let args = crate::args(argc, argv);
    let (port, dir) = match args.as_slice() {
        [] => (DEFAULT_PORT, "."),
        [p] if p.parse::<u16>().is_ok() => (p.parse().unwrap(), "."),
        [d] => (DEFAULT_PORT, *d),
        [p, d, ..] => match p.parse::<u16>() {
            Ok(p) => (p, *d),
            Err(_) => {
                println!("usage: serve [port] [dir]");
                return 2;
            }
        },
    };
    if !anshi::is_dir(dir) {
        println!("serve: {dir}: not a directory");
        return 1;
    }
    let mut listener = match net::tcp_listen(port, BACKLOG) {
        Ok(l) => l,
        Err(e) => {
            println!("serve: cannot listen on port {port}: {e}");
            return 1;
        }
    };

    println!("serve: {dir} on port {port}, at:");
    for i in net::interfaces() {
        let name = net::field(&i.name);
        if i.address != [0; 4] {
            println!("  http://{}:{port}/  ({name})", net::Ipv4(i.address));
        }
        for k in 0..i.v6_count as usize {
            let a = net::Ipv6(i.v6[k]);
            if !a.is_link_local() {
                println!("  http://[{a}]:{port}/  ({name})");
            }
        }
    }
    println!("q or Ctrl-C stops");

    loop {
        if crate::interrupted() {
            println!("serve: stopped");
            return 0;
        }
        match listener.try_accept() {
            Ok(Some((conn, peer))) => handle(conn, peer, dir),
            Ok(None) => anshi::sched_yield(),
            Err(_) => {
                println!("serve: the listening socket failed");
                return 1;
            }
        }
    }
}

/// Reads the request head: up to the blank line, within limits.
fn read_request(conn: &mut Socket) -> Option<String> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") && !raw.windows(2).any(|w| w == b"\n\n") {
        let n = conn.read(&mut buf, REQUEST_TIMEOUT_MS).ok()?;
        if n == 0 || raw.len() + n > REQUEST_MAX {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    String::from_utf8(raw).ok()
}

/// %xx escapes undone; None if one is malformed or the result isn't UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = core::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The request path as clean components below the served directory, or
/// None if it tries to climb out of it.
fn components(path: &str) -> Option<Vec<String>> {
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut parts: Vec<String> = Vec::new();
    for p in percent_decode(path)?.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(String::from(p)),
        }
    }
    Some(parts)
}

fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e).to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "txt" | "md" | "rs" | "c" | "h" | "toml" | "conf" | "sh" | "py" | "log" => "text/plain; charset=utf-8",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

fn read_file(path: &str) -> Option<Vec<u8>> {
    let mut f = File::open(&CString::new(path).ok()?)?;
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf);
        if n == 0 {
            return Some(data);
        }
        if data.len() + n > FILE_MAX {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn listing(url_path: &str, fs_path: &str) -> Option<Vec<u8>> {
    let mut entries = anshi::read_dir(&CString::new(fs_path).ok()?)?;
    entries.sort_by(|a, b| (!a.is_dir, &a.name).cmp(&(!b.is_dir, &b.name)));
    let base = if url_path.ends_with('/') { String::from(url_path) } else { format!("{url_path}/") };
    let title = escape(&base);
    let mut html = format!("<!doctype html><meta charset=utf-8><title>{title}</title><h1>{title}</h1><ul>\n");
    if base != "/" {
        html.push_str("<li><a href=\"../\">../</a></li>\n");
    }
    for e in entries {
        let slash = if e.is_dir { "/" } else { "" };
        let name = escape(&e.name);
        html.push_str(&format!("<li><a href=\"{base}{name}{slash}\">{name}{slash}</a></li>\n"));
    }
    html.push_str("</ul><hr><small>served by Anshi</small>\n");
    Some(html.into_bytes())
}

fn respond(conn: &mut Socket, status: &str, kind: &str, body: &[u8], head_only: bool) {
    let head = format!(
        "HTTP/1.0 {status}\r\nServer: anshi-serve/0.1\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if conn.write_all(head.as_bytes(), SEND_TIMEOUT_MS).is_ok() && !head_only {
        let _ = conn.write_all(body, SEND_TIMEOUT_MS);
    }
}

fn handle(mut conn: Socket, peer: SocketAddr, dir: &str) {
    let Some(request) = read_request(&mut conn) else {
        println!("{peer} (no request)");
        return;
    };
    let mut words = request.lines().next().unwrap_or("").split(' ');
    let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    let head_only = method == "HEAD";
    let (status, size) = if method != "GET" && !head_only {
        respond(&mut conn, "405 Method Not Allowed", "text/plain", b"only GET and HEAD\n", head_only);
        ("405", 0)
    } else if let Some(parts) = components(target) {
        let fs_path = parts.iter().fold(String::from(dir.trim_end_matches('/')), |p, c| format!("{p}/{c}"));
        let fs_path = if fs_path.is_empty() { String::from("/") } else { fs_path };
        let index = format!("{fs_path}/index.html");
        if anshi::is_dir(&fs_path) {
            let page = if anshi::exists(&index) {
                read_file(&index).map(|b| (b, "text/html; charset=utf-8"))
            } else {
                listing(target.split('?').next().unwrap_or("/"), &fs_path).map(|b| (b, "text/html; charset=utf-8"))
            };
            match page {
                Some((body, kind)) => {
                    respond(&mut conn, "200 OK", kind, &body, head_only);
                    ("200", body.len())
                }
                None => {
                    respond(&mut conn, "500 Internal Server Error", "text/plain", b"cannot read\n", head_only);
                    ("500", 0)
                }
            }
        } else if let Some(body) = read_file(&fs_path) {
            respond(&mut conn, "200 OK", content_type(&fs_path), &body, head_only);
            ("200", body.len())
        } else {
            respond(&mut conn, "404 Not Found", "text/plain", b"not found\n", head_only);
            ("404", 0)
        }
    } else {
        respond(&mut conn, "403 Forbidden", "text/plain", b"forbidden\n", head_only);
        ("403", 0)
    };
    println!("{peer} {method} {target} {status} {size}");
}
