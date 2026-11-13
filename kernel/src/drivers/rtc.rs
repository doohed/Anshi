//! The CMOS real-time clock, read once at boot: what wall-clock time
//! (CLOCK_REALTIME) is counted from, the uptime timer carrying it on.
//! Certificate checks in TLS need it. Taken as UTC, which is what QEMU
//! and Linux keep it in; a PC whose clock was set by Windows keeps local
//! time instead, and is off by the time zone.

use crate::arch::x86_64::io::{inb, outb};
use crate::drivers::timer;
use crate::sync::Global;

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;
const STATUS_A_UPDATING: u8 = 0x80;
const STATUS_B_24H: u8 = 0x02;
const STATUS_B_BINARY: u8 = 0x04;

/// Unix time in milliseconds when uptime_ms() was 0; 0 when unknown.
static BASE_MS: Global<u64> = Global::new(0);

fn read(reg: u8) -> u8 {
    // Bit 7 of the index port is NMI-disable; leave NMIs enabled.
    outb(INDEX, reg & 0x7f);
    inb(DATA)
}

type Raw = [u8; 7];

fn read_raw() -> Raw {
    let mut spins = 0;
    while read(0x0a) & STATUS_A_UPDATING != 0 && spins < 1_000_000 {
        spins += 1;
        core::hint::spin_loop();
    }
    [read(0x00), read(0x02), read(0x04), read(0x07), read(0x08), read(0x09), read(0x32)]
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn init() {
    // Read until two reads agree, so no field rolled over in between.
    let mut raw = read_raw();
    for _ in 0..5 {
        let again = read_raw();
        if again == raw {
            break;
        }
        raw = again;
    }
    let b = read(0x0b);
    let bcd = |v: u8| if b & STATUS_B_BINARY != 0 { v } else { (v & 0x0f) + (v >> 4) * 10 };
    let [sec, min, hour_raw, day, month, year, century] = raw;
    let pm = b & STATUS_B_24H == 0 && hour_raw & 0x80 != 0;
    let mut hour = bcd(hour_raw & 0x7f);
    if b & STATUS_B_24H == 0 {
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    let century = bcd(century);
    let full_year =
        if (19..=21).contains(&century) { century as i64 * 100 + bcd(year) as i64 } else { 2000 + bcd(year) as i64 };
    let (month, day) = (bcd(month) as u32, bcd(day) as u32);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || full_year < 2000 {
        kprintln!("rtc: clock unreadable or unset -- wall-clock time unknown");
        return;
    }
    let secs =
        days_from_civil(full_year, month, day) * 86_400 + hour as i64 * 3600 + bcd(min) as i64 * 60 + bcd(sec) as i64;
    unsafe { *BASE_MS.get() = (secs as u64 * 1000).saturating_sub(timer::uptime_ms()) };
    kprintln!("rtc: {full_year:04}-{month:02}-{day:02} {hour:02}:{:02}:{:02} UTC", bcd(min), bcd(sec));
}

/// Milliseconds since 1970-01-01 UTC, or None if the clock wasn't read.
pub fn now_ms() -> Option<u64> {
    let base = unsafe { *BASE_MS.get() };
    (base != 0).then(|| base + timer::uptime_ms())
}
