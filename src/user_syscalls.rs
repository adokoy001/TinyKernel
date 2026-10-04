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
        StorageError::Fs(FsError::ReadOnly) => abi::EROFS,
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
    let mut buffer = [0u8; abi::MAX_IO];
    let count = storage::read_capability_at(cap.name(), cap.identity, cap.cursor, &mut buffer[..len])
        .map_err(|error| capability_storage(error, cap, Op::Read))?;
    copy_output(pointer, &buffer[..count])?;
    caps(pid).update(pid, token, cap.identity, cap.cursor + count).map_err(handle_errno)?;
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
fn seek(pid: u32, token: u64, offset: u64) -> Result<i64, i64> {
    if offset > MAX_FILE_SIZE as u64 { return Err(abi::EINVAL); }
    let cap = capability(pid, token, Op::Read)?;
    let op = if cap.permits(handles::READ) { Op::Read } else { Op::Write };
    storage::seek_capability(cap.name(), cap.identity, op)
        .map_err(|error| capability_storage(error, cap, op))?;
    caps(pid).update(pid, token, cap.identity, offset as usize).map_err(handle_errno)?;
    Ok(offset as i64)
}
fn write_cursor(pid: u32, token: u64, pointer: u64, raw_len: u64) -> Result<i64, i64> {
    let len = abi::io_length(raw_len)?;
    let mut buffer = [0u8; abi::MAX_IO];
    copy_input(pointer, &mut buffer[..len])?;
    let cap = capability(pid, token, Op::Write)?;
    if !cap.permits(handles::WRITE) { return Err(capability_denied(cap, Op::Write)); }
    let after = storage::write_capability_at(cap.name(), cap.identity, cap.cursor, &buffer[..len])
        .map_err(|error| capability_storage(error, cap, Op::Write))?;
    caps(pid).update(pid, token, after, cap.cursor + len).map_err(handle_errno)?;
    Ok(len as i64)
}
fn truncate(pid: u32, token: u64, size: u64) -> Result<i64, i64> {
    if size > MAX_FILE_SIZE as u64 { return Err(abi::EINVAL); }
    let cap = capability(pid, token, Op::Write)?;
    if !cap.permits(handles::WRITE) { return Err(capability_denied(cap, Op::Write)); }
    let after = storage::truncate_capability(cap.name(), cap.identity, size as usize)
        .map_err(|error| capability_storage(error, cap, Op::Write))?;
    caps(pid).update(pid, token, after, cap.cursor).map_err(handle_errno)?;
    Ok(size as i64)
}

fn spawn_errno(error: tasks::SpawnError) -> i64 {
    match error {
        tasks::SpawnError::Denied(_) => abi::EACCES,
        tasks::SpawnError::ChildPending => abi::EAGAIN,
        tasks::SpawnError::NoMemory(_) => abi::ENOMEM,
        tasks::SpawnError::Failed(_) => abi::EINVAL,
    }
}
fn child_errno(error: process::ChildError) -> i64 {
    match error {
        process::ChildError::NotChild | process::ChildError::AlreadyExited => abi::ECHILD,
        process::ChildError::InvalidPointer => abi::EFAULT,
        process::ChildError::Denied(_) => abi::EACCES,
        process::ChildError::Failed(_) => abi::ECHILD,
    }
}
/// Copy the complete descriptor and both complete payloads before publishing
/// a child or reading its executable. No borrowed user memory crosses spawn.
fn spawn_user(pointer: u64) -> Result<i64, i64> {
    let mut bytes = [0u8; abi::SPAWN_BYTES];
    copy_input(pointer, &mut bytes)?;
    let request = abi::SpawnRequest::decode(&bytes)?;
    let mut name = [0u8; MAX_NAME];
    copy_input(request.name, &mut name[..request.name_len])?;
    if !fs::valid_name(&name[..request.name_len]) { return Err(abi::EINVAL); }
    let name = core::str::from_utf8(&name[..request.name_len]).map_err(|_| abi::EINVAL)?;
    let mut argument = [0u8; crate::usermem::ARGS_MAX];
    copy_input(request.argument, &mut argument[..request.argument_len])?;
    let argument = core::str::from_utf8(&argument[..request.argument_len]).map_err(|_| abi::EINVAL)?;
    if request.file {
        process::spawn_file(name, argument).map(|pid| pid as i64).map_err(|error| match error {
            process::FileSpawnError::Storage(error) => storage_errno(error),
            process::FileSpawnError::Spawn(error) => spawn_errno(error),
        })
    } else {
        if crate::user_images::builtin(name).is_none() { return Err(abi::ENOENT); }
        process::spawn_builtin(name, argument).map(|pid| pid as i64).map_err(spawn_errno)
    }
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
        14 => {
            if b != 0 || c != 0 || a > crate::usermem::HEAP_MAX_PAGES as u64 { return Err(abi::EINVAL); }
            process::resize_current(a as usize).map_err(|error| match error {
                process::HeapResizeError::NotUser => abi::EACCES,
                process::HeapResizeError::Denied(_) => abi::EACCES,
                process::HeapResizeError::Memory(crate::usermem::HeapError::TooLarge) => abi::EINVAL,
                process::HeapResizeError::Memory(crate::usermem::HeapError::OutOfMemory) => abi::ENOMEM,
            })?;
            Ok(crate::usermem::HEAP_BASE as i64)
        }
        15 => { if b != 0 || c != 0 { Err(abi::EINVAL) } else { spawn_user(a) } }
        17 => {
            if b != 0 || c != 0 { return Err(abi::EINVAL); }
            process::kill_child(abi::process_id(a)?).map_err(child_errno)?;
            Ok(0)
        }
        18 => { if c != 0 { Err(abi::EINVAL) } else { seek(pid, a, b) } }
        19 => write_cursor(pid, a, b, c),
        20 => { if c != 0 { Err(abi::EINVAL) } else { truncate(pid, a, b) } }
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
            16 => {
                let result = if c != 0 { Err(abi::EINVAL) } else {
                    abi::process_id(a).and_then(|pid|
                        process::wait_child(frame as *mut Frame, pid, b).map_err(child_errno))
                };
                match result {
                    Ok(next) => next,
                    Err(error) => { frame.rax = error as u64; frame as *mut Frame }
                }
            }
            _ => {
                frame.rax = ordinary(tasks::current().0, number, a, b, c).unwrap_or_else(|error| error) as u64;
                frame as *mut Frame
            }
        }
    })
}
