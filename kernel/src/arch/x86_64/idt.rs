//! The IDT: CPU exceptions (vectors 0-31), the 16 legacy IRQs (32-47,
//! where pic.rs remaps them) and the syscall gate (0x80), all entering
//! through stubs.rs.

use core::arch::asm;

use super::gdt::SEL_KCODE;
use super::pic::{self, PIC_IRQ_BASE};
use super::{halt_forever, lapic, read_cr2};
use crate::drivers::timer;
use crate::exec::process::{self, State};
use crate::sync::Global;

/// Register state as the entry stubs push it, in the order the handlers
/// see it: the general-purpose registers (rax lowest, pushed last), the
/// vector and error code the stub pushed, then what the CPU pushed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InterruptFrame {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl InterruptFrame {
    /// Whether the interrupted code was running in ring 3.
    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

const VECTOR_SYSCALL: usize = 0x80;

static IDT: Global<[IdtEntry; 256]> = Global::new(
    [IdtEntry { offset_low: 0, selector: 0, ist: 0, type_attr: 0, offset_mid: 0, offset_high: 0, zero: 0 }; 256],
);
static IRQ_HANDLERS: Global<[Option<fn()>; 16]> = Global::new([None; 16]);

unsafe extern "C" {
    // stubs.rs
    static anshi_isr_table: [u64; 32];
    static anshi_irq_table: [u64; 16];
    fn anshi_syscall_entry();
}

fn set_gate(vector: usize, handler: u64, type_attr: u8) {
    unsafe {
        IDT.get()[vector] = IdtEntry {
            offset_low: handler as u16,
            selector: SEL_KCODE,
            ist: 0,
            type_attr,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            zero: 0,
        }
    };
}

pub fn init() {
    // Present, DPL0, 64-bit interrupt gate (0x8E); IST unused.
    for (vector, &stub) in unsafe { anshi_isr_table.iter() }.enumerate() {
        set_gate(vector, stub, 0x8E);
    }
    for (irq, &stub) in unsafe { anshi_irq_table.iter() }.enumerate() {
        set_gate(PIC_IRQ_BASE as usize + irq, stub, 0x8E);
    }
    // Present, DPL3 (callable via `int 0x80` from ring 3), 64-bit interrupt
    // gate (0xEE) -- the one deliberately non-DPL0 gate in the table.
    set_gate(VECTOR_SYSCALL, anshi_syscall_entry as *const () as u64, 0xEE);

    let idtr = Idtr { limit: (size_of::<[IdtEntry; 256]>() - 1) as u16, base: IDT.as_ptr() as u64 };
    unsafe { asm!("lidt [{}]", in(reg) &idtr, options(readonly, nostack, preserves_flags)) };
}

/// Registers a handler for hardware IRQ `irq` (0-15, pre-remap numbering).
/// The EOI is sent after the handler returns; handlers must not send it.
/// Unregistered IRQs are silently EOI'd and ignored -- a safe default for
/// spurious interrupts.
pub fn irq_register(irq: u8, handler: fn()) {
    if let Some(slot) = unsafe { IRQ_HANDLERS.get() }.get_mut(irq as usize) {
        *slot = Some(handler);
    }
}

const EXCEPTION_NAMES: [&str; 32] = [
    "Divide-by-zero",
    "Debug",
    "NMI",
    "Breakpoint",
    "Overflow",
    "Bound Range Exceeded",
    "Invalid Opcode",
    "Device Not Available",
    "Double Fault",
    "Reserved",
    "Invalid TSS",
    "Segment Not Present",
    "Stack-Segment Fault",
    "General Protection Fault",
    "Page Fault",
    "Reserved",
    "x87 FPU Error",
    "Alignment Check",
    "Machine Check",
    "SIMD FP Exception",
    "Virtualization Exception",
    "Control Protection Exception",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Hypervisor Injection",
    "VMM Communication",
    "Security Exception",
    "Reserved",
];

fn exception_name(vector: u64) -> &'static str {
    EXCEPTION_NAMES.get(vector as usize).copied().unwrap_or("Unknown")
}

/// Every CPU exception lands here. A fault taken from ring 3 doesn't
/// bring down the kernel: it ends *that* process -- a zombie, exactly as
/// if it had called exit(), so the scheduler reaps it and a top-level
/// scheduler::run_until() caller (the shell's `run`) doesn't hang waiting
/// for a slot that will never become a zombie any other way -- and returns
/// to the scheduler. A fault from ring 0 is a kernel bug, and fatal.
pub extern "C" fn isr_handler(frame: *mut InterruptFrame) {
    let f = unsafe { &*frame };
    if f.from_user() {
        kprintln!(
            "\nuser program crashed: {} (vector {}, error 0x{:x}) at rip=0x{:x}",
            exception_name(f.vector),
            f.vector,
            f.error_code,
            f.rip
        );
        if let Some(me) = process::current() {
            process::close_files(me);
            me.state = State::Zombie;
            me.exit_status = -1;
        }
        super::usermode::return_to_kernel(-1);
    }

    kprintln!(
        "\n--- CPU EXCEPTION: {} (vector {}, error 0x{:x}) ---",
        exception_name(f.vector),
        f.vector,
        f.error_code
    );
    if f.vector == 14 {
        kprintln!("faulting address (cr2)=0x{:x}", read_cr2());
    }
    kprintln!("rip=0x{:x} cs=0x{:x} rflags=0x{:x} rsp=0x{:x} ss=0x{:x}", f.rip, f.cs, f.rflags, f.rsp, f.ss);
    kprintln!("rax=0x{:x} rbx=0x{:x} rcx=0x{:x} rdx=0x{:x}", f.rax, f.rbx, f.rcx, f.rdx);
    kprintln!("rsi=0x{:x} rdi=0x{:x} rbp=0x{:x}", f.rsi, f.rdi, f.rbp);
    kprintln!("r8=0x{:x} r9=0x{:x} r10=0x{:x} r11=0x{:x}", f.r8, f.r9, f.r10, f.r11);
    kprintln!("r12=0x{:x} r13=0x{:x} r14=0x{:x} r15=0x{:x}", f.r12, f.r13, f.r14, f.r15);
    halt_forever();
}

/// Every hardware IRQ lands here: the registered handler, then the EOI.
///
/// M13 Phase B preemption: if this was the tick interrupting ring-3 code,
/// the CPU goes to another runnable process instead of back to this one.
/// That happens *after* the handler and EOI -- the tick still counts every
/// time, and skipping the EOI would leave IRQ0 "in service", blocking
/// every later tick. The preempted process isn't finished the way an
/// exit or a crash is: its whole register state is already on its own
/// kernel stack, exactly where `frame` points, so resuming it later only
/// needs that address and the Runnable state.
pub extern "C" fn irq_handler(frame: *mut InterruptFrame) {
    let f = unsafe { &*frame };
    let irq = f.vector.wrapping_sub(PIC_IRQ_BASE as u64);
    if irq < 16
        && let Some(handler) = unsafe { IRQ_HANDLERS.get() }[irq as usize]
    {
        handler();
    }
    if irq == 0 && timer::uses_lapic() {
        lapic::eoi(); // The tick came from the Local APIC timer, not the 8259.
    } else {
        pic::send_eoi(irq as u8);
    }

    if irq == 0
        && f.from_user()
        && let Some(me) = process::current()
    {
        me.saved_rsp = frame as u64;
        me.state = State::Runnable;
        super::usermode::return_to_kernel(0); // Back to the scheduler loop.
    }
}
