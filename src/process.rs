//! Ring 3 process ownership, bounded output and retained exit outcomes.
//!
//! The scheduler owns kernel stacks; this module owns address spaces. A
//! terminal process retains an independent, bounded result while all live
//! memory and syscall capabilities are erased once the CPU has left it.

use crate::executable::Image;
use crate::interrupts::{self, Frame};
use crate::mac::{self, Domain, Op};
use crate::security::{self, Denied};
use crate::tasks::{self, SpawnError};
use crate::usermem::AddressSpace;
use core::ptr::{addr_of, addr_of_mut};

pub const OUTPUT_BYTES: usize = 1024;
pub const KEPT_RESULTS: usize = 8;
pub const NAME_BYTES: usize = crate::fs::MAX_NAME;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    Exit(i64),
    Fault { vector: u64, error: u64, rip: u64, address: u64 },
    Killed,
}

impl ExitReason {
    pub fn state(self) -> &'static str {
        match self { Self::Exit(_) => "exited", Self::Fault { .. } => "faulted", Self::Killed => "killed" }
    }
}

#[derive(Clone, Copy)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub domain: Domain,
    pub state: &'static str,
    pub cpu_ticks: u64,
    pub frames: u32,
    pub heap_pages: usize,
    pub output_len: usize,
    /// A whole stdout write was refused because the bounded queue was full.
    pub truncated: bool,
    pub reason: Option<ExitReason>,
    name: [u8; NAME_BYTES],
    name_len: usize,
}

impl ProcessInfo {
    const EMPTY: Self = Self { pid: 0, parent_pid: 0, domain: Domain::User, state: "free",
        cpu_ticks: 0, frames: 0, heap_pages: 0, output_len: 0, truncated: false, reason: None,
        name: [0; NAME_BYTES], name_len: 0 };

    pub fn name(&self) -> &str {
        // Install accepts an already validated ASCII builtin/file name.
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("user")
    }
    pub fn finished(&self) -> bool { self.reason.is_some() }
}

struct Live {
    info: ProcessInfo,
    space: Option<AddressSpace>,
    output: [u8; OUTPUT_BYTES],
    terminal: bool,
    /// One reserved, unconsumed child relation per live ring 3 parent. This
    /// completion cannot be evicted by the separate shell-visible result log.
    child: Child,
    waiting_output: Option<u64>,
}

impl Live {
    const EMPTY: Self = Self { info: ProcessInfo::EMPTY, space: None, output: [0; OUTPUT_BYTES], terminal: false,
        child: Child::None, waiting_output: None };
}

#[derive(Clone, Copy)]
enum Child { None, Running(u32), Completed(u32, ExitReason) }

#[derive(Clone, Copy)]
struct Outcome { info: ProcessInfo, output: [u8; OUTPUT_BYTES] }
impl Outcome {
    const EMPTY: Self = Self { info: ProcessInfo::EMPTY, output: [0; OUTPUT_BYTES] };
}

static mut LIVE: [Live; tasks::MAX_TASKS] = [const { Live::EMPTY }; tasks::MAX_TASKS];
static mut RESULTS: [Outcome; KEPT_RESULTS] = [Outcome::EMPTY; KEPT_RESULTS];
static mut NEXT_RESULT: usize = 0;

