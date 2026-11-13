//! `net` -- the network settings screen. Shows every interface the
//! kernel found (link, IPv4 and IPv6 addresses, gateways, DNS, which one
//! carries traffic to other networks, traffic counts), and for the
//! selected one switches between DHCP and a static IPv4 address, asks the
//! DHCP server again, or runs a connection test. Settings are saved to
//! /etc/net.conf, a section per interface, which the kernel reads at boot
//! (kernel/src/net/mod.rs), and flushed to disk. IPv6 configures itself
//! (SLAAC) and has nothing to set.
//!
//! The kernel does the connecting by itself; this only shows it and
//! changes it. Refreshes twice a second, so a lease arriving or a cable
//! coming out shows up while it's open.

#![no_std]

extern crate alloc;

mod ui;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};

use anshi::KeyPoll;
use anshi::net::{
    self, Family, IpAddr, Ipv4, Ipv6, NET_MODE_DHCP, NET_MODE_RENEW, NET_MODE_STATIC, NET_STATE_CONNECTED, NetConfig,
    NetInfo, SocketAddr,
};

const REFRESH_MS: u64 = 500;
/// How long to wait for the rest of an escape sequence (an arrow key)
/// before taking a lone ESC as the Esc key.
const ESC_WAIT_MS: u64 = 40;
const CONF_DIR: &str = "/etc";
const CONF_PATH: &core::ffi::CStr = c"/etc/net.conf";
const TEST_HOST: &str = "example.com";
const TEST_TIMEOUT_MS: u64 = 3000;
const FIELD_MAX: usize = 18;

pub enum Key {
    Char(u8),
    Up,
    Down,
    Tab,
    BackTab,
    Enter,
    Esc,
    Backspace,
    Quit,
}

fn wait_byte(ms: u64) -> Option<u8> {
    let deadline = anshi::now_ms() + ms;
    loop {
        if let KeyPoll::Key(b) = anshi::poll_key() {
            return Some(b);
        }
        if anshi::now_ms() >= deadline {
            return None;
        }
        anshi::sched_yield();
    }
}

/// The next key, without waiting for one.
fn read_key() -> Option<Key> {
    let b = match anshi::poll_key() {
        KeyPoll::Empty => return None,
        KeyPoll::Closed => return Some(Key::Quit),
        KeyPoll::Key(b) => b,
    };
    Some(match b {
        0x1b => match wait_byte(ESC_WAIT_MS) {
            Some(b'[') => match wait_byte(ESC_WAIT_MS) {
                Some(b'A') => Key::Up,
                Some(b'B') => Key::Down,
                Some(b'Z') => Key::BackTab,
                _ => return None,
            },
            _ => Key::Esc,
        },
        b'\r' | b'\n' => Key::Enter,
        b'\t' => Key::Tab,
        0x7f | 0x08 => Key::Backspace,
        3 => Key::Quit,
        b => Key::Char(b),
    })
}

/// The static-address form: address/prefix, gateway, DNS.
pub struct Form {
    pub fields: [String; 3],
    pub focus: usize,
}

pub const FIELD_LABELS: [&str; 3] = ["Address", "Gateway", "DNS"];

pub enum Check {
    Waiting,
    Running,
    Pass(String),
    Fail(String),
}

pub struct Step {
    pub label: String,
    pub check: Check,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Info,
    Good,
    Bad,
}

/// The connection test's steps.
#[derive(Clone, Copy)]
enum Probe {
    Gateway4,
    Gateway6,
    Dns,
    Tcp,
}

pub struct App {
    pub ifaces: Vec<NetInfo>,
    pub selected: usize,
    pub form: Option<Form>,
    pub test: Vec<Step>,
    probes: Vec<Probe>,
    /// What the test's DNS step found, for its TCP step.
    test_ip: Option<IpAddr>,
    pub message: Option<(String, Tone)>,
}

impl App {
    fn refresh(&mut self) {
        self.ifaces = net::interfaces();
        self.selected = self.selected.min(self.ifaces.len().saturating_sub(1));
    }

    pub fn current(&self) -> Option<&NetInfo> {
        self.ifaces.get(self.selected)
    }

