//! Reads a disk's partition table (GPT, checking both of its CRCs, or an
//! MBR) and prints each partition with its type, size, name and the
//! filesystem found in it -- read-only, for `lsblk` and the boot log.

use alloc::string::String;
use alloc::vec;

use super::block::BlockDevice;
use crate::util::crc32::crc32;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct GptHeader {
    signature: [u8; 8], // "EFI PART"
    revision: u32,
    header_size: u32,
    header_crc: u32,
    reserved: u32,
    my_lba: u64,
    alternate_lba: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: [u8; 16],
    entries_lba: u64,
    entry_count: u32,
    entry_size: u32,
    entries_crc: u32,
}

#[repr(C, packed)]
struct GptEntry {
    kind: [u8; 16],
    unique: [u8; 16],
    first_lba: u64,
    last_lba: u64,
    attributes: u64,
    name: [u16; 36], // UTF-16LE
}

/// GUIDs as printed, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B".
const GPT_TYPES: [(&str, &str); 11] = [
    ("C12A7328-F81F-11D2-BA4B-00A0C93EC93B", "EFI system"),
    ("E3C9E316-0B5C-4DB8-817D-F92DF00215AE", "Microsoft reserved"),
    ("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7", "Microsoft basic data"),
    ("DE94BBA4-06D1-4D40-A16A-BFD50179D6AC", "Windows recovery"),
    ("0FC63DAF-8483-4772-8E79-3D69D8477DE4", "Linux filesystem"),
    ("4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709", "Linux root (x86-64)"),
    ("0657FD6D-A4AB-43C4-84E5-0933C84B4F4F", "Linux swap"),
    ("E6D6D379-F507-44C2-A23C-238F2A3DF928", "Linux LVM"),
    ("A19D880F-05FC-4D3B-A006-743F0F84911E", "Linux RAID"),
    ("21686148-6449-6E6F-744E-656564454649", "BIOS boot"),
    ("48465300-0000-11AA-AA11-00306543ECAC", "Apple HFS+"),
];

/// GUIDs are stored with their first three fields little-endian.
fn guid_string(g: &[u8; 16]) -> String {
    const ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut s = String::new();
    for (i, &o) in ORDER.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&alloc::format!("{:02X}", g[o]));
    }
    s
}

fn print_size(bytes: u64) {
    if bytes >= 10 << 30 {
        kprint!("{} GiB", bytes >> 30);
    } else if bytes >= 10 << 20 {
        kprint!("{} MiB", bytes >> 20);
    } else {
        kprint!("{} KiB", bytes >> 10);
    }
}

/// What's in a partition, judged by its first 68 KiB.
fn detect_fs(dev: &mut BlockDevice, lba: u64, sectors: u64) -> &'static str {
    let count = (68 * 1024u64).div_ceil(dev.sector_size as u64).min(sectors);
    let mut b = vec![0u8; (count * dev.sector_size as u64) as usize];
    if !dev.read(lba, count, &mut b) {
        return "unreadable";
    }
    let at = |off: usize, magic: &[u8]| b.get(off..off + magic.len()) == Some(magic);
    if at(3, b"NTFS    ") {
        "NTFS"
    } else if at(3, b"-FVE-FS-") {
        "BitLocker"
    } else if at(3, b"EXFAT   ") {
        "exFAT"
    } else if at(82, b"FAT32   ") {
        "FAT32"
    } else if at(54, b"FAT1") {
        "FAT12/16"
    } else if at(0, b"LUKS\xba\xbe") {
        "LUKS"
    } else if b.len() > 1082 && b[1080] == 0x53 && b[1081] == 0xEF {
        "ext2/3/4"
    } else if at(65600, b"_BHRfS_M") {
        "btrfs"
    } else if at(32769, b"CD001") {
        "ISO 9660"
    } else if at(0, b"ANFS") {
        "Anshi blkfs"
    } else if at(0, b"XFSB") {
        "XFS"
    } else {
        ""
    }
}

