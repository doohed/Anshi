//! `curl [options] <url>...`: a curl-compatible HTTP(S) client on
//! http.rs, taking the options people actually type -- `-o`/`-O`, `-L`,
//! `-i`/`-I`, `-X`, `-H`, `-d` and friends, `--json`, `-T`, `-u`, `-f`,
//! `-s`/`-S`, `-v`, `-w`, `-m` -- with short flags combinable
//! (`-fsSL`) and curl's own exit codes. The body streams to stdout or a
//! file as it arrives, so the size isn't limited by the program's heap.
//! See docs/net.md for the full list and what's missing.

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};

use anshi::net::Family;
use anshi::{CString, File, eprint, eprintln, print};

use crate::http::{self, Error, Response, Url};

const VERSION: &str = "8.11.1";
const DEFAULT_MAX_REDIRS: usize = 50;
const CHUNK: usize = 16 * 1024;
const PROGRESS_EVERY_MS: u64 = 250;
/// Exit code for a usage mistake, and for a transfer stopped with Ctrl-C
/// (a shell's 128 + SIGINT).
const E_USAGE: i32 = 2;
const E_INTERRUPTED: i32 = 130;

const HELP: &str = "\
Usage: curl [options] <url>...
 -o, --output <file>       write the body to <file> (- for stdout)
 -O, --remote-name         write it to a file named like the URL's last part
 -L, --location            follow redirects
     --max-redirs <n>      at most <n> of them (default 50)
 -i, --include             show the reply headers before the body
 -I, --head                send HEAD, show only the headers
 -D, --dump-header <file>  save the reply headers to <file> (- for stdout)
 -X, --request <method>    the method to use
 -H, --header <line>       add a header ('Name: value'), drop one ('Name:')
 -d, --data <data>         POST <data> (@file reads a file); repeats join with &
     --data-raw <data>     the same, with no @file
     --data-binary <data>  the same, with @file sent byte for byte
     --data-urlencode <d>  POST <d> URL-encoded (name=value, name@file)
     --json <data>         POST JSON (@file reads a file)
 -G, --get                 put the -d data in the URL's query, send GET
 -T, --upload-file <file>  PUT <file>
 -u, --user <user:pass>    HTTP Basic authentication
 -A, --user-agent <name>   the User-Agent to send
 -e, --referer <url>       the Referer to send
 -b, --cookie <a=b; c=d>   the Cookie header to send
 -f, --fail                fail with exit code 22 on an HTTP error, no body
 -s, --silent              no progress, no error messages
 -S, --show-error          error messages even with -s
 -v, --verbose             show the connection, request and reply headers
 -w, --write-out <format>  print %{http_code}, %{size_download}, ... after
 -m, --max-time <seconds>  give up after this long
 -4, -6                    IPv4 or IPv6 only
 -h, --help, -V, --version";

enum Output {
    File(String),
    RemoteName,
}

struct Opts {
    urls: Vec<String>,
    /// Matched to the URLs in order.
    outputs: Vec<Output>,
    follow: bool,
    max_redirs: usize,
    include: bool,
    head: bool,
    method: Option<String>,
    headers: Vec<String>,
    /// -d and friends, already decoded; joined with & when sent.
    data: Vec<Vec<u8>>,
    json: bool,
    get: bool,
    upload: Option<String>,
    user: Option<String>,
    agent: String,
    referer: Option<String>,
    cookie: Option<String>,
    write_out: Option<String>,
    dump_header: Option<String>,
    fail: bool,
    silent: bool,
    show_error: bool,
    verbose: bool,
    max_time_ms: Option<u64>,
    family: Family,
}

enum Action {
    Run(Opts),
    Help,
    Version,
}