    fn name(&self) -> String {
        self.current().map_or_else(String::new, |i| net::field(&i.name))
    }

    fn say(&mut self, text: impl Into<String>, tone: Tone) {
        self.message = Some((text.into(), tone));
    }

    /// False to quit.
    fn handle(&mut self, key: Key) -> bool {
        if self.form.is_some() {
            self.handle_form(key);
            return true;
        }
        match key {
            Key::Quit | Key::Esc | Key::Char(b'q') => return false,
            Key::Down | Key::Char(b'j') => {
                self.selected = (self.selected + 1).min(self.ifaces.len().saturating_sub(1));
                self.test.clear();
            }
            Key::Up | Key::Char(b'k') => {
                self.selected = self.selected.saturating_sub(1);
                self.test.clear();
            }
            Key::Char(c @ (b'r' | b'd' | b's' | b'p')) if !self.ifaces.is_empty() => match c {
                b'r' => self.renew(),
                b'd' => self.use_dhcp(),
                b's' => self.open_form(),
                _ => self.start_test(),
            },
            _ => {}
        }
        true
    }

    fn renew(&mut self) {
        let name = self.name();
        if self.current().is_some_and(|i| i.mode == NET_MODE_STATIC) {
            self.say(format!("{name} has a static address -- press d to switch to DHCP"), Tone::Bad);
            return;
        }
        let ok = net::configure(self.selected as u32, &config(NET_MODE_RENEW, None, None, None));
        self.say(
            if ok { format!("{name}: asking the DHCP server again") } else { String::from("renew failed") },
            if ok { Tone::Info } else { Tone::Bad },
        );
        self.refresh();
    }

    fn use_dhcp(&mut self) {
        let name = self.name();
        if !net::configure(self.selected as u32, &config(NET_MODE_DHCP, None, None, None)) {
            self.say("could not switch to DHCP", Tone::Bad);
            return;
        }
        let saved = save(self.selected, "mode=dhcp\n");
        self.say(format!("{name} now uses DHCP{saved}"), Tone::Good);
        self.refresh();
    }

    fn open_form(&mut self) {
        let Some(i) = self.current() else { return };
        let show = |b: [u8; 4]| if b == [0; 4] { String::new() } else { format!("{}", Ipv4(b)) };
        let address =
            if i.address != [0; 4] { format!("{}/{}", Ipv4(i.address), i.prefix) } else { String::from("/24") };
        self.form = Some(Form { fields: [address, show(i.gateway), show(i.dns)], focus: 0 });
        self.message = None;
    }

    fn handle_form(&mut self, key: Key) {
        let Some(form) = self.form.as_mut() else { return };
        match key {
            Key::Esc | Key::Quit => {
                self.form = None;
                self.say("cancelled", Tone::Info);
            }
            Key::Tab | Key::Down => form.focus = (form.focus + 1) % 3,
            Key::BackTab | Key::Up => form.focus = (form.focus + 2) % 3,
            Key::Backspace => {
                form.fields[form.focus].pop();
            }
            Key::Char(c)
                if (c.is_ascii_digit() || c == b'.' || c == b'/') && form.fields[form.focus].len() < FIELD_MAX =>
            {
                form.fields[form.focus].push(c as char)
            }
            Key::Enter => self.apply_form(),
            _ => {}
        }
    }

