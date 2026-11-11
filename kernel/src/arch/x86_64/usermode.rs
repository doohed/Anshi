//! Running a program in ring 3, on top of the transition in stubs.rs:
//! switch into its address space, load its FPU state and kernel stack,
//! and enter (or resume) it -- "blocking" from the caller's point of
//! view until it exits, faults, makes a blocking call, or is preempted.

use core::sync::atomic::{AtomicI32, Ordering};

use super::{fpu, gdt};
use crate::exec::Usertask;
use crate::mm::vmm;
use crate::sync::Global;

/// What return_to_kernel() was given -- written by the assembly just
/// before it jumps back.
pub static KERNEL_RESUME_STATUS: AtomicI32 = AtomicI32::new(0);

unsafe extern "C" {
    // stubs.rs
    fn anshi_enter_usermode(entry: u64, user_rsp: u64, save_area: *mut u64);
    fn anshi_resume_process(saved_rsp: u64, save_area: *mut u64);
    fn anshi_return_to_kernel(status: i32, save_area: *mut u64) -> !;
}

/// The task whose context ring 3 is executing in (or whose kernel-side
/// call most recently returned into) at the *innermost* active dispatch;
/// null if none. dispatch() saves and restores it around its own call, so
/// it unwinds correctly across nested dispatches (wait() running another
/// process from inside a syscall).
static CURRENT_TASK: Global<*mut Usertask> = Global::new(core::ptr::null_mut());

/// The running task: what the syscall handlers act on.
pub fn current_task() -> Option<&'static mut Usertask> {
    unsafe { (*CURRENT_TASK.get()).as_mut() }
}

/// Switches into `t`'s address space and enters it (fresh) or resumes it
/// from `saved_rsp`; returns, with the status return_to_kernel() was
/// given, once it stops. Restores the caller's address space and current
/// task first, so it's safe to call again for a different task before an
/// earlier call has returned -- wait() does exactly that.
fn dispatch(t: *mut Usertask, fresh: bool, saved_rsp: u64) -> i32 {
    let caller_as = vmm::current_address_space();
    let outer = unsafe { *CURRENT_TASK.get() };
    let task = unsafe { &mut *t };

    // The x87/SSE registers hold the outer task's live state right now, if
    // there is one (it's mid-syscall, e.g. in wait(), and kernel code never
    // touches them), so park it before loading `t`'s.
    if let Some(o) = unsafe { outer.as_mut() } {
        fpu::save(o.fpu_state.0.as_mut_ptr());
    }
    fpu::restore(task.fpu_state.0.as_ptr());

    gdt::set_kernel_stack(task.kernel_stack_top);
    vmm::switch(&task.aspace);
    unsafe { *CURRENT_TASK.get() = t };

    let save_area = task.kernel_resume.as_mut_ptr();
    unsafe {
        if fresh {
            anshi_enter_usermode(task.entry, task.user_stack_top, save_area);
        } else {
            anshi_resume_process(saved_rsp, save_area);
        }
    }
    // Back here, via return_to_kernel()'s `ret`, once the task exits,
    // blocks, or is preempted. Whatever stopped it left its registers
    // untouched, so they're still its own. CR3 is still the task's.
    let task = unsafe { &mut *t };
    fpu::save(task.fpu_state.0.as_mut_ptr());
    if let Some(o) = unsafe { outer.as_mut() } {
        fpu::restore(o.fpu_state.0.as_ptr());
    }
    // Restore, not just clear: a task can call wait(), which dispatches a
    // *different* task through a recursive call while this one's own
    // dispatch is still on the stack -- when that nested one returns here,
    // the current task has to go back to this one.
    unsafe { *CURRENT_TASK.get() = outer };
    vmm::switch(&caller_as);
    KERNEL_RESUME_STATUS.load(Ordering::Relaxed)
}

/// Launches `t` at its entry point; its exit status.
pub fn enter(t: *mut Usertask) -> i32 {
    dispatch(t, true, 0)
}

/// Resumes a task that has run before, from `saved_rsp` -- the address of
/// its own InterruptFrame, captured wherever it last stopped.
pub fn resume(t: *mut Usertask, saved_rsp: u64) -> i32 {
    dispatch(t, false, saved_rsp)
}

/// Abandons the ring-3 context currently running (there is always exactly
/// one, while inside a dispatch) and resumes the kernel context that
/// dispatched it, as if that call had just returned with `status`. Called
/// by exit(), execve(), sched_yield(), preemption and the user-fault path.
/// Nothing on the abandoned stack is dropped, so callers let go of
/// anything that owns memory first.
pub fn return_to_kernel(status: i32) -> ! {
    let t = current_task().expect("return_to_kernel() outside a dispatch");
    unsafe { anshi_return_to_kernel(status, t.kernel_resume.as_mut_ptr()) }
}
