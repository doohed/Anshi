//! Rust's global allocator: an address-ordered free list over the
//! process's brk-managed heap (kernel/src/exec/syscall.rs's
//! sys_brk()), deliberately mirroring kernel/src/mm/heap.rs's design --
//! same block-header shape, same first-fit and forward-only coalescing --
//! just growing with brk() instead of pulling pages from the PMM.
//!
//! Every payload is 16-byte aligned, which covers everything but the odd
//! over-aligned type; those are over-allocated and the block's real start
//! is stashed in the 8 bytes right below the aligned pointer.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::null_mut;

use crate::sys;

const HEAP_ALIGN: usize = 16;
/// brk() grows a page at a time anyway.
const GROW_CHUNK: usize = 4096;

/// 32 bytes, so the payload right after it stays 16-byte aligned.
#[repr(C, align(16))]
struct Block {
    /// Next block in address order, free or not.
    next: *mut Block,
    /// Usable size, not counting this header.
    size: usize,
    free: bool,
}

const HEADER: usize = size_of::<Block>();

struct State {
    /// First block ever, address order.
    head: *mut Block,
    /// Last block -- O(1) append when growing.
    tail: *mut Block,
    /// 0 until the first grow() asks the kernel where the heap starts.
    brk: usize,
}

pub struct Heap(UnsafeCell<State>);

// A process is a single thread, and fork() gives the child its own copy
// of the address space, so nothing ever shares this.
unsafe impl Sync for Heap {}

fn align_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

impl State {
    /// Grows the heap by enough whole GROW_CHUNKs for `min_size` usable
    /// bytes plus a header, as one free block appended to the list.
    unsafe fn grow(&mut self, min_size: usize) -> *mut Block {
        if self.brk == 0 {
            // The kernel's elf::load() starts the heap page-aligned;
            // aligning anyway keeps every payload on HEAP_ALIGN if that
            // ever changes.
            self.brk = align_up(sys::brk(0) as usize, HEAP_ALIGN);
        }
        let grow = align_up(HEADER + min_size, GROW_CHUNK);
        let new_end = self.brk + grow;
        if sys::brk(new_end) != new_end as i64 {
            return null_mut(); // out of memory (HEAP_MAX_BYTES in the kernel's syscall.rs)
        }

        let block = self.brk as *mut Block;
        unsafe { block.write(Block { next: null_mut(), size: grow - HEADER, free: true }) };
        self.brk = new_end;
        if self.tail.is_null() {
            self.head = block;
        } else {
            unsafe { (*self.tail).next = block };
        }
        self.tail = block;
        block
    }

    /// Carves `size` bytes off the front of `block`, the remainder
    /// becoming a free block right after it -- unless the remainder is
    /// too small to be worth tracking, in which case the slack just stays
    /// attached to this allocation.
    unsafe fn split(&mut self, block: *mut Block, size: usize) {
        let b = unsafe { &mut *block };
        if b.size < size + HEADER + HEAP_ALIGN {
            return;
        }
        let rest = (block as usize + HEADER + size) as *mut Block;
        unsafe { rest.write(Block { next: b.next, size: b.size - size - HEADER, free: true }) };
        b.size = size;
        b.next = rest;
        if self.tail == block {
            self.tail = rest;
        }
    }

    unsafe fn malloc(&mut self, size: usize) -> *mut u8 {
        let size = align_up(size.max(1), HEAP_ALIGN);
        let mut b = self.head;
        while !b.is_null() && !unsafe { (*b).free && (*b).size >= size } {
            b = unsafe { (*b).next };
        }
        if b.is_null() {
            b = unsafe { self.grow(size) };
            if b.is_null() {
                return null_mut();
            }
        }
        unsafe {
            self.split(b, size);
            (*b).free = false;
        }
        (b as usize + HEADER) as *mut u8
    }

    unsafe fn free(&mut self, ptr: *mut u8) {
        let block = (ptr as usize - HEADER) as *mut Block;
        let b = unsafe { &mut *block };
        b.free = true;

        // Forward coalescing only, and only when the next block really is
        // adjacent -- always true here, since brk() only ever extends the
        // heap upward, but kept as an explicit check for the same reason
        // heap.c does: it costs nothing and stays correct if that changes.
        let next = b.next;
        if !next.is_null() && unsafe { (*next).free } && next as usize == block as usize + HEADER + b.size {
            let n = unsafe { &*next };
            b.size += HEADER + n.size;
            b.next = n.next;
            if self.tail == next {
                self.tail = block;
            }
        }
    }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let state = unsafe { &mut *self.0.get() };
        let align = layout.align();
        if align <= HEAP_ALIGN {
            return unsafe { state.malloc(layout.size()) };
        }
        // `raw` is 16-aligned and `align` a larger power of two, so the
        // first aligned address at least 8 bytes in is at most
        // `align - 8` bytes in: `align` extra bytes always cover it.
        let raw = unsafe { state.malloc(layout.size() + align) };
        if raw.is_null() {
            return raw;
        }
        let aligned = align_up(raw as usize + 8, align);
        unsafe { ((aligned - 8) as *mut usize).write(raw as usize) };
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let state = unsafe { &mut *self.0.get() };
        let raw = if layout.align() <= HEAP_ALIGN { ptr } else { unsafe { *((ptr as usize - 8) as *const usize) as *mut u8 } };
        unsafe { state.free(raw) };
    }
}

#[global_allocator]
static HEAP: Heap = Heap(UnsafeCell::new(State { head: null_mut(), tail: null_mut(), brk: 0 }));