/// Long options: name, the short letter (or key) they stand for, and
/// whether they take a value.
const LONG: &[(&str, &str, bool)] = &[
    ("output", "o", true),
    ("remote-name", "O", false),
    ("location", "L", false),
    ("max-redirs", "max-redirs", true),
    ("include", "i", false),
    ("head", "I", false),
    ("dump-header", "D", true),
    ("request", "X", true),
    ("header", "H", true),
    ("data", "d", true),
    ("data-ascii", "d", true),
    ("data-raw", "data-raw", true),
    ("data-binary", "data-binary", true),
    ("data-urlencode", "data-urlencode", true),
    ("json", "json", true),
    ("get", "G", false),
    ("upload-file", "T", true),
    ("user", "u", true),
    ("user-agent", "A", true),
    ("referer", "e", true),
    ("cookie", "b", true),
    ("fail", "f", false),
    ("silent", "s", false),
    ("show-error", "S", false),
    ("verbose", "v", false),
    ("write-out", "w", true),
    ("max-time", "m", true),
    ("ipv4", "4", false),
    ("ipv6", "6", false),
    ("url", "url", true),
    ("help", "h", false),
    ("version", "V", false),
    ("insecure", "k", false),
    // Accepted and ignored: what they ask for is already the case here
    // (no compression offered, HTTP/1.1 only, output never buffered), or
    // only changes the look of the progress line.
    ("compressed", "-", false),
    ("http1.1", "-", false),
    ("no-buffer", "-", false),
    ("progress-bar", "-", false),
    ("no-progress-meter", "no-progress", false),
];
const SHORT_WITH_VALUE: &str = "oXHdTuAebwmD";
const SHORT_FLAGS: &str = "OLiIGfsSv46hVkN#";

fn usage(msg: impl Into<String>) -> Error {
    Error::new(E_USAGE, msg)
}

fn read_file(path: &str) -> Result<Vec<u8>, Error> {
    let fail = || Error::new(http::E_READ_FILE, alloc::format!("Failed to open/read local data from file {path}"));
    let mut f = CString::new(path).ok().and_then(|p| File::open(&p)).ok_or_else(fail)?;
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match f.read(&mut buf) {
            0 => return Ok(data),
            n => data.extend_from_slice(&buf[..n]),
        }
    }
}

/// Percent-encodes everything but unreserved characters.
fn urlencode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in data {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b);
        } else {
            out.extend_from_slice(alloc::format!("%{b:02X}").as_bytes());
        }
    }
    out
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

impl Opts {
    fn new() -> Opts {
        Opts {
            urls: Vec::new(),
            outputs: Vec::new(),
            follow: false,
            max_redirs: DEFAULT_MAX_REDIRS,
            include: false,
            head: false,
            method: None,
            headers: Vec::new(),
            data: Vec::new(),
            json: false,
            get: false,
            upload: None,
            user: None,
            agent: alloc::format!("curl/{VERSION}"),
            referer: None,
            cookie: None,
            write_out: None,
            dump_header: None,
            fail: false,
            silent: false,
            show_error: false,
            verbose: false,
            max_time_ms: None,
            family: Family::Any,
        }
    }

    /// Applies one option; `value` is there exactly when it takes one.
    fn apply(&mut self, key: &str, value: Option<&str>) -> Result<Option<Action>, Error> {
        let v = value.unwrap_or("");
        match key {
            "o" => self.outputs.push(Output::File(String::from(v))),
            "O" => self.outputs.push(Output::RemoteName),
            "L" => self.follow = true,
            "max-redirs" => self.max_redirs = v.parse().map_err(|_| usage("--max-redirs needs a number"))?,
            "i" => self.include = true,
            "I" => self.head = true,
            "D" => self.dump_header = Some(String::from(v)),
            "X" => self.method = Some(String::from(v)),
            "H" => self.headers.push(String::from(v)),
            "d" => self.data.push(match v.strip_prefix('@') {
                Some(path) => read_file(path)?.into_iter().filter(|&b| b != b'\r' && b != b'\n').collect(),
                None => v.as_bytes().to_vec(),
            }),
            "data-raw" => self.data.push(v.as_bytes().to_vec()),
            "data-binary" | "json" => {
                self.data.push(match v.strip_prefix('@') {
                    Some(path) => read_file(path)?,
                    None => v.as_bytes().to_vec(),
                });
                self.json |= key == "json";
            }
            "data-urlencode" => {
                let encoded = if let Some((name, content)) = v.split_once('=') {
                    let mut out = name.as_bytes().to_vec();
                    if !name.is_empty() {
                        out.push(b'=');
                    }
                    out.extend(urlencode(content.as_bytes()));
                    out
                } else if let Some((name, path)) = v.split_once('@') {
                    let mut out = name.as_bytes().to_vec();
                    if !name.is_empty() {
                        out.push(b'=');
                    }
                    out.extend(urlencode(&read_file(path)?));
                    out
                } else {
                    urlencode(v.as_bytes())
                };
                self.data.push(encoded);
            }
            "G" => self.get = true,
            "T" => self.upload = Some(String::from(v)),
            "u" => self.user = Some(String::from(v)),
            "A" => self.agent = String::from(v),
            "e" => self.referer = Some(String::from(v)),
            "b" => self.cookie = Some(String::from(v)),
            "f" => self.fail = true,
            "s" => self.silent = true,
            "S" => self.show_error = true,
            "v" => self.verbose = true,
            "w" => self.write_out = Some(String::from(v)),
            "m" => {
                let ms = parse_seconds_ms(v).ok_or_else(|| usage("-m needs a number of seconds"))?;
                self.max_time_ms = Some(ms);
            }
            "4" => self.family = Family::V4,
            "6" => self.family = Family::V6,
            "url" => self.urls.push(String::from(v)),
            "h" => return Ok(Some(Action::Help)),
            "V" => return Ok(Some(Action::Version)),
            "k" => return Err(usage("-k is not supported: certificates are always checked")),
            "no-progress" | "N" | "#" | "-" => {}
            _ => return Err(usage(alloc::format!("option {key}: is unknown"))),
        }
        Ok(None)
    }
}

