//! The assembly the kernel can't do without: one entry stub per CPU
//! exception (vectors 0-31), hardware IRQ (32-47) and the syscall gate
//! (0x80), all pushing the same InterruptFrame (idt.rs) and calling into
//! Rust; plus the hand-written ring-0 <-> ring-3 transition usermode.rs
//! is built on -- an iretq frame and a bespoke one-shot context switch
//! that Rust can't express.

use core::arch::global_asm;

use super::idt::{irq_handler, isr_handler};
use super::usermode::KERNEL_RESUME_STATUS;
use crate::exec::syscall::syscall_dispatch;

// Exception stubs. Vectors that don't push a hardware error code get a
// dummy 0 pushed, so the frame layout is the same whichever vector fired.
// See the Intel SDM Vol. 3, on exception and interrupt vectors, for which
// ones push a code: 8, 10-14, 17, 21 and 30.
//
// The register block is pushed r15 first, rax last: `push` stores at the
// new (decremented) %rsp, so rax ends up lowest, at offset 0 -- matching
// InterruptFrame's field order.
global_asm!(
    r#"
    .text
    .macro isr_no_err vec
    anshi_isr_\vec:
        push $0
        push $\vec
        jmp anshi_isr_common
    .endm
    .macro isr_err vec
    anshi_isr_\vec:
        push $\vec
        jmp anshi_isr_common
    .endm

    .irp v, 0,1,2,3,4,5,6,7,9,15,16,18,19,20,22,23,24,25,26,27,28,29,31
    isr_no_err \v
    .endr
    .irp v, 8,10,11,12,13,14,17,21,30
    isr_err \v
    .endr

    .macro push_registers
        push %r15
        push %r14
        push %r13
        push %r12
        push %r11
        push %r10
        push %r9
        push %r8
        push %rbp
        push %rdi
        push %rsi
        push %rdx
        push %rcx
        push %rbx
        push %rax
    .endm

    anshi_isr_common:
        push_registers
        mov %rsp, %rdi
        call {isr_handler}
        jmp anshi_return_from_interrupt

    // Hardware IRQs never push an error code.
    .macro irq_stub vec
    anshi_irq_\vec:
        push $0
        push $\vec
        jmp anshi_irq_common
    .endm
    .irp v, 32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
    irq_stub \v
    .endr

    anshi_irq_common:
        push_registers
        mov %rsp, %rdi
        call {irq_handler}
        jmp anshi_return_from_interrupt

    // The syscall gate. syscall_dispatch() writes its result into the
    // frame's rax -- the same slot `pop %rax` below restores from.
    .global anshi_syscall_entry
    anshi_syscall_entry:
        push $0
        push $0x80
        push_registers
        mov %rsp, %rdi
        call {syscall_dispatch}
        jmp anshi_return_from_interrupt

    // Shared by every stub above, and by anshi_resume_process below --
    // restoring a process's saved %rsp and falling in here is exactly how
    // a preempted, forked or blocked process resumes, since its full
    // register state is already on its own kernel stack in this layout.
    anshi_return_from_interrupt:
        pop %rax
        pop %rbx
        pop %rcx
        pop %rdx
        pop %rsi
        pop %rdi
        pop %rbp
        pop %r8
        pop %r9
        pop %r10
        pop %r11
        pop %r12
        pop %r13
        pop %r14
        pop %r15
        add $16, %rsp // vector + error code
        iretq

    .pushsection .rodata
    .balign 8
    .global anshi_isr_table
    anshi_isr_table:
    .irp v, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        .quad anshi_isr_\v
    .endr
    .global anshi_irq_table
    anshi_irq_table:
    .irp v, 32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
        .quad anshi_irq_\v
    .endr
    .popsection
    "#,
    isr_handler = sym isr_handler,
    irq_handler = sym irq_handler,
    syscall_dispatch = sym syscall_dispatch,
    options(att_syntax),
);

// The ring-3 transition. Selectors match gdt.rs's table, RPL 3 baked in.
global_asm!(
    r#"
    .text
    // anshi_enter_usermode(entry, user_rsp, save_area)
    //
    // Saves the kernel's callee-saved registers and rsp into `save_area`
    // (7 quads: rbx, rbp, r12-r15, rsp -- a hand-rolled setjmp, one slot
    // per task, see Usertask::kernel_resume), then builds an iretq frame
    // and jumps into ring 3. Never returns directly; the only way back is
    // anshi_return_to_kernel's `ret`, landing at this call's return
    // address as if it had returned normally.
    .global anshi_enter_usermode
    anshi_enter_usermode:
        mov %rbx, 0(%rdx)
        mov %rbp, 8(%rdx)
        mov %r12, 16(%rdx)
        mov %r13, 24(%rdx)
        mov %r14, 32(%rdx)
        mov %r15, 40(%rdx)
        mov %rsp, 48(%rdx) // points at our own return address right now

        // iretq pops rip, cs, rflags, rsp, ss -- pushed in reverse.
        pushq $0x23         // user data selector
        pushq %rsi          // user_rsp
        pushq $0x202        // rflags: reserved bit 1, IF=1
        pushq $0x1B         // user code selector
        pushq %rdi          // entry
        iretq

    // anshi_return_to_kernel(status, save_area)
    //
    // Restores the context anshi_enter_usermode/anshi_resume_process
    // saved into `save_area` and resumes it via `ret` -- to their caller
    // it looks as if they just returned. Never returns to its own caller.
    .global anshi_return_to_kernel
    anshi_return_to_kernel:
        mov %edi, {status}(%rip)
        mov 0(%rsi), %rbx
        mov 8(%rsi), %rbp
        mov 16(%rsi), %r12
        mov 24(%rsi), %r13
        mov 32(%rsi), %r14
        mov 40(%rsi), %r15
        mov 48(%rsi), %rsp
        ret

    // anshi_resume_process(saved_rsp, save_area)
    //
    // Counterpart to anshi_enter_usermode for a process that has already
    // run (M13): resumes it exactly where it left off. `saved_rsp` is the
    // address of its own already-populated InterruptFrame, on its own
    // kernel stack exactly as the entry stubs left it (by fork() building
    // its child's first frame, a timer preemption, or sched_yield). Saves
    // the kernel context first, exactly like anshi_enter_usermode.
    .global anshi_resume_process
    anshi_resume_process:
        mov %rbx, 0(%rsi)
        mov %rbp, 8(%rsi)
        mov %r12, 16(%rsi)
        mov %r13, 24(%rsi)
        mov %r14, 32(%rsi)
        mov %r15, 40(%rsi)
        mov %rsp, 48(%rsi)
        mov %rdi, %rsp
        jmp anshi_return_from_interrupt
    "#,
    status = sym KERNEL_RESUME_STATUS,
    options(att_syntax),
);
