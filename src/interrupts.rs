//! CPU exceptions, the legacy 8259 PICs and the 8254 PIT timer.
//!
//! The kernel installs its own GDT with a TSS so that a double fault runs on a
//! separate known-good stack (IST1). Vectors 0..=31 are CPU exceptions,
//! 32..=47 are the remapped PIC IRQs, 48 is the kernel task yield and 128 is
//! the ring 3 system-call gate. Every stub
//! saves the general registers and calls `tane_interrupt_dispatch` with a
//! pointer to the saved frame. The dispatcher returns the frame to resume:
//! returning another task's saved frame is the context switch.

use core::arch::{asm, global_asm};
use core::fmt::Write;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, Ordering};

pub const TIMER_HZ: u64 = 100;
const PIT_HZ: u64 = 1_193_182;
const PIT_DIVISOR: u64 = (PIT_HZ + TIMER_HZ / 2) / TIMER_HZ;
const IRQ_BASE: u64 = 32;
const IRQ_TIMER: u64 = 0;
const IRQ_KEYBOARD: u64 = 1;
const IRQ_COM1: u64 = 4;
const YIELD_VECTOR: u64 = 48;
const SYSCALL_VECTOR: u64 = 128;

const KERNEL_CODE: u16 = 0x08;
const KERNEL_DATA: u16 = 0x10;
const TSS_SELECTOR: u16 = 0x18;
const USER_DATA: u16 = 0x2b;
const USER_CODE: u16 = 0x33;
const DOUBLE_FAULT_IST: u8 = 1;
const TSS_BYTES: u64 = 104;

static TICKS: AtomicU64 = AtomicU64::new(0);

#[repr(C, align(16))]
struct Gdt([u64; 7]);

// 0x08: ring 0 code, 0x10: ring 0 data, 0x18: TSS (two slots),
// 0x28: ring 3 data, 0x30: 64-bit ring 3 code. Selector RPL is added separately.
static mut GDT: Gdt = Gdt([
    0, 0x00af9a000000ffff, 0x00cf92000000ffff, 0, 0,
    0x00cff2000000ffff, 0x00affa000000ffff,
]);

// The 104-byte 64-bit TSS as 32-bit words: RSP0 at word 1, IST1 at word 9.
#[repr(C, align(16))]
struct Tss([u32; 26]);
static mut TSS: Tss = Tss([0; 26]);

#[repr(C, align(16))]
struct Stack([u8; 16384]);
static mut DOUBLE_FAULT_STACK: Stack = Stack([0; 16384]);

#[repr(C, align(16))]
struct Idt([[u64; 2]; 256]);
static mut IDT: Idt = Idt([[0; 2]; 256]);

#[repr(C, packed)]
struct DescriptorPointer {
    limit: u16,
    base: u64,
}

extern "C" {
    static tane_isr_table: [u64; 49];
    static isr_128: u8;
}

/// Registers saved by `isr_common`, lowest address first.
#[repr(C)]
#[derive(Default)]
pub struct Frame {
    pub(crate) r15: u64, pub(crate) r14: u64, pub(crate) r13: u64, pub(crate) r12: u64,
    pub(crate) r11: u64, pub(crate) r10: u64, pub(crate) r9: u64, pub(crate) r8: u64,
    pub(crate) rbp: u64, pub(crate) rdi: u64, pub(crate) rsi: u64, pub(crate) rdx: u64,
    pub(crate) rcx: u64, pub(crate) rbx: u64, pub(crate) rax: u64,
    pub(crate) vector: u64,
    pub(crate) error: u64,
    pub(crate) rip: u64,
    pub(crate) cs: u64,
    pub(crate) rflags: u64,
    pub(crate) rsp: u64,
    pub(crate) ss: u64,
}

impl Frame {
    /// A frame that `iretq` turns into a call of `entry(argument)` on a new
    /// stack, with interrupts enabled.
    pub fn task(entry: extern "C" fn(u64) -> !, argument: u64, rsp: u64) -> Self {
        Frame {
            rdi: argument,
            rip: entry as usize as u64,
            cs: KERNEL_CODE as u64,
            rflags: 0x202,
            rsp,
            ss: KERNEL_DATA as u64,
            ..Frame::default()
        }
    }

    /// Enter a user image with a literal argument buffer in RDI/RSI. IF is
    /// enabled, IOPL remains zero, and no inherited kernel register survives.
    pub fn user(entry: u64, rsp: u64, argptr: u64, arglen: u64) -> Self {
        Frame {
            rdi: argptr,
            rsi: arglen,
            rip: entry,
            cs: USER_CODE as u64,
            rflags: 0x202,
            rsp,
            ss: USER_DATA as u64,
            ..Frame::default()
        }
    }

