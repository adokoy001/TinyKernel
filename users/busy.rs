#![no_std]
#![no_main]
mod common;
#[no_mangle] #[link_section=".text.entry"]
pub unsafe extern "C" fn _start(_: *const u8, _: usize) -> ! {
    common::print(b"busy: preemptible user loop\n");
    loop { core::arch::asm!("pause", options(nomem,nostack)); }
}
