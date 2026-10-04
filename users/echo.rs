#![no_std]
#![no_main]
mod common;
#[no_mangle] #[link_section=".text.entry"]
pub unsafe extern "C" fn _start(ptr: *const u8, len: usize) -> ! {
    common::print(common::args(ptr,len)); common::print(b"\n"); common::exit(0)
}
