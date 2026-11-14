//! `fetch [-4|-6] <url> [file]`: an HTTP/1.1 GET over http:// or
//! https://. The body goes to stdout, or into `file` with a one-line
//! summary. Follows up to five redirects (http to https included) and
//! understands chunked replies. IPv6 hosts go in brackets, as in
//! http://[fec0::2]:8000/. `curl` (curl.rs) is the full-featured
//! version; both sit on http.rs.

use alloc::string::String;
use alloc::vec;
use core::ffi::{c_char, c_int};

use anshi::net::Family;
use anshi::{CString, File, println};

use crate::http::{self, Response, Url};

const MAX_REDIRECTS: usize = 5;
/// The reply is held in full; a program's heap tops out at 16 MiB.
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

fn get(url: &Url, family: Family) -> Result<(Response, alloc::vec::Vec<u8>), http::Error> {
    let to = http::resolve(url, family)?;
    let mut stream = http::connect(url, &to)?;
    let headers = vec![
        (String::from("User-Agent"), String::from("anshi-fetch/0.2")),
        (String::from("Accept"), String::from("*/*")),
    ];
    stream.write_all(http::request_head("GET", url, &headers).as_bytes())?;
    let mut resp = Response::read(stream, false)?;
    let body = resp.read_to_end(MAX_RESPONSE)?;
    Ok((resp, body))
}

#[unsafe(no_mangle)]
pub extern "C" fn fetch_main(argc: c_int, argv: *const *const c_char) -> c_int {
    let (family, args) = crate::family_flag(crate::args(argc, argv));
    let Some(&first) = args.first() else {
        println!("usage: fetch [-4|-6] <http[s]://host[:port]/path> [file]");
        return 2;
    };

    let mut url = match Url::parse(first) {
        Ok(u) => u,
        Err(e) => {
            println!("fetch: {e}");
            return 1;
        }
    };
    for _ in 0..=MAX_REDIRECTS {
        let (resp, body) = match get(&url, family) {
            Ok(r) => r,
            Err(e) => {
                println!("fetch: {e}");
                return 1;
            }
        };
        if resp.is_redirect() {
            let Some(location) = resp.header("location") else {
                println!("fetch: redirect {} without a Location", resp.status);
                return 1;
            };
            url = match url.join(location) {
                Ok(u) => u,
                Err(e) => {
                    println!("fetch: {e}");
                    return 1;
                }
            };
            println!("fetch: {} -> {}", resp.status, url.full());
            continue;
        }
        return finish(&resp, &body, args.get(1).copied());
    }
    println!("fetch: too many redirects");
    1
}

fn finish(resp: &Response, body: &[u8], file: Option<&str>) -> c_int {
    let ok = (200..300).contains(&resp.status);
    let status_line = resp.head.lines().next().unwrap_or("");
    match file {
        Some(path) => {
            if !ok {
                println!("fetch: server answered {status_line}");
                return 1;
            }
            let saved = CString::new(path).ok().and_then(|p| File::create(&p)).is_some_and(|mut f| f.write_all(body));
            if !saved {
                println!("fetch: cannot write {path}");
                return 1;
            }
            println!("fetch: saved {} bytes to {path}", body.len());
        }
        None => {
            if !ok {
                println!("fetch: server answered {status_line}");
            }
            anshi::write_all(1, body);
            if !body.ends_with(b"\n") && !body.is_empty() {
                anshi::write_all(1, b"\n");
            }
        }
    }
    if ok { 0 } else { 1 }
}
