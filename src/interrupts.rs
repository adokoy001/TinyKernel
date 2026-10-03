//! CPU exceptions, the legacy 8259 PICs and the 8254 PIT timer.
//!
//! The kernel installs its own GDT with a TSS so that a double fault runs on a
//! separate known-good stack (IST1). Vectors 0..=31 are CPU exceptions and
//! 32..=47 are the remapped PIC IRQs. Every stub saves the general registers
//! and calls `tane_interrupt_dispatch` with a pointer to the saved frame.

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

const KERNEL_CODE: u16 = 0x08;
const KERNEL_DATA: u16 = 0x10;
const TSS_SELECTOR: u16 = 0x18;
const DOUBLE_FAULT_IST: u8 = 1;

static TICKS: AtomicU64 = AtomicU64::new(0);

#[repr(C, align(16))]
struct Gdt([u64; 5]);

// 0x08: 64-bit ring 0 code, 0x10: ring 0 data, 0x18: 64-bit TSS (two slots).
static mut GDT: Gdt = Gdt([0, 0x00af9a000000ffff, 0x00cf92000000ffff, 0, 0]);

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
    static tane_isr_table: [u64; 48];
}

/// Registers saved by `isr_common`, lowest address first.
#[repr(C)]
pub struct Frame {
    r15: u64, r14: u64, r13: u64, r12: u64, r11: u64, r10: u64, r9: u64, r8: u64,
    rbp: u64, rdi: u64, rsi: u64, rdx: u64, rcx: u64, rbx: u64, rax: u64,
    vector: u64,
    error: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

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
.irp n, 32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
ISR_NOERR \n
.endr

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
    /* The CPU aligned RSP to 16 bytes; 22 saved quadwords keep it aligned. */
    mov rdi, rsp
    cld
    call tane_interrupt_dispatch
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
.irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
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

/// Load the kernel GDT/TSS/IDT, remap the PICs, start the PIT and enable IRQs.
pub unsafe fn init() {
    let tss = addr_of!(TSS) as u64;
    let ist_top = addr_of!(DOUBLE_FAULT_STACK) as u64 + core::mem::size_of::<Stack>() as u64;
    let words = &mut (*addr_of_mut!(TSS)).0;
    words[9] = ist_top as u32;
    words[10] = (ist_top >> 32) as u32;
    words[25] = 104 << 16; // I/O permission bitmap base beyond the limit: none.

    let limit = core::mem::size_of::<Tss>() as u64 - 1;
    let gdt = &mut (*addr_of_mut!(GDT)).0;
    gdt[3] = (limit & 0xffff) | (tss & 0xff_ffff) << 16 | 0x89 << 40
        | ((limit >> 16) & 0xf) << 48 | ((tss >> 24) & 0xff) << 56;
    gdt[4] = tss >> 32;
    let pointer = DescriptorPointer { limit: core::mem::size_of::<Gdt>() as u16 - 1, base: addr_of!(GDT) as u64 };
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
        idt[vector][0] = (handler & 0xffff) | (KERNEL_CODE as u64) << 16 | (ist as u64) << 32
            | 0x8e << 40 | ((handler >> 16) & 0xffff) << 48;
        idt[vector][1] = handler >> 32;
    }
    let pointer = DescriptorPointer { limit: core::mem::size_of::<Idt>() as u16 - 1, base: addr_of!(IDT) as u64 };
    asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));

    remap_pics();
    // Channel 0, low/high divisor bytes, mode 2 (rate generator).
    crate::outb(0x43, 0x34);
    crate::outb(0x40, PIT_DIVISOR as u8);
    crate::outb(0x40, (PIT_DIVISOR >> 8) as u8);
    crate::outb(0x21, !((1 << IRQ_TIMER) | (1 << IRQ_KEYBOARD) | (1 << IRQ_COM1)) as u8);
    crate::outb(0xa1, 0xff);
    enable();
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

pub fn enable() {
    unsafe { asm!("sti", options(nomem, nostack)); }
}

pub fn disable() {
    unsafe { asm!("cli", options(nomem, nostack)); }
}

/// Enable interrupts and sleep until the next one. STI delays recognition by
/// one instruction, so an IRQ arriving after a check under CLI still wakes HLT.
pub fn wait() {
    unsafe { asm!("sti", "hlt", options(nomem, nostack)); }
}

#[no_mangle]
extern "C" fn tane_interrupt_dispatch(frame: &mut Frame) {
    match frame.vector {
        3 => {
            report(frame);
            kprintln!("Breakpoint handled; resuming.");
        }
        0..=31 => {
            report(frame);
            kprintln!("HALTED");
            crate::halt();
        }
        _ => irq(frame.vector - IRQ_BASE),
    }
}

fn irq(irq: u64) {
    unsafe {
        // A spurious IRQ 7/15 has no in-service bit and must not receive EOI.
        if irq == 7 || irq == 15 {
            let command = if irq == 7 { 0x20 } else { 0xa0 };
            crate::outb(command, 0x0b);
            if crate::inb(command) & 0x80 == 0 {
                if irq == 15 { crate::outb(0x20, 0x20); }
                return;
            }
        }
        if irq == IRQ_TIMER {
            TICKS.fetch_add(1, Ordering::Relaxed);
        }
        // Keyboard and COM1 IRQs only wake HLT; the main loop reads the data.
        if irq >= 8 { crate::outb(0xa0, 0x20); }
        crate::outb(0x20, 0x20);
    }
}

fn report(frame: &Frame) {
    let vector = frame.vector as usize;
    let (mnemonic, name) = EXCEPTIONS[vector];
    let console = crate::console();
    console.color = 0x0c;
    writeln!(console, "\nCPU EXCEPTION {} {}: {}", vector, mnemonic, name).ok();
    console.color = 0x07;
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
