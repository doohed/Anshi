//! The process table and the round-robin scheduler. A process is a
//! Usertask plus its pid, parent and state. There's no init process:
//! the convention is that a process which fork()s a child waits for it
//! before exiting, and the kernel itself (the shell, or kmain booting sh)
//! waits for the processes it launches.

use super::elf;
use super::pipe;
use super::{OpenFile, Usertask};
use crate::arch::x86_64::idt::InterruptFrame;
use crate::arch::x86_64::{fpu, gdt, usermode, wait_for_interrupt};
use crate::fs::vfs::Vnode;
use crate::mm::vmm::{self, AddrSpace};
use crate::net;
use crate::sync::Global;

pub const MAX_PROCESSES: usize = 16;

/// The parent of a process the kernel launched itself (the shell's `run`,
/// or kmain booting sh) rather than one fork()ed by another process:
/// nobody will wait() for it but the top-level run_until(None) loop.
pub const KERNEL_PARENT_PID: i32 = -1;

#[derive(Clone, Copy, PartialEq)]
pub enum State {
    Unused,
    Runnable,
    Running,
    Zombie,
}

pub struct Process {
    pub pid: i32,
    pub parent_pid: i32,
    pub state: State,
    pub exit_status: i32,
    /// Whether it has been dispatched before: false means its next
    /// dispatch is a fresh launch at task.entry; true means resuming from
    /// saved_rsp -- true for a fork()'d child from the start, and for any
    /// process once it's been preempted or has yielded.
    pub has_run: bool,
    pub saved_rsp: u64,
    pub task: Usertask,
    /// A previous exec() left this process's *old* address space and
    /// kernel stack behind to free later: they can't be freed the moment
    /// they're replaced, since exec() is still running on the old kernel
    /// stack then. free_pending() frees them at the start of the next
    /// dispatch, once it's definitely off both.
    pending_free_as: Option<AddrSpace>,
    pending_free_kstack: Option<u64>,
}

impl Process {
    const EMPTY: Process = Process {
        pid: 0,
        parent_pid: 0,
        state: State::Unused,
        exit_status: 0,
        has_run: false,
        saved_rsp: 0,
        task: Usertask::EMPTY,
        pending_free_as: None,
        pending_free_kstack: None,
    };
}

struct Table {
    processes: [Process; MAX_PROCESSES],
    next_pid: i32,
    /// The process inside a dispatch somewhere in the current call chain.
    current: Option<usize>,
    /// Where the round-robin search starts next.
    last_picked: usize,
}

static TABLE: Global<Table> = Global::new(Table {
    processes: [Process::EMPTY; MAX_PROCESSES],
    next_pid: 1,
    current: None,
    last_picked: MAX_PROCESSES - 1,
});

// The table is reached from re-entrant paths -- a syscall running inside
// a dispatch the scheduler started, possibly recursing into the scheduler
// for wait() -- so nothing holds on to a reference into it across a
// dispatch: each access goes through these short-lived ones.
fn table() -> &'static mut Table {
    unsafe { TABLE.get() }
}

fn slot(i: usize) -> &'static mut Process {
    &mut table().processes[i]
}

pub fn by_pid(pid: i32) -> Option<&'static mut Process> {
    table().processes.iter_mut().find(|p| p.state != State::Unused && p.pid == pid)
}

/// The process inside a dispatch somewhere in the current call chain.
/// Unlike usermode::current_task(), this can be briefly stale while a
/// nested run_until() unwinds back to an outer one; nothing relies on it
/// then.
pub fn current() -> Option<&'static mut Process> {
    table().current.map(slot)
}

/// Whether any live process's working directory is `dir` or inside it --
/// whether removing `dir` would leave a process holding a freed vnode.
pub fn cwd_within(dir: *const Vnode) -> bool {
    table().processes.iter().filter(|p| p.state != State::Unused).any(|p| {
        let mut v = p.task.cwd as *const Vnode;
        while !v.is_null() {
            if v == dir {
                return true;
            }
            v = unsafe { (*v).parent };
        }
        false
    })
}

/// A child of `parent_pid`: exactly `pid` if given, otherwise any --
/// preferring an exited one, so wait()-for-any completes at once if a
/// child already has. None means "no such child at all" (wait()'s error),
/// not "found one, still running", which the caller tells from its state.
pub fn find_child(parent_pid: i32, pid: Option<i32>) -> Option<&'static mut Process> {
    if let Some(pid) = pid {
        return by_pid(pid).filter(|p| p.parent_pid == parent_pid);
    }
    let is_child = |p: &Process| p.state != State::Unused && p.parent_pid == parent_pid;
    let all = &table().processes;
    let i =
        all.iter().position(|p| is_child(p) && p.state == State::Zombie).or_else(|| all.iter().position(is_child))?;
    Some(slot(i))
}

fn alloc_slot() -> Option<usize> {
    table().processes.iter().position(|p| p.state == State::Unused)
}

