#![no_std]
#![no_main]

// Tane OS: an original, single-task kernel. No allocator and no external crates.

// Each print borrows the global console only for the duration of one line.
macro_rules! kprintln {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        writeln!(crate::console(), $($arg)*).ok();
    }};
}

mod interrupts;
mod shell;

use core::arch::asm;
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};
use shell::{Action, Fault};

const VGA: *mut u16 = 0xb8000 as *mut u16;
const WIDTH: usize = 80;
const HEIGHT: usize = 25;
const SERIAL: u16 = 0x3f8;
const LINE_SIZE: usize = 128;

static mut CONSOLE: Console = Console { x: 0, y: 0, color: 0x07 };
static SERIAL_PRESENT: AtomicBool = AtomicBool::new(false);

fn console() -> &'static mut Console {
    // Single CPU. Only exception reports print from interrupt context.
    unsafe { &mut *addr_of_mut!(CONSOLE) }
}

extern "C" {
    static mut __bss_start: u8;
    static mut __bss_end: u8;
    static __kernel_end: u8;
}

// The BIOS loader calls this address with paging on, IF=0, and a valid stack.
// Interrupts are enabled once the kernel's own GDT, TSS and IDT are loaded.
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    unsafe {
        asm!("cld", "cli", options(nomem, nostack));
        let mut p = addr_of_mut!(__bss_start);
        while p < addr_of_mut!(__bss_end) {
            write_volatile(p, 0);
            p = p.add(1);
        }
        // Mask the legacy PICs until the IDT exists.
        outb(0x21, 0xff);
        outb(0xa1, 0xff);
        serial_init();
    }
    console().clear();
    console().color = 0x0b;
    kprintln!("TANE OS / RUST BARE METAL");
    console().color = 0x07;
    kprintln!("Self-made BIOS loader + Rust kernel. External crates: 0.");
    kprintln!("64-bit mode | VGA + COM1 | PS/2 keyboard | no heap");
    unsafe { interrupts::init(); }
    kprintln!("IDT: 32 exception handlers, #DF on IST1 | PIT timer {} Hz", interrupts::TIMER_HZ);
    if !SERIAL_PRESENT.load(Ordering::Relaxed) {
        kprintln!("COM1 not detected; VGA and keyboard only.");
    }
    kprintln!("Type help. English keyboard / ASCII input.");
    kprintln!("READY");

    let mut keyboard = Keyboard::new();
    let mut line = [0u8; LINE_SIZE];
    let mut used = 0;
    let mut previous_was_cr = false;
    prompt();
    loop {
        // Serial and PS/2 are both hardware drivers implemented here. Check
        // with IRQs off so an arriving byte cannot slip in before HLT.
        interrupts::disable();
        let byte = unsafe { serial_read() }.or_else(|| keyboard.read());
        interrupts::enable();
        if let Some(b) = byte {
            if b == b'\n' && previous_was_cr {
                previous_was_cr = false;
                continue;
            }
            previous_was_cr = b == b'\r';
            match b {
                b'\r' | b'\n' => {
                    console().byte(b'\n');
                    // Only printable ASCII bytes enter this buffer.
                    let command = core::str::from_utf8(&line[..used]).unwrap_or("");
                    execute(command);
                    used = 0;
                    prompt();
                }
                8 | 127 if used > 0 => {
                    used -= 1;
                    console().backspace();
                }
                b' '..=b'~' if used < LINE_SIZE - 1 => {
                    line[used] = b;
                    used += 1;
                    console().byte(b);
                }
                _ => {} // Ignore controls and safely discard excess input.
            }
        } else {
            // Re-check after any interrupt: timer, keyboard, or COM1 data.
            interrupts::disable();
            if !unsafe { serial_ready() } && !keyboard.ready() {
                interrupts::wait();
            }
            interrupts::enable();
        }
    }
}

fn prompt() {
    console().color = 0x0a;
    write!(console(), "tane> ").ok();
    console().color = 0x07;
}