/// "2", "0.5", "1.25" seconds as milliseconds.
fn parse_seconds_ms(s: &str) -> Option<u64> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let mut ms = 0;
    for (i, c) in frac.chars().take(3).enumerate() {
        ms += c.to_digit(10)? as u64 * [100, 10, 1][i];
    }
    Some(whole * 1000 + ms)
}

fn parse(args: &[&str]) -> Result<Action, Error> {
    let mut o = Opts::new();
    let mut i = 0;
    let mut only_urls = false;
    while i < args.len() {
        let arg = args[i];
        i += 1;
        if only_urls || !arg.starts_with('-') || arg == "-" {
            o.urls.push(String::from(arg));
            continue;
        }
        if arg == "--" {
            only_urls = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            let &(_, key, takes) = LONG
                .iter()
                .find(|(n, ..)| *n == name)
                .ok_or_else(|| usage(alloc::format!("option --{name}: is unknown")))?;
            let value = match (takes, inline) {
                (false, _) => None,
                (true, Some(v)) => Some(v),
                (true, None) => {
                    i += 1;
                    Some(*args.get(i - 1).ok_or_else(|| usage(alloc::format!("option --{name}: requires a value")))?)
                }
            };
            if let Some(action) = o.apply(key, value)? {
                return Ok(action);
            }
            continue;
        }
        // A run of short flags, the last of which may take a value: the
        // rest of this argument, or the next one.
        for (at, c) in arg.char_indices().skip(1) {
            let mut key = [0u8; 4];
            let key: &str = c.encode_utf8(&mut key);
            if SHORT_WITH_VALUE.contains(c) {
                let rest = &arg[at + c.len_utf8()..];
                let value = if !rest.is_empty() {
                    rest
                } else {
                    i += 1;
                    *args.get(i - 1).ok_or_else(|| usage(alloc::format!("option -{c}: requires a value")))?
                };
                if let Some(action) = o.apply(key, Some(value))? {
                    return Ok(action);
                }
                break;
            }
            if !SHORT_FLAGS.contains(c) {
                return Err(usage(alloc::format!("option -{c}: is unknown")));
            }
            if let Some(action) = o.apply(key, None)? {
                return Ok(action);
            }
        }
    }
    if o.urls.is_empty() {
        return Err(usage("no URL specified"));
    }
    Ok(Action::Run(o))
}

/// Where the body goes.
enum Sink {
    Stdout,
    File(File),
}

impl Sink {
    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        match self {
            Sink::Stdout => {
                anshi::write_all(1, data);
                Ok(())
            }
            Sink::File(f) => {
                if f.write_all(data) {
                    Ok(())
                } else {
                    Err(Error::new(http::E_WRITE, "Failure writing output to destination"))
                }
            }
        }
    }
}

/// What -w can print, filled in as the transfer goes.
#[derive(Default)]
struct Stats {
    status: u16,
    size_download: u64,
    size_upload: u64,
    size_header: u64,
    url_effective: String,
    content_type: String,
    time_ms: u64,
    remote_ip: String,
    remote_port: u16,
    num_redirects: usize,
    redirect_url: String,
}

