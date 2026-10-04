#![no_std]
#![no_main]
mod common;
use core::arch::asm;
const BASE: u64 = common::HEAP_BASE;
unsafe fn fill(start: usize, end: usize, value: u8) {
    for offset in start..end { ((BASE as usize + offset) as *mut u8).write_volatile(value); }
}
unsafe fn verify(start: usize, end: usize, value: u8) -> bool {
    for offset in start..end { if ((BASE as usize + offset) as *const u8).read_volatile() != value { return false; } }
    true
}
fn check(ok: bool) { if !ok { common::print(b"heap: check failed\n"); common::exit(1); } }
#[no_mangle] #[link_section=".text.entry"]
pub unsafe extern "C" fn _start(ptr: *const u8, len: usize) -> ! {
    let arg = common::args(ptr, len);
    if arg == b"quota" {
        check(common::heap(1) == -13);
        common::print(b"heap: shared user quota rejected growth\n"); common::exit(0);
    }
    if let Some(seed) = arg.strip_prefix(b"hold ") {
        let value = common::number(seed, 85) as u8;
        check(common::heap(8) == BASE as i64 && verify(0, 32768, 0));
        fill(0, 32768, value);
        common::print(b"heap: holding 8 zeroed pages\n"); common::sleep(5000);
        check(verify(0, 32768, value));
        common::print(b"heap: held bytes intact\n"); common::exit(0);
    }
    if arg == b"freed" {
        check(common::heap(2) == BASE as i64);
        fill(4096, 4097, 0x55); check(common::heap(1) == BASE as i64);
        common::print(b"heap: accessing freed page\n");
        asm!("mov al, byte ptr [rdx]", in("rdx") BASE + 4096, out("al") _, options(nostack));
    } else if arg == b"nx" {
        check(common::heap(1) == BASE as i64); fill(0, 1, 0xc3);
        common::print(b"heap: executing NX page\n"); asm!("call rax", in("rax") BASE);
    } else if arg == b"guard" {
        common::print(b"heap: accessing unmapped guard\n");
        asm!("mov al, byte ptr [rdx]", in("rdx") BASE - 4096, out("al") _, options(nostack));
    } else if arg == b"limit" {
        check(common::heap(9) == -22 && common::heap(u64::MAX) == -22);
        check(common::syscall(14, 1, 1, 0) == -22 && common::syscall(14, 1, 0, 1) == -22);
        check(common::heap(8) == BASE as i64 && verify(0, 32768, 0));
        fill(0, 32768, 0xab); check(common::heap(0) == BASE as i64);
        check(common::heap(8) == BASE as i64 && verify(0, 32768, 0));
        common::print(b"heap: limit flags and full release-regrow zeroing ok\n"); common::exit(0);
    } else {
        check(arg.is_empty() || arg == b"basic");
        check(common::heap(0) == BASE as i64 && common::heap(3) == BASE as i64);
        check(verify(0, 12288, 0)); fill(0, 1, 0x5a);
        for (index, byte) in b"CROSSPAGE".iter().enumerate() { ((BASE + 4093 + index as u64) as *mut u8).write_volatile(*byte); }
        check(common::syscall(1, BASE + 4093, 9, 0) == 9); common::print(b"\n");
        check(common::heap(1) == BASE as i64 && verify(0, 1, 0x5a));
        check(common::heap(3) == BASE as i64 && verify(4096, 12288, 0));
        check(verify(0, 1, 0x5a) && common::heap(0) == BASE as i64);
        common::print(b"heap: grow cross-page shrink-regrow zeroing ok\n"); common::exit(0);
    }
    common::print(b"heap: forbidden access returned\n"); common::exit(2)
}
