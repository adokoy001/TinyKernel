#![no_std]
#![no_main]

// Tane OS: an original preemptive kernel. No kernel heap or external crates.
// Hardened with W^X paging and a compiled-in mandatory access control policy.

// Each print borrows the global console only for the duration of one line.
macro_rules! kprintln {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        writeln!(crate::console(), $($arg)*).ok();
    }};
}

mod ata;
mod editor;
mod executable;
mod frames;
mod fs;
mod handles;
mod interrupts;
mod inet;
mod mac;
mod net;
mod netstack;
mod operations;
mod paging;
mod pci;
mod plans;
mod process;
mod records;
mod resources;
mod rtl8139;
mod sched;
mod security;
mod shell;
mod shell_lang;
mod shell_runtime;
mod storage;
mod tasks;
mod user_images;
mod user_abi;
mod user_syscalls;
mod usermem;
mod variables;

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
/// The boot sector stores the BIOS E820 map here (see boot.S).
const E820_COUNT: *const u16 = 0x5000 as *const u16;
const E820_ENTRIES: *const E820Entry = 0x5010 as *const E820Entry;
const E820_MAX: usize = 64;
/// The low MiB contains BIOS, the loaded kernel, VGA and the boot stack.
/// BSS begins at 1 MiB; its rounded end is also reserved from allocation.
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