/// IF=0 throughout install and publication to the scheduler.
pub(crate) unsafe fn install(slot: usize, pid: u32, parent_pid: u32, name: &str, space: AddressSpace) {
    assert!(slot < tasks::MAX_TASKS && name.len() <= NAME_BYTES);
    let live = &mut (*addr_of_mut!(LIVE))[slot];
    assert!(live.space.is_none(), "process slot still owns an address space");
    let mut info = ProcessInfo { pid, parent_pid, state: "ready", frames: tasks::STACK_FRAMES as u32
        + crate::usermem::SPACE_FRAMES as u32, ..ProcessInfo::EMPTY };
    info.name[..name.len()].copy_from_slice(name.as_bytes());
    info.name_len = name.len();
    live.info = info;
    live.output.fill(0);
    live.terminal = false;
    live.space = Some(space);
    live.child = Child::None;
    live.waiting_output = None;
    // Register only after all image/stack allocations succeeded. The check in
    // spawn_user and this publication share IF=0, so no child is lost between.
    for parent in (*addr_of_mut!(LIVE)).iter_mut() {
        if parent.info.pid == parent_pid && parent.space.is_some() && !parent.terminal {
            assert!(matches!(parent.child, Child::None), "parent child reservation changed");
            parent.child = Child::Running(pid);
            break;
        }
    }
}

/// IF=0. Ring 0 creators do not use child-wait capabilities; User creators
/// must consume a previous completion before allocating the next child.
pub(crate) fn check_child_capacity() -> Result<(), SpawnError> {
    if !tasks::is_user() { return Ok(()); }
    unsafe {
        let live = &(*addr_of!(LIVE))[tasks::current_slot()];
        if live.terminal || live.space.is_none() || !matches!(live.child, Child::None) {
            return Err(SpawnError::ChildPending);
        }
    }
    Ok(())
}

/// Only syscall dispatch may borrow this, with IF=0. A current process's
/// address space cannot be destroyed or replaced before dispatch returns.
pub fn current_space() -> Option<&'static AddressSpace> {
    if !tasks::is_user() { return None; }
    unsafe { (*addr_of!(LIVE))[tasks::current_slot()].space.as_ref() }
}

pub enum HeapResizeError {
    NotUser,
    Memory(crate::usermem::HeapError),
    Denied(Denied),
}

/// Growth charges the immutable owner before allocation, with a full rollback
/// if physical allocation fails. Shrink invalidates mappings before returning
/// quota; teardown uses the scheduler's matching dynamic frame count.
pub fn resize_current(pages: usize) -> Result<usize, HeapResizeError> {
    interrupts::without(|| unsafe {
        if !tasks::is_user() { return Err(HeapResizeError::NotUser); }
        if pages > crate::usermem::HEAP_MAX_PAGES {
            return Err(HeapResizeError::Memory(crate::usermem::HeapError::TooLarge));
        }
        security::check(Op::Alloc, None, None).map_err(HeapResizeError::Denied)?;
        let live = &mut (*addr_of_mut!(LIVE))[tasks::current_slot()];
        let space = live.space.as_mut().ok_or(HeapResizeError::NotUser)?;
        let old = space.heap_pages();
        let owner = tasks::current_charge_owner();
        if pages > old {
            security::charge_domain(owner, Op::Alloc, 0, (pages - old) as u32)
                .map_err(HeapResizeError::Denied)?;
        }
        if let Err(error) = space.resize_heap(pages) {
            if pages > old { security::release(owner, 0, (pages - old) as u32); }
            return Err(HeapResizeError::Memory(error));
        }
        if pages < old { security::release(owner, 0, (old - pages) as u32); }
        let frames = tasks::STACK_FRAMES as u32 + space.owned_frames() as u32;
        live.info.frames = frames;
        live.info.heap_pages = pages;
        tasks::set_current_frames(frames);
        Ok(pages)
    })
}

