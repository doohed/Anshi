//! The Anshi kernel: booted by Limine into kmain() in 64-bit long mode,
//! with the higher-half direct map, the memory map and the framebuffer
//! the boot requests (boot/requests.rs) asked for. kmain brings the
//! machine up in order -- descriptor tables, memory, interrupts and the
//! timer, PCI and drivers, the filesystem -- then boots into /bin/sh.

#![no_std]

extern crate alloc;

#[macro_use]
mod mmio;
#[macro_use]
pub mod drivers;

pub mod arch;
pub mod boot;
pub mod console;
pub mod exec;
pub mod fs;
pub mod mm;
pub mod shell;
pub mod sync;
pub mod util;

use arch::x86_64::{self as x86, fpu, gdt, halt_forever, idt, pic};
use boot::requests;
use console::{fbconsole, splash};
use drivers::virtio::{self, blk};
use drivers::{audio, block, display, nvme, partition, pci, serial, timer, usb};
use exec::process::{self, KERNEL_PARENT_PID};
use fs::{blkfs, vfs};
use mm::{pmm, vmm};

/// Reports the panic on every log output (serial, and the screen once
/// it's up), then halts this CPU for good.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("\nKERNEL PANIC: {info}");
    halt_forever()
}

/// Logs what the bootloader handed over.
fn log_boot_info() {
    if let Some(hhdm) = requests::hhdm() {
        kprintln!("HHDM offset: 0x{:x}", hhdm.offset);
    }
    if let Some(mm) = requests::memmap() {
        let usable: u64 = mm.entries().filter(|e| e.kind == requests::MEMMAP_USABLE).map(|e| e.length).sum();
        kprintln!("Memory map: {} entries, {} KiB usable", mm.entry_count, usable / 1024);
    }
    if let Some(fb) = requests::framebuffer().filter(|r| r.framebuffer_count > 0).map(|r| unsafe { &**r.framebuffers })
    {
        kprintln!("Boot framebuffer: {}x{} @ {} bpp, pitch {}", fb.width, fb.height, fb.bpp, fb.pitch);
    }
    if let Some(rsdp) = requests::rsdp() {
        kprintln!("RSDP: {:p}", rsdp.address);
    }
}

/// Grabs a batch of pages, checks they're all distinct, gives them back
/// and checks the free count returns to where it started.
fn pmm_self_test() {
    let free_before = pmm::free_page_count();
    let mut pages = [0u64; 16];
    for i in 0..pages.len() {
        pages[i] = pmm::alloc_page();
        if pages[i] != 0 && pages[..i].contains(&pages[i]) {
            kprintln!("PMM PANIC: pmm::alloc_page() returned a duplicate address");
            halt_forever();
        }
    }
    pages.iter().for_each(|&p| pmm::free_page(p));
    if pmm::free_page_count() != free_before {
        kprintln!("PMM PANIC: free page count did not return to baseline after freeing");
        halt_forever();
    }
    kprintln!("PMM self-test OK: allocated/freed 16 distinct pages");
}