static mut CONSOLE: Console = Console { x: 0, y: 0, color: 0x07, scrolls: 0 };
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
    static __loaded_kernel_end: u8;
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
        // Until an extended CPU-state ABI exists, hardware prevents user
        // programs from using x87/MMX/SSE. The build checks kernel code too.
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        cr0 |= (1 << 3) | (1 << 1); // TS + MP: unsupported state traps as #NM.
        asm!("mov cr0, {}", in(reg) cr0, options(nostack, preserves_flags));
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        cr4 &= !((1 << 16) | (1 << 18)); // No FSGSBASE or XSAVE/AVX ABI.
        asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags));
        for msr in [0xc000_0100u32, 0xc000_0101, 0xc000_0102] {
            asm!("wrmsr", in("ecx") msr, in("eax") 0u32, in("edx") 0u32, options(nostack));
        }
        let (efer_low, efer_high): (u32, u32);
        asm!("rdmsr", in("ecx") 0xc000_0080u32, out("eax") efer_low, out("edx") efer_high, options(nostack));
        // int 0x80 is the sole userspace entry ABI; inherited firmware MSRs
        // must not enable alternate entries into an unprepared kernel stack.
        asm!("wrmsr", in("ecx") 0xc000_0080u32, in("eax") efer_low & !1, in("edx") efer_high, options(nostack));
        if core::arch::x86_64::__cpuid(1).edx & (1 << 11) != 0 {
            asm!("wrmsr", in("ecx") 0x174u32, in("eax") 0u32, in("edx") 0u32, options(nostack));
        }
        serial_init();
    }
    console().clear();
    console().color = 0x0b;
    kprintln!("TANE OS / RUST BARE METAL");
    console().color = 0x07;
    kprintln!("Self-made BIOS loader + Rust kernel. External crates: 0.");
    kprintln!("64-bit mode | VGA + COM1 | PS/2 keyboard | bounded user heap");
    let map = e820_map();
    let reserved_end = ((addr_of!(__kernel_end) as u64 + FRAME_SIZE - 1) & !(FRAME_SIZE - 1)).max(LOW_MEMORY_END);
    with_frames(|frames| frames.init(map, reserved_end));
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
    storage::init();
    match storage::status() {
        storage::Status::Absent => kprintln!("Disk: none (attach an IDE disk for TaneFS)"),
        storage::Status::Unformatted { error, .. } => kprintln!("Disk: ATA found; {}", error.message()),
        storage::Status::Mounted { files, .. } => kprintln!("Disk: TaneFS mounted, {} files", files),
    }
    match net::init() {
        Ok(info) => kprintln!("Network: RTL8139 MAC {} | static IPv4 + IPv6", inet::Mac(info.mac)),
        Err(error) => kprintln!("Network: unavailable ({})", error.message()),
    }
    if !SERIAL_PRESENT.load(Ordering::Relaxed) {
        kprintln!("COM1 not detected; VGA and keyboard only.");
    }
    kprintln!("Type help. English keyboard / ASCII input.");
    kprintln!("READY");

    let mut keyboard = Keyboard::new();
    let mut editor = editor::Editor::new();
    prompt();
    let mut display = EditorDisplay::new();
    loop {
        net::poll();
        // Serial and PS/2 are both hardware drivers implemented here. Check
        // with IRQs off so an arriving byte cannot slip in before HLT.
        interrupts::disable();
        // Keep physical navigation keys distinct from serial control bytes.
        let serial = unsafe { serial_read() };
        let physical = if serial.is_none() { keyboard.read_key() } else { None };
        interrupts::enable();
        if serial.is_some() || physical.is_some() {
            let old_len = editor.length();
            let old_cursor = editor.cursor();
            let mut old_line = [0u8; editor::LINE_CAPACITY];
            old_line[..old_len].copy_from_slice(editor.line().as_bytes());
            let event = match (serial, physical) {
                (Some(byte), _) => editor.feed(byte),
                (_, Some(key)) => editor.feed_key(key),
                _ => editor::Event::None,
            };
            match event {
                editor::Event::Submit => {
                    display.finish();
                    console().byte(b'\n');
                    if editor.overflowed() {
                        kprintln!("error: input line too long (maximum {} bytes); command rejected", editor::LINE_CAPACITY);
                        shell_runtime::reject_input();
                    } else {
                        // Runtime may erase privileged history after `drop`.
                        // Keep its command independent of that mutable editor.
                        let mut command = [0u8; editor::LINE_CAPACITY];
                        let length = editor.length();
                        command[..length].copy_from_slice(editor.line().as_bytes());
                        let line = core::str::from_utf8(&command[..length]).unwrap_or("");
                        shell_runtime::execute(line, &mut keyboard, &mut editor);
                    }
                    editor.clear();
                    prompt();
                    display = EditorDisplay::new();
                }
                editor::Event::Cancel => {
                    display.finish();
                    shell_runtime::cancel_input();
                    kprintln!("^C");
                    prompt();
                    display = EditorDisplay::new();
                }
                editor::Event::Complete => {
                    if shell_runtime::complete(&mut editor) {
                        prompt();
                        display = EditorDisplay::new();
                    }
                    display.redraw(editor.line(), editor.cursor());
                }
                editor::Event::Changed => {
                    if old_cursor == old_len && editor.length() == old_len + 1
                        && editor.cursor() == editor.length()
                        && editor.line().as_bytes()[..old_len] == old_line[..old_len] {
                        console().byte(editor.line().as_bytes()[old_len]);
                        display.updated(&editor, true);
                    } else if old_cursor == old_len && old_len > 0
                        && editor.length() + 1 == old_len && editor.cursor() == editor.length()
                        && editor.line().as_bytes() == &old_line[..editor.length()]
                        && (6 + old_cursor) % WIDTH != 0 {
                        console().backspace();
                        display.updated(&editor, false);
                    } else {
                        display.redraw(editor.line(), editor.cursor());
                    }
                }
                editor::Event::Full => unsafe { serial_write(7); },
                editor::Event::None => {}
            }
        } else {
            if net::present() {
                // Polling NIC: the PIT wakes this consumer every 10 ms so
                // peers can resolve addresses even at an idle prompt.
                tasks::sleep_until(interrupts::ticks().saturating_add(1));
            } else {
                tasks::wait_for_input(|| unsafe { serial_ready() } || keyboard.ready());
            }
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

/// The input anchor uses an absolute VGA row, so scrolling while a long line
/// wraps does not lose the position of its beginning. Serial redraw assumes
/// an 80-column ANSI terminal, matching the VGA console.
struct EditorDisplay {
    anchor: usize,
    length: usize,
    cursor: usize,
    serial_pending_wrap: bool,
}

impl EditorDisplay {
    fn new() -> Self {
        let c = console();
        Self { anchor: (c.scrolls + c.y) * WIDTH + c.x,
            length: 0, cursor: 0, serial_pending_wrap: false }
    }

    fn updated(&mut self, editor: &editor::Editor, appended: bool) {
        self.length = editor.length();
        self.cursor = editor.cursor();
        self.serial_pending_wrap = appended && (self.anchor + self.cursor) % WIDTH == 0;
    }

    fn serial_row(&self) -> usize {
        let row = (self.anchor % WIDTH + self.cursor) / WIDTH;
        if self.serial_pending_wrap { row.saturating_sub(1) } else { row }
    }

    fn serial_position(&mut self, cursor: usize) {
        let current_row = self.serial_row();
        let target_row = (self.anchor % WIDTH + cursor) / WIDTH;
        unsafe { serial_write(b'\r'); }
        if target_row < current_row { serial_csi(current_row - target_row, b'A'); }
        else if target_row > current_row { serial_csi(target_row - current_row, b'B'); }
        serial_csi((self.anchor + cursor) % WIDTH + 1, b'G');
        self.cursor = cursor;
        self.serial_pending_wrap = false;
    }

    fn redraw(&mut self, line: &str, cursor: usize) {
        // Clear and repaint the input on the serial terminal, including all
        // wrapped rows. Unsupported escape input never reaches this output.
        let old_row = self.serial_row();
        unsafe { serial_write(b'\r'); }
        serial_csi(old_row, b'A');
        for &byte in b"\x1b[Jtane" { unsafe { serial_write(byte); } }
        unsafe {
            serial_write(if tasks::current_domain() == Domain::Admin { b'>' } else { b'$' });
            serial_write(b' ');
        }
        for byte in line.bytes() { unsafe { serial_write(byte); } }
        self.cursor = line.len();
        self.serial_pending_wrap = (self.anchor % WIDTH + line.len()) % WIDTH == 0;
        self.serial_position(cursor);

        let c = console();
        let end_row = (self.anchor + line.len()) / WIDTH;
        while end_row >= c.scrolls + HEIGHT { c.scroll_one(); }
        let viewport = c.scrolls * WIDTH;
        let span = self.length.max(line.len());
        for index in 0..span {
            let cell = self.anchor + index;
            if cell >= viewport && cell < viewport + WIDTH * HEIGHT {
                unsafe { write_volatile(VGA.add(cell - viewport), ((c.color as u16) << 8) | 0x20); }
            }
        }
        for (index, byte) in line.bytes().enumerate() {
            let cell = self.anchor + index;
            if cell >= viewport && cell < viewport + WIDTH * HEIGHT {
                unsafe { write_volatile(VGA.add(cell - viewport), ((c.color as u16) << 8) | byte as u16); }
            }
        }
        c.x = (self.anchor + cursor) % WIDTH;
        c.y = (self.anchor + cursor) / WIDTH - c.scrolls;
        c.cursor();
        self.length = line.len();
        self.cursor = cursor;
    }

    fn finish(&mut self) {
        if self.cursor != self.length { self.serial_position(self.length); }
        let c = console();
        c.x = (self.anchor + self.length) % WIDTH;
        c.y = (self.anchor + self.length) / WIDTH - c.scrolls;
        c.cursor();
        self.cursor = self.length;
    }
}

fn serial_csi(mut number: usize, final_byte: u8) {
    if number == 0 { return; }
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    while number > 0 { start -= 1; digits[start] = b'0' + (number % 10) as u8; number /= 10; }
    unsafe { serial_write(0x1b); serial_write(b'['); }
    for &byte in &digits[start..] { unsafe { serial_write(byte); } }
    unsafe { serial_write(final_byte); }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecState { Success, Error, Denied, Cancelled, CommitUnknown }

pub(crate) fn cancelled(keyboard: &mut Keyboard) -> bool {
    unsafe { serial_read() }.or_else(|| keyboard.read()) == Some(3)
}

pub(crate) fn execute_action(action: Action<'_>, keyboard: &mut Keyboard) -> ExecState {
    let mut state = ExecState::Success;
    match action {
        Action::Help => {
            shell_runtime::help(&[]);
        }
        Action::About => {
            kprintln!("Tane OS 0.9 - an original Rust kernel with isolated user processes.");
            kprintln!("No Linux code, GRUB, external crates, libc, or host OS calls.");
            kprintln!("Firmware loads our 512-byte boot sector; it loads this kernel.");
            kprintln!("Ring 0, own GDT/TSS/IDT, PIT at {} Hz, E820 RAM in 4 KiB frames.", interrupts::TIMER_HZ);
            kprintln!("Preemptive round-robin kernel tasks with stacks from the frame allocator.");
            kprintln!("W^X paging, NX data, table-driven MAC, per-domain quotas, audit log.");
            kprintln!("ATA disk with TaneFS: labelled, checksummed files that survive reboots.");
            kprintln!("RTL8139, ARP/IPv4/ICMP, NDP/IPv6/ICMPv6; static network configuration.");
            kprintln!("Tane Shell: bounded editor, typed records, pipelines, preview and explicit apply.");
            kprintln!("Kernel tasks use ring 0; user processes use ring 3 and separate address spaces.");
            kprintln!("User faults terminate only the process. Bounded int 0x80 syscalls enforce MAC.");
            kprintln!("FPU/SIMD user instructions are unsupported and trap as #NM.");
        }
        Action::Memory => {
            let end = addr_of!(__kernel_end) as u64;
            kprintln!("Page tables: in kernel BSS (the boot sector's at 0x1000..0x4000 are retired)");
            kprintln!("E820 table:  0x5000..0x5610 (written by the boot sector)");
            kprintln!("Boot sector: 0x7c00..0x7e00");
            kprintln!("Kernel image: 0x10000..0x{:x}", addr_of!(__loaded_kernel_end) as u64);
            kprintln!("Kernel BSS:   0x100000..0x{:x}; frames reserved through 0x{:x}", end, (end + FRAME_SIZE - 1) & !(FRAME_SIZE - 1));
            kprintln!("Shell stack: 0x80000..0x90000 (grows down)");
            kprintln!("VGA text:    0xb8000");
            kprintln!("First 1 GiB identity mapped: 4 KiB pages below 2 MiB, then 2 MiB pages.");
            kprintln!("No kernel heap. User heap: 0-8 pages. Command buffer: {} bytes (max {} input).", editor::LINE_CAPACITY, editor::LINE_CAPACITY);
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
        | Action::Free(Err(error)) | Action::Spawn(Err(error)) | Action::Kill(Err(error))
        | Action::Cat(Err(error)) | Action::Remove(Err(error)) | Action::WriteUsage(error)
        | Action::Ping(Err(error)) => { kprintln!("error: {}", error); state = ExecState::Error; },
        Action::Sleep(Ok(ms)) => {
            // Round up so the wait is never shorter than requested.
            let deadline = interrupts::ticks().saturating_add((ms * interrupts::TIMER_HZ).div_ceil(1000));
            while interrupts::ticks() < deadline {
                net::poll();
                if unsafe { serial_read() }.or_else(|| keyboard.read()) == Some(3) {
                    kprintln!("cancelled");
                    return ExecState::Cancelled;
                }
                tasks::sleep_until(interrupts::ticks().saturating_add(1).min(deadline));
            }
            kprintln!("slept {} ms", ms);
        }
        Action::Alloc => match allocate_frame() {
            Ok((address, free)) => kprintln!("allocated 0x{:x} (zeroed); {} frames free", address, free),
            Err(Err(denied)) => { report_denied(denied); state = ExecState::Denied; },
            Err(Ok(error)) => { kprintln!("error: {}", error); state = ExecState::Error; },
        },
        Action::Free(Ok(address)) => match free_frame(address) {
            FreeOutcome::Freed(free) => kprintln!("freed 0x{:x}; {} frames free", address, free),
            FreeOutcome::Denied(denied) => { report_denied(denied); state = ExecState::Denied; },
            FreeOutcome::TaskStack(pid) => { kprintln!("error: 0x{:x} is in the stack of pid {}; use kill", address, pid); state = ExecState::Error; },
            FreeOutcome::Error(error) => { kprintln!("error: 0x{:x}: {}", address, error); state = ExecState::Error; },
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
            Err(tasks::SpawnError::Denied(denied)) => { report_denied(denied); state = ExecState::Denied; },
            Err(tasks::SpawnError::Failed(error)) => { kprintln!("error: {}", error); state = ExecState::Error; },
            Err(tasks::SpawnError::NoMemory(error)) => { kprintln!("error: {}", error); state = ExecState::Error; },
            Err(tasks::SpawnError::ChildPending) => { kprintln!("error: consume the pending child result first"); state = ExecState::Error; },
        },
        Action::Kill(Ok(pid)) => match tasks::kill(pid) {
            Ok(name) => kprintln!("killed pid {} ({}); {} frames free", pid, name, with_frames(|frames| frames.free_frames())),
            Err(tasks::KillError::Denied(denied)) => { report_denied(denied); state = ExecState::Denied; },
            Err(tasks::KillError::Failed(error)) => { kprintln!("error: {}", error); state = ExecState::Error; },
        },
        Action::Fault(Ok(fault)) => {
            if permitted(Op::Fault) {
                raise(fault);
            } else { state = ExecState::Denied; }
        }
        Action::Security => show_security(),
        Action::Disk => match storage::status() {
            storage::Status::Absent => kprintln!("No ATA disk on the primary channel."),
            storage::Status::Unformatted { model, sectors, error } =>
                kprintln!("ATA disk \"{}\", {} KiB: {}", model, sectors / 2, error.message()),
            storage::Status::Mounted { model, sectors, files, .. } =>
                kprintln!("ATA disk \"{}\", {} KiB: TaneFS mounted, {} of {} files used", model, sectors / 2,
                    files, fs::MAX_FILES),
        },
        Action::Format => match storage::format() {
            Ok(()) => kprintln!("formatted: empty TaneFS, {} files of up to {} bytes", fs::MAX_FILES, fs::MAX_FILE_SIZE),
            Err(error) => state = report_storage_mutation(error),
        },
        Action::List => {
            kprintln!("SLOT LABEL    SIZE  GEN NAME");
            let mut shown = 0;
            let result = storage::list(|file| {
                kprintln!("{:>4} {:<6} {:>6} {:>4} {}", file.slot, file.label.name(), file.size, file.generation, file.name);
                shown += 1;
            });
            match result {
                Ok(()) => kprintln!("{} file(s) readable by domain {}", shown, tasks::current_domain().name()),
                Err(error) => state = report_storage(error),
            }
        }
        Action::Cat(Ok(name)) => {
            let mut buffer = [0u8; fs::MAX_FILE_SIZE];
            match storage::read(name, &mut buffer) {
                Ok(size) => {
                    for &byte in &buffer[..size] {
                        // Show text as is; other bytes as dots.
                        console().byte(if byte == b'\n' || (b' '..=b'~').contains(&byte) { byte } else { b'.' });
                    }
                    if size == 0 || buffer[size - 1] != b'\n' {
                        kprintln!();
                    }
                }
                Err(error) => state = report_storage(error),
            }
        }
        Action::Write { name, text, append } => match storage::write(name, text.as_bytes(), append) {
            Ok(true) => kprintln!("created {} ({} bytes, label {})", name, text.len(), tasks::current_domain().name()),
            Ok(false) => kprintln!("{} {} ({} bytes)", if append { "appended to" } else { "wrote" }, name, text.len()),
            Err(error) => state = report_storage_mutation(error),
        },
        Action::Remove(Ok(name)) => match storage::remove(name) {
            Ok(()) => kprintln!("removed {}", name),
            Err(error) => state = report_storage_mutation(error),
        },
        Action::Resources => show_resources(),
        Action::Network => show_network(),
        Action::Ping(Ok(request)) => {
            let Some(target) = inet::parse(request.address) else {
                kprintln!("error: invalid literal IPv4/IPv6 address (DNS is not implemented)");
                return ExecState::Error;
            };
            let result = net::ping(target, request.count, request.timeout_ms,
                || unsafe { serial_read() }.or_else(|| keyboard.read()) == Some(3),
                |event| match event {
                    net::Event::Reply(reply) => kprintln!("reply from {}: seq={} bytes={} ttl={} time={} ms",
                        reply.source, reply.sequence, reply.bytes, reply.ttl, reply.rtt_ticks * 1000 / interrupts::TIMER_HZ),
                    net::Event::Timeout { sequence } => kprintln!("timeout: seq={}", sequence),
                });
            match result {
                Ok(summary) => {
                    if summary.completion == net::Completion::Cancelled { kprintln!("cancelled"); state = ExecState::Cancelled; }
                    else if summary.received != summary.sent { state = ExecState::Error; }
                    kprintln!("{} sent, {} received", summary.sent, summary.received);
                }
                Err(net::Error::Denied(denied)) => { report_denied(denied); state = ExecState::Denied; },
                Err(net::Error::Absent) => { kprintln!("error: network unavailable (no RTL8139 detected)"); state = ExecState::Error; },
                Err(net::Error::InvalidRequest) => { kprintln!("error: invalid ping bounds"); state = ExecState::Error; },
                Err(net::Error::Device(error)) => { kprintln!("error: network device: {}", error.message()); state = ExecState::Error; },
                Err(net::Error::Protocol(error)) => { kprintln!("error: network: {}", error.message()); state = ExecState::Error; },
            }
        }
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
                    let reason = match record.reason { mac::Reason::Policy => "policy", mac::Reason::Quota => "quota", mac::Reason::Capability => "capability" };
                    match (record.op, record.target) {
                        (Op::Kill, Some(pid)) => kprintln!(" (pid {}) DENIED by {}", pid, reason),
                        (Op::Free, Some(address)) => kprintln!(" (0x{:x}) DENIED by {}", address, reason),
                        (_, Some(slot)) => kprintln!(" (file #{}) DENIED by {}", slot, reason),
                        (_, None) => kprintln!(" DENIED by {}", reason),
                    }
                });
            } else { state = ExecState::Denied; }
        }
        Action::DropToUser => match tasks::lower_domain(Domain::User) {
            Ok(from) => kprintln!("domain {} -> user; this cannot be undone until reboot", from.name()),
            Err(Domain::User) => kprintln!("already in domain user; no command raises a domain"),
            Err(current) => { kprintln!("error: domain {} cannot be lowered to user", current.name()); state = ExecState::Error; },
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
            } else { state = ExecState::Denied; }
        }
        Action::Reboot => {
            if !permitted(Op::Reboot) {
                return ExecState::Denied;
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
        Action::Unknown => { kprintln!("Unknown command. Type help."); state = ExecState::Error; },
        Action::Empty => {}
    }
    state
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
    if let Some(quota) = denied.quota {
        kprintln!("denied: {} {} limit reached ({}/{}) for {} (quota; audited)", denied.subject.name(),
            quota.resource.name(), quota.used, quota.limit, denied.op.name());
        return;
    }
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
        let slot = registry.iter().position(Option::is_none).ok_or(Ok("alloc registry is full (32 frames)"))?;
        security::charge(Op::Alloc, 0, 1).map_err(Err)?;
        let Some(address) = frames.allocate() else {
            security::release(domain, 0, 1);
            return Err(Ok("no free physical frames"));
        };
        registry[slot] = Some((address, domain));
        Ok(address)
    })?;
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
                if let Some(owner) = label {
                    security::release(owner, 0, 1);
                }
                FreeOutcome::Freed(frames.free_frames())
            }
            Err(error) => FreeOutcome::Error(error.message()),
        }
    })
}

