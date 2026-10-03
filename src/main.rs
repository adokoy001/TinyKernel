#![no_std]
#![no_main]

// Tane OS: an original, small preemptive kernel. No heap and no external crates.
// Hardened with W^X paging and a compiled-in mandatory access control policy.

// Each print borrows the global console only for the duration of one line.
macro_rules! kprintln {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        writeln!(crate::console(), $($arg)*).ok();
    }};
}

mod frames;
mod interrupts;
mod mac;
mod paging;
mod sched;
mod security;
mod shell;
mod tasks;

use core::arch::asm;
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};
use frames::{E820Entry, FrameAllocator, FRAME_SIZE};
use mac::{Domain, Op};
use security::Denied;
use shell::{Action, Fault};

const VGA: *mut u16 = 0xb8000 as *mut u16;
const WIDTH: usize = 80;
const HEIGHT: usize = 25;
const SERIAL: u16 = 0x3f8;
const LINE_SIZE: usize = 128;
/// The boot sector stores the BIOS E820 map here (see boot.S).
const E820_COUNT: *const u16 = 0x5000 as *const u16;
const E820_ENTRIES: *const E820Entry = 0x5010 as *const E820Entry;
const E820_MAX: usize = 64;
/// Frames below 1 MiB hold the kernel, its stack, BIOS data and the VGA
/// buffer; the allocator only hands out RAM from here up to 1 GiB, the
/// identity-mapped limit.
const LOW_MEMORY_END: u64 = 0x100000;
const FRAME_WORDS: usize = 4096;
type Frames = FrameAllocator<FRAME_WORDS>;

static mut FRAMES: Frames = Frames::new();

/// Frames handed out by `alloc`, with the MAC label of the task that took
/// them. `free` only accepts addresses listed here.
const ALLOC_SLOTS: usize = 32;
static mut ALLOCATED: [Option<(u64, Domain)>; ALLOC_SLOTS] = [None; ALLOC_SLOTS];

/// Bytes in the data section; `fault nx` jumps here to prove data is not executable.
static mut NOT_CODE: [u8; 16] = [0xc3; 16]; // `ret` if it were ever executed

static mut CONSOLE: Console = Console { x: 0, y: 0, color: 0x07 };
static SERIAL_PRESENT: AtomicBool = AtomicBool::new(false);

fn console() -> &'static mut Console {
    // Single CPU. Tasks other than the shell never print; only exception
    // reports print from interrupt context.
    unsafe { &mut *addr_of_mut!(CONSOLE) }
}

/// The frame allocator is shared with the scheduler, so use it under CLI.
fn with_frames<R>(f: impl FnOnce(&mut Frames) -> R) -> R {
    interrupts::without(|| f(unsafe { &mut *addr_of_mut!(FRAMES) }))
}

fn e820_map() -> &'static [E820Entry] {
    unsafe {
        let count = (read_volatile(E820_COUNT) as usize).min(E820_MAX);
        core::slice::from_raw_parts(E820_ENTRIES, count)
    }
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
        paging::init();
        serial_init();
    }
    console().clear();
    console().color = 0x0b;
    kprintln!("TANE OS / RUST BARE METAL");
    console().color = 0x07;
    kprintln!("Self-made BIOS loader + Rust kernel. External crates: 0.");
    kprintln!("64-bit mode | VGA + COM1 | PS/2 keyboard | no heap");
    let map = e820_map();
    with_frames(|frames| frames.init(map, LOW_MEMORY_END));
    let usable = with_frames(|frames| frames.usable_frames());
    kprintln!("RAM: {} E820 entries | {} free 4 KiB frames ({} KiB) above 1 MiB",
        map.len(), usable, usable as u64 * FRAME_SIZE / 1024);
    unsafe {
        interrupts::init();
        tasks::init();
    }
    interrupts::enable();
    kprintln!("IDT: 32 exception handlers, #DF on IST1 | PIT timer {} Hz", interrupts::TIMER_HZ);
    kprintln!("Tasks: preemptive round robin, up to {} tasks", tasks::MAX_TASKS);
    kprintln!("Protection: {} | kernel text read-only | null page unmapped | CR0.WP",
        if paging::nx_enabled() { "NX data" } else { "NX unsupported" });
    kprintln!("MAC: enforcing compiled-in policy | shell domain admin (drop lowers it)");
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
            // Block the shell task; other tasks or idle run until input.
            tasks::wait_for_input(|| unsafe { serial_ready() } || keyboard.ready());
        }
    }
}