    pub fn user_mode(&self) -> bool {
        self.cs & 3 == 3
    }
}

// isr_common uses the CS offset below before popping the saved registers.
const _: () = assert!(core::mem::size_of::<Frame>() == 22 * 8);
const _: () = assert!(core::mem::offset_of!(Frame, cs) == 18 * 8);

// Vectors whose CPU-pushed error code is kept; the others push a zero.
global_asm!(r#"
.section .text.tane_isr, "ax"
.macro ISR_NOERR n
isr_\n:
    push 0
    push \n
    jmp isr_common
.endm
.macro ISR_ERR n
isr_\n:
    push \n
    jmp isr_common
.endm

ISR_NOERR 0
ISR_NOERR 1
ISR_NOERR 2
ISR_NOERR 3
ISR_NOERR 4
ISR_NOERR 5
ISR_NOERR 6
ISR_NOERR 7
ISR_ERR   8
ISR_NOERR 9
ISR_ERR   10
ISR_ERR   11
ISR_ERR   12
ISR_ERR   13
ISR_ERR   14
ISR_NOERR 15
ISR_NOERR 16
ISR_ERR   17
ISR_NOERR 18
ISR_NOERR 19
ISR_NOERR 20
ISR_ERR   21
ISR_NOERR 22
ISR_NOERR 23
ISR_NOERR 24
ISR_NOERR 25
ISR_NOERR 26
ISR_NOERR 27
ISR_NOERR 28
ISR_ERR   29
ISR_ERR   30
ISR_NOERR 31
.irp n, 32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48
ISR_NOERR \n
.endr
ISR_NOERR 128
.global isr_128

isr_common:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    /* Long mode aligns RSP, then pushes SS/RSP even for ring 0 interrupts.
       Five CPU words plus the two stub words and 15 GPRs total 22, keeping
       the stack 16-byte aligned before CALL for both privilege paths. */
    mov rdi, rsp
    cld
    /* A user may leave DS/ES null or set to user data. Rust always runs with
       the kernel data selectors; every GDT segment has a zero base. */
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    call tane_interrupt_dispatch
    mov rsp, rax              /* The frame to resume, possibly another task's. */
    mov ax, 0x10
    test byte ptr [rsp + 144], 3
    jz 1f
    mov ax, 0x2b
1:
    mov ds, ax
    mov es, ax
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    add rsp, 16
    iretq

.section .rodata.tane_isr_table, "a"
.balign 8
.global tane_isr_table
tane_isr_table:
.irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48
    .quad isr_\n
.endr
.section .text
"#);

const EXCEPTIONS: [(&str, &str); 32] = [
    ("#DE", "Divide error"),
    ("#DB", "Debug"),
    ("NMI", "Non-maskable interrupt"),
    ("#BP", "Breakpoint"),
    ("#OF", "Overflow"),
    ("#BR", "Bound range exceeded"),
    ("#UD", "Invalid opcode"),
    ("#NM", "Device not available"),
    ("#DF", "Double fault"),
    ("#09", "Coprocessor segment overrun"),
    ("#TS", "Invalid TSS"),
    ("#NP", "Segment not present"),
    ("#SS", "Stack-segment fault"),
    ("#GP", "General protection fault"),
    ("#PF", "Page fault"),
    ("#15", "Reserved"),
    ("#MF", "x87 floating-point error"),
    ("#AC", "Alignment check"),
    ("#MC", "Machine check"),
    ("#XM", "SIMD floating-point error"),
    ("#VE", "Virtualization exception"),
    ("#CP", "Control protection"),
    ("#22", "Reserved"),
    ("#23", "Reserved"),
    ("#24", "Reserved"),
    ("#25", "Reserved"),
    ("#26", "Reserved"),
    ("#27", "Reserved"),
    ("#HV", "Hypervisor injection"),
    ("#VC", "VMM communication"),
    ("#SX", "Security exception"),
    ("#31", "Reserved"),
];

/// Load the kernel GDT/TSS/IDT, remap the PICs and start the PIT. The caller
/// enables interrupts once the task table is ready.
pub unsafe fn init() {
    let tss = addr_of!(TSS) as u64;
    let ist_top = addr_of!(DOUBLE_FAULT_STACK) as u64 + core::mem::size_of::<Stack>() as u64;
    let words = &mut (*addr_of_mut!(TSS)).0;
    words[9] = ist_top as u32;
    words[10] = (ist_top >> 32) as u32;
    words[25] = (TSS_BYTES as u32) << 16; // No I/O bitmap: ring 3 I/O is denied.

    // Tss has 16-byte alignment and therefore trailing Rust padding. Its
    // descriptor covers exactly the architectural 104 bytes: including that
    // padding would turn the zero-filled bytes at I/O base 104 into an
    // accidental permission bitmap granting ring 3 access to low I/O ports.
    let limit = TSS_BYTES - 1;
    let gdt = &mut (*addr_of_mut!(GDT)).0;
    gdt[3] = (limit & 0xffff) | (tss & 0xff_ffff) << 16 | 0x89 << 40
        | ((limit >> 16) & 0xf) << 48 | ((tss >> 24) & 0xff) << 56;
    gdt[4] = tss >> 32;
    let pointer = DescriptorPointer { limit: core::mem::size_of::<[u64; 7]>() as u16 - 1, base: addr_of!(GDT) as u64 };
    asm!(
        "lgdt [{pointer}]",
        "push {code}",
        "lea {scratch}, [rip + 2f]",
        "push {scratch}",
        "retfq",
        "2:",
        "mov ds, {data:x}",
        "mov es, {data:x}",
        "mov ss, {data:x}",
        "ltr {tss:x}",
        pointer = in(reg) &pointer,
        code = const KERNEL_CODE as u64,
        data = in(reg) KERNEL_DATA as u64,
        tss = in(reg) TSS_SELECTOR as u64,
        scratch = out(reg) _,
    );

    let idt = &mut (*addr_of_mut!(IDT)).0;
    for (vector, &handler) in (*addr_of!(tane_isr_table)).iter().enumerate() {
        let ist = if vector == 8 { DOUBLE_FAULT_IST } else { 0 };
        // Present, ring 0, 64-bit interrupt gate (IF is cleared on entry).
        idt[vector] = gate(handler, ist, 0x8e);
    }
    // The only software interrupt that ring 3 may invoke. It also clears IF;
    // every syscall and scheduler mutation starts on the task's TSS RSP0.
    idt[SYSCALL_VECTOR as usize] = gate(addr_of!(isr_128) as u64, 0, 0xee);
    let pointer = DescriptorPointer { limit: core::mem::size_of::<Idt>() as u16 - 1, base: addr_of!(IDT) as u64 };
    asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));

    remap_pics();
    // Channel 0, low/high divisor bytes, mode 2 (rate generator).
    crate::outb(0x43, 0x34);
    crate::outb(0x40, PIT_DIVISOR as u8);
    crate::outb(0x40, (PIT_DIVISOR >> 8) as u8);
    crate::outb(0x21, !((1 << IRQ_TIMER) | (1 << IRQ_KEYBOARD) | (1 << IRQ_COM1)) as u8);
    crate::outb(0xa1, 0xff);
}