fn report_storage_mutation(error: storage::StorageError) -> ExecState {
    let uncertain = error.commit_unknown();
    let state = report_storage(error);
    if uncertain { ExecState::CommitUnknown } else { state }
}

fn report_storage(error: storage::StorageError) -> ExecState {
    match error {
        storage::StorageError::NoDisk => { kprintln!("error: no disk"); ExecState::Error }
        storage::StorageError::Denied(denied) => { report_denied(denied); ExecState::Denied }
        storage::StorageError::Stale => { kprintln!("error: file capability target changed"); ExecState::Error }
        storage::StorageError::IdentityExhausted => { kprintln!("error: file identity exhausted"); ExecState::Error }
        storage::StorageError::Fs(error) => {
            kprintln!("error: {}", error.message());
            ExecState::Error
        }
    }
}

fn process_read_error(error: process::ReadError) -> ExecState {
    match error {
        process::ReadError::Denied(denied) => { report_denied(denied); ExecState::Denied }
        process::ReadError::Missing => { kprintln!("error: no such user process or retained result"); ExecState::Error }
    }
}

fn process_spawn(result: Result<u32, tasks::SpawnError>) -> ExecState {
    match result {
        Ok(pid) => { kprintln!("started user process as pid {} (ring 3, domain user, 12 frames)", pid); ExecState::Success }
        Err(tasks::SpawnError::Denied(denied)) => { report_denied(denied); ExecState::Denied }
        Err(tasks::SpawnError::Failed(message)) => { kprintln!("error: {}", message); ExecState::Error }
        Err(tasks::SpawnError::NoMemory(message)) => { kprintln!("error: {}", message); ExecState::Error }
        Err(tasks::SpawnError::ChildPending) => { kprintln!("error: consume the pending child result first"); ExecState::Error }
    }
}

