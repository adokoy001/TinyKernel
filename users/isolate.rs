#![no_std]
#![no_main]
mod common;
// Both instances use this same virtual address, backed by different frames.
static mut SENTINEL: u64=0;
#[no_mangle] #[link_section=".text.entry"]
pub unsafe extern "C" fn _start(ptr: *const u8, len: usize) -> ! {
    let seed=common::number(common::args(ptr,len),common::pid());
    let slot=&raw mut SENTINEL;
    if slot.read_volatile()!=0 { common::print(b"isolate: dirty new page\n"); common::exit(2); }
    slot.write_volatile(seed);
    let start=common::ticks();
    while common::ticks().wrapping_sub(start)<20 {
        if slot.read_volatile()!=seed { common::print(b"isolate: corruption\n"); common::exit(3); }
        // No voluntary yield: the timer must preempt this process.
        core::arch::asm!("pause",options(nomem,nostack));
    }
    common::print(b"isolate: ok seed="); common::print_number(seed); common::print(b"\n"); common::exit(0)
}
