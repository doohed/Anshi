//! The kernel-resident shell: what the system falls back to when sh
//! exits, with the kernel diagnostics (meminfo, lspci, lsblk, crash,
//! reboot...) sh can't reach. Reads lines from every keyboard, echoes to
//! the console, and dispatches to its builtins; command names match
//! case-insensitively ("HELP" is help), and their arguments pass through
//! exactly as typed. Anything else runs as a program from the working
//! directory or /bin.

use alloc::string::String;
use alloc::vec::Vec;
use core::arch::asm;

use crate::arch::x86_64::{enable_interrupts, halt_forever};
use crate::console::fbconsole;
use crate::drivers::timer::{self, TIMER_HZ};
use crate::drivers::virtio::blk;
use crate::drivers::{block, disktest, input, nic, partition, pci};
use crate::exec::process::{self, KERNEL_PARENT_PID};
use crate::fs::blkfs;
use crate::fs::vfs::{self, Vnode};
use crate::mm::pmm::{self, PAGE_SIZE};
use crate::net;
use crate::sync::Global;

const LINE_MAX: usize = 256;

static CWD: Global<*mut Vnode> = Global::new(core::ptr::null_mut());

fn cwd() -> *mut Vnode {
    unsafe { *CWD.get() }
}

type Builtin = fn(&str);

const BUILTINS: [(&str, &str, Builtin); 25] = [
    ("help", "list commands", cmd_help),
    ("echo", "echo <text> -- print text back", |args| kprintln!("{args}")),
    ("clear", "clear the screen", |_| fbconsole::clear()),
    ("uname", "print kernel/arch info", |_| kprintln!("Anshi x86_64 (UEFI/Limine)")),
    ("meminfo", "print physical memory allocator stats", cmd_meminfo),
    ("lspci", "list PCI devices found at boot", |_| pci::print_devices()),
    ("netinfo", "list network interfaces and their addresses", |_| net::print_info()),
    ("nicdebug", "dump network card registers and ring state (for a card that won't work)", |_| nic::debug_all()),
    ("lsblk", "list disks, their partitions and filesystems (read-only)", cmd_lsblk),
    ("disktest", "disktest <disk> [-w] -- check a disk driver (-w: writes too)", cmd_disktest),
    ("uptime", "print time elapsed since interrupts were enabled", cmd_uptime),
    ("crash", "deliberately trigger a #DE to test exception handling", cmd_crash),
    ("reboot", "reset the machine", cmd_reboot),
    ("halt", "halt the CPU forever", cmd_halt),
    ("create", "create dir|file <name> -- make a directory or empty file", cmd_create),
    ("delete", "delete dir|file <name> -- remove one (directories recursively)", cmd_delete),
    ("copy", "copy dir|file <src> <dest> -- duplicate a file or directory tree", cmd_copy),
    ("move", "move dir|file <src> <dest> -- rename/relocate", cmd_move),
    ("cd", "cd [path] -- change directory (no path -- go to /)", cmd_cd),
    ("pwd", "print the current directory", |_| kprintln!("{}", vfs::path(cwd()))),
    ("ls", "ls [path] -- list a directory's contents", |args| vfs::list(cwd(), args)),
    ("cat", "cat <file> -- print a file's contents", cmd_cat),
    ("write", "write <file> <text...> -- overwrite a file's contents", cmd_write),
    ("sync", "flush the filesystem to disk now (also happens automatically)", cmd_sync),
    ("run", "run <path> -- execute a static ELF64 program in ring 3", cmd_run),
];

/// Blocks until Enter, polling every keyboard (input.rs) -- including
/// COM1, the one that works headless. Accepts both what the keyboard
/// drivers produce ('\n' for Enter, '\b' for Backspace) and what a serial
/// terminal in raw mode commonly sends instead ('\r', DEL). Echoes as it
/// goes; Backspace erases the last character on the console and, via
/// "\b \b", on the serial terminal.
fn read_line() -> String {
    let mut line = String::new();
    // Inside an ESC [ ... sequence (an arrow key etc.): this reader has no
    // cursor movement or history, so those are swallowed rather than
    // echoed as a literal "[A".
    let mut in_escape = false;
    loop {
        let Some(c) = input::poll_char() else {
            net::poll();
            core::hint::spin_loop();
            continue;
        };
        if c == 0x1b {
            in_escape = true;
            continue;
        }
        if in_escape {
            in_escape = c == b'[' || c.is_ascii_digit() || c == b';';
            continue;
        }
        match c {
            b'\n' | b'\r' => {
                kprintln!();
                return line;
            }
            b'\x08' | 0x7F => {
                if line.pop().is_some() {
                    kprint!("\x08 \x08");
                }
            }
            _ if line.len() + 1 < LINE_MAX && c.is_ascii() => {
                line.push(c as char);
                kprint!("{}", c as char);
            }
            _ => {}
        }
    }
}

