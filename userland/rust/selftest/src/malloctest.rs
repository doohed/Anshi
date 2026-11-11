//! M11 test payload: proves the brk()-backed heap works -- several
//! allocations of varying sizes, a pattern-fill/verify pass through each,
//! a free-and-reuse check, then a clean exit.

use alloc::alloc::{alloc, dealloc};
use core::alloc::Layout;
use core::ffi::c_int;
use core::slice;

use anshi::println;

use crate::Argv;

/// Fills `len` bytes at `p` with `byte`, then checks they all read back.
fn fill_and_check(p: *mut u8, len: usize, byte: u8) -> bool {
    let block = unsafe { slice::from_raw_parts_mut(p, len) };
    block.fill(byte);
    block.iter().all(|&b| b == byte)
}

#[unsafe(no_mangle)]
pub extern "C" fn malloctest_main(_argc: c_int, _argv: Argv) -> c_int {
    println!("malloc/free test");

    let layout = |size| Layout::from_size_align(size, 1).unwrap();
    let a = unsafe { alloc(layout(16)) };
    let b = unsafe { alloc(layout(256)) };
    let c = unsafe { alloc(layout(4096)) };
    println!("a={a:p} b={b:p} c={c:p}");

    if a.is_null() || b.is_null() || c.is_null() {
        println!("allocation failed!");
        return 1;
    }

    let ok = fill_and_check(a, 16, 0xAA) & fill_and_check(b, 256, 0xBB) & fill_and_check(c, 4096, 0xCC);
    println!("pattern check: {}", if ok { "OK" } else { "FAIL" });

    unsafe { dealloc(b, layout(256)) };
    let d = unsafe { alloc(layout(200)) }; // should reuse b's freed slot (first-fit)
    println!("d={d:p} (reused freed slot: {})", if d == b { "yes" } else { "no" });

    unsafe {
        dealloc(a, layout(16));
        dealloc(c, layout(4096));
        dealloc(d, layout(200));
    }

    println!("malloc test done");
    if ok { 0 } else { 1 }
}
