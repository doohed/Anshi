//! `_start`, the entry point userland/link.ld names. Reads argc/argv off
//! the System V process initialization stack elf::load() builds
//! (kernel/src/exec/elf.rs: RSP points at argc, with the argv pointer
//! array immediately above it), passes them to the program's `main`, then
//! exits with its return value.
//!
//! The stack has to be 16-byte aligned at a call, and RSP points at argc
//! rather than a return address on entry -- so argc is read first, then
//! the stack aligned, rather than the other way round.

use core::arch::global_asm;
use core::ffi::{c_char, c_int};

global_asm!(
    ".globl _start",
    "_start:",
    "xor ebp, ebp", // mark the outermost frame
    "mov rdi, [rsp]", // argc
    "lea rsi, [rsp + 8]", // argv
    "and rsp, -16",
    "call {start}",
    start = sym start,
);

unsafe extern "C" {
    /// The program's own entry point: every program exports one with
    /// `#[unsafe(no_mangle)] pub extern "C" fn main`. One that declares no
    /// parameters simply ignores the two registers.
    fn main(argc: c_int, argv: *const *const c_char) -> c_int;
}

extern "C" fn start(argc: c_int, argv: *const *const c_char) -> ! {
    crate::exit(unsafe { main(argc, argv) })
}
