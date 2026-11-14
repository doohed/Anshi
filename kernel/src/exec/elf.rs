//! Loads a static, non-PIE ELF64 x86_64 executable into a fresh address
//! space: every PT_LOAD segment mapped (user-accessible, read-write -- no
//! W^X/NX enforcement in this first pass), a user stack holding the
//! arguments, and a kernel stack.

use alloc::alloc::{Layout, alloc, dealloc};
use alloc::vec::Vec;

use super::Usertask;
use crate::arch::x86_64::fpu;
use crate::boot::requests::phys_to_virt;
use crate::drivers::tty::Termios;
use crate::fs::vfs::{self, Vnode};
use crate::mm::pmm::{self, PAGE_SIZE};
use crate::mm::vmm::{self, PAGE_PRESENT, PAGE_USER, PAGE_WRITABLE};

/// Every process's kernel stack (TSS.RSP0 while it's the one running).
pub const KERNEL_STACK_SIZE: usize = 16 * 1024;

const PT_LOAD: u32 = 1;
const EM_X86_64: u16 = 62;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

/// Fixed user-space layout: plenty of headroom below the canonical
/// boundary (0x0000800000000000) so it can never collide with a
/// reasonably-linked non-PIE binary's own segments (those typically start
/// around 0x400000).
const USER_STACK_TOP: u64 = 0x0000_7000_0000_0000;
/// PT_LOAD segments tracked for page sharing.
const MAX_LOAD_SEGMENTS: usize = 16;
/// 128 KiB: minimp3's mp3dec_decode_frame() alone (play) puts a ~16 KiB
/// scratch struct on the stack -- the old 16 KiB overflowed -- and up to
/// ARG_SPACE of it can go to the arguments and environment.
const USER_STACK_PAGES: u64 = 32;

/// Caps on the argument vector and the environment, each: how many
/// strings and how long one may be. Together they must also fit in
/// ARG_SPACE, so a caller can't push the initial stack down past the
/// pages actually mapped for it, nor leave the program no stack to run on.
pub const MAX_ARGS: usize = 256;
pub const MAX_ARG_LEN: usize = 4096;
const ARG_SPACE: u64 = 64 * 1024;

#[repr(C, packed)]
struct Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

#[repr(C, packed)]
struct Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

fn stack_layout() -> Layout {
    Layout::from_size_align(KERNEL_STACK_SIZE, 16).unwrap()
}

/// A kernel stack from the heap -- reachable from every address space,
/// since the HHDM is part of the shared higher half. Its top, or None.
pub fn alloc_kernel_stack() -> Option<u64> {
    let base = unsafe { alloc(stack_layout()) };
    (!base.is_null()).then(|| base as u64 + KERNEL_STACK_SIZE as u64)
}

pub fn free_kernel_stack(top: u64) {
    unsafe { dealloc((top - KERNEL_STACK_SIZE as u64) as *mut u8, stack_layout()) };
}