fn execute(line: &str) {
    match shell::parse(line) {
        Action::Help => {
            kprintln!("help          Show commands");
            kprintln!("about         Describe this kernel");
            kprintln!("mem           Show the fixed memory layout");
            kprintln!("uptime        Time since boot, counted by timer interrupts");
            kprintln!("echo TEXT     Print text");
            kprintln!("calc A OP B   Integer + - * / (example: calc 12 * 3)");
            kprintln!("sleep MS      Wait MS milliseconds (0-60000) with HLT");
            kprintln!("fault KIND    Raise a CPU exception: bp de ud gp pf df");
            kprintln!("clear         Clear the screen");
            kprintln!("reboot        Restart the virtual machine");
            kprintln!("halt          Stop the CPU (close QEMU to exit)");
        }
        Action::About => {
            kprintln!("Tane OS 0.2 - a small original Rust kernel.");
            kprintln!("No Linux code, GRUB, external crates, libc, or host OS calls.");
            kprintln!("Firmware loads our 512-byte boot sector; it loads this kernel.");
            kprintln!("One task, ring 0, fixed memory, own GDT/TSS/IDT, PIT at {} Hz.", interrupts::TIMER_HZ);
            kprintln!("CPU exceptions print registers; no process isolation, filesystem, or network.");
        }
        Action::Memory => {
            let end = addr_of!(__kernel_end) as usize;
            kprintln!("Page tables: 0x1000..0x4000");
            kprintln!("Boot sector: 0x7c00..0x7e00");
            kprintln!("Kernel:      0x10000..0x{:x} ({} bytes incl. BSS)", end, end - 0x10000);
            kprintln!("Stack range: 0x80000..0x90000 (grows down)");
            kprintln!("VGA text:    0xb8000");
            kprintln!("First 1 GiB identity mapped; this is NOT detected RAM size.");
            kprintln!("No heap. Command buffer: {} bytes (max {} input).", LINE_SIZE, LINE_SIZE - 1);
        }
        Action::Uptime => {
            let ticks = interrupts::ticks();
            let hundredths = ticks * 100 / interrupts::TIMER_HZ;
            kprintln!("up {}.{:02} s ({} timer ticks at {} Hz)", hundredths / 100, hundredths % 100, ticks, interrupts::TIMER_HZ);
        }
        Action::Echo(text) => kprintln!("{}", text),
        Action::Calc(Ok(result)) => kprintln!("= {}", result),
        Action::Calc(Err(error)) | Action::Sleep(Err(error)) | Action::Fault(Err(error)) => kprintln!("error: {}", error),
        Action::Sleep(Ok(ms)) => {
            // Round up so the wait is never shorter than requested.
            let target = interrupts::ticks() + (ms * interrupts::TIMER_HZ).div_ceil(1000);
            while interrupts::ticks() < target {
                interrupts::wait();
            }
            kprintln!("slept {} ms", ms);
        }
        Action::Fault(Ok(fault)) => raise(fault),
        Action::Clear => {
            console().clear();
            // Clear common ANSI terminals on the serial side as well.
            for b in b"\x1b[2J\x1b[H" { unsafe { serial_write(*b); } }
        }
        Action::Halt => {
            kprintln!("HALTED");
            halt();
        }
        Action::Reboot => {
            kprintln!("REBOOTING");
            // QEMU's PC implements the 8042 controller reset command.
            unsafe {
                for _ in 0..100_000 {
                    if inb(0x64) & 2 == 0 {
                        outb(0x64, 0xfe);
                        break;
                    }
                }
            }
            halt();
        }
        Action::Unknown => kprintln!("Unknown command. Type help."),
        Action::Empty => {}
    }
}

/// Deliberately execute an instruction that makes the CPU raise `fault`.
fn raise(fault: Fault) {
    unsafe {
        match fault {
            Fault::Breakpoint => asm!("int3", options(nomem, nostack)),
            Fault::DivideError => asm!("div {0:e}", in(reg) 0u32, inout("eax") 1u32 => _, inout("edx") 0u32 => _,
                                       options(nomem, nostack)),
            Fault::InvalidOpcode => asm!("ud2", options(nomem, nostack)),
            // Bit 63 set without the upper bits: a non-canonical address.
            Fault::GeneralProtection => asm!("mov {0}, qword ptr [{0}]", inout(reg) 0x8000_0000_0000_0000u64 => _,
                                             options(readonly, nostack)),
            // Only the first 1 GiB is mapped.
            Fault::PageFault => asm!("mov {0}, qword ptr [{0}]", inout(reg) 0x4000_0000u64 => _,
                                     options(readonly, nostack)),
            // With RSP unmapped, delivering #UD page-faults and delivering that
            // #PF faults again: the CPU escalates to #DF, which uses IST1.
            Fault::DoubleFault => asm!("mov rsp, {}", "ud2", in(reg) 0x4000_1000u64, options(noreturn)),
        }
    }
    // Only #BP returns; every other exception halts in its handler.
    kprintln!("returned to the shell after the exception");
}

