//! The net screen, in the shared look (anshi_tui::chrome): a "net" tab
//! and a rule with the selected interface's state at its end, keycaps
//! along the bottom. The accent follows that state -- green connected,
//! yellow waiting for an address, red with no link.
//!
//! The interface list runs down the left, the selected one's details on
//! the right; under them, the static-address form or the connection
//! test while one is open. A narrow terminal (a tile pane) drops the
//! list and shows the details alone.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use anshi::net::{
    Ipv4, Ipv6, NET_MODE_STATIC, NET_PRIMARY_DNS, NET_PRIMARY_V4, NET_PRIMARY_V6, NET_STATE_ACQUIRING,
    NET_STATE_CONNECTED, NetInfo, field,
};
use anshi_tui::chrome::{self, GREY, ON_ACCENT, fg};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::{App, Check, FIELD_LABELS, Tone};

const LIST_WIDTH: u16 = 18;
const LABEL_WIDTH: u16 = 10;
/// Below this many columns the interface list is left out.
const NARROW: u16 = 56;

fn state_color(i: &NetInfo) -> Color {
    match i.state {
        NET_STATE_CONNECTED => Color::Green,
        NET_STATE_ACQUIRING => Color::Yellow,
        _ => Color::Red,
    }
}

fn state_text(i: &NetInfo) -> String {
    let how = if i.mode == NET_MODE_STATIC { "static" } else { "DHCP" };
    match i.state {
        NET_STATE_CONNECTED => format!("Connected ({how})"),
        NET_STATE_ACQUIRING if i.mode == NET_MODE_STATIC => String::from("Configuring"),
        NET_STATE_ACQUIRING => String::from("Waiting for an address"),
        _ => String::from("No link (cable out?)"),
    }
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{}.{} MB", n >> 20, (n & ((1 << 20) - 1)) * 10 >> 20),
        n if n >= 1 << 10 => format!("{} KB", n >> 10),
        n => format!("{n} B"),
    }
}

fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        s if s >= 3600 => format!("{}h {:02}m", s / 3600, s / 60 % 60),
        s if s >= 60 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

