//! M10 isolation test payload: deliberately faults (a write to address
//! 0) to prove a user-mode crash ends just this task, not the kernel --
//! see the kernel's non-fatal user-mode-fault path (arch/x86_64/idt.rs).

use core::arch::asm;
use core::ffi::c_int;

use crate::Argv;

#[unsafe(no_mangle)]
pub extern "C" fn crash_main(_argc: c_int, _argv: Argv) -> c_int {
    anshi::write_all(1, b"about to write through a null pointer...\n");
    // In asm rather than through a Rust pointer: a null write is UB to
    // the compiler, which may then assume this line never runs.
    unsafe { asm!("mov dword ptr [{}], 1", in(reg) 0usize) };
    0 // unreachable
}