struct Console { x: usize, y: usize, color: u8 }

impl Console {
    fn clear(&mut self) {
        for i in 0..WIDTH * HEIGHT {
            unsafe { write_volatile(VGA.add(i), 0x0720); }
        }
        self.x = 0;
        self.y = 0;
        self.cursor();
    }

    fn newline(&mut self) {
        self.x = 0;
        self.y += 1;
        if self.y == HEIGHT {
            for i in 0..WIDTH * (HEIGHT - 1) {
                unsafe { write_volatile(VGA.add(i), read_volatile(VGA.add(i + WIDTH))); }
            }
            for i in WIDTH * (HEIGHT - 1)..WIDTH * HEIGHT {
                unsafe { write_volatile(VGA.add(i), 0x0720); }
            }
            self.y = HEIGHT - 1;
        }
    }

    fn byte(&mut self, b: u8) {
        unsafe {
            if b == b'\n' { serial_write(b'\r'); }
            serial_write(b);
        }
        match b {
            b'\n' => self.newline(),
            b'\r' => self.x = 0,
            _ => {
                unsafe { write_volatile(VGA.add(self.y * WIDTH + self.x), ((self.color as u16) << 8) | b as u16); }
                self.x += 1;
                if self.x == WIDTH { self.newline(); }
            }
        }
        self.cursor();
    }

    fn backspace(&mut self) {
        if self.x == 0 {
            if self.y == 0 { return; }
            self.y -= 1;
            self.x = WIDTH;
        }
        self.x -= 1;
        unsafe {
            write_volatile(VGA.add(self.y * WIDTH + self.x), ((self.color as u16) << 8) | 0x20);
            serial_write(8); serial_write(b' '); serial_write(8);
        }
        self.cursor();
    }

    fn cursor(&self) {
        let position = (self.y * WIDTH + self.x) as u16;
        unsafe {
            outb(0x3d4, 14); outb(0x3d5, (position >> 8) as u8);
            outb(0x3d4, 15); outb(0x3d5, position as u8);
        }
    }
}

impl Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() { self.byte(byte); }
        Ok(())
    }
}

struct Keyboard { left_shift: bool, right_shift: bool, caps: bool, extended: bool, skip: u8 }

impl Keyboard {
    fn new() -> Self { Self { left_shift: false, right_shift: false, caps: false, extended: false, skip: 0 } }

    fn ready(&self) -> bool {
        unsafe { inb(0x64) & 1 != 0 }
    }

    fn read(&mut self) -> Option<u8> {
        let status = unsafe { inb(0x64) };
        if status & 1 == 0 { return None; }
        let scan = unsafe { inb(0x60) };
        if status & 0xe0 != 0 { return None; } // Discard AUX/parity/timeout bytes.
        self.decode(scan)
    }