fn report_gpt(dev: &mut BlockDevice, lba1: &[u8]) -> bool {
    let h = unsafe { lba1.as_ptr().cast::<GptHeader>().read_unaligned() };
    if &h.signature != b"EFI PART" {
        return false;
    }
    let mut copy = h;
    copy.header_crc = 0;
    let header_size = (h.header_size as usize).min(size_of::<GptHeader>());
    let copy_bytes = unsafe { core::slice::from_raw_parts((&raw const copy).cast::<u8>(), header_size) };
    let header_ok = crc32(copy_bytes) == h.header_crc;

    let (entry_count, entry_size) = (h.entry_count as usize, h.entry_size as usize);
    let array_bytes = entry_count * entry_size;
    if entry_size < size_of::<GptEntry>() || array_bytes > 1024 * 1024 {
        kprintln!("  GPT with an implausible partition array -- not reading it");
        return true;
    }
    let sectors = (array_bytes as u64).div_ceil(dev.sector_size as u64);
    let mut array = vec![0u8; (sectors * dev.sector_size as u64) as usize];
    if !dev.read(h.entries_lba, sectors, &mut array) {
        kprintln!("  GPT, but its partition array is unreadable");
        return true;
    }
    let array_ok = crc32(&array[..array_bytes]) == h.entries_crc;

    let entries = || {
        array
            .chunks(entry_size)
            .take(entry_count)
            .enumerate()
            .map(|(i, e)| (i, unsafe { e.as_ptr().cast::<GptEntry>().read_unaligned() }))
            .filter(|(_, e)| e.kind != [0; 16])
    };
    kprintln!(
        "  GPT: {} partitions, header CRC {}, partition array CRC {}",
        entries().count(),
        if header_ok { "ok" } else { "BAD" },
        if array_ok { "ok" } else { "BAD" }
    );

    for (i, e) in entries() {
        let guid = guid_string(&e.kind);
        let kind = GPT_TYPES.iter().find(|(g, _)| *g == guid).map_or(guid.as_str(), |(_, n)| n);
        let (first, last) = (e.first_lba, e.last_lba);
        let sectors = if last >= first { last - first + 1 } else { 0 };
        kprint!("  {}p{}: {kind}, ", dev.name, i + 1);
        print_size(sectors * dev.sector_size as u64);
        let fs = if sectors > 0 { detect_fs(dev, first, sectors) } else { "" };
        if !fs.is_empty() {
            kprint!(", {fs}");
        }
        let name = e.name;
        let name: String =
            name.iter().take_while(|&&c| c != 0).map(|&c| if c < 0x80 { c as u8 as char } else { '?' }).collect();
        if !name.is_empty() {
            kprint!(" \"{name}\"");
        }
        kprintln!();
    }
    true
}

fn report_mbr(dev: &mut BlockDevice, lba0: &[u8]) {
    if lba0[510] != 0x55 || lba0[511] != 0xAA {
        let count = dev.sector_count;
        let fs = detect_fs(dev, 0, count);
        kprintln!("  no partition table{}{fs}", if fs.is_empty() { "" } else { "; whole disk: " });
        return;
    }
    kprintln!("  MBR partition table");
    for i in 0..4 {
        let p = &lba0[446 + i * 16..462 + i * 16];
        let kind = p[4];
        let start = u32::from_le_bytes(p[8..12].try_into().unwrap()) as u64;
        let count = u32::from_le_bytes(p[12..16].try_into().unwrap()) as u64;
        if kind == 0 || count == 0 {
            continue;
        }
        kprint!("  {}p{}: type 0x{kind:x}, ", dev.name, i + 1);
        print_size(count * dev.sector_size as u64);
        let fs = if start < dev.sector_count { detect_fs(dev, start, count) } else { "" };
        if !fs.is_empty() {
            kprint!(", {fs}");
        }
        kprintln!();
    }
}

pub fn report(dev: &mut BlockDevice) {
    kprint!("{}: {}, ", dev.name, if dev.model.is_empty() { "(no model)" } else { &dev.model });
    print_size(dev.sector_count * dev.sector_size as u64);
    kprintln!(", {}-byte sectors", dev.sector_size);

    let sector = dev.sector_size as usize;
    let mut b = vec![0u8; 2 * sector];
    if !dev.read(0, 2, &mut b) {
        kprintln!("  can't read the first sectors");
        return;
    }
    if !report_gpt(dev, &b[sector..]) {
        report_mbr(dev, &b);
    }
    // A hybrid ISO (like the Anshi live image) is also ISO 9660 from
    // sector 0, whatever partition table it carries for firmware.
    let count = dev.sector_count;
    if detect_fs(dev, 0, count) == "ISO 9660" {
        kprintln!("  whole disk: ISO 9660 (a hybrid ISO image, like the Anshi live USB)");
    }
}