fn gate(handler: u64, ist: u8, flags: u64) -> [u64; 2] {
    [
        (handler & 0xffff) | (KERNEL_CODE as u64) << 16 | (ist as u64) << 32
            | flags << 40 | ((handler >> 16) & 0xffff) << 48,
        handler >> 32,
    ]
}

/// Select the supervisor stack for the next ring 3 interrupt. The scheduler
/// calls this with IF cleared alongside CR3 switching; both words belong to
/// one 64-bit RSP0 field and must not be observed half updated.
pub unsafe fn set_rsp0(top: u64) {
    let words = addr_of_mut!((*addr_of_mut!(TSS)).0);
    (*words)[1] = top as u32;
    (*words)[2] = (top >> 32) as u32;
}

unsafe fn remap_pics() {
    // ICW1..ICW4: edge triggered, cascaded, vectors 32..=47, 8086 mode.
    for (port, value) in [(0x20, 0x11), (0xa0, 0x11), (0x21, IRQ_BASE as u8), (0xa1, IRQ_BASE as u8 + 8),
                          (0x21, 4), (0xa1, 2), (0x21, 1), (0xa1, 1)] {
        crate::outb(port, value);
        crate::outb(0x80, 0); // Short I/O delay for old controllers.
    }
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

// These are compiler barriers too (no `nomem`): memory accesses guarded by
// CLI/STI must not be moved across them.
pub fn enable() {
    unsafe { asm!("sti", options(nostack)); }
}

pub fn disable() {
    unsafe { asm!("cli", options(nostack)); }
}

/// Enable interrupts and sleep until the next one. STI delays recognition by
/// one instruction, so an IRQ arriving after a check under CLI still wakes HLT.
pub fn wait() {
    unsafe { asm!("sti", "hlt", options(nostack)); }
}

/// Run `f` with interrupts disabled, then restore the previous IF state.
pub fn without<R>(f: impl FnOnce() -> R) -> R {
    let flags: u64;
    unsafe { asm!("pushfq", "pop {}", "cli", out(reg) flags); }
    let result = f();
    if flags & 0x200 != 0 {
        enable();
    }
    result
}

/// Enter the scheduler from task context. The saved RFLAGS keep the
/// caller's IF, so a caller holding CLI resumes with CLI.
pub fn yield_now() {
    unsafe { asm!("int 48"); }
}

#[no_mangle]
extern "C" fn tane_interrupt_dispatch(frame: &mut Frame) -> *mut Frame {
    if frame.vector <= 31 && frame.user_mode() {
        let address = if frame.vector == 14 {
            let cr2: u64;
            unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)); }
            cr2
        } else { 0 };
        // Do not print here: an interrupt may have preempted the kernel
        // console. Faults become per-process state for later shell inspection.
        return crate::tasks::terminate_current(frame, crate::process::ExitReason::Fault {
            vector: frame.vector, error: frame.error, rip: frame.rip, address,
        });
    }
    match frame.vector {
        3 => {
            report(frame);
            kprintln!("Breakpoint handled; resuming.");
            frame
        }
        0..=31 => {
            report(frame);
            kprintln!("HALTED");
            crate::halt();
        }
        YIELD_VECTOR => crate::tasks::switch(frame, None),
        SYSCALL_VECTOR => {
            if frame.user_mode() && crate::tasks::is_user() {
                crate::user_syscalls::dispatch(frame)
            } else {
                // A kernel caller cannot impersonate a user task. Reject the
                // request without turning an accidental INT into a panic.
                frame.rax = (-1i64) as u64;
                frame
            }
        }
        vector @ IRQ_BASE..=47 => irq(frame, vector - IRQ_BASE),
        _ => frame,
    }
}