fn write_out(format: &str, s: &Stats) -> String {
    let mut out = String::new();
    let mut rest = format;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        match c {
            '%' if rest.starts_with('%') => {
                out.push('%');
                rest = &rest[1..];
            }
            '%' if rest.starts_with('{') => {
                let Some(end) = rest.find('}') else {
                    out.push('%');
                    continue;
                };
                let name = &rest[1..end];
                rest = &rest[end + 1..];
                let value = match name {
                    "http_code" | "response_code" => alloc::format!("{:03}", s.status),
                    "size_download" => alloc::format!("{}", s.size_download),
                    "size_upload" => alloc::format!("{}", s.size_upload),
                    "size_header" => alloc::format!("{}", s.size_header),
                    "url_effective" => s.url_effective.clone(),
                    "content_type" => s.content_type.clone(),
                    "time_total" => alloc::format!("{}.{:06}", s.time_ms / 1000, s.time_ms % 1000 * 1000),
                    "remote_ip" => s.remote_ip.clone(),
                    "remote_port" => alloc::format!("{}", s.remote_port),
                    "num_redirects" => alloc::format!("{}", s.num_redirects),
                    "redirect_url" => s.redirect_url.clone(),
                    _ => alloc::format!("%{{{name}}}"),
                };
                out.push_str(&value);
            }
            '\\' if !rest.is_empty() => {
                let e = rest.chars().next().unwrap_or('\\');
                rest = &rest[e.len_utf8()..];
                match e {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '\\' => out.push('\\'),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return alloc::format!("{bytes} B");
    }
    let mut unit = 0;
    let mut tenths = bytes * 10 / 1024;
    while tenths >= 10240 && unit + 1 < UNITS.len() {
        tenths /= 1024;
        unit += 1;
    }
    alloc::format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
}

/// The one-line meter on stderr while a body goes into a file.
struct Progress {
    start: u64,
    last: u64,
    total: Option<u64>,
    shown: bool,
}

impl Progress {
    fn draw(&mut self, done: u64, force: bool) {
        let now = anshi::now_ms();
        if !force && now - self.last < PROGRESS_EVERY_MS {
            return;
        }
        self.last = now;
        self.shown = true;
        let rate = done * 1000 / (now - self.start).max(1);
        match self.total {
            Some(t) if t > 0 => {
                eprint!("\r  {} of {}  {:3}%  {}/s   ", human(done), human(t), done * 100 / t, human(rate))
            }
            _ => eprint!("\r  {}  {}/s   ", human(done), human(rate)),
        }
    }

    fn finish(&mut self, done: u64) {
        if self.shown {
            self.draw(done, true);
            eprintln!();
        }
    }
}

/// The request headers after Host: curl's defaults, then the -H lines on
/// top. `same_host` is false after a redirect to another host, where
/// credentials and cookies aren't sent on.
fn headers(o: &Opts, url: &Url, body: Option<&[u8]>, same_host: bool) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = Vec::new();
    let s = String::from;
    let user = o.user.clone().or_else(|| url.user.clone()).filter(|_| same_host);
    if let Some(user) = user {
        let user = if user.contains(':') { user } else { alloc::format!("{user}:") };
        h.push((s("Authorization"), alloc::format!("Basic {}", base64(user.as_bytes()))));
    }
    h.push((s("User-Agent"), o.agent.clone()));
    h.push((s("Accept"), s(if o.json { "application/json" } else { "*/*" })));
    if let Some(r) = &o.referer {
        h.push((s("Referer"), r.clone()));
    }
    if let Some(c) = o.cookie.as_ref().filter(|_| same_host) {
        h.push((s("Cookie"), c.clone()));
    }
    if let Some(body) = body {
        if o.json {
            h.push((s("Content-Type"), s("application/json")));
        } else if o.upload.is_none() {
            h.push((s("Content-Type"), s("application/x-www-form-urlencoded")));
        }
        h.push((s("Content-Length"), alloc::format!("{}", body.len())));
    }
    for line in &o.headers {
        // "Name: value" sets, "Name:" removes, "Name;" sends it empty.
        let (name, value, remove) = match line.split_once(':') {
            Some((n, v)) => (n.trim(), v.trim(), v.trim().is_empty()),
            None => match line.strip_suffix(';') {
                Some(n) => (n.trim(), "", false),
                None => continue,
            },
        };
        if !same_host && (name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("cookie")) {
            continue;
        }
        h.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        if !remove {
            h.push((s(name), s(value)));
        }
    }
    h
}

/// Writes one header block (for -i, -I and -D) the way it arrived.
fn head_bytes(resp: &Response) -> Vec<u8> {
    let mut out = resp.head.as_bytes().to_vec();
    out.extend_from_slice(b"\r\n\r\n");
    out
}

fn verbose_lines(prefix: char, text: &str) {
    for line in text.trim_end_matches(['\r', '\n']).split("\r\n") {
        eprintln!("{prefix} {line}");
    }
    eprintln!("{prefix}");
}

struct Transfer<'a> {
    o: &'a Opts,
    start: u64,
    stats: Stats,
    sink: Option<Sink>,
    output: Option<&'a Output>,
    /// The URL as given, which -O names the file after.
    first: Option<Url>,
    dumped: Vec<u8>,
}