/// Process output stays in a private kernel queue until a shell explicitly
/// reads it. Escape control bytes that could act on the host serial terminal.
pub(crate) fn escaped_process_output(bytes: &[u8], out: &mut impl Write) -> fmt::Result {
    for &byte in bytes {
        match byte {
            b'\n' | b'\t' | 0x20..=0x7e => out.write_char(byte as char)?,
            _ => write!(out, "\\x{:02x}", byte)?,
        }
    }
    Ok(())
}

pub(crate) fn execute_process(operation: operations::Operation, arguments: &[&str], keyboard: &mut Keyboard) -> ExecState {
    use operations::Operation as O;
    match operation {
        O::ProcessPrograms => {
            for program in user_images::PROGRAMS { kprintln!("{} {} bytes", program.name, program.bytes.len()); }
            ExecState::Success
        }
        O::ProcessList => {
            kprintln!("PID PARENT NAME DOMAIN STATE CPU_TICKS FRAMES OUTPUT");
            process::list(|info| kprintln!("{} {} {} {} {} {} {} {}", info.pid, info.parent_pid,
                info.name(), info.domain.name(), info.state, info.cpu_ticks, info.frames, info.output_len));
            ExecState::Success
        }
        O::ProcessRun => process_spawn(process::spawn_builtin(arguments[0], arguments.get(1).copied().unwrap_or(""))),
        O::ProcessExec => match process::spawn_file(arguments[0], arguments.get(1).copied().unwrap_or("")) {
            Ok(pid) => process_spawn(Ok(pid)),
            Err(process::FileSpawnError::Spawn(error)) => process_spawn(Err(error)),
            Err(process::FileSpawnError::Storage(error)) => report_storage(error),
        },
        O::ProcessInstall => {
            let Some(bytes) = user_images::builtin(arguments[0]) else { kprintln!("error: unknown builtin user program"); return ExecState::Error; };
            match storage::write(arguments[1], bytes, false) {
                Ok(_) => { kprintln!("installed {} as {} ({} bytes, label {})", arguments[0], arguments[1], bytes.len(), tasks::current_domain().name()); ExecState::Success }
                Err(error) => report_storage_mutation(error),
            }
        }
        O::ProcessPause | O::ProcessResume => {
            let pid = arguments[0].parse::<u32>().unwrap_or(0);
            let result = if operation == O::ProcessPause { tasks::pause(pid) } else { tasks::resume(pid) };
            match result {
                Ok(name) => { kprintln!("{} pid {} ({})", if operation == O::ProcessPause { "paused" } else { "resumed" }, pid, name); ExecState::Success }
                Err(tasks::KillError::Denied(denied)) => { report_denied(denied); ExecState::Denied }
                Err(tasks::KillError::Failed(message)) => { kprintln!("error: {}", message); ExecState::Error }
            }
        }
        O::ProcessWait | O::ProcessOutput => {
            let pid = arguments[0].parse::<u32>().unwrap_or(0);
            if operation == O::ProcessWait {
                loop {
                    match process::info(pid) {
                        Ok(info) if info.finished() => break,
                        Ok(_) => {},
                        Err(error) => return process_read_error(error),
                    }
                    if cancelled(keyboard) { kprintln!("cancelled: wait for process {}", pid); return ExecState::Cancelled; }
                    net::poll();
                    tasks::sleep_until(interrupts::ticks().saturating_add(1));
                }
            }
            let mut state = ExecState::Success;
            let result = process::output(pid, |info, bytes| {
                if !bytes.is_empty() {
                    escaped_process_output(bytes, console()).ok();
                    if bytes.last() != Some(&b'\n') { kprintln!(); }
                }
                if operation == O::ProcessWait {
                    match info.reason {
                        Some(process::ExitReason::Exit(code)) => {
                            kprintln!("process {} exited status {} | domain user | cpu {} ticks", pid, code, info.cpu_ticks);
                            if code != 0 { state = ExecState::Error; }
                        }
                        Some(process::ExitReason::Fault { vector, error, rip, address }) => {
                            kprintln!("process {} fault vector {} error 0x{:x} rip 0x{:x} address 0x{:x}; process terminated", pid, vector, error, rip, address);
                            state = ExecState::Error;
                        }
                        Some(process::ExitReason::Killed) => { kprintln!("process {} killed", pid); state = ExecState::Error; }
                        None => {},
                    }
                    if info.truncated { kprintln!("process output queue refused an overflowing write"); }
                }
            });
            match result { Ok(()) => state, Err(error) => process_read_error(error) }
        }
        _ => ExecState::Error,
    }
}

