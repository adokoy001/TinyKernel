#![allow(dead_code)]
use core::arch::asm;

// int 0x80 preserves all registers except RAX; the kernel copies pointer data.
#[inline(always)]
pub fn syscall(number: u64, a: u64, b: u64, c: u64) -> i64 {
    let result: i64;
    unsafe { asm!("int 0x80", inlateout("rax") number => result, in("rdi") a,
        in("rsi") b, in("rdx") c, options(nostack)); }
    result
}
pub fn exit(status: i64) -> ! { syscall(0,status as u64,0,0); loop { core::hint::spin_loop(); } }
pub fn print(text: &[u8]) -> i64 { syscall(1,text.as_ptr() as u64,text.len() as u64,0) }
pub fn pid() -> u64 { syscall(2,0,0,0) as u64 }
pub fn yield_now() { syscall(3,0,0,0); }
pub fn sleep(ms: u64) -> i64 { syscall(4,ms,0,0) }
pub fn ticks() -> u64 { syscall(10,0,0,0) as u64 }
pub unsafe fn args<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    core::slice::from_raw_parts(ptr, len.min(128))
}
pub fn number(bytes: &[u8], default: u64) -> u64 {
    if bytes.is_empty() { return default; }
    let mut n=0u64;
    for &b in bytes { if !b.is_ascii_digit() { return default; } n=n.saturating_mul(10).saturating_add((b-b'0') as u64); }
    n
}
pub fn print_number(mut n: u64) {
    let mut buf=[0u8;20]; let mut at=buf.len();
    loop { at-=1; buf[at]=b'0'+(n%10) as u8; n/=10; if n==0 { break; } }
    print(&buf[at..]);
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { print(b"user panic\n"); exit(127) }
// Volatile byte loops keep these compiler runtime hooks integer-only.
#[no_mangle] pub unsafe extern "C" fn memcpy(dst:*mut u8,src:*const u8,n:usize)->*mut u8 {
    for i in 0..n { dst.add(i).write_volatile(src.add(i).read_volatile()); } dst
}
#[no_mangle] pub unsafe extern "C" fn memset(dst:*mut u8,value:i32,n:usize)->*mut u8 {
    for i in 0..n { dst.add(i).write_volatile(value as u8); } dst
}
#[no_mangle] pub unsafe extern "C" fn memmove(dst:*mut u8,src:*const u8,n:usize)->*mut u8 {
    if (dst as usize)<(src as usize) { memcpy(dst,src,n) } else { for i in (0..n).rev() { dst.add(i).write_volatile(src.add(i).read_volatile()); } dst }
}
#[no_mangle] pub unsafe extern "C" fn memcmp(a:*const u8,b:*const u8,n:usize)->i32 {
    for i in 0..n { let av=a.add(i).read_volatile(); let bv=b.add(i).read_volatile(); if av!=bv { return av as i32-bv as i32; } } 0
}