pub enum ChildError {
    NotChild,
    AlreadyExited,
    InvalidPointer,
    Failed(&'static str),
    Denied(Denied),
}

/// Outcome ABI is five little-endian words; fault addresses and kernel
/// metadata are deliberately absent. Ordinary signed exit codes keep all bits.
fn child_outcome(pid: u32, reason: ExitReason) -> [u8; 40] {
    let (kind, code, vector, error) = match reason {
        ExitReason::Exit(status) => (0, status as u64, 0, 0),
        ExitReason::Fault { vector, error, .. } => (1, 0, vector, error),
        ExitReason::Killed => (2, 0, 0, 0),
    };
    let words = [pid as u64, kind, code, vector, error];
    let mut bytes = [0; 40];
    for (index, word) in words.iter().enumerate() {
        bytes[index * 8..index * 8 + 8].copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

/// Validate the complete writable result span before observing, consuming or
/// blocking on a child. Ownership and block publication are atomic under IF=0.
pub fn wait_child(frame: *mut Frame, pid: u32, output_pointer: u64) -> Result<*mut Frame, ChildError> {
    interrupts::without(|| unsafe {
        if !tasks::is_user() { return Err(ChildError::NotChild); }
        let slot = tasks::current_slot();
        let live = &mut (*addr_of_mut!(LIVE))[slot];
        let space = live.space.as_ref().ok_or(ChildError::NotChild)?;
        space.validate_write(output_pointer, 40).map_err(|_| ChildError::InvalidPointer)?;
        match live.child {
            Child::Completed(child, reason) if child == pid && pid != 0 => {
                space.copy_to_user(output_pointer, &child_outcome(child, reason))
                    .map_err(|_| ChildError::InvalidPointer)?;
                live.child = Child::None;
                (*frame).rax = child as u64;
                Ok(frame)
            }
            Child::Running(child) if child == pid && pid != 0 => {
                live.waiting_output = Some(output_pointer);
                // No LIVE loan survives the scheduler callback below.
                Ok(tasks::wait_child_current(frame, child))
            }
            _ => {
                security::deny_capability(Op::Read, Some(Domain::User), Some(pid as u64));
                Err(ChildError::NotChild)
            }
        }
    })
}

/// This is the only user syscall route to killing another process. A shared
/// User domain alone never conveys authority over an unrelated User PID.
pub fn kill_child(pid: u32) -> Result<(), ChildError> {
    interrupts::without(|| unsafe {
        if !tasks::is_user() { return Err(ChildError::NotChild); }
        let child = (*addr_of!(LIVE))[tasks::current_slot()].child;
        match child {
            Child::Running(child) if child == pid && pid != 0 => {},
            Child::Completed(child, _) if child == pid && pid != 0 => return Err(ChildError::AlreadyExited),
            _ => {
                security::deny_capability(Op::Kill, Some(Domain::User), Some(pid as u64));
                return Err(ChildError::NotChild);
            }
        }
        tasks::kill(pid).map(|_| ()).map_err(|error| match error {
            tasks::KillError::Denied(denied) => ChildError::Denied(denied),
            tasks::KillError::Failed(message) => ChildError::Failed(message),
        })
    })
}

/// The syscall boundary validates user input first. Writes are all or
/// nothing; a full queue never reports that bytes were successfully written.
pub fn append_output(bytes: &[u8]) -> Result<(), &'static str> {
    if !tasks::is_user() { return Err("current task is not a user process"); }
    unsafe {
        let live = &mut (*addr_of_mut!(LIVE))[tasks::current_slot()];
        let available = OUTPUT_BYTES - live.info.output_len;
        if bytes.len() > available {
            live.info.truncated = true;
            return Err("process output buffer is full");
        }
        let start = live.info.output_len;
        live.output[start..start + bytes.len()].copy_from_slice(bytes);
        live.info.output_len += bytes.len();
    }
    Ok(())
}

/// Capture before invalidating capabilities and before leaving the current
/// stack. Result replacement physically wipes the older output.
pub(crate) unsafe fn finish(slot: usize, cpu_ticks: u64, reason: ExitReason) {
    let (pid, parent_pid) = {
        let live = &mut (*addr_of_mut!(LIVE))[slot];
        assert!(live.space.is_some() && !live.terminal, "user process finished twice");
        live.info.cpu_ticks = cpu_ticks;
        live.info.state = reason.state();
        live.info.reason = Some(reason);
        live.info.frames = 0;
        live.info.heap_pages = 0;
        let result = &mut (*addr_of_mut!(RESULTS))[NEXT_RESULT];
        result.output.fill(0);
        result.info = live.info;
        result.output[..live.info.output_len].copy_from_slice(&live.output[..live.info.output_len]);
        NEXT_RESULT = (NEXT_RESULT + 1) % KEPT_RESULTS;
        live.output.fill(0);
        live.terminal = true;
        // Children survive a parent exit as orphans. Drop its completion
        // authority; immutable PIDs prevent a reused slot becoming a parent.
        live.child = Child::None;
        live.waiting_output = None;
        (live.info.pid, live.info.parent_pid)
    };
    let mut wake = None;
    for parent in (*addr_of_mut!(LIVE)).iter_mut() {
        if parent.info.pid != parent_pid || parent.terminal || parent.space.is_none() { continue; }
        if !matches!(parent.child, Child::Running(child) if child == pid) { continue; }
        parent.child = Child::Completed(pid, reason);
        if let Some(pointer) = parent.waiting_output.take() {
            let copied = parent.space.as_ref().unwrap().copy_to_user(pointer, &child_outcome(pid, reason));
            let result = if copied.is_ok() {
                parent.child = Child::None;
                pid as i64
            } else {
                // Preserve Completed for a retry; a defensive wake never
                // writes through an invalid pointer or loses an exit status.
                crate::user_abi::EFAULT
            };
            wake = Some((parent_pid, pid, result));
        }
        break;
    }
    // All LIVE loans end before updating a task's saved frame.
    if let Some((parent, child, result)) = wake { tasks::wake_child_wait(parent, child, result); }
}

/// IF=0 and the CPU has left this process's CR3 and kernel stack.
pub(crate) unsafe fn reap(slot: usize) {
    let live = &mut (*addr_of_mut!(LIVE))[slot];
    assert!(live.terminal, "a running address space cannot be reaped");
    if let Some(space) = live.space.take() { space.destroy(); }
    live.output.fill(0);
    live.info = ProcessInfo::EMPTY;
    live.terminal = false;
    live.child = Child::None;
    live.waiting_output = None;
}

pub fn spawn_builtin(name: &str, args: &str) -> Result<u32, SpawnError> {
    if args.len() > crate::executable::ARGS_MAX {
        return Err(SpawnError::Failed("process arguments exceed 128 bytes"));
    }
    let program = crate::user_images::PROGRAMS.iter().find(|program| program.name == name)
        .ok_or(SpawnError::Failed("unknown builtin user program"))?;
    let image = Image::parse(program.bytes).map_err(|error| SpawnError::Failed(error.message()))?;
    tasks::spawn_user(&image, args.as_bytes(), program.name, program.name)
}

pub enum FileSpawnError {
    Storage(crate::storage::StorageError),
    Spawn(SpawnError),
}

pub fn spawn_file(name: &str, args: &str) -> Result<u32, FileSpawnError> {
    if args.len() > crate::executable::ARGS_MAX {
        return Err(FileSpawnError::Spawn(SpawnError::Failed("process arguments exceed 128 bytes")));
    }
    // Checking Spawn first prevents a refused caller from reading even the
    // header. read_for_domain applies both caller and destination User gates.
    security::check(Op::Spawn, None, None)
        .map_err(|denied| FileSpawnError::Spawn(SpawnError::Denied(denied)))?;
    interrupts::without(|| {
        let mut buffer = [0u8; crate::fs::MAX_FILE_SIZE];
        let result = (|| {
            let count = crate::storage::read_for_domain(Domain::User, name, &mut buffer)
                .map_err(FileSpawnError::Storage)?;
            let image = Image::parse(&buffer[..count])
                .map_err(|error| FileSpawnError::Spawn(SpawnError::Failed(error.message())))?;
            tasks::spawn_user(&image, args.as_bytes(), "user-file", name)
                .map_err(FileSpawnError::Spawn)
        })();
        // AddressSpace::create copied code/data; the temporary executable is
        // not retained and is erased even when the quota or allocator failed.
        buffer.fill(0);
        result
    })
}

pub enum ReadError { Denied(Denied), Missing }

fn permit(pid: u32) -> Result<(), ReadError> {
    security::check(Op::Read, Some(Domain::User), Some(pid as u64)).map_err(ReadError::Denied)
}

/// One bounded metadata snapshot, with completed process resources reclaimed
/// before a shell can inspect the corresponding account and frame totals.
pub fn info(pid: u32) -> Result<ProcessInfo, ReadError> {
    permit(pid)?;
    tasks::reap_exited();
    interrupts::without(|| unsafe {
        for (slot, live) in (*addr_of!(LIVE)).iter().enumerate() {
            if live.info.pid == pid && live.space.is_some() && !live.terminal {
                let task = tasks::slot_info(slot).ok_or(ReadError::Missing)?;
                let mut info = live.info;
                info.state = task.state;
                info.cpu_ticks = task.cpu_ticks;
                info.frames = task.frames;
                return Ok(info);
            }
        }
        (*addr_of!(RESULTS)).iter().find(|result| result.info.pid == pid && pid != 0)
            .map(|result| result.info).ok_or(ReadError::Missing)
    })
}

/// Live processes followed by retained outcomes, copied under CLI before
/// calling printing code. The ordinary domain policy also controls visibility.
pub fn list(mut each: impl FnMut(&ProcessInfo)) {
    tasks::reap_exited();
    let mut snapshot: [Option<ProcessInfo>; tasks::MAX_TASKS + KEPT_RESULTS] = [None; tasks::MAX_TASKS + KEPT_RESULTS];
    interrupts::without(|| unsafe {
        if !mac::allowed(tasks::current_domain(), Op::Read, Some(Domain::User)) { return; }
        for (slot, live) in (*addr_of!(LIVE)).iter().enumerate() {
            if live.space.is_some() && !live.terminal {
                if let Some(task) = tasks::slot_info(slot) {
                    let mut info = live.info;
                    info.state = task.state;
                    info.cpu_ticks = task.cpu_ticks;
                    info.frames = task.frames;
                    snapshot[slot] = Some(info);
                }
            }
        }
        // Oldest retained first. Empty slots have PID 0 and stay invisible.
        for ordinal in 0..KEPT_RESULTS {
            let result = &(*addr_of!(RESULTS))[(NEXT_RESULT + ordinal) % KEPT_RESULTS];
            if result.info.pid != 0 { snapshot[tasks::MAX_TASKS + ordinal] = Some(result.info); }
        }
    });
    for item in snapshot.iter().flatten() { each(item); }
}

/// Copy output before invoking the renderer, so it never holds a reference
/// to mutable process storage while interrupts or another process run.
pub fn output(pid: u32, render: impl FnOnce(&ProcessInfo, &[u8])) -> Result<(), ReadError> {
    permit(pid)?;
    tasks::reap_exited();
    let mut copy = Outcome::EMPTY;
    interrupts::without(|| unsafe {
        for (slot, live) in (*addr_of!(LIVE)).iter().enumerate() {
            if live.info.pid == pid && live.space.is_some() && !live.terminal {
                let task = tasks::slot_info(slot).ok_or(ReadError::Missing)?;
                copy.info = live.info;
                copy.info.state = task.state;
                copy.info.cpu_ticks = task.cpu_ticks;
                copy.info.frames = task.frames;
                copy.output[..live.info.output_len].copy_from_slice(&live.output[..live.info.output_len]);
                return Ok(());
            }
        }
        let result = (*addr_of!(RESULTS)).iter().find(|result| result.info.pid == pid && pid != 0)
            .ok_or(ReadError::Missing)?;
        copy = *result;
        Ok(())
    })?;
    render(&copy.info, &copy.output[..copy.info.output_len]);
    copy.output.fill(0);
    Ok(())
}