    fn apply_form(&mut self) {
        let Some(form) = self.form.as_ref() else { return };
        let (ip, prefix) = match form.fields[0].split_once('/') {
            Some((ip, p)) => (ip, p.parse::<u8>().ok().filter(|&p| (1..=32).contains(&p))),
            None => (form.fields[0].as_str(), Some(24)),
        };
        let Some(address) = Ipv4::parse(ip).filter(|a| !a.is_zero()) else {
            return self.say("address must look like 192.168.1.20/24", Tone::Bad);
        };
        let Some(prefix) = prefix else { return self.say("prefix must be 1-32, as in /24", Tone::Bad) };
        let optional = |s: &str| if s.trim().is_empty() { Some(None) } else { Ipv4::parse(s).map(Some) };
        let Some(gateway) = optional(&form.fields[1]) else { return self.say("gateway is not an address", Tone::Bad) };
        let Some(dns) = optional(&form.fields[2]) else { return self.say("DNS is not an address", Tone::Bad) };

        let mut c = config(NET_MODE_STATIC, Some(address), gateway, dns);
        c.prefix = prefix;
        if !net::configure(self.selected as u32, &c) {
            return self.say("the kernel refused those settings", Tone::Bad);
        }
        let mut text = format!("mode=static\naddress={address}/{prefix}\n");
        if let Some(g) = gateway {
            text.push_str(&format!("gateway={g}\n"));
        }
        if let Some(d) = dns {
            text.push_str(&format!("dns={d}\n"));
        }
        let saved = save(self.selected, &text);
        let name = self.name();
        self.form = None;
        self.say(format!("{name} is {address}/{prefix}{saved}"), Tone::Good);
        self.refresh();
    }

    fn start_test(&mut self) {
        let Some(i) = self.current() else { return };
        let mut probes = Vec::new();
        if i.gateway != [0; 4] {
            probes.push((Probe::Gateway4, format!("gateway {} answers ping", Ipv4(i.gateway))));
        }
        if i.v6_gateway != [0; 16] {
            probes.push((Probe::Gateway6, format!("IPv6 router {} answers ping", Ipv6(i.v6_gateway))));
        }
        probes.push((Probe::Dns, format!("DNS finds {TEST_HOST}")));
        probes.push((Probe::Tcp, format!("HTTP port of {TEST_HOST} opens")));
        self.test = probes.iter().map(|(_, label)| Step { label: label.clone(), check: Check::Waiting }).collect();
        self.probes = probes.into_iter().map(|(p, _)| p).collect();
        self.test_ip = None;
        self.message = None;
    }

    /// The test step that should run next, marked Running.
    fn next_step(&mut self) -> Option<usize> {
        let i = self.test.iter().position(|s| matches!(s.check, Check::Waiting))?;
        self.test[i].check = Check::Running;
        Some(i)
    }

    fn run_step(&mut self, step: usize) {
        let Some(info) = self.current().copied() else { return };
        let result = match self.probes[step] {
            _ if info.state != NET_STATE_CONNECTED => Err(String::from("not connected")),
            Probe::Gateway4 => ping_once(IpAddr::V4(Ipv4(info.gateway)), 0),
            // A router's address is usually link-local: say which
            // interface it's on.
            Probe::Gateway6 => ping_once(IpAddr::V6(Ipv6(info.v6_gateway)), self.selected as u32 + 1),
            Probe::Dns => match net::resolve(TEST_HOST, Family::Any, TEST_TIMEOUT_MS) {
                Ok((ip, _)) => {
                    self.test_ip = Some(ip);
                    Ok(format!("{ip}"))
                }
                Err(e) => Err(format!("{e}")),
            },
            Probe::Tcp => match self.test_ip {
                None => Err(String::from("skipped, no address")),
                Some(ip) => {
                    let start = anshi::now_ms();
                    net::tcp_connect(&SocketAddr::new(ip, 80), TEST_TIMEOUT_MS)
                        .map(|_| format!("{} ms", anshi::now_ms() - start))
                        .map_err(|e| format!("{e}"))
                }
            },
        };
        self.test[step].check = match result {
            Ok(s) => Check::Pass(s),
            Err(s) => Check::Fail(s),
        };
    }
}

fn config(mode: u8, address: Option<Ipv4>, gateway: Option<Ipv4>, dns: Option<Ipv4>) -> NetConfig {
    let b = |a: Option<Ipv4>| a.map_or([0; 4], |a| a.0);
    NetConfig { mode, prefix: 0, reserved: [0; 2], address: b(address), gateway: b(gateway), dns: b(dns) }
}

