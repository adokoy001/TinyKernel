#![no_std]
#![no_main]
mod common;
use core::arch::asm;
// Explicit architectural accesses avoid Rust's null-pointer validity rules.
#[inline(always)] unsafe fn read_byte(address:u64) { asm!("mov al, byte ptr [rdx]",in("rdx")address,out("al")_,options(nostack)); }
#[inline(always)] unsafe fn write_byte(address:u64,byte:u8) { asm!("mov byte ptr [rdx], al",in("rdx")address,in("al")byte,options(nostack)); }
#[no_mangle] #[link_section=".text.entry"]
pub unsafe extern "C" fn _start(ptr:*const u8,len:usize)->! {
    let arg=common::args(ptr,len);
    if arg==b"badptr" {
        if common::syscall(1,0x10000,16,0)!=-14 ||
           common::syscall(1,0,16,0)!=-14 ||
           common::syscall(1,0x40005ff0,32,0)!=-14 ||
           common::syscall(1,0xfffffffffffffff0,32,0)!=-14 ||
           common::syscall(1,ptr as u64,257,0)!=-22 ||
           common::syscall(999,0,0,0)!=-38 {
            common::print(b"probe: syscall validation failure\n"); common::exit(2);
        }
        common::print(b"probe: bad pointers/length/syscall rejected\n"); common::exit(0);
    }
    if arg==b"denied" {
        for n in 11..=13 { if common::syscall(n,0,0,0)!=-13 { common::print(b"probe: privileged syscall accepted\n"); common::exit(3); } }
        common::print(b"probe: privileged syscalls denied\n"); common::exit(0);
    }
    common::print(b"probe: "); common::print(arg); common::print(b"\n");
    match arg {
        b"kernel-read" => read_byte(0x10000),
        b"kernel-data" => read_byte(0x100000),
        b"kernel-data-write" => write_byte(0x100000,0),
        b"kernel-write" => write_byte(0x10000,0),
        b"vga" => write_byte(0xb8000,b'X'),
        b"vga-read" => read_byte(0xb8000),
        b"code-write" => write_byte(0x40000000,0),
        b"nx" => { write_byte(0x40001000,0xc3); asm!("call rax",in("rax")0x40001000u64); },
        b"stack-nx" => { let mut byte=0xc3u8; asm!("call rax",in("rax")(&raw mut byte) as u64); },
        b"null" => read_byte(0),
        b"guard" => write_byte(0x40003000,0),
        b"stack-end" => write_byte(0x40006000,0),
        b"cli" => asm!("cli",options(nomem,nostack)),
        b"hlt" => asm!("hlt",options(nomem,nostack)),
        b"in" => asm!("in al, dx",in("dx")0x20u16,out("al")_,options(nomem,nostack)),
        b"out" => asm!("out dx, al",in("dx")0x20u16,in("al")0u8,options(nomem,nostack)),
        b"int48" => asm!("int 0x30",options(nomem,nostack)),
        b"sse" => asm!("xorps xmm0, xmm0",options(nomem,nostack)),
        b"x87" => asm!("fldz",options(nomem,nostack)),
        b"syscall" => asm!("syscall",out("rcx")_,out("r11")_,options(nostack)),
        b"sysenter" => asm!("sysenter",options(nostack)),
        b"fsgsbase" => asm!("wrfsbase rax",in("rax")0u64,options(nostack)),
        b"xsave" => asm!("xsave [rdi]",in("rdi")0x40001000u64,in("eax")0u32,in("edx")0u32,options(nostack)),
        b"ud2" => asm!("ud2",options(nomem,nostack)),
        _ => { common::print(b"probe: unknown case\n"); common::exit(1); },
    }
    common::print(b"probe: forbidden instruction returned\n"); common::exit(4)
}