fn show_resources() {
    kprintln!("DOMAIN  TASKS   FRAMES     FILES   CPU(last 1 s)  CPU CAP");
    for domain in [Domain::Admin, Domain::User] {
        let usage = security::usage(domain);
        let limit = resources::limits(domain);
        let files = storage::files_owned(domain).map(|n| n as i64).unwrap_or(-1);
        write!(console(), "{:<6} {:>2}/{:<2} {:>5}/{:<5} ", domain.name(), usage.tasks, limit.tasks,
            usage.frames, limit.frames).ok();
        if files < 0 {
            write!(console(), "   -/{:<3}", limit.files).ok();
        } else {
            write!(console(), "{:>4}/{:<3}", files, limit.files).ok();
        }
        kprintln!("  {:>3}% ({:>3} ticks)  {:>3}%", usage.last_window_ticks * 100 / resources::CPU_WINDOW,
            usage.last_window_ticks, limit.cpu_percent);
    }
    kprintln!("Shell and idle are not charged. CPU caps apply while other domains wait.");
}

fn show_network() {
    let Some(status) = net::status() else {
        kprintln!("Network: unavailable (no RTL8139 detected)");
        return;
    };
    let config = status.config;
    kprintln!("RTL8139 I/O 0x{:x} MAC {}", status.info.io, inet::Mac(status.info.mac));
    kprintln!("IPv4 {} mask {} gateway {}", inet::IpAddr::V4(config.ipv4),
        inet::IpAddr::V4(config.netmask4), inet::IpAddr::V4(config.gateway4));
    kprintln!("IPv6 {}/{} gateway {}", inet::IpAddr::V6(config.ipv6), config.prefix6,
        inet::IpAddr::V6(config.gateway6));
    kprintln!("RX {} TX {} dropped {} | {} cached neighbors",
        status.stats.rx, status.stats.tx, status.stats.dropped, status.neighbors);
    kprintln!("Polling, MTU 1500, static addresses; 10 ms timer resolution");
}

