//! Ring 3 int 0x80 boundary. No user address is ever dereferenced directly.
//! Entry, copies, file target validation and file effects run with IF=0;
//! stdout only enters the process's bounded queue, never the shell console.
//! See user_abi for register convention, lengths and signed errno values.

use crate::fs::{self, FsError, MAX_FILE_SIZE, MAX_NAME};
use crate::handles::{self, Capabilities, Identity, TokenIssuer};
use crate::interrupts::{self, Frame};
use crate::mac::{Domain, Op};
use crate::process::{self, ExitReason};
use crate::security;
use crate::storage::{self, StorageError};
use crate::{tasks, user_abi as abi};
use core::ptr::addr_of_mut;

static mut CAPS: [Capabilities; tasks::MAX_TASKS] = [const { Capabilities::new() }; tasks::MAX_TASKS];
static mut TOKENS: TokenIssuer = TokenIssuer::new();

fn caps(pid: u32) -> &'static mut Capabilities {
    // IF=0, current_slot is a live user slot; no syscall callback switches
    // while this selected-slot loan is in use.
    unsafe {
        let set = &mut (*addr_of_mut!(CAPS))[tasks::current_slot()];
        if set.owner() != pid { set.reset(pid); }
        set
    }
}

/// Called before the process's address space or stack can be reclaimed.
/// Revocation clears names, rights, identity and cursor as well as tokens.
pub fn revoke(pid: u32) {
    interrupts::without(|| unsafe {
        for set in &mut *addr_of_mut!(CAPS) {
            if set.owner() == pid { set.reset(0); }
        }
    });
}

fn storage_errno(error: StorageError) -> i64 {
    match error {
        StorageError::Denied(_) => abi::EACCES,
        StorageError::Stale => abi::ESTALE,
        StorageError::IdentityExhausted => abi::EOVERFLOW,
        StorageError::NoDisk => abi::EIO,
        StorageError::Fs(FsError::NotFound) => abi::ENOENT,
        StorageError::Fs(FsError::BadName) => abi::EINVAL,
        StorageError::Fs(FsError::Full | FsError::TooLarge) => abi::ENOSPC,
        StorageError::Fs(_) => abi::EIO,
    }
}
fn handle_errno(error: handles::Error) -> i64 {
    match error {
        handles::Error::Full => abi::ENOSPC,
        handles::Error::Exhausted => abi::EOVERFLOW,
        handles::Error::InvalidRights | handles::Error::InvalidName => abi::EINVAL,
        handles::Error::BadHandle | handles::Error::WrongOwner => abi::EBADF,
    }
}

fn capability(pid: u32, token: u64, op: Op) -> Result<handles::Capability, i64> {
    caps(pid).get(pid, token).map_err(|error| {
        security::deny_capability(op, None, Some(token));
        handle_errno(error)
    })
}
fn capability_denied(cap: handles::Capability, op: Op) -> i64 {
    let label = match cap.identity.label { 1 => Domain::Admin, 2 => Domain::User, _ => Domain::Kernel };
    security::deny_capability(op, Some(label), Some(cap.identity.slot as u64));
    abi::EACCES
}
fn capability_storage(error: StorageError, cap: handles::Capability, op: Op) -> i64 {
    if matches!(error, StorageError::Stale) {
        let label = match cap.identity.label { 1 => Domain::Admin, 2 => Domain::User, _ => Domain::Kernel };
        security::deny_capability(op, Some(label), Some(cap.identity.slot as u64));
    }
    storage_errno(error)
}

fn copy_input(pointer: u64, output: &mut [u8]) -> Result<(), i64> {
    process::current_space().ok_or(abi::EFAULT)?.copy_from_user(pointer, output).map_err(|_| abi::EFAULT)
}
fn writable(pointer: u64, length: usize) -> Result<(), i64> {
    process::current_space().ok_or(abi::EFAULT)?.validate_write(pointer, length).map_err(|_| abi::EFAULT)
}
fn copy_output(pointer: u64, bytes: &[u8]) -> Result<(), i64> {
    process::current_space().ok_or(abi::EFAULT)?.copy_to_user(pointer, bytes).map_err(|_| abi::EFAULT)
}
fn path<'a>(pointer: u64, raw_length: u64, buffer: &'a mut [u8; MAX_NAME]) -> Result<&'a str, i64> {
    let len = abi::name_length(raw_length)?;
    copy_input(pointer, &mut buffer[..len])?;
    if !fs::valid_name(&buffer[..len]) { return Err(abi::EINVAL); }
    core::str::from_utf8(&buffer[..len]).map_err(|_| abi::EINVAL)
}

