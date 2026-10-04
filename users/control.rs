#![no_std]
#![no_main]
mod common;
use common::{ChildStatus, SpawnRequest};
fn check(ok: bool) { if !ok { common::print(b"control: check failed\n"); common::exit(1); } }
fn wait(child: i64) -> ChildStatus {
    check(child > 0); let mut status = ChildStatus::empty();
    check(common::wait(child as u64, &mut status) == child && status.pid == child as u64); status
}
fn announce(child: i64) { common::print(b"control: child="); common::print_number(child as u64); common::print(b"\n"); }
#[no_mangle] #[link_section=".text.entry"]
pub unsafe extern "C" fn _start(ptr: *const u8, len: usize) -> ! {
    let arg = common::args(ptr, len);
    if arg == b"exit37" { common::exit(37); }
    if let Some(text) = arg.strip_prefix(b"foreign ") {
        let pid = common::number(text, 0); let mut status = ChildStatus::empty();
        check(common::wait(pid, &mut status) == -10 && common::kill(pid) == -10);
        common::print(b"control: live foreign process protected\n"); common::exit(0);
    }
    if arg == b"orphan" {
        let child = common::spawn(b"sleep", b"1500", 0); check(child > 0); announce(child);
        common::print(b"control: parent leaving live child\n"); common::exit(0);
    }
    if arg == b"live" {
        let child = common::spawn(b"sleep", b"1500", 0); check(child > 0); announce(child);
        let status = wait(child); check(status.kind == 0 && status.code == 0);
        common::print(b"control: blocked wait resumed\n"); common::exit(0);
    }
    if arg == b"quota" {
        check(common::heap(1) == common::HEAP_BASE as i64);
        check(common::spawn(b"hello", b"", 0) == -13);
        check(common::heap(0) == common::HEAP_BASE as i64);
        let status = wait(common::spawn(b"hello", b"", 0)); check(status.kind == 0 && status.code == 0);
        common::print(b"control: child quota rollback and retry ok\n"); common::exit(0);
    }
    if let Some(name) = arg.strip_prefix(b"file ") {
        let status = wait(common::spawn(name, b"user child file argument", 1));
        check(status.kind == 0 && status.code == 0);
        common::print(b"control: file child exited\n"); common::exit(0);
    }
    if let Some(name) = arg.strip_prefix(b"deniedfile ") {
        check(common::spawn(name, b"", 1) == -13);
        common::print(b"control: privileged child file denied\n"); common::exit(0);
    }
    if arg == b"pending" {
        let child = common::spawn(b"hello", b"", 0); check(child > 0); announce(child);
        common::sleep(1500); check(common::spawn(b"hello", b"", 0) == -11);
        let status = wait(child); check(status.kind == 0 && status.code == 0);
        common::print(b"control: completed child retained until wait\n"); common::exit(0);
    }
    if arg == b"validate" {
        let name = b"sleep";
        let mut request = SpawnRequest { name: name.as_ptr() as u64, name_len: name.len() as u64,
            argument: name.as_ptr() as u64, argument_len: 0, kind: 0 };
        let address = (&raw const request) as u64;
        check(common::syscall(15, 0x10000, 0, 0) == -14);
        check(common::syscall(15, 0x40005ff0, 0, 0) == -14);
        check(common::syscall(15, address, 1, 0) == -22 && common::syscall(15, address, 0, 1) == -22);
        request.kind = 2; check(common::syscall(15, address, 0, 0) == -22); request.kind = 0;
        request.argument = 0x10000; check(common::syscall(15, address, 0, 0) == -14);
        request.argument = name.as_ptr() as u64; request.argument_len = 129;
        check(common::syscall(15, address, 0, 0) == -22); request.argument_len = 0;
        request.name_len = 48; check(common::syscall(15, address, 0, 0) == -22);
        request.name = 0x40001ff0; request.name_len = 47;
        check(common::syscall(15, address, 0, 0) == -14);
        request.name = name.as_ptr() as u64; request.name_len = name.len() as u64;
        request.argument = 0x40005ff0; request.argument_len = 32;
        check(common::syscall(15, address, 0, 0) == -14);
        let mut result = ChildStatus::empty();
        check(common::wait(999999, &mut result) == -10 && common::kill(999999) == -10);
        let child = common::spawn(b"sleep", b"30", 0); check(child > 0);
        check(common::syscall(16, child as u64, 0x10000, 0) == -14);
        check(common::syscall(16, child as u64, 0x40000000, 0) == -14);
        check(common::syscall(16, child as u64, 0x40005ff0, 0) == -14);
        check(common::syscall(16, child as u64, (&raw mut result) as u64, 1) == -22);
        check(common::syscall(17, child as u64, 1, 0) == -22 && common::syscall(17, child as u64, 0, 1) == -22);
        let target = 0x40004ff0u64;
        check(common::syscall(16, child as u64, target, 0) == child);
        check((target as *const u64).read_volatile() == child as u64 &&
            ((target + 8) as *const u64).read_volatile() == 0 &&
            ((target + 16) as *const u64).read_volatile() == 0);
        check(common::wait(child as u64, &mut result) == -10 && common::kill(child as u64) == -10);
        common::print(b"control: pointers flags foreign and consumed child rejected\n"); common::exit(0);
    }
    let child = if arg == b"exit" { common::spawn(b"control", b"exit37", 0) }
        else if arg == b"fault" { common::spawn(b"probe", b"null", 0) }
        else if arg == b"kill" { common::spawn(b"sleep", b"5000", 0) }
        else { check(arg.is_empty() || arg == b"basic"); common::spawn(b"echo", b"child literal | command", 0) };
    check(child > 0); announce(child);
    if arg == b"kill" {
        check(common::spawn(b"hello", b"", 0) == -11);
        check(common::kill(child as u64) == 0);
    }
    let status = wait(child);
    if arg == b"exit" { check(status.kind == 0 && status.code == 37); }
    else if arg == b"fault" { check(status.kind == 1 && status.vector == 14); }
    else if arg == b"kill" { check(status.kind == 2); }
    else { check(status.kind == 0 && status.code == 0); }
    let status = wait(common::spawn(b"hello", b"", 0)); check(status.kind == 0 && status.code == 0);
    common::print(b"control: outcome consumed and child slot reusable\n"); common::exit(0)
}
