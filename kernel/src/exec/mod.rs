//! Running programs: the ELF loader, the process table and scheduler,
//! pipes, and the syscalls programs make.

pub mod blobs;
pub mod elf;
pub mod pipe;
pub mod process;
pub mod syscall;

use crate::arch::x86_64::fpu;
use crate::drivers::tty::{Termios, Winsize};
use crate::fs::vfs::Vnode;
use crate::mm::vmm::AddrSpace;
use pipe::Pipe;

/// Open (non-stdio) files per task: fd = 3 + index into open_files.
pub const MAX_OPEN_FILES: usize = 8;

/// One of a task's open file descriptors.
#[derive(Clone, Copy)]
pub enum OpenFile {
    Free,
    File {
        vnode: *mut Vnode,
        offset: usize,
        writable: bool,
    },
    /// One end of a pipe() (see pipe.rs).
    Pipe {
        pipe: *mut Pipe,
        write_end: bool,
    },
    /// A network socket (net/socket.rs), by its id there.
    Socket {
        id: usize,
    },
}

/// A task's x87/SSE registers while it isn't on the CPU. FXSAVE/FXRSTOR
/// need 16-byte alignment, which this guarantees wherever it lives.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FpuState(pub [u8; fpu::STATE_SIZE]);

/// Everything needed to run one program in ring 3: its entry point, its
/// own address space, the two stacks it needs (a user stack for its own
/// code, and a kernel stack -- TSS.RSP0, used only while the CPU is
/// inside a syscall or exception taken on this task's behalf), its
/// brk-managed heap, its open files and its working directory.
#[derive(Clone, Copy)]
pub struct Usertask {
    pub entry: u64,
    pub user_stack_top: u64,
    pub kernel_stack_top: u64,
    pub aspace: AddrSpace,
    pub heap_start: u64,
    pub heap_end: u64,
    pub open_files: [OpenFile; MAX_OPEN_FILES],
    pub cwd: *mut Vnode,
    /// When set (by use_as_stdio()), fd 0/1/2 read and write these pipes
    /// instead of the physical console. Kept across exec() the same way
    /// open_files/cwd/termios are; fork() copies them like everything else
    /// here (and bumps the pipes' counts, see process::fork()).
    pub stdin_pipe: *mut Pipe,
    pub stdout_pipe: *mut Pipe,
    /// This task's console terminal mode -- kept across exec() and
    /// inherited by fork(): real Unix semantics, terminal settings belong
    /// to the terminal, not the program running on it.
    pub termios: Termios,
    /// A TIOCSWINSZ override; ws_col == 0 means "none -- report the
    /// physical console's size". Kept and inherited like termios.
    pub winsize: Winsize,
    /// Where the transition (arch/x86_64/stubs.rs) saves the kernel
    /// context this task's dispatch returns to: rbx, rbp, r12-r15, rsp.
    /// One per task, not one shared slot, since wait() dispatches a
    /// *different* task while this one is still mid-syscall -- a shared
    /// slot would be clobbered by that nested dispatch.
    pub kernel_resume: [u64; 7],
    /// Saved and restored around every dispatch (arch/x86_64/usermode.rs).
    /// A fork()'d child gets a snapshot of its parent's *live* registers,
    /// not this possibly-stale copy.
    pub fpu_state: FpuState,
}

impl Usertask {
    pub const EMPTY: Usertask = Usertask {
        entry: 0,
        user_stack_top: 0,
        kernel_stack_top: 0,
        aspace: AddrSpace { pml4_phys: 0 },
        heap_start: 0,
        heap_end: 0,
        open_files: [OpenFile::Free; MAX_OPEN_FILES],
        cwd: core::ptr::null_mut(),
        stdin_pipe: core::ptr::null_mut(),
        stdout_pipe: core::ptr::null_mut(),
        termios: Termios::ZERO,
        winsize: Winsize::ZERO,
        kernel_resume: [0; 7],
        fpu_state: FpuState([0; fpu::STATE_SIZE]),
    };
}