fn cmd_help(_: &str) {
    kprintln!("Available commands:");
    for (name, help, _) in BUILTINS {
        kprintln!("  {name} - {help}");
    }
}

fn cmd_meminfo(_: &str) {
    let (free, total) = (pmm::free_page_count(), pmm::total_page_count());
    kprintln!(
        "{free} / {total} pages free ({} / {} MiB)",
        free * PAGE_SIZE / (1024 * 1024),
        total * PAGE_SIZE / (1024 * 1024)
    );
}

fn cmd_lsblk(_: &str) {
    if block::count() == 0 {
        kprintln!("no disks found");
    }
    (0..block::count()).filter_map(block::get).for_each(partition::report);
}

fn cmd_disktest(args: &str) {
    let (name, rest) = args.split_once(' ').unwrap_or((args, ""));
    let Some(dev) = block::find(name) else {
        kprintln!("usage: disktest <disk> [-w] -- disks are listed by lsblk");
        return;
    };
    disktest::run(dev, rest.trim_start() == "-w");
}

fn cmd_uptime(_: &str) {
    let ms = timer::uptime_ms();
    kprintln!("{}s ({ms} ms, {} ticks @ {TIMER_HZ} Hz)", ms / 1000, timer::ticks());
}

fn cmd_crash(_: &str) {
    kprintln!("Triggering a deliberate #DE...");
    // A real DIV with a zero divisor -- in asm, so nothing can optimize it
    // away. Vector 0, #DE, no error code. Never returns: the exception
    // handler halts after dumping state.
    unsafe { asm!("xor edx, edx", "mov eax, 1", "xor ecx, ecx", "div ecx", out("eax") _, out("ecx") _, out("edx") _) };
    kprintln!("unreachable if the #DE handler fired correctly.");
}

/// A deliberate triple fault: load an empty IDT, so no interrupt has
/// anywhere valid to go, then raise one. The CPU faults handling the
/// fault (#GP -> nowhere -> double fault -> nowhere -> triple fault),
/// which every x86 CPU treats as a full reset -- unlike chipset-specific
/// reset registers (the 8042 pulse, ICH9's 0xCF9), it needs nothing
/// beyond the CPU itself. Confirmed with QEMU's QMP `query-status` that
/// it really resets; if it looks stuck instead of rebooting, check for
/// -no-reboot on the QEMU command line, which turns that reset into a
/// permanent pause.
fn cmd_reboot(_: &str) {
    kprintln!("Resetting...");
    let null_idt = [0u8; 10];
    unsafe { asm!("cli", "lidt [{}]", "int3", in(reg) &null_idt) };
    loop {
        unsafe { asm!("hlt") }; // Unreachable if the triple fault worked.
    }
}

fn cmd_halt(_: &str) {
    kprintln!("Halting.");
    halt_forever();
}

/// Saves after every successful change (silently -- blkfs only speaks up
/// on failure), so changes survive a reboot without remembering `sync`.
/// A no-op without a disk.
fn autosave(changed: bool) {
    if changed {
        blkfs::save();
    }
}

/// "dir|file <name> ..." with exactly `n` names, or None after printing
/// `usage`.
fn kind_and_names<'a>(args: &'a str, n: usize, usage: &str) -> Option<(&'a str, Vec<&'a str>)> {
    let mut words = args.split_whitespace();
    let kind = words.next().filter(|k| k.eq_ignore_ascii_case("dir") || k.eq_ignore_ascii_case("file"));
    let names: Vec<&str> = words.take(n).collect();
    match kind {
        Some(kind) if names.len() == n => Some((kind, names)),
        _ => {
            kprintln!("usage: {usage}");
            None
        }
    }
}

fn cmd_create(args: &str) {
    let Some((kind, names)) = kind_and_names(args, 1, "create dir|file <name>") else { return };
    let made =
        if kind.eq_ignore_ascii_case("dir") { vfs::mkdir(cwd(), names[0]) } else { vfs::create_file(cwd(), names[0]) };
    autosave(made);
}

fn cmd_delete(args: &str) {
    let Some((_, names)) = kind_and_names(args, 1, "delete dir|file <name>") else { return };
    autosave(vfs::remove(cwd(), names[0], cwd()));
}