fn prompt() {
    console().color = 0x0a;
    // `$` marks a lowered (user domain) shell, like an unprivileged Unix shell.
    let mark = if tasks::current_domain() == Domain::Admin { '>' } else { '$' };
    write!(console(), "tane{} ", mark).ok();
    console().color = 0x07;
}

fn execute(line: &str) {
    match shell::parse(line) {
        Action::Help => {
            kprintln!("help          Show commands");
            kprintln!("about         Describe this kernel");
            kprintln!("mem           Show the memory layout, E820 map and frames");
            kprintln!("alloc         Take one zeroed 4 KiB physical frame");
            kprintln!("free ADDR     Return a frame (example: free 0x100000)");
            kprintln!("ps            List tasks");
            kprintln!("spawn KIND    Start a task: spin (CPU), beat (sleeps), once (exits)");
            kprintln!("kill PID      Stop a task and free its stack frames");
            kprintln!("uptime        Time since boot, counted by timer interrupts");
            kprintln!("echo TEXT     Print text");
            kprintln!("calc A OP B   Integer + - * / (example: calc 12 * 3)");
            kprintln!("sleep MS      Block the shell for MS milliseconds (0-60000)");
            kprintln!("fault KIND    Raise a CPU exception: bp de ud gp pf df null ro nx");
            kprintln!("sec           Show memory protection and the MAC policy");
            kprintln!("audit         Show MAC denials (admin only)");
            kprintln!("drop          Lower this shell to the user domain until reboot");
            kprintln!("clear         Clear the screen");
            kprintln!("reboot        Restart the virtual machine");
            kprintln!("halt          Stop the CPU (close QEMU to exit)");
        }
        Action::About => {
            kprintln!("Tane OS 0.4 - a small original Rust kernel.");
            kprintln!("No Linux code, GRUB, external crates, libc, or host OS calls.");
            kprintln!("Firmware loads our 512-byte boot sector; it loads this kernel.");
            kprintln!("Ring 0, own GDT/TSS/IDT, PIT at {} Hz, E820 RAM in 4 KiB frames.", interrupts::TIMER_HZ);
            kprintln!("Preemptive round-robin kernel tasks with stacks from the frame allocator.");
            kprintln!("W^X paging, NX data, mandatory access control with an audit log.");
            kprintln!("CPU exceptions print registers; no process isolation, filesystem, or network.");
        }
        Action::Memory => {
            let end = addr_of!(__kernel_end) as usize;
            kprintln!("Page tables: in kernel BSS (the boot sector's at 0x1000..0x4000 are retired)");
            kprintln!("E820 table:  0x5000..0x5610 (written by the boot sector)");
            kprintln!("Boot sector: 0x7c00..0x7e00");
            kprintln!("Kernel:      0x10000..0x{:x} ({} bytes incl. BSS)", end, end - 0x10000);
            kprintln!("Shell stack: 0x80000..0x90000 (grows down)");
            kprintln!("VGA text:    0xb8000");
            kprintln!("First 1 GiB identity mapped: 4 KiB pages below 2 MiB, then 2 MiB pages.");
            kprintln!("No heap. Command buffer: {} bytes (max {} input).", LINE_SIZE, LINE_SIZE - 1);
            let map = e820_map();
            kprintln!("BIOS E820 map ({} entries):", map.len());
            for entry in map {
                kprintln!("  0x{:010x}..0x{:010x} {:>9} KiB {}{}", entry.base, entry.end(), entry.length / 1024,
                    entry.kind_name(), if entry.ignored() { " (ignored)" } else { "" });
            }
            let (usable, free) = with_frames(|frames| (frames.usable_frames(), frames.free_frames()));
            kprintln!("Frames: {} usable, {} free, {} in use (4 KiB each, 1 MiB..1 GiB)", usable, free, usable - free);
        }
        Action::Uptime => {
            let ticks = interrupts::ticks();
            let hundredths = ticks * 100 / interrupts::TIMER_HZ;
            kprintln!("up {}.{:02} s ({} timer ticks at {} Hz)", hundredths / 100, hundredths % 100, ticks, interrupts::TIMER_HZ);
        }
        Action::Echo(text) => kprintln!("{}", text),
        Action::Calc(Ok(result)) => kprintln!("= {}", result),
        Action::Calc(Err(error)) | Action::Sleep(Err(error)) | Action::Fault(Err(error))
        | Action::Free(Err(error)) | Action::Spawn(Err(error)) | Action::Kill(Err(error)) => kprintln!("error: {}", error),
        Action::Sleep(Ok(ms)) => {
            // Round up so the wait is never shorter than requested.
            tasks::sleep_until(interrupts::ticks() + (ms * interrupts::TIMER_HZ).div_ceil(1000));
            kprintln!("slept {} ms", ms);
        }
        Action::Alloc => match allocate_frame() {
            Ok((address, free)) => kprintln!("allocated 0x{:x} (zeroed); {} frames free", address, free),
            Err(Err(denied)) => report_denied(denied),
            Err(Ok(error)) => kprintln!("error: {}", error),
        },
        Action::Free(Ok(address)) => match free_frame(address) {
            FreeOutcome::Freed(free) => kprintln!("freed 0x{:x}; {} frames free", address, free),
            FreeOutcome::Denied(denied) => report_denied(denied),
            FreeOutcome::TaskStack(pid) => kprintln!("error: 0x{:x} is in the stack of pid {}; use kill", address, pid),
            FreeOutcome::Error(error) => kprintln!("error: 0x{:x}: {}", address, error),
        },
        Action::Tasks => {
            kprintln!("PID NAME   DOMAIN STATE       CPU s     COUNTER  STACK");
            tasks::list(|task| {
                let hundredths = task.cpu_ticks * 100 / interrupts::TIMER_HZ;
                write!(console(), "{:>3} {:<6} {:<6} {:<8} {:>5}.{:02} ", task.pid, task.name, task.domain.name(),
                    task.state, hundredths / 100, hundredths % 100).ok();
                match task.counter {
                    Some(counter) => write!(console(), "{:>11}", counter).ok(),
                    None => write!(console(), "{:>11}", "-").ok(),
                };
                if task.owns_stack {
                    kprintln!("  0x{:x} ({} frames)", task.stack, tasks::STACK_FRAMES);
                } else {
                    kprintln!("  0x{:x} (kernel)", task.stack);
                }
            });
        }
        Action::Spawn(Ok(kind)) => match tasks::spawn(kind) {
            Ok(pid) => kprintln!("started {} as pid {} in domain {}", kind.name(), pid, tasks::current_domain().name()),
            Err(tasks::SpawnError::Denied(denied)) => report_denied(denied),
            Err(tasks::SpawnError::Failed(error)) => kprintln!("error: {}", error),
        },
        Action::Kill(Ok(pid)) => match tasks::kill(pid) {
            Ok(name) => kprintln!("killed pid {} ({}); {} frames free", pid, name, with_frames(|frames| frames.free_frames())),
            Err(tasks::KillError::Denied(denied)) => report_denied(denied),
            Err(tasks::KillError::Failed(error)) => kprintln!("error: {}", error),
        },
        Action::Fault(Ok(fault)) => {
            if permitted(Op::Fault) {
                raise(fault);
            }
        }
        Action::Security => show_security(),
        Action::Audit => {
            if permitted(Op::ReadAudit) {
                kprintln!("MAC denials: {} since boot (newest {} kept)", security::denials(), security::AUDIT_RECORDS);
                security::audit_records(|number, record| {
                    let hundredths = record.tick * 100 / interrupts::TIMER_HZ;
                    write!(console(), "#{:<3} {:>4}.{:02}s pid {} {} {}", number, hundredths / 100, hundredths % 100,
                        record.pid, record.subject.name(), record.op.name()).ok();
                    if let Some(object) = record.object {
                        write!(console(), " {} object", object.name()).ok();
                    }
                    match (record.op, record.target) {
                        (Op::Kill, Some(pid)) => kprintln!(" (pid {}) DENIED", pid),
                        (_, Some(address)) => kprintln!(" (0x{:x}) DENIED", address),
                        (_, None) => kprintln!(" DENIED"),
                    }
                });
            }
        }
        Action::DropToUser => match tasks::lower_domain(Domain::User) {
            Ok(from) => kprintln!("domain {} -> user; this cannot be undone until reboot", from.name()),
            Err(Domain::User) => kprintln!("already in domain user; no command raises a domain"),
            Err(current) => kprintln!("error: domain {} cannot be lowered to user", current.name()),
        },
        Action::Clear => {
            console().clear();
            // Clear common ANSI terminals on the serial side as well.
            for b in b"\x1b[2J\x1b[H" { unsafe { serial_write(*b); } }
        }
        Action::Halt => {
            if permitted(Op::Halt) {
                kprintln!("HALTED");
                halt();
            }
        }
        Action::Reboot => {
            if !permitted(Op::Reboot) {
                return;
            }
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

/// The MAC enforcement point for commands without an object.
fn permitted(op: Op) -> bool {
    match security::check(op, None, None) {
        Ok(()) => true,
        Err(denied) => {
            report_denied(denied);
            false
        }
    }
}

fn report_denied(denied: Denied) {
    match denied.object {
        Some(object) => kprintln!("denied: {} may not {} objects of domain {} (MAC policy; audited)",
            denied.subject.name(), denied.op.name(), object.name()),
        None => kprintln!("denied: {} may not {} (MAC policy; audited)", denied.subject.name(), denied.op.name()),
    }
}

/// Allocate, zero and register one frame under the caller's label.
fn allocate_frame() -> Result<(u64, usize), Result<&'static str, Denied>> {
    security::check(Op::Alloc, None, None).map_err(Err)?;
    let domain = tasks::current_domain();
    let address = with_frames(|frames| {
        let registry = unsafe { &mut *addr_of_mut!(ALLOCATED) };
        let slot = registry.iter().position(Option::is_none).ok_or("alloc registry is full (32 frames)")?;
        let address = frames.allocate().ok_or("no free physical frames")?;
        registry[slot] = Some((address, domain));
        Ok(address)
    }).map_err(Ok)?;
    // Object reuse: zero before use. Identity mapping makes it writable here.
    for offset in (0..FRAME_SIZE).step_by(8) {
        unsafe { write_volatile((address + offset) as *mut u64, 0); }
    }
    Ok((address, with_frames(|frames| frames.free_frames())))
}

enum FreeOutcome {
    Freed(usize),
    Denied(Denied),
    TaskStack(u32),
    Error(&'static str),
}

/// Free a frame from `alloc` if the policy allows the caller to touch its
/// label. The lookup, check and free happen in one CLI section.
fn free_frame(address: u64) -> FreeOutcome {
    with_frames(|frames| {
        let registry = unsafe { &mut *addr_of_mut!(ALLOCATED) };
        let Some(slot) = registry.iter().position(|entry| matches!(entry, Some((a, _)) if *a == address)) else {
            return match frames.check_free(address, 1) {
                Err(error) => FreeOutcome::Error(error.message()),
                Ok(()) => match tasks::stack_owner(address) {
                    Some(pid) => FreeOutcome::TaskStack(pid),
                    None => FreeOutcome::Error("not a frame from alloc"),
                },
            };
        };
        let label = registry[slot].map(|(_, domain)| domain);
        if let Err(denied) = security::check(Op::Free, label, Some(address)) {
            return FreeOutcome::Denied(denied);
        }
        match frames.free(address) {
            Ok(()) => {
                registry[slot] = None;
                FreeOutcome::Freed(frames.free_frames())
            }
            Err(error) => FreeOutcome::Error(error.message()),
        }
    })
}

fn show_security() {
    let (pid, name) = tasks::current();
    let (text_start, text_end) = paging::text_range();
    kprintln!("Subject: pid {} ({}) in domain {}", pid, name, tasks::current_domain().name());
    kprintln!("Paging:  NX {} | text 0x{:x}..0x{:x} read-only | rodata read-only | data NX",
        if paging::nx_enabled() { "on" } else { "unsupported" }, text_start, text_end);
    kprintln!("         page 0 unmapped | CR0.WP on | IST1 stack for #DF | stack canaries");
    kprintln!("MAC:     enforcing; policy compiled in, no command changes it");
    kprintln!("  admin  halt reboot fault audit alloc spawn; free/kill admin+user objects");
    kprintln!("  user   alloc spawn; free/kill user objects only");
    kprintln!("  kernel objects (idle) are off limits; domains only go admin -> user");
    kprintln!("Objects: tasks and alloc frames carry their creator's domain; frames are");
    kprintln!("         zeroed before reuse. Audit: {} denials since boot.", security::denials());
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
            // Page 0 is not mapped.
            Fault::NullPointer => asm!("mov {0}, qword ptr [{0}]", inout(reg) 0u64 => _, options(readonly, nostack)),
            // Kernel code is mapped read-only and CR0.WP makes ring 0 obey it.
            Fault::ReadOnly => asm!("mov byte ptr [{}], 0xcc", in(reg) paging::text_range().0, options(nostack)),
            // Data pages are NX: the fetch faults before any byte runs.
            Fault::NoExecute => asm!("call {}", in(reg) addr_of!(NOT_CODE) as u64, clobber_abi("C")),
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
    outb(SERIAL + 2, 0x07); // FIFO enable/reset; IRQ at every received byte.
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