/// M10 prep: a fresh address space with a scratch page mapped
/// user-accessible, switched into, written and read back -- proves the
/// per-address-space page tables work before anything runs in ring 3.
fn vmm_self_test() {
    let boot_as = vmm::current_address_space();
    let test_as = vmm::new_address_space();
    let scratch = pmm::alloc_page();
    let virt = 0x40_0000;
    vmm::map(&test_as, virt, scratch, vmm::PAGE_PRESENT | vmm::PAGE_WRITABLE | vmm::PAGE_USER);

    vmm::switch(&test_as);
    let p = virt as *mut u64;
    unsafe { p.write_volatile(0xDEAD_BEEF_CAFE_BABE) };
    let readback = unsafe { p.read_volatile() };
    vmm::switch(&boot_as);
    pmm::free_page(scratch);

    if readback != 0xDEAD_BEEF_CAFE_BABE {
        kprintln!("VMM PANIC: address-space smoke test read back 0x{readback:x}, expected something else");
        halt_forever();
    }
    kprintln!("VMM self-test OK: per-address-space page tables work");
}

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    serial::init();
    kprintln!("\nAnshi booting (x86_64 / UEFI / Limine)");
    if !requests::base_revision_supported() {
        kprintln!("PANIC: bootloader does not support the requested base revision");
        halt_forever();
    }
    log_boot_info();
    kprintln!("M0 complete.");

    gdt::init();
    kprintln!("GDT/TSS loaded.");
    idt::init();
    kprintln!("IDT loaded.");
    fpu::init();
    kprintln!("x87/SSE enabled for userland.");

    pmm::init();
    pmm_self_test();
    kprintln!("Heap ready (Rust's allocator over the PMM).");
    vmm_self_test();

    // pic::remap() maps the Local APIC's MMIO page to relay the legacy
    // PIC's interrupts through it -- which needs the PMM (and its page
    // table allocations) up first. timer::init() likewise maps ACPI
    // tables.
    pic::remap();
    timer::init();
    x86::enable_interrupts();
    timer::check(); // Never trust that ticks arrive -- a real PC may not route them.
    timer::log_status();
    kprintln!("Interrupts enabled.");
    kprintln!("M2 complete.");

    pci::enumerate();
    kprintln!("M3 complete.");

    // virtio-gpu under QEMU, the firmware's GOP framebuffer on a real PC.
    if let Some(fb) = display::init() {
        let name = display::name().unwrap_or("?");
        kprintln!("Display: {name}, {}x{}.", fb.width, fb.height);
        // The console comes first: the splash draws its caption through it,
        // and its clear doubles as the splash's blank backdrop.
        fbconsole::init(fb);
        splash::show(&fb); // Logo + looping "..." while the rest of boot logs to serial.
        fbconsole::clear(); // Wipe the splash before the scrolling log takes over.

        serial::set_console_sink(fbconsole::sink);
        fbconsole::write(b"Anshi -- x86_64 / UEFI / Limine / ");
        fbconsole::write(name.as_bytes());
        fbconsole::write(b"\n\n");
        kprintln!("M5 complete: framebuffer console live via {name}.");
        timer::log_status(); // Again, now that it reaches the screen too.
    } else {
        kprintln!("Skipping M5 (no display: no virtio-gpu, no firmware framebuffer).");
    }

    // The keyboard is optional: COM1 always works as one, and a real PC
    // with no keyboard driver should still reach the shell, if only to
    // show that everything else did.
    let virtio_kbd = virtio::input::init();
    if virtio_kbd {
        kprintln!("M6 complete: virtio-input keyboard ready.");
    }
    let controllers = usb::init();
    if usb::kbd::count() > 0 {
        kprintln!("USB keyboard ready ({controllers} xHCI controller(s)).");
    } else if !virtio_kbd {
        kprintln!(
            "No keyboard found ({controllers} xHCI controller(s)) -- one plugged in later still works; until then, input only over COM1, if there is one."
        );
    }

    vfs::init();
    kprintln!("M7 complete: in-memory filesystem ready.");
    if blk::init() {
        blkfs::load(); // A blank disk is fine: nothing to load yet.
        kprintln!("M9 complete: persistent storage ready.");
    } else {
        kprintln!(
            "Skipping M9 (no virtio-blk device) -- filesystem stays in-memory only. Boot QEMU with -device virtio-blk-pci for persistence."
        );
    }

    // Disks: found and described, never written (see drivers/block.rs).
    block::init();
    nvme::init();
    drivers::ahci::init();
    (0..block::count()).filter_map(block::get).for_each(partition::report);

    match audio::init().then(audio::name).flatten() {
        Some(name) => kprintln!("M17 complete: {name} ready."),
        None => kprintln!("No sound device -- audio playback unavailable."),
    }

    // The programs, written fresh into /bin on every boot (it's where
    // shells look for a bare command name), plus the self-tests' fixtures
    // in /: filetest.txt for filetest to read, patch and append to, and a
    // tone for play.
    let root = vfs::root();
    vfs::mkdir(root, "bin");
    let bin = vfs::resolve(root, "/bin").unwrap_or(root);
    for (i, (name, image)) in exec::blobs::PROGRAMS.iter().enumerate() {
        vfs::write_bytes(bin, name, image);
        if i == 3 {
            vfs::write_file(root, "filetest.txt", b"hello file test\n", false);
        }
    }
    vfs::write_bytes(root, "testtone.wav", exec::blobs::TESTTONE_WAV);

    // Boot into /bin/sh. If it ever exits -- `exit`, or a crash -- fall
    // back to the kernel shell, which also has the kernel diagnostics sh
    // can't reach; `sh` there starts it again.
    if let Some(sh) = vfs::resolve(root, "/bin/sh").map(|n| unsafe { &*n }).filter(|n| !n.is_dir())
        && process::spawn(&sh.data, &[b"sh"], root, KERNEL_PARENT_PID).is_some()
    {
        process::run_until(None);
        kprintln!("\x1b[0m\nsh exited -- this is the kernel shell (`sh` starts it again)");
    }
    shell::run()
}