fn open(pid: u32, pointer: u64, raw_len: u64, raw_rights: u64) -> Result<i64, i64> {
    // Validate the full-width flags and copy the whole pathname before any
    // filesystem lookup, quota change, create or token publication.
    let rights = abi::rights(raw_rights)?;
    let mut name_buffer = [0u8; MAX_NAME];
    let name = path(pointer, raw_len, &mut name_buffer)?;
    let token = caps(pid).reserve(pid, unsafe { &mut *addr_of_mut!(TOKENS) }).map_err(handle_errno)?;
    let identity = storage::open_capability(name, rights).map_err(storage_errno)?;
    caps(pid).install(pid, token, rights, name, identity).map_err(handle_errno)?;
    Ok(token as i64)
}

#[inline(never)]
fn read(pid: u32, token: u64, pointer: u64, raw_len: u64) -> Result<i64, i64> {
    let len = abi::io_length(raw_len)?;
    // An invalid destination, including RX code or a span crossing an
    // unmapped page, causes no disk I/O and no cursor change.
    writable(pointer, len)?;
    let cap = capability(pid, token, Op::Read)?;
    if !cap.permits(handles::READ) { return Err(capability_denied(cap, Op::Read)); }
    let mut buffer = [0u8; MAX_FILE_SIZE];
    let size = storage::read_capability(cap.name(), cap.identity, &mut buffer).map_err(|error| capability_storage(error, cap, Op::Read))?;
    let start = cap.cursor.min(size);
    let count = len.min(size - start);
    copy_output(pointer, &buffer[start..start + count])?;
    caps(pid).update(pid, token, cap.identity, start + count).map_err(handle_errno)?;
    Ok(count as i64)
}

fn write(pid: u32, token: u64, pointer: u64, raw_len: u64) -> Result<i64, i64> {
    let len = abi::io_length(raw_len)?;
    let mut buffer = [0u8; abi::MAX_IO];
    copy_input(pointer, &mut buffer[..len])?;
    let cap = capability(pid, token, Op::Write)?;
    if !cap.permits(handles::WRITE) { return Err(capability_denied(cap, Op::Write)); }
    let after: Identity = storage::append_capability(cap.name(), cap.identity, &buffer[..len]).map_err(|error| capability_storage(error, cap, Op::Write))?;
    // A handle keeps its read cursor when appending; its pinned mutation
    // identity follows only its own successful write.
    caps(pid).update(pid, token, after, cap.cursor).map_err(handle_errno)?;
    Ok(len as i64)
}
fn unlink(pointer: u64, raw_len: u64) -> Result<i64, i64> {
    let mut name_buffer = [0u8; MAX_NAME];
    let name = path(pointer, raw_len, &mut name_buffer)?;
    storage::remove(name).map_err(storage_errno)?;
    Ok(0)
}

fn ordinary(pid: u32, number: u64, a: u64, b: u64, c: u64) -> Result<i64, i64> {
    match number {
        1 => {
            let len = abi::io_length(b)?;
            let mut buffer = [0u8; abi::MAX_IO];
            copy_input(a, &mut buffer[..len])?;
            process::append_output(&buffer[..len]).map_err(|_| abi::EAGAIN)?;
            Ok(len as i64)
        }
        2 => Ok(pid as i64),
        5 => open(pid, a, b, c),
        6 => read(pid, a, b, c),
        7 => write(pid, a, b, c),
        8 => { caps(pid).close(pid, a).map_err(handle_errno)?; Ok(0) }
        9 => unlink(a, b),
        10 => Ok(interrupts::ticks().min(i64::MAX as u64) as i64),
        // These diagnostic numbers intentionally have no privileged
        // implementation. The ordinary policy gate audits User's refusal
        // before any format, audit-log disclosure or transmitted packet.
        11 | 12 | 13 => {
            let op = match number { 11 => Op::Format, 12 => Op::ReadAudit, _ => Op::NetPing };
            security::check_domain(Domain::User, op, None, None).map_err(|_| abi::EACCES)?;
            Err(abi::ENOSYS)
        }
        _ => Err(abi::ENOSYS),
    }
}

/// Interrupt dispatcher preserves all registers except RAX. Exit/yield/
/// sleep select a saved frame; other calls resume the original frame.
pub fn dispatch(frame: &mut Frame) -> *mut Frame {
    interrupts::without(|| {
        if frame.cs & 3 != 3 || !tasks::is_user() || tasks::current_domain() != Domain::User {
            frame.rax = abi::EACCES as u64;
            return frame as *mut Frame;
        }
        let number = frame.rax;
        let (a, b, c) = (frame.rdi, frame.rsi, frame.rdx);
        match number {
            0 => {
                frame.rax = 0;
                tasks::terminate_current(frame as *mut Frame, ExitReason::Exit(a as i64))
            }
            3 => { frame.rax = 0; tasks::yield_current(frame as *mut Frame) }
            4 => match abi::sleep_millis(a) {
                Ok(ms) => { frame.rax = 0; tasks::sleep_current(frame as *mut Frame, ms) }
                Err(error) => { frame.rax = error as u64; frame as *mut Frame }
            },
            _ => {
                frame.rax = ordinary(tasks::current().0, number, a, b, c).unwrap_or_else(|error| error) as u64;
                frame as *mut Frame
            }
        }
    })
}