    fn decode(&mut self, scan: u8) -> Option<u8> {
        if self.skip > 0 { self.skip -= 1; return None; }
        if scan == 0xe1 { self.skip = 5; return None; } // Pause sequence.
        if scan == 0xe0 { self.extended = true; return None; }
        if self.extended {
            self.extended = false;
            return if scan == 0x1c { Some(b'\n') } else { None };
        }
        match scan {
            0x2a => { self.left_shift = true; return None; }
            0xaa => { self.left_shift = false; return None; }
            0x36 => { self.right_shift = true; return None; }
            0xb6 => { self.right_shift = false; return None; }
            0x3a => { self.caps = !self.caps; return None; }
            0x80..=0xff => return None,
            _ => {}
        }
        let shift = self.left_shift || self.right_shift;
        let b = match scan {
            0x02..=0x0d => {
                let table = if shift { b"!@#$%^&*()_+" } else { b"1234567890-=" };
                table[(scan - 0x02) as usize]
            }
            0x10..=0x19 => b"qwertyuiop"[(scan - 0x10) as usize],
            0x1e..=0x26 => b"asdfghjkl"[(scan - 0x1e) as usize],
            0x2c..=0x32 => b"zxcvbnm"[(scan - 0x2c) as usize],
            0x0e => 8,
            0x1c => b'\n',
            0x39 => b' ',
            0x1a => if shift { b'{' } else { b'[' },
            0x1b => if shift { b'}' } else { b']' },
            0x27 => if shift { b':' } else { b';' },
            0x28 => if shift { b'"' } else { b'\'' },
            0x29 => if shift { b'~' } else { b'`' },
            0x2b => if shift { b'|' } else { b'\\' },
            0x33 => if shift { b'<' } else { b',' },
            0x34 => if shift { b'>' } else { b'.' },
            0x35 => if shift { b'?' } else { b'/' },
            _ => return None,
        };
        Some(if b.is_ascii_lowercase() && shift != self.caps { b - 32 } else { b })
    }
}

pub(crate) unsafe fn outb(port: u16, value: u8) {
    asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
}

pub(crate) unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags));
    value
}

unsafe fn serial_init() {
    // Without a UART, reads float to 0xff; the scratch register detects that.
    outb(SERIAL + 7, 0x5a);
    if inb(SERIAL + 7) != 0x5a { return; }
    outb(SERIAL + 7, 0xa5);
    if inb(SERIAL + 7) != 0xa5 { return; }
    SERIAL_PRESENT.store(true, Ordering::Relaxed);
    outb(SERIAL + 1, 0x00); // UART interrupt enable = off while configuring.
    outb(SERIAL + 3, 0x80); // Access baud divisor.
    outb(SERIAL, 0x01); outb(SERIAL + 1, 0x00); // 115200 baud.
    outb(SERIAL + 3, 0x03); // 8 data bits, no parity, one stop bit.
    outb(SERIAL + 2, 0xc7); // FIFO enable/reset.
    outb(SERIAL + 4, 0x0b); // DTR + RTS + OUT2, which gates IRQ 4 on PCs.
    outb(SERIAL + 1, 0x01); // IRQ on received data, so HLT wakes for input.
}

unsafe fn serial_write(byte: u8) {
    if !SERIAL_PRESENT.load(Ordering::Relaxed) { return; }
    // Bound the wait so a missing UART does not hang the VGA console.
    for _ in 0..100_000 {
        if inb(SERIAL + 5) & 0x20 != 0 { outb(SERIAL, byte); return; }
    }
}

unsafe fn serial_ready() -> bool {
    SERIAL_PRESENT.load(Ordering::Relaxed) && inb(SERIAL + 5) & 1 != 0
}

unsafe fn serial_read() -> Option<u8> {
    if serial_ready() { Some(inb(SERIAL)) } else { None }
}

pub(crate) fn halt() -> ! {
    loop { unsafe { asm!("cli", "hlt", options(nomem, nostack)); } }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    interrupts::disable();
    console().color = 0x0c;
    kprintln!("\nKERNEL PANIC: {}", info);
    halt()
}

// Compiler memory intrinsics. Volatile byte operations prevent the optimizer
// from replacing these implementations with a recursive call to themselves.
#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut u8, value: i32, count: usize) -> *mut u8 {
    for i in 0..count { write_volatile(dst.add(i), value as u8); }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    for i in 0..count { write_volatile(dst.add(i), read_volatile(src.add(i))); }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    if (dst as usize) <= src as usize {
        memcpy(dst, src, count);
    } else {
        for i in (0..count).rev() { write_volatile(dst.add(i), read_volatile(src.add(i))); }
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, count: usize) -> i32 {
    for i in 0..count {
        let difference = read_volatile(a.add(i)) as i32 - read_volatile(b.add(i)) as i32;
        if difference != 0 { return difference; }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, count: usize) -> i32 {
    memcmp(a, b, count)
}