fn irq(frame: &mut Frame, irq: u64) -> *mut Frame {
    unsafe {
        // A spurious IRQ 7/15 has no in-service bit and must not receive EOI.
        if irq == 7 || irq == 15 {
            let command = if irq == 7 { 0x20 } else { 0xa0 };
            crate::outb(command, 0x0b);
            if crate::inb(command) & 0x80 == 0 {
                if irq == 15 { crate::outb(0x20, 0x20); }
                return frame;
            }
        }
        // Acknowledge before a possible switch: the next task may run long.
        if irq >= 8 { crate::outb(0xa0, 0x20); }
        crate::outb(0x20, 0x20);
    }
    match irq {
        IRQ_TIMER => {
            let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
            crate::tasks::on_timer(frame, now)
        }
        // The data stays in the device; the woken shell reads it.
        IRQ_KEYBOARD | IRQ_COM1 => crate::tasks::on_input(frame),
        _ => frame,
    }
}

fn report(frame: &Frame) {
    let vector = frame.vector as usize;
    let (mnemonic, name) = EXCEPTIONS[vector];
    let console = crate::console();
    console.color = 0x0c;
    writeln!(console, "\nCPU EXCEPTION {} {}: {}", vector, mnemonic, name).ok();
    console.color = 0x07;
    let (pid, task) = crate::tasks::current();
    writeln!(console, "task: pid {} ({})", pid, task).ok();
    if matches!(vector, 8 | 10..=14 | 17 | 21 | 29 | 30) {
        write!(console, "error=0x{:x}", frame.error).ok();
        if vector == 14 {
            let cr2: u64;
            unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)); }
            write!(console, " ({} {}{}) cr2=0x{:016x}",
                if frame.error & 1 != 0 { "protection" } else { "not-present" },
                if frame.error & 2 != 0 { "write" } else { "read" },
                if frame.error & 16 != 0 { " fetch" } else { "" },
                cr2).ok();
        }
        writeln!(console).ok();
    }
    writeln!(console, "rip={:016x} cs={:04x} rflags={:016x}", frame.rip, frame.cs, frame.rflags).ok();
    writeln!(console, "rsp={:016x} ss={:04x}", frame.rsp, frame.ss).ok();
    let registers = [
        ("rax", frame.rax), ("rbx", frame.rbx), ("rcx", frame.rcx), ("rdx", frame.rdx),
        ("rsi", frame.rsi), ("rdi", frame.rdi), ("rbp", frame.rbp), ("r8 ", frame.r8),
        ("r9 ", frame.r9), ("r10", frame.r10), ("r11", frame.r11), ("r12", frame.r12),
        ("r13", frame.r13), ("r14", frame.r14), ("r15", frame.r15),
    ];
    for row in registers.chunks(3) {
        for (index, (name, value)) in row.iter().enumerate() {
            write!(console, "{}{}={:016x}", if index == 0 { "" } else { " " }, name, value).ok();
        }
        writeln!(console).ok();
    }
}