/// /etc/net.conf as (interface index, its lines) sections, in file order.
/// Lines before any `[ethN]` header are eth0's.
fn read_conf() -> Vec<(usize, Vec<String>)> {
    let mut sections = alloc::vec![(0usize, Vec::new())];
    let Some(mut f) = anshi::File::open(CONF_PATH) else { return sections };
    let mut raw = alloc::vec![0u8; 4096];
    let n = f.read_full(&mut raw);
    for line in String::from_utf8_lossy(&raw[..n]).lines().map(str::trim).filter(|l| !l.is_empty()) {
        match line.strip_prefix("[eth").and_then(|r| r.strip_suffix(']')).and_then(|n| n.parse().ok()) {
            Some(index) => sections.push((index, Vec::new())),
            None => sections.last_mut().unwrap().1.push(String::from(line)),
        }
    }
    sections
}

/// Replaces interface `index`'s section of /etc/net.conf with `body`,
/// keeping the others, and flushes it to disk. What to add to the
/// confirmation: ", saved", or why it won't survive a reboot.
fn save(index: usize, body: &str) -> &'static str {
    if !anshi::is_dir(CONF_DIR) && !anshi::mkdir(CONF_DIR) {
        return " (could not create /etc, not saved)";
    }
    let mut sections: Vec<(usize, Vec<String>)> =
        read_conf().into_iter().filter(|(i, l)| *i != index && !l.is_empty()).collect();
    sections.push((index, body.lines().map(String::from).collect()));
    sections.sort_by_key(|(i, _)| *i);
    let mut text = String::new();
    for (i, lines) in sections {
        text.push_str(&format!("[eth{i}]\n"));
        for l in lines {
            text.push_str(&l);
            text.push('\n');
        }
    }
    let written = anshi::File::create(CONF_PATH).is_some_and(|mut f| f.write_all(text.as_bytes()));
    if !written {
        " (could not write /etc/net.conf, not saved)"
    } else if !anshi::sync() {
        " (no disk: kept until reboot only)"
    } else {
        ", saved"
    }
}

/// One echo request (ICMP or ICMPv6); the round trip.
fn ping_once(ip: IpAddr, scope: u32) -> Result<String, String> {
    let mut sock = net::icmp_connect(ip, scope).map_err(|e| format!("{e}"))?;
    let (request, reply) = if ip.is_v6() { (128, 129) } else { (8, 0) };
    let mut packet = [0u8; 16];
    packet[0] = request;
    let start = anshi::now_ms();
    sock.write_all(&packet, TEST_TIMEOUT_MS).map_err(|e| format!("{e}"))?;
    let mut buf = [0u8; 64];
    while anshi::now_ms() - start < TEST_TIMEOUT_MS {
        match sock.try_read(&mut buf) {
            Ok(Some(n)) if n >= 8 && buf[0] == reply => return Ok(format!("{} ms", anshi::now_ms() - start)),
            Ok(_) => anshi::sched_yield(),
            Err(e) => return Err(format!("{e}")),
        }
    }
    Err(String::from("no reply"))
}

#[unsafe(no_mangle)]
pub extern "C" fn main(_argc: c_int, _argv: *const *const c_char) -> c_int {
    let Some(raw_mode) = anshi::RawMode::enable() else {
        anshi::write_all(1, b"net: cannot put the terminal in raw mode\n");
        return 1;
    };
    let mut terminal = anshi_tui::init();
    let mut app = App {
        ifaces: Vec::new(),
        selected: 0,
        form: None,
        test: Vec::new(),
        probes: Vec::new(),
        test_ip: None,
        message: None,
    };
    app.refresh();

    let mut last_refresh = anshi::now_ms();
    let mut dirty = true;
    loop {
        if dirty {
            let _ = terminal.draw(|frame| ui::render(frame, &app));
            dirty = false;
        }
        // A test step runs between frames, so each one shows as running
        // before it starts and its result as soon as it's done.
        if let Some(i) = app.next_step() {
            let _ = terminal.draw(|frame| ui::render(frame, &app));
            app.run_step(i);
            dirty = true;
            continue;
        }
        match read_key() {
            Some(key) => {
                if !app.handle(key) {
                    break;
                }
                dirty = true;
            }
            None if anshi::now_ms() - last_refresh >= REFRESH_MS => {
                app.refresh();
                last_refresh = anshi::now_ms();
                dirty = true;
            }
            None => anshi::sched_yield(),
        }
    }

    anshi_tui::restore();
    drop(raw_mode);
    0
}