fn show_security() {
    let (pid, name) = tasks::current();
    let (text_start, text_end) = paging::text_range();
    kprintln!("Subject: pid {} ({}) in domain {}", pid, name, tasks::current_domain().name());
    kprintln!("Paging:  NX {} | text 0x{:x}..0x{:x} read-only | rodata read-only | data NX",
        if paging::nx_enabled() { "on" } else { "unsupported" }, text_start, text_end);
    kprintln!("         page 0 unmapped | CR0.WP on | IST1 stack for #DF | stack canaries");
    kprintln!("MAC:     enforcing; this table is the whole policy (compiled in):");
    for rule in mac::POLICY.iter() {
        write!(console(), "  {:<6} {:<7}", rule.subject.name(), rule.class.name()).ok();
        for op in rule.ops {
            write!(console(), "{} ", op.name()).ok();
        }
        if rule.ops.iter().any(|op| op.has_object()) {
            write!(console(), "| objects:").ok();
            for label in rule.objects {
                write!(console(), " {}", label.name()).ok();
            }
        }
        kprintln!();
    }
    kprintln!("  Not listed = denied. Kernel objects: never. Domains: admin -> user only.");
    kprintln!("Objects: labelled with their creator's domain; frames zeroed before reuse.");
    kprintln!("Audit:   {} denials since boot (policy and quota). Limits: see top.", security::denials());
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

struct Console { x: usize, y: usize, color: u8, scrolls: usize }

impl Console {
    fn clear(&mut self) {
        for i in 0..WIDTH * HEIGHT {
            unsafe { write_volatile(VGA.add(i), 0x0720); }
        }
        self.x = 0;
        self.y = 0;
        self.scrolls = 0;
        self.cursor();
    }

    fn newline(&mut self) {
        self.x = 0;
        self.y += 1;
        if self.y == HEIGHT {
            self.scroll_one();
        }
    }

    fn scroll_one(&mut self) {
        for i in 0..WIDTH * (HEIGHT - 1) {
            unsafe { write_volatile(VGA.add(i), read_volatile(VGA.add(i + WIDTH))); }
        }
        for i in WIDTH * (HEIGHT - 1)..WIDTH * HEIGHT {
            unsafe { write_volatile(VGA.add(i), 0x0720); }
        }
        self.scrolls = self.scrolls.saturating_add(1);
        self.y = HEIGHT - 1;
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

struct Keyboard { left_shift: bool, right_shift: bool, left_ctrl: bool, right_ctrl: bool, caps: bool, extended: bool, skip: u8 }

impl Keyboard {
    fn new() -> Self { Self { left_shift: false, right_shift: false, left_ctrl: false, right_ctrl: false, caps: false, extended: false, skip: 0 } }

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

    fn read_key(&mut self) -> Option<editor::Key> {
        use editor::Key;
        Some(match self.read()? {
            // Private codes emitted only by extended PS/2 scan decoding.
            0x80 => Key::Left, 0x81 => Key::Right, 0x82 => Key::Home,
            0x83 => Key::End, 0x84 => Key::Up, 0x85 => Key::Down,
            0x86 => Key::Delete,
            1 => Key::Home, 2 => Key::Left, 3 => Key::Cancel, 4 => Key::Delete,
            5 => Key::End, 6 => Key::Right, 8 | 127 => Key::Backspace,
            9 => Key::Complete, 10 | 13 => Key::Submit, 11 => Key::ClearAfter,
            14 => Key::Down, 16 => Key::Up, 21 => Key::ClearBefore, 23 => Key::ClearWord,
            byte @ b' '..=b'~' => Key::Character(byte),
            _ => return None,
        })
    }

    fn decode(&mut self, scan: u8) -> Option<u8> {
        if self.skip > 0 { self.skip -= 1; return None; }
        if scan == 0xe1 { self.skip = 5; return None; } // Pause sequence.
        if scan == 0xe0 { self.extended = true; return None; }
        if self.extended {
            self.extended = false;
            if scan == 0x1d { self.right_ctrl = true; return None; }
            if scan == 0x9d { self.right_ctrl = false; return None; }
            return match scan {
                0x1c => Some(b'\n'), 0x4b => Some(0x80), 0x4d => Some(0x81),
                0x47 => Some(0x82), 0x4f => Some(0x83), 0x48 => Some(0x84),
                0x50 => Some(0x85), 0x53 => Some(0x86), _ => None,
            };
        }
        match scan {
            0x1d => { self.left_ctrl = true; return None; }
            0x9d => { self.left_ctrl = false; return None; }
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
            0x0f => 9,
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
        if (self.left_ctrl || self.right_ctrl) && b.is_ascii_lowercase() { return Some(b - b'a' + 1); }
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
