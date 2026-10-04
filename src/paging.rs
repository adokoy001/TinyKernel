//! Kernel page tables with W^X. The boot sector maps the first 1 GiB as
//! writable and executable 2 MiB pages; this replaces that with:
//!   - page 0 unmapped, so null pointer accesses fault,
//!   - kernel .text read-only and executable,
//!   - kernel .rodata read-only and non-executable,
//!   - every other page writable and non-executable (NX),
//! and sets CR0.WP so ring 0 code also honours read-only pages.

use core::arch::asm;
use core::arch::x86_64::__cpuid;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, Ordering};

const PRESENT: u64 = 1;
const WRITABLE: u64 = 1 << 1;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
const PAGE: u64 = 4096;
const LARGE_PAGE: u64 = 2 << 20;
const EFER: u32 = 0xc000_0080;
const EFER_NXE: u64 = 1 << 11;
const CR0_WP: u64 = 1 << 16;

#[repr(C, align(4096))]
struct Table([u64; 512]);

static mut PML4: Table = Table([0; 512]);
static mut PDPT: Table = Table([0; 512]);
static mut PD: Table = Table([0; 512]);
/// 4 KiB pages for the first 2 MiB, where the kernel image lives.
static mut LOW_PT: Table = Table([0; 512]);

static NX_ENABLED: AtomicBool = AtomicBool::new(false);

extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_end: u8;
}

pub fn nx_enabled() -> bool {
    NX_ENABLED.load(Ordering::Relaxed)
}

/// The kernel's original address space. Process roots share its supervisor
/// page directory, never a user-accessible alias of the kernel mapping.
pub fn kernel_root() -> u64 {
    addr_of!(PML4) as u64
}

pub fn kernel_pd() -> u64 {
    addr_of!(PD) as u64
}

/// Change address space while executing in the shared supervisor mapping.
/// The caller must own a complete, aligned root and serialize scheduling.
pub unsafe fn activate(root: u64) {
    asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
}

/// Build and load the hardened tables. Call once, with interrupts disabled.
pub unsafe fn init() {
    let extended = __cpuid(0x8000_0000).eax;
    let nx_supported = extended >= 0x8000_0001 && __cpuid(0x8000_0001).edx & (1 << 20) != 0;
    let nx = if nx_supported { NO_EXECUTE } else { 0 };
    if nx_supported {
        // NXE must be on before any entry sets bit 63, or it is a reserved bit.
        let (low, high): (u32, u32);
        asm!("rdmsr", in("ecx") EFER, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags));
        let value = ((high as u64) << 32 | low as u64) | EFER_NXE;
        asm!("wrmsr", in("ecx") EFER, in("eax") value as u32, in("edx") (value >> 32) as u32,
             options(nostack, preserves_flags));
    }

    let text = addr_of!(__text_start) as u64..addr_of!(__text_end) as u64;
    let rodata = text.end..addr_of!(__rodata_end) as u64;
    let low_pt = &mut (*addr_of_mut!(LOW_PT)).0;
    for (index, entry) in low_pt.iter_mut().enumerate() {
        let address = index as u64 * PAGE;
        *entry = if index == 0 {
            0
        } else if text.contains(&address) {
            address | PRESENT
        } else if rodata.contains(&address) {
            address | PRESENT | nx
        } else {
            address | PRESENT | WRITABLE | nx
        };
    }
    let pd = &mut (*addr_of_mut!(PD)).0;
    pd[0] = addr_of!(LOW_PT) as u64 | PRESENT | WRITABLE;
    for (index, entry) in pd.iter_mut().enumerate().skip(1) {
        *entry = index as u64 * LARGE_PAGE | PRESENT | WRITABLE | HUGE | nx;
    }
    // Upper levels allow everything; the leaf entries decide.
    (*addr_of_mut!(PDPT)).0[0] = addr_of!(PD) as u64 | PRESENT | WRITABLE;
    (*addr_of_mut!(PML4)).0[0] = addr_of!(PDPT) as u64 | PRESENT | WRITABLE;

    asm!("mov cr3, {}", in(reg) addr_of!(PML4) as u64, options(nostack, preserves_flags));
    let mut cr0: u64;
    asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
    cr0 |= CR0_WP;
    asm!("mov cr0, {}", in(reg) cr0, options(nostack, preserves_flags));
    NX_ENABLED.store(nx_supported, Ordering::Relaxed);
}

pub fn text_range() -> (u64, u64) {
    (addr_of!(__text_start) as u64, addr_of!(__text_end) as u64)
}