fn next_pid() -> i32 {
    let t = table();
    t.next_pid += 1;
    t.next_pid - 1
}

/// Loads `image` into a new process with `parent_pid` as its parent. The
/// new pid, or None (after saying why).
pub fn spawn(image: &[u8], argv: &[&[u8]], cwd: *mut Vnode, parent_pid: i32) -> Option<i32> {
    let Some(i) = alloc_slot() else {
        kprintln!("process: too many processes (max {MAX_PROCESSES})");
        return None;
    };
    let task = elf::load(image, argv, &[], cwd)?;
    let p = slot(i);
    *p = Process { pid: next_pid(), parent_pid, state: State::Runnable, task, ..Process::EMPTY };
    Some(p.pid)
}

/// Closes everything `p` has open -- its fds' pipe ends and sockets, and
/// its terminal pipes -- done wherever a process becomes a zombie (exit(),
/// a crash). A pipeline's reader sees EOF the moment its writer is gone,
/// a pane's shell exiting shows up promptly to whoever reads its output,
/// and a connection whose program is gone doesn't stay half-open.
pub fn close_files(p: &mut Process) {
    for f in p.task.stdio.iter_mut().chain(p.task.open_files.iter_mut()) {
        f.release();
        *f = OpenFile::Free;
    }
    if !p.task.tty_in.is_null() {
        pipe::close_read(p.task.tty_in);
        p.task.tty_in = core::ptr::null_mut();
    }
    if !p.task.tty_out.is_null() {
        pipe::close_write(p.task.tty_out);
        p.task.tty_out = core::ptr::null_mut();
    }
}

/// fork(): a new process with a full copy of `parent`'s address space (no
/// copy-on-write), heap range, open files and cwd, Runnable, whose first
/// resume is an exact copy of `frame` (the parent's own syscall frame)
/// except rax=0 -- so the child "returns" 0 from fork(), right after the
/// same `int 0x80` the parent used. The child's pid, or None.
pub fn fork(parent: &mut Process, frame: &InterruptFrame) -> Option<i32> {
    let i = alloc_slot()?;
    // Open files are copied as they are, sharing the same vnodes -- each
    // file's offset is copied, not the "shared open file description" a
    // real fork() gives, a deliberate simplification. The address space is
    // replaced below with a real clone.
    let mut task = parent.task;
    // parent.task's FPU copy is only refreshed when the parent is switched
    // away from; it's mid-syscall now, so its live state is in the
    // registers.
    fpu::save(task.fpu_state.0.as_mut_ptr());

    // Pipes need a second step: unlike a vnode, a pipe's open/closed state
    // is shared kernel state -- every pipe entry the child inherited is a
    // genuinely new reference, and the pipe's count has to say so, or it
    // will think it has fewer live references than it really does the
    // moment either process closes its copy. Sockets are shared the same way.
    for f in task.stdio.iter().chain(task.open_files.iter()) {
        f.add_ref();
    }
    if let Some(p) = unsafe { task.tty_in.as_mut() } {
        p.read_refs += 1;
    }
    if let Some(p) = unsafe { task.tty_out.as_mut() } {
        p.write_refs += 1;
    }

    task.aspace = vmm::new_address_space();
    if !vmm::clone_user_pages(&task.aspace, &parent.task.aspace) {
        vmm::free_user_pages(&task.aspace); // Don't leak the partial clone.
        return None;
    }
    let Some(kstack_top) = elf::alloc_kernel_stack() else {
        vmm::free_user_pages(&task.aspace);
        return None;
    };
    task.kernel_stack_top = kstack_top;

    // The child's first frame, at the top of its own kernel stack.
    let child_frame = (kstack_top - size_of::<InterruptFrame>() as u64) as *mut InterruptFrame;
    unsafe { child_frame.write(InterruptFrame { rax: 0, ..*frame }) };

    let pid = next_pid();
    *slot(i) = Process {
        pid,
        parent_pid: parent.pid,
        state: State::Runnable,
        has_run: true,
        saved_rsp: child_frame as u64,
        task,
        ..Process::EMPTY
    };
    Some(pid)
}