fn mac(m: [u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

fn ip_or_dash(b: [u8; 4]) -> String {
    if b == [0; 4] { String::from("-") } else { format!("{}", Ipv4(b)) }
}

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    if area.width < 30 || area.height < 8 {
        buf.set_stringn(0, 0, "net: terminal too small", area.width as usize, Style::new());
        return;
    }
    header(buf, area, app);
    footer(buf, area, app);

    let Some(info) = app.current() else {
        let y = area.y + area.height / 2 - 1;
        let x = area.x + 2;
        chrome::put(buf, x, y, area.right(), "No network device", fg(Color::White).add_modifier(Modifier::BOLD));
        chrome::put(
            buf,
            x,
            y + 2,
            area.right(),
            "Under QEMU: boot with -device virtio-net-pci,disable-legacy=on",
            fg(Color::Gray),
        );
        return;
    };

    let top = area.y + 2;
    let bottom = area.bottom().saturating_sub(3); // above the message and footer rows
    let mut x = area.x + 2;
    if area.width >= NARROW {
        list(buf, Rect { x, y: top, width: LIST_WIDTH, height: bottom.saturating_sub(top) }, app);
        for y in top..bottom {
            chrome::put(buf, x + LIST_WIDTH, y, area.right(), "│", fg(GREY));
        }
        x += LIST_WIDTH + 3;
    }
    let right = area.right().saturating_sub(2);
    let mut y = details(buf, x, top, right, info);
    y += 1;
    if let Some(form) = &app.form {
        y = form_rows(buf, x, y, right, form, &field(&info.name));
    } else if !app.test.is_empty() {
        y = test_rows(buf, x, y, right, app);
    }
    let _ = y;

    if let Some((text, tone)) = &app.message {
        let color = match tone {
            Tone::Info => Color::Gray,
            Tone::Good => Color::Green,
            Tone::Bad => Color::Red,
        };
        chrome::put(buf, area.x + 2, area.bottom() - 2, area.right(), text, fg(color));
    }
}

/// The tab, a rule, and "eth0 connected" at the rule's end.
fn header(buf: &mut Buffer, area: Rect, app: &App) {
    let (status, color) = match app.current() {
        Some(i) => {
            let word = match i.state {
                NET_STATE_CONNECTED => "connected",
                NET_STATE_ACQUIRING => "connecting",
                _ => "no link",
            };
            (format!(" {} {word}", field(&i.name)), state_color(i))
        }
        None => (String::from(" offline"), Color::Red),
    };
    let status_x = area.right().saturating_sub(status.len() as u16 + 1);
    let x = chrome::tab(buf, area.x + 1, area.y, area.right(), "net", color, ON_ACCENT);
    chrome::rule(buf, x + 1, area.y, status_x);
    if status_x > x + 1 {
        chrome::put(buf, status_x, area.y, area.right(), &status, fg(Color::Gray));
    }
}

fn footer(buf: &mut Buffer, area: Rect, app: &App) {
    let row = Rect { y: area.bottom() - 1, height: 1, ..area };
    if app.form.is_some() {
        chrome::key_hints(buf, row, &[("TAB", "next field"), ("ENTER", "apply"), ("ESC", "cancel")]);
    } else if app.ifaces.is_empty() {
        chrome::key_hints(buf, row, &[("Q", "quit")]);
    } else {
        let keys = [("J/K", "select"), ("R", "renew"), ("S", "static"), ("D", "dhcp"), ("P", "test"), ("Q", "quit")];
        chrome::key_hints(buf, row, &keys);
    }
}

fn list(buf: &mut Buffer, area: Rect, app: &App) {
    chrome::put(buf, area.x, area.y, area.right(), "Interfaces", fg(Color::Gray));
    for (n, i) in app.ifaces.iter().enumerate() {
        let y = area.y + 2 + n as u16;
        if y >= area.bottom() {
            break;
        }
        let x = chrome::put(buf, area.x, y, area.right(), "● ", fg(state_color(i)));
        let style = if n == app.selected {
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            fg(Color::Gray)
        };
        chrome::put(buf, x, y, area.right(), &format!(" {} ", field(&i.name)), style);
    }
}

fn row(buf: &mut Buffer, x: u16, y: u16, right: u16, label: &str, value: &str, style: Style) {
    chrome::put(buf, x, y, right, label, fg(Color::Gray));
    chrome::put(buf, x + LABEL_WIDTH, y, right, value, style);
}

/// What traffic the interface carries for the whole machine.
fn roles(i: &NetInfo) -> String {
    let mut out = Vec::new();
    match (i.primary & NET_PRIMARY_V4 != 0, i.primary & NET_PRIMARY_V6 != 0) {
        (true, true) => out.push("default route (IPv4, IPv6)"),
        (true, false) => out.push("default route (IPv4)"),
        (false, true) => out.push("default route (IPv6)"),
        _ => {}
    }
    if i.primary & NET_PRIMARY_DNS != 0 {
        out.push("DNS");
    }
    if out.is_empty() { String::from("own networks only") } else { out.join(", ") }
}

/// The selected interface; the row after the last one drawn.
fn details(buf: &mut Buffer, x: u16, mut y: u16, right: u16, i: &NetInfo) -> u16 {
    let nx = chrome::put(buf, x, y, right, &field(&i.name), fg(Color::White).add_modifier(Modifier::BOLD));
    chrome::put(buf, nx + 2, y, right, &field(&i.model), fg(Color::Gray));
    y += 2;
    let white = fg(Color::White);
    row(buf, x, y, right, "State", &state_text(i), fg(state_color(i)));
    y += 1;
    let address = if i.address != [0; 4] { format!("{}/{}", Ipv4(i.address), i.prefix) } else { String::from("-") };
    row(buf, x, y, right, "Address", &address, white);
    row(buf, x, y + 1, right, "Gateway", &ip_or_dash(i.gateway), white);
    row(buf, x, y + 2, right, "DNS", &ip_or_dash(i.dns), white);
    y += 3;
    for k in 0..i.v6_count as usize {
        let a = Ipv6(i.v6[k]);
        let label = if k == 0 { "IPv6" } else { "" };
        let note = if a.is_link_local() { "  link-local" } else { "" };
        let style = if a.is_link_local() { fg(Color::Gray) } else { white };
        let x2 = chrome::put(buf, x + LABEL_WIDTH, y, right, &format!("{a}/{}", i.v6_prefix[k]), style);
        chrome::put(buf, x, y, right, label, fg(Color::Gray));
        chrome::put(buf, x2, y, right, note, fg(GREY));
        y += 1;
    }
    if i.v6_gateway != [0; 16] {
        row(buf, x, y, right, "Router", &format!("{}", Ipv6(i.v6_gateway)), white);
        y += 1;
    }
    row(buf, x, y, right, "MAC", &mac(i.mac), white);
    row(buf, x, y + 1, right, "Role", &roles(i), white);
    y += 2;
    if i.address != [0; 4] {
        row(buf, x, y, right, "Up for", &duration(i.connected_ms), white);
        y += 1;
    }
    let traffic = format!("rx {}   tx {}", bytes(i.rx_bytes), bytes(i.tx_bytes));
    row(buf, x, y, right, "Traffic", &traffic, white);
    y + 1
}

fn form_rows(buf: &mut Buffer, x: u16, mut y: u16, right: u16, form: &crate::Form, name: &str) -> u16 {
    chrome::rule(buf, x, y, right);
    y += 1;
    let title = format!("Static IPv4 address for {name}");
    chrome::put(buf, x, y, right, &title, fg(Color::White).add_modifier(Modifier::BOLD));
    y += 2;
    for (n, label) in FIELD_LABELS.iter().enumerate() {
        let focused = n == form.focus;
        chrome::put(buf, x, y, right, label, fg(if focused { Color::Blue } else { Color::Gray }));
        // The box starts a column early so its text lines up with the
        // values above.
        let box_x = x + LABEL_WIDTH - 1;
        let width = crate::FIELD_MAX as u16 + 2;
        let style = if focused {
            Style::new().fg(Color::White).bg(GREY)
        } else {
            Style::new().fg(Color::White).bg(Color::Black)
        };
        chrome::fill(buf, Rect { x: box_x, y, width: width.min(right.saturating_sub(box_x)), height: 1 }, style);
        let end = chrome::put(buf, box_x + 1, y, right, &form.fields[n], style);
        if focused {
            chrome::put(buf, end, y, right, " ", Style::new().bg(Color::White));
        }
        y += 1;
    }
    y += 1;
    chrome::put(buf, x, y, right, "Gateway and DNS may be left empty.", fg(GREY));
    y + 1
}

fn test_rows(buf: &mut Buffer, x: u16, mut y: u16, right: u16, app: &App) -> u16 {
    chrome::rule(buf, x, y, right);
    y += 1;
    chrome::put(buf, x, y, right, "Connection test", fg(Color::White).add_modifier(Modifier::BOLD));
    y += 2;
    for step in &app.test {
        let (mark, color, note) = match &step.check {
            Check::Waiting => ("·", GREY, ""),
            Check::Running => ("…", Color::Yellow, "checking"),
            Check::Pass(s) => ("✔", Color::Green, s.as_str()),
            Check::Fail(s) => ("✘", Color::Red, s.as_str()),
        };
        let mut nx = chrome::put(buf, x, y, right, mark, fg(color));
        nx = chrome::put(buf, nx + 1, y, right, &step.label, fg(Color::White));
        chrome::put(
            buf,
            nx + 2,
            y,
            right,
            note,
            fg(if matches!(step.check, Check::Fail(_)) { Color::Red } else { Color::Gray }),
        );
        y += 1;
    }
    y
}