/// Loads `image` (e.g. a file's contents) as a ready-to-run task whose
/// arguments are `argv`, environment `envp` (`NAME=value` strings) and
/// working directory `cwd` (or the root). None, after saying why, for a
/// malformed or unsupported file, or arguments that don't fit.
pub fn load(image: &[u8], argv: &[&[u8]], envp: &[&[u8]], cwd: *mut Vnode) -> Option<Usertask> {
    let strings: usize = argv.iter().chain(envp).map(|s| s.len() + 1).sum();
    let too_long = argv.iter().chain(envp).any(|s| s.len() >= MAX_ARG_LEN);
    if argv.len() > MAX_ARGS || envp.len() > MAX_ARGS || too_long || strings as u64 > ARG_SPACE / 2 {
        kprintln!("elf: arguments and environment too long");
        return None;
    }
    if image.len() < size_of::<Ehdr>() {
        kprintln!("elf: file too small to be an ELF header");
        return None;
    }
    let eh = unsafe { image.as_ptr().cast::<Ehdr>().read_unaligned() };
    if &eh.e_ident[..4] != b"\x7fELF" {
        kprintln!("elf: not an ELF file");
        return None;
    }
    if eh.e_ident[4] != ELFCLASS64 || eh.e_ident[5] != ELFDATA2LSB {
        kprintln!("elf: not a 64-bit little-endian ELF");
        return None;
    }
    if eh.e_machine != EM_X86_64 {
        kprintln!("elf: not an x86_64 binary");
        return None;
    }
    let (phoff, phnum, phentsize) = (eh.e_phoff, eh.e_phnum as u64, eh.e_phentsize as u64);
    if phoff + phnum * phentsize > image.len() as u64 {
        kprintln!("elf: program header table out of bounds");
        return None;
    }

    let aspace = vmm::new_address_space();
    // The highest PT_LOAD segment's mapped end: where the heap starts.
    let mut heap_base = 0;
    // Segments loaded so far: their first page, page count, and where
    // their (contiguous) pages are in kernel memory. Two segments may share
    // a page -- segments only have to be page-aligned *relative to their
    // file offset*, so one can end and the next begin mid-page (rustc's
    // .got right after .rodata does exactly that). Such a page is already
    // mapped by the earlier segment; the later one writes its bytes into it
    // rather than mapping a fresh zeroed page over it, which would wipe the
    // earlier segment's tail.
    let mut loaded: [(u64, u64, *mut u8); MAX_LOAD_SEGMENTS] = [(0, 0, core::ptr::null_mut()); MAX_LOAD_SEGMENTS];
    let mut nloaded = 0;

    for i in 0..phnum {
        let ph = unsafe { image.as_ptr().add((phoff + i * phentsize) as usize).cast::<Phdr>().read_unaligned() };
        if ph.p_type != PT_LOAD {
            continue;
        }
        let (vaddr, offset, filesz, memsz) = (ph.p_vaddr, ph.p_offset, ph.p_filesz, ph.p_memsz);
        if offset + filesz > image.len() as u64 || filesz > memsz {
            kprintln!("elf: PT_LOAD segment {i} out of bounds");
            return None;
        }

        let seg_start = vaddr & !(PAGE_SIZE - 1);
        let seg_end = vaddr + memsz;
        let pages = (seg_end - seg_start).div_ceil(PAGE_SIZE);
        let base_phys = pmm::alloc_pages(pages);
        if base_phys == 0 {
            kprintln!("elf: out of memory for segment {i} ({pages} pages)");
            return None;
        }
        let kbase: *mut u8 = phys_to_virt(base_phys);
        unsafe {
            core::ptr::write_bytes(kbase, 0, (pages * PAGE_SIZE) as usize);
            core::ptr::copy_nonoverlapping(
                image.as_ptr().add(offset as usize),
                kbase.add((vaddr - seg_start) as usize),
                filesz as usize,
            );
        }

        for p in 0..pages {
            let vpage = seg_start + p * PAGE_SIZE;
            // The first segment covering the page is the one that actually
            // mapped it (later ones only wrote into it).
            let shared = loaded[..nloaded]
                .iter()
                .find(|(start, n, _)| vpage >= *start && vpage < start + n * PAGE_SIZE)
                .map(|&(start, _, k)| unsafe { k.add((vpage - start) as usize) });
            let Some(shared) = shared else {
                vmm::map(&aspace, vpage, base_phys + p * PAGE_SIZE, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
                continue;
            };
            // Copy only this segment's own bytes into the shared page (file
            // bytes and zero-filled .bss tail alike), then give back the
            // duplicate page it was going to use.
            let from = vaddr.max(vpage);
            let to = seg_end.min(vpage + PAGE_SIZE);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    kbase.add((from - seg_start) as usize),
                    shared.add((from - vpage) as usize),
                    (to - from) as usize,
                )
            };
            pmm::free_page(base_phys + p * PAGE_SIZE);
        }
        if nloaded < MAX_LOAD_SEGMENTS {
            loaded[nloaded] = (seg_start, pages, kbase);
            nloaded += 1;
        }
        heap_base = heap_base.max(seg_start + pages * PAGE_SIZE);
    }

    let stack_phys = pmm::alloc_pages(USER_STACK_PAGES);
    if stack_phys == 0 {
        kprintln!("elf: out of memory for the user stack");
        return None;
    }
    let stack_kernel: *mut u8 = phys_to_virt(stack_phys);
    unsafe { core::ptr::write_bytes(stack_kernel, 0, (USER_STACK_PAGES * PAGE_SIZE) as usize) };
    let stack_base = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;
    for p in 0..USER_STACK_PAGES {
        vmm::map(
            &aspace,
            stack_base + p * PAGE_SIZE,
            stack_phys + p * PAGE_SIZE,
            PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER,
        );
    }

    let Some(kernel_stack_top) = alloc_kernel_stack() else {
        kprintln!("elf: out of memory for the kernel stack");
        return None;
    };
    if heap_base == 0 {
        heap_base = 0x60_0000; // No PT_LOAD segments (degenerate) -- a safe fallback.
    }

    // The System V AMD64 process-initialization stack. At entry RSP points
    // at argc, then the argv pointer array and its NULL terminator, the
    // envp array and its NULL, and an empty auxv vector; the strings sit
    // higher up. anshi's _start reads argc/argv/envp straight off this.
    //
    // Written through the HHDM rather than through the new address space:
    // the stack pages are one physically contiguous run, so
    // `stack_kernel + (uaddr - stack_base)` reaches any of them without
    // switching CR3.
    let at = |uaddr: u64| unsafe { stack_kernel.add((uaddr - stack_base) as usize) };
    let mut sp = USER_STACK_TOP;
    let mut place = |s: &[u8]| {
        sp -= s.len() as u64 + 1;
        unsafe {
            core::ptr::copy_nonoverlapping(s.as_ptr(), at(sp), s.len());
            *at(sp + s.len() as u64) = 0;
        }
        sp
    };
    let arg_ptrs: Vec<u64> = argv.iter().map(|a| place(a)).collect();
    let env_ptrs: Vec<u64> = envp.iter().map(|e| place(e)).collect();

    // argc, argv[n], NULL, envp[m], NULL, and AT_NULL's key/value pair.
    // Aligned down afterwards, which only ever leaves more room.
    let words = 1 + arg_ptrs.len() + 1 + env_ptrs.len() + 1 + 2;
    sp -= words as u64 * 8;
    sp &= !0xF;
    if sp < USER_STACK_TOP - ARG_SPACE {
        kprintln!("elf: arguments and environment too long");
        return None;
    }
    let frame = at(sp).cast::<u64>();
    let table = core::iter::once(argv.len() as u64)
        .chain(arg_ptrs.iter().copied())
        .chain([0])
        .chain(env_ptrs.iter().copied())
        .chain([0, 0, 0]);
    for (i, word) in table.enumerate() {
        unsafe { frame.add(i).write(word) };
    }

    let mut task = Usertask {
        entry: eh.e_entry,
        user_stack_top: sp,
        kernel_stack_top,
        aspace,
        heap_start: heap_base,
        heap_end: heap_base, // Zero-size until the first brk().
        // Inherits the launching shell's directory (or the exec'ing task's)
        // -- M12 started every task at the root instead, which made
        // `scarf .` meaningless.
        cwd: if cwd.is_null() { vfs::root() } else { cwd },
        termios: Termios::default_console(),
        ..Usertask::EMPTY
    };
    fpu::state_init(&mut task.fpu_state.0);
    Some(task)
}