/// exec(): replaces `p`'s program in place with `image` -- same pid, same
/// open files, cwd, terminal settings and stdio pipes, parent unchanged.
/// The next dispatch is a fresh launch at the new entry point. False (the
/// old program carries on, as after any failed syscall) if `image` won't
/// load.
pub fn exec(p: &mut Process, image: &[u8], argv: &[&[u8]], envp: &[&[u8]]) -> bool {
    let old = p.task;
    let Some(mut task) = elf::load(image, argv, envp, old.cwd) else { return false };

    // The old address space and kernel stack can't be freed *here* -- this
    // is still running on that kernel stack, and CR3 may still be that
    // address space. free_pending() frees them later.
    p.pending_free_as = Some(old.aspace);
    p.pending_free_kstack = Some(old.kernel_stack_top);

    task.stdio = old.stdio;
    task.open_files = old.open_files;
    task.cwd = old.cwd;
    // Terminal settings belong to the terminal, not the program running on
    // it -- real Unix exec() doesn't reset them either.
    task.termios = old.termios;
    task.winsize = old.winsize;
    // M19: a pane's pipe-backed terminal must survive its own execve()
    // into the real pane program.
    task.tty_in = old.tty_in;
    task.tty_out = old.tty_out;
    // kernel_resume is *live*: the return_to_kernel() the caller makes
    // right after this returns reads it to unwind to whichever dispatch
    // launched this instance of `p`.
    task.kernel_resume = old.kernel_resume;
    p.task = task;
    // Load the new image's clean FPU state into the live registers now:
    // the dispatch saves whatever is live into the task the moment it
    // unwinds, which would otherwise carry the old program's registers
    // into the new one.
    fpu::restore(p.task.fpu_state.0.as_ptr());
    p.has_run = false; // Next dispatch is a fresh launch at the new entry.
    // Was Running; the caller is about to abandon this dispatch with
    // return_to_kernel() -- without this, it would never be picked again.
    p.state = State::Runnable;
    true
}

/// Frees whatever an earlier exec() left pending (see Process). Safe to
/// call any time the CPU is off the process's old stack and address
/// space -- a no-op for a process that never exec()'d.
fn free_pending(p: &mut Process) {
    if let Some(aspace) = p.pending_free_as.take() {
        vmm::free_user_pages(&aspace);
    }
    if let Some(top) = p.pending_free_kstack.take() {
        elf::free_kernel_stack(top);
    }
}

/// Frees a zombie's address space and kernel stack and returns its slot.
/// Only ever called once it's a zombie, which -- by the dispatch's own
/// ordering -- means CR3 and TSS.RSP0 have already moved off it.
fn reap(i: usize) {
    let p = slot(i);
    free_pending(p);
    vmm::free_user_pages(&p.task.aspace);
    elf::free_kernel_stack(p.task.kernel_stack_top);
    p.state = State::Unused;
}

fn pick_next_runnable() -> Option<usize> {
    let t = table();
    let i = (1..=MAX_PROCESSES)
        .map(|k| (t.last_picked + k) % MAX_PROCESSES)
        .find(|&i| t.processes[i].state == State::Runnable)?;
    t.last_picked = i;
    Some(i)
}

/// Runs processes, round-robin among the runnable ones, until:
///
/// - `wait_pid` is Some: that process exits -- it's reaped and its exit
///   status returned (-1 if there's no such process). Called by wait() as
///   an ordinary, *recursive* call from deep inside another process's own
///   syscall.
/// - `wait_pid` is None: the table is empty. Any zombie met along the way
///   is reaped at once (with no init process to reparent orphans to, the
///   only zombie this should see is its own top-level launch). Returns 0.
///   The shell's `run` uses it to wait for a whole process family.
pub fn run_until(wait_pid: Option<i32>) -> i32 {
    let outer = table().current;
    let result = loop {
        // Between any two dispatches: at most one tick apart, which is
        // what keeps connections moving while programs run.
        net::poll();
        if let Some(pid) = wait_pid {
            let Some(target) = table().processes.iter().position(|p| p.state != State::Unused && p.pid == pid) else {
                break -1;
            };
            if slot(target).state == State::Zombie {
                let status = slot(target).exit_status;
                reap(target);
                break status;
            }
        } else {
            let mut any_alive = false;
            for i in 0..MAX_PROCESSES {
                match slot(i).state {
                    State::Zombie => reap(i),
                    State::Unused => {}
                    _ => any_alive = true,
                }
            }
            if !any_alive {
                break 0;
            }
        }

        let Some(next) = pick_next_runnable() else {
            // Nothing runnable right now: wait for an interrupt rather than
            // spin.
            wait_for_interrupt();
            continue;
        };
        // Anything a previous exec() left pending is safe to free here: the
        // CPU is on the scheduler's own stack and address space.
        free_pending(slot(next));

        let p = slot(next);
        p.state = State::Running;
        table().current = Some(next);
        let task: *mut Usertask = &mut p.task;
        if p.has_run {
            usermode::resume(task, p.saved_rsp);
        } else {
            p.has_run = true;
            usermode::enter(task);
        }
        // Whatever stopped `next` already updated its state (Zombie, or
        // Runnable after exec()/preemption/yield) before returning to the
        // kernel; the checks at the top notice it.
    };

    table().current = outer;
    // TSS.RSP0 was left pointing at whichever process ran last (possibly
    // several dispatches deep) -- point it back at the one this call is
    // unwinding into, if any (None is the kernel itself, which never takes
    // a ring-3 interrupt). Otherwise that process's next syscall builds its
    // frame on the wrong kernel stack.
    if let Some(i) = outer {
        gdt::set_kernel_stack(slot(i).task.kernel_stack_top);
    }
    result
}