fn cmd_copy(args: &str) {
    let Some((_, names)) = kind_and_names(args, 2, "copy dir|file <src> <dest>") else { return };
    autosave(vfs::copy(cwd(), names[0], names[1]));
}

fn cmd_move(args: &str) {
    let Some((_, names)) = kind_and_names(args, 2, "move dir|file <src> <dest>") else { return };
    autosave(vfs::move_node(cwd(), names[0], names[1]));
}

fn cmd_cd(args: &str) {
    if args.is_empty() {
        unsafe { *CWD.get() = vfs::root() };
        return;
    }
    match vfs::resolve(cwd(), args) {
        None => kprintln!("cd: {args}: no such directory"),
        Some(dir) if !unsafe { (*dir).is_dir() } => kprintln!("cd: {args}: not a directory"),
        Some(dir) => unsafe { *CWD.get() = dir },
    }
}

fn cmd_cat(args: &str) {
    if args.is_empty() {
        kprintln!("usage: cat <file>");
        return;
    }
    vfs::cat(cwd(), args);
}

fn cmd_write(args: &str) {
    let args = args.trim_start_matches(' ');
    if args.is_empty() {
        kprintln!("usage: write <file> <text...>");
        return;
    }
    let (name, text) = args.split_once(' ').unwrap_or((args, ""));
    autosave(vfs::write_file(cwd(), name, text.trim_start_matches(' ').as_bytes(), false));
}

fn cmd_sync(_: &str) {
    if !blk::is_ready() {
        kprintln!("sync: no virtio-blk device -- nothing to persist to");
        return;
    }
    if blkfs::save() {
        kprintln!("Filesystem synced.");
    }
}

/// Resolves a program name the way a shell with a $PATH does: a name
/// containing '/' is a path (relative or absolute), and a bare name is
/// looked up in the working directory first, then in /bin. One search
/// directory, hardcoded -- there's no environment to hold a real PATH.
fn resolve_program(name: &str) -> Option<&'static Vnode> {
    let file = |n: *mut Vnode| Some(unsafe { &*n }).filter(|n| !n.is_dir());
    if let Some(node) = vfs::resolve(cwd(), name).and_then(file) {
        return Some(node);
    }
    if name.contains('/') {
        return None; // An explicit path that didn't resolve.
    }
    let bin = vfs::resolve(vfs::root(), "/bin").filter(|b| unsafe { (**b).is_dir() })?;
    vfs::resolve(bin, name).and_then(file)
}

fn cmd_run(args: &str) {
    // Whitespace-separated, no quoting, at most 16 arguments.
    let argv: Vec<&[u8]> = args.split_whitespace().take(16).map(str::as_bytes).collect();
    let Some(&name) = argv.first() else {
        kprintln!("usage: run <path> [args...]");
        return;
    };
    let name = core::str::from_utf8(name).unwrap_or("");
    let Some(node) = resolve_program(name) else {
        kprintln!("run: {name}: no such program");
        return;
    };
    // The program starts in the shell's own directory, so a relative
    // argument like "." means what it looks like it means.
    if process::spawn(&node.data, &argv, cwd(), KERNEL_PARENT_PID).is_none() {
        return;
    }
    // Waits for everything this launch spawned (through fork()) to exit,
    // not just the one pid -- there's no init process to reparent orphans
    // to. Each one's exit is reported as it happens.
    process::run_until(None);
}

/// Never returns.
pub fn run() -> ! {
    unsafe { *CWD.get() = vfs::root() };
    kprintln!("\nType 'help' for a list of commands.");
    loop {
        // A program that just finished left the kernel here from inside a
        // syscall, with interrupts still off -- which would stop the tick
        // (uptime, timeouts) for as long as this shell runs.
        enable_interrupts();
        kprint!("Anshi:{}> ", vfs::path(cwd()));
        let line = read_line();
        let line = line.trim_start_matches(' ');
        let (cmd, args) = line.split_once(' ').unwrap_or((line, ""));
        let args = args.trim_start_matches(' ');
        if cmd.is_empty() {
            continue;
        }
        if let Some((_, _, f)) = BUILTINS.iter().find(|(name, _, _)| name.eq_ignore_ascii_case(cmd)) {
            f(args);
        } else if resolve_program(cmd).is_some() {
            // Not a builtin: run it as a program, so `scarf .` works like in
            // any shell rather than needing `run scarf .`. Builtins win a
            // name collision -- a file dropped in /bin must never be able to
            // shadow `ls` or `cd`.
            cmd_run(line);
        } else {
            kprintln!("unknown command: {} (try 'help')", cmd.to_ascii_lowercase());
        }
    }
}