impl Transfer<'_> {
    fn check_time(&self) -> Result<(), Error> {
        match self.o.max_time_ms {
            Some(max) if anshi::now_ms() - self.start > max => Err(Error::new(
                http::E_TIMEOUT,
                alloc::format!(
                    "Operation timed out after {} milliseconds with {} bytes received",
                    anshi::now_ms() - self.start,
                    self.stats.size_download
                ),
            )),
            _ => Ok(()),
        }
    }

    /// The sink, opened on first use: a file isn't created for a
    /// transfer that fails before there's anything to put in it.
    fn sink(&mut self) -> Result<&mut Sink, Error> {
        if self.sink.is_none() {
            let path = match self.output {
                None => None,
                Some(Output::File(p)) if p == "-" => None,
                Some(Output::File(p)) => Some(p.clone()),
                Some(Output::RemoteName) => {
                    let name = self.first.as_ref().map_or("", |u| u.file_name());
                    if name.is_empty() {
                        return Err(Error::new(http::E_WRITE, "Remote file name has no length"));
                    }
                    Some(String::from(name))
                }
            };
            self.sink = Some(match path {
                None => Sink::Stdout,
                Some(p) => {
                    let file = CString::new(p.as_str()).ok().and_then(|c| File::create(&c));
                    Sink::File(
                        file.ok_or_else(|| Error::new(http::E_WRITE, alloc::format!("Failed to open the file {p}")))?,
                    )
                }
            });
        }
        Ok(self.sink.as_mut().unwrap())
    }

    fn run(&mut self, raw_url: &str) -> Result<(), Error> {
        let o = self.o;
        let data = (!o.data.is_empty()).then(|| o.data.join(&b'&'));
        let mut url_text = String::from(raw_url);
        let mut body = None;
        if o.get {
            if let Some(d) = &data {
                url_text.push(if url_text.contains('?') { '&' } else { '?' });
                url_text.push_str(&String::from_utf8_lossy(d));
            }
        } else {
            body = data;
        }
        if let Some(path) = &o.upload {
            body = Some(read_file(path)?);
            if url_text.ends_with('/') {
                url_text.push_str(path.rsplit('/').next().unwrap_or(path));
            }
        }
        let mut url = Url::parse(&url_text)?;
        self.first = Some(url.clone());
        let mut method = o.method.clone().unwrap_or_else(|| {
            String::from(if o.head {
                "HEAD"
            } else if o.upload.is_some() {
                "PUT"
            } else if body.is_some() {
                "POST"
            } else {
                "GET"
            })
        });
        let first_host = url.host.clone();

        loop {
            self.check_time()?;
            self.stats.url_effective = url.full();
            let to = http::resolve(&url, o.family)?;
            if o.verbose {
                eprintln!("* Host {}:{} was resolved.", url.host, url.port);
                eprintln!("*   Trying {to}...");
            }
            let mut stream = http::connect(&url, &to)?;
            self.stats.remote_ip = alloc::format!("{}", to.ip);
            self.stats.remote_port = to.port;
            if o.verbose {
                eprintln!("* Connected to {} ({}) port {}", url.host, to.ip, to.port);
                if url.https {
                    eprintln!("* TLS handshake done, certificate verified for {}", url.host);
                }
            }

            let hs = headers(o, &url, body.as_deref(), url.host == first_host);
            let mut request = http::request_head(&method, &url, &hs).into_bytes();
            if o.verbose {
                verbose_lines('>', &String::from_utf8_lossy(&request));
            }
            if let Some(b) = &body {
                request.extend_from_slice(b);
                self.stats.size_upload = b.len() as u64;
            }
            stream.write_all(&request)?;

            let mut resp = Response::read(stream, method.eq_ignore_ascii_case("HEAD"))?;
            self.stats.status = resp.status;
            self.stats.size_header += resp.head.len() as u64 + 4;
            self.stats.content_type = String::from(resp.header("content-type").unwrap_or(""));
            if o.verbose {
                verbose_lines('<', &resp.head);
            }
            if o.dump_header.is_some() {
                self.dumped.extend(head_bytes(&resp));
            }

            let location = resp.header("location").filter(|_| resp.is_redirect()).map(String::from);
            if let Some(loc) = &location {
                let next = url.join(loc)?;
                if !o.follow {
                    self.stats.redirect_url = next.full();
                } else {
                    if o.include || o.head {
                        let h = head_bytes(&resp);
                        self.sink()?.write(&h)?;
                    }
                    self.stats.num_redirects += 1;
                    if self.stats.num_redirects > o.max_redirs {
                        return Err(Error::new(
                            http::E_REDIRECTS,
                            alloc::format!("Maximum ({}) redirects followed", o.max_redirs),
                        ));
                    }
                    // 303, and a POST answered with 301 or 302, continue as a
                    // GET without the body -- what browsers and curl do.
                    let to_get = resp.status == 303 && !method.eq_ignore_ascii_case("HEAD")
                        || matches!(resp.status, 301 | 302) && method == "POST" && o.method.is_none();
                    if to_get {
                        method = String::from("GET");
                        body = None;
                    }
                    if o.verbose {
                        eprintln!("* Issue another request to this URL: '{}'", next.full());
                    }
                    url = next;
                    continue;
                }
            }

            if o.fail && resp.status >= 400 {
                return Err(Error::new(
                    http::E_HTTP,
                    alloc::format!("The requested URL returned error: {}", resp.status),
                ));
            }
            if o.include || o.head {
                let h = head_bytes(&resp);
                self.sink()?.write(&h)?;
            }
            self.sink()?;
            return self.body(&mut resp);
        }
    }

    /// Streams the body into the sink.
    fn body(&mut self, resp: &mut Response) -> Result<(), Error> {
        let to_file = matches!(self.sink, Some(Sink::File(_)));
        // First drawn a moment in, so a quick transfer doesn't flash one.
        let now = anshi::now_ms();
        let mut progress = Progress { start: now, last: now, total: resp.length(), shown: false };
        let meter = to_file && !self.o.silent && !self.o.verbose;
        let mut buf = alloc::vec![0u8; CHUNK];
        let result = loop {
            if crate::interrupted() {
                break Err(Error::new(E_INTERRUPTED, "interrupted"));
            }
            if let Err(e) = self.check_time() {
                break Err(e);
            }
            let n = match resp.read_body(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => n,
                Err(e) => break Err(e),
            };
            if let Err(e) = self.sink.as_mut().unwrap().write(&buf[..n]) {
                break Err(e);
            }
            self.stats.size_download += n as u64;
            if meter {
                progress.draw(self.stats.size_download, false);
            }
        };
        if meter {
            progress.finish(self.stats.size_download);
        }
        if self.o.verbose {
            eprintln!("* Closing connection");
        }
        result
    }
}

