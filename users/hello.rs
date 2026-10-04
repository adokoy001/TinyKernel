#![no_std]
#![no_main]
mod common;
#[no_mangle] #[link_section=".text.entry"]
pub unsafe extern "C" fn _start(_: *const u8, _: usize) -> ! {
    common::print(b"Hello from Rust ring3. pid="); common::print_number(common::pid());
    common::print(b"\n"); common::exit(0)
}
