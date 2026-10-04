#![no_std]
#![no_main]
mod common;
#[no_mangle] #[link_section=".text.entry"]
pub unsafe extern "C" fn _start(ptr: *const u8, len: usize) -> ! {
    let ms=common::number(common::args(ptr,len),100).min(60_000);
    common::print(b"sleep: begin\n"); let result=common::sleep(ms);
    if result<0 { common::print(b"sleep: error\n"); common::exit(1); }
    common::print(b"sleep: awake\n"); common::exit(0)
}