fn report(o: Option<&Opts>, e: &Error) {
    if o.is_none_or(|o| !o.silent || o.show_error) {
        eprintln!("curl: ({}) {e}", e.code);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn curl_main(argc: c_int, argv: *const *const c_char) -> c_int {
    let args = crate::args(argc, argv);
    let o = match parse(&args) {
        Ok(Action::Run(o)) => o,
        Ok(Action::Help) => {
            print!("{HELP}\n");
            return 0;
        }
        Ok(Action::Version) => {
            print!("curl {VERSION} (x86_64-anshi) rustls\nProtocols: http https\nFeatures: IPv6 SSL\n");
            return 0;
        }
        Err(e) => {
            report(None, &e);
            eprintln!("curl: try 'curl --help' for more information");
            return e.code;
        }
    };

    let mut code = 0;
    for (i, url) in o.urls.iter().enumerate() {
        let mut t = Transfer {
            o: &o,
            start: anshi::now_ms(),
            stats: Stats::default(),
            sink: None,
            output: o.outputs.get(i),
            first: None,
            dumped: Vec::new(),
        };
        let result = t.run(url);
        t.stats.time_ms = anshi::now_ms() - t.start;
        if let Some(path) = &o.dump_header {
            let saved = if path == "-" {
                anshi::write_all(1, &t.dumped);
                true
            } else {
                CString::new(path.as_str())
                    .ok()
                    .and_then(|p| File::create(&p))
                    .is_some_and(|mut f| f.write_all(&t.dumped))
            };
            if !saved {
                report(Some(&o), &Error::new(http::E_WRITE, alloc::format!("Failed to write the headers to {path}")));
            }
        }
        if let Err(e) = &result {
            report(Some(&o), e);
            code = e.code;
        }
        if let Some(format) = &o.write_out {
            print!("{}", write_out(format, &t.stats));
        }
    }
    code
}
