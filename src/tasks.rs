//! Kernel tasks: preemptive round-robin switching on the timer IRQ, stacks
//! taken from the physical frame allocator, and blocking sleep/input waits.
//!
//! Every switch happens inside an interrupt: the timer IRQ, an input IRQ, or
//! `int 48` from `interrupts::yield_now`. The interrupted task's registers
//! are already saved on its own stack, so a switch records that frame and
//! returns another task's. The task table is only touched with IF=0.

use crate::frames::FRAME_SIZE;
use crate::interrupts::{self, Frame};
use crate::mac::{self, Domain, Op};
use crate::security::{self, Denied};
use crate::sched::{self, State};
use crate::shell::TaskKind;
use crate::process::{self, ExitReason};
use core::mem::size_of;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

pub const MAX_TASKS: usize = 8;
pub const STACK_FRAMES: usize = 4;
const STACK_BYTES: u64 = STACK_FRAMES as u64 * FRAME_SIZE;
const SHELL: usize = 0;
const IDLE: usize = 1;
const BOOT_STACK_BASE: u64 = 0x80000;
/// Written at the lowest word of every stack and checked at every switch.
const CANARY: u64 = 0x2154_4b53_454e_4154; // "TANESTK!"
/// Input waiters are also woken this often, in case an IRQ edge is lost.
const INPUT_RECHECK_TICKS: u64 = 10;

#[derive(Clone, Copy)]
struct Task {
    pid: u32,
    name: &'static str,
    context: u64,
    stack: u64,
    owns_stack: bool,
    cpu_ticks: u64,
    /// MAC label: the subject domain, inherited from the creating task.
    domain: Domain,
    /// The immutable domain charged at creation, even if the running task
    /// subsequently lowers its access label.
    charged_domain: Domain,
    root: u64,
    user: bool,
    /// Suspension is orthogonal to sleep/child wait: resuming preserves both.
    paused: bool,
    waiting_child: u32,
    /// Immutable charge owner above, current number of owned frames here.
    frames: u32,
}

impl Task {
    const EMPTY: Task = Task { pid: 0, name: "", context: 0, stack: 0, owns_stack: false, cpu_ticks: 0, domain: Domain::Kernel, charged_domain: Domain::Kernel, root: 0, user: false, paused: false, waiting_child: 0, frames: 0 };
}

static mut STATES: [State; MAX_TASKS] = [State::Free; MAX_TASKS];
static mut TASKS: [Task; MAX_TASKS] = [Task::EMPTY; MAX_TASKS];
static mut CURRENT: usize = SHELL;
static mut NEXT_PID: u32 = 2;
/// Progress counters of the demonstration tasks, updated without CLI.
static COUNTERS: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

#[repr(C, align(16))]
struct IdleStack([u8; 8192]);
static mut IDLE_STACK: IdleStack = IdleStack([0; 8192]);

/// A snapshot of one task for `ps`.
#[derive(Clone, Copy)]
pub struct Info {
    pub pid: u32,
    pub name: &'static str,
    pub domain: Domain,
    pub state: &'static str,
    pub cpu_ticks: u64,
    pub counter: Option<u64>,
    pub stack: u64,
    pub owns_stack: bool,
    pub user: bool,
    pub frames: u32,
}

/// Callers hold IF=0: interrupt handlers, or code inside `interrupts::without`.
unsafe fn table() -> (&'static mut [State; MAX_TASKS], &'static mut [Task; MAX_TASKS]) {
    (&mut *addr_of_mut!(STATES), &mut *addr_of_mut!(TASKS))
}

/// Adopt the running boot code as the shell task and create the idle task.
/// Call with interrupts disabled, before they are first enabled.
pub unsafe fn init() {
    let (states, tasks) = table();
    write_volatile(BOOT_STACK_BASE as *mut u64, CANARY);
    let root = crate::paging::kernel_root();
    tasks[SHELL] = Task { pid: 1, name: "shell", stack: BOOT_STACK_BASE, domain: Domain::Admin, root, ..Task::EMPTY };
    states[SHELL] = State::Ready;
    let base = addr_of!(IDLE_STACK) as u64;
    let context = prepare(base, size_of::<IdleStack>() as u64, idle_main, 0);
    tasks[IDLE] = Task { pid: 0, name: "idle", context, stack: base, root, ..Task::EMPTY };
    states[IDLE] = State::Ready;
}

/// Write the canary and an initial interrupt frame; returns the context.
unsafe fn prepare(base: u64, size: u64, entry: extern "C" fn(u64) -> !, argument: u64) -> u64 {
    write_volatile(base as *mut u64, CANARY);
    let top = base + size;
    // The entry starts with RSP % 16 == 8, as after a call; it never returns.
    let rsp = top - 8;
    write_volatile(rsp as *mut u64, 0);
    let frame = (top - 16 - size_of::<Frame>() as u64) as *mut Frame;
    write_volatile(frame, Frame::task(entry, argument, rsp));
    frame as u64
}

pub fn current() -> (u32, &'static str) {
    unsafe {
        let task = &(*addr_of!(TASKS))[CURRENT];
        (task.pid, task.name)
    }
}

pub fn current_domain() -> Domain {
    interrupts::without(|| unsafe { (*addr_of!(TASKS))[CURRENT].domain })
}

/// Stable table slot while IF=0. Syscall capability tables use the same
/// slot together with its PID, so a reused slot cannot reuse old handles.
pub fn current_slot() -> usize { unsafe { CURRENT } }

pub fn is_user() -> bool {
    unsafe { (*addr_of!(TASKS))[CURRENT].user }
}

pub fn current_cpu_ticks() -> u64 {
    unsafe { (*addr_of!(TASKS))[CURRENT].cpu_ticks }
}

/// IF=0; the heap manager charges this creation-time owner even after a
/// subject transition, and publishes frame accounting after successful resize.
pub(crate) fn current_charge_owner() -> Domain {
    unsafe { (*addr_of!(TASKS))[CURRENT].charged_domain }
}

pub(crate) fn set_current_frames(frames: u32) {
    unsafe {
        let task = &mut (*addr_of_mut!(TASKS))[CURRENT];
        assert!(task.user, "only user address spaces have a heap");
        task.frames = frames;
    }
}

fn task_state(state: State, task: &Task, current: bool) -> &'static str {
    if current { "running" }
    else if state == State::Exited { "exited" }
    else if task.paused { "stopped" }
    else if task.waiting_child != 0 { "waiting" }
    else { state.name() }
}

/// Reclaim completed tasks after the CPU has returned to another stack.
/// The same rule is used in switches and before shell resource snapshots.
pub fn reap_exited() {
    interrupts::without(|| unsafe {
        let current = CURRENT;
        let (states, tasks) = table();
        for slot in 0..MAX_TASKS {
            if states[slot] == State::Exited && slot != current {
                release(states, tasks, slot);
            }
        }
    });
}

/// Copy one slot. Called under IF=0 by the process metadata snapshot.
pub(crate) fn slot_info(slot: usize) -> Option<Info> {
    unsafe {
        if slot >= MAX_TASKS || (*addr_of!(STATES))[slot] == State::Free { return None; }
        let task = &(*addr_of!(TASKS))[slot];
        Some(Info {
            pid: task.pid, name: task.name, domain: task.domain,
            state: task_state((*addr_of!(STATES))[slot], task, slot == CURRENT),
            cpu_ticks: task.cpu_ticks,
            counter: if task.owns_stack && !task.user { Some(COUNTERS[slot].load(Ordering::Relaxed)) } else { None },
            stack: task.stack, owns_stack: task.owns_stack, user: task.user,
            frames: task.frames,
        })
    }
}

/// Lower the current task's domain. Raising it is refused by the policy.
pub fn lower_domain(to: Domain) -> Result<Domain, Domain> {
    interrupts::without(|| unsafe {
        let task = &mut table().1[CURRENT];
        if !mac::may_transition(task.domain, to) {
            return Err(task.domain);
        }
        let from = task.domain;
        task.domain = to;
        Ok(from)
    })
}

/// Save `frame` as the current task's context and pick the task to resume.
/// Called only from interrupt handlers (IF=0).
pub fn switch(frame: *mut Frame, prefer: Option<usize>) -> *mut Frame {
    unsafe {
        let (states, tasks) = table();
        let current = CURRENT;
        tasks[current].context = frame as u64;
        if read_volatile(tasks[current].stack as *const u64) != CANARY {
            panic!("stack overflow in task pid {} ({})", tasks[current].pid, tasks[current].name);
        }
        // An exited task's stack is still in use until this interrupt
        // returns elsewhere, so only tasks the CPU has already left are freed.
        for slot in 0..MAX_TASKS {
            if states[slot] == State::Exited && slot != current {
                release(states, tasks, slot);
            }
        }
        // Keep the original sleep deadline/input state while suspended.
        // Child waits have no input state, so keyboard IRQs cannot wake them.
        let mut eligible = *states;
        for slot in 0..MAX_TASKS {
            if tasks[slot].paused || tasks[slot].waiting_child != 0 {
                eligible[slot] = State::Free;
            }
        }
        let next = match prefer {
            Some(slot) if eligible[slot] == State::Ready => slot,
            // A domain past its CPU share waits while others are ready.
            _ => sched::next(&eligible, current, IDLE, |slot| {
                tasks[slot].owns_stack && security::over_cpu_share(tasks[slot].domain)
            }),
        };
        CURRENT = next;
        // Every root retains supervisor mappings for all kernel stacks and
        // tables. The CPU leaves an exiting process's stack only after this
        // dispatcher returns; release() therefore skips the current slot.
        crate::paging::activate(tasks[next].root);
        let top = if next == SHELL { 0x90000 }
            else if next == IDLE { tasks[next].stack + size_of::<IdleStack>() as u64 }
            else { tasks[next].stack + STACK_BYTES };
        interrupts::set_rsp0(top);
        tasks[next].context as *mut Frame
    }
}

unsafe fn release(states: &mut [State; MAX_TASKS], tasks: &mut [Task; MAX_TASKS], slot: usize) {
    if tasks[slot].owns_stack {
        if tasks[slot].user { process::reap(slot); }
        // Zero a released kernel stack too: it can contain private syscall
        // arguments even when the user address space has already been wiped.
        core::ptr::write_bytes(tasks[slot].stack as *mut u8, 0, STACK_BYTES as usize);
        let freed = crate::with_frames(|frames| frames.free_contiguous(tasks[slot].stack, STACK_FRAMES));
        assert!(freed.is_ok(), "task stack frames were not allocated");
        security::release(tasks[slot].charged_domain, 1, tasks[slot].frames);
    }
    tasks[slot] = Task::EMPTY;
    states[slot] = State::Free;
}

/// Syscall paths already entered with IF=0. They must switch the saved
/// user frame directly, never STI or create a second software interrupt.
pub fn yield_current(frame: *mut Frame) -> *mut Frame { switch(frame, None) }

pub fn sleep_current(frame: *mut Frame, milliseconds: u64) -> *mut Frame {
    let quantum = 1000 / interrupts::TIMER_HZ;
    let delay = milliseconds / quantum + u64::from(milliseconds % quantum != 0);
    let deadline = interrupts::ticks().saturating_add(delay);
    unsafe { table().0[CURRENT] = State::Sleeping(deadline); }
    switch(frame, None)
}

/// The ownership check, outcome-pointer validation and registration already
/// happened in process::wait_child with IF=0. Save this frame before switching.
pub(crate) fn wait_child_current(frame: *mut Frame, pid: u32) -> *mut Frame {
    unsafe {
        let task = &mut (*addr_of_mut!(TASKS))[CURRENT];
        assert!(task.user && task.waiting_child == 0 && pid != 0);
        task.waiting_child = pid;
    }
    switch(frame, None)
}

/// Publish a completed wait result in the stopped parent's saved user frame.
/// Its pause flag is retained; a paused waiter remains unschedulable.
pub(crate) fn wake_child_wait(parent: u32, child: u32, result: i64) {
    unsafe {
        for task in (*addr_of_mut!(TASKS)).iter_mut() {
            if task.user && task.pid == parent && task.waiting_child == child {
                assert!(task.context != 0);
                (*(task.context as *mut Frame)).rax = result as u64;
                task.waiting_child = 0;
                return;
            }
        }
    }
}

pub fn terminate_current(frame: *mut Frame, reason: ExitReason) -> *mut Frame {
    unsafe {
        let slot = CURRENT;
        let task = (*addr_of!(TASKS))[slot];
        assert!(task.user, "only a user process may use user termination");
        process::finish(slot, task.cpu_ticks, reason);
        crate::user_syscalls::revoke(task.pid);
        table().0[slot] = State::Exited;
    }
    switch(frame, None)
}

/// Timer IRQ: account the tick, wake sleepers, and preempt.
pub fn on_timer(frame: *mut Frame, now: u64) -> *mut Frame {
    unsafe {
        let (states, tasks) = table();
        let task = &mut tasks[CURRENT];
        task.cpu_ticks += 1;
        // Only spawned tasks count against a domain's CPU share.
        security::tick(if task.owns_stack { Some(task.domain) } else { None });
        sched::wake_sleepers(states, now);
        if now % INPUT_RECHECK_TICKS == 0 {
            sched::wake_input(states);
        }
    }
    switch(frame, None)
}

/// Keyboard or COM1 IRQ: run a waiting reader (the shell) right away.
pub fn on_input(frame: *mut Frame) -> *mut Frame {
    match unsafe { sched::wake_input(table().0) } {
        Some(slot) => switch(frame, Some(slot)),
        None => frame,
    }
}

/// Block the current task until `ready()` holds or input IRQs arrive.
/// The check and the block happen under CLI, so no wakeup is lost.
pub fn wait_for_input(ready: impl Fn() -> bool) {
    interrupts::disable();
    if !ready() {
        unsafe { table().0[CURRENT] = State::Input; }
        interrupts::yield_now();
    }
    interrupts::enable();
}

/// Block the current task until the timer tick count reaches `deadline`.
pub fn sleep_until(deadline: u64) {
    loop {
        interrupts::disable();
        if interrupts::ticks() >= deadline {
            break;
        }
        unsafe { table().0[CURRENT] = State::Sleeping(deadline); }
        interrupts::yield_now();
    }
    interrupts::enable();
}

fn exit() -> ! {
    interrupts::disable();
    unsafe { table().0[CURRENT] = State::Exited; }
    interrupts::yield_now();
    crate::halt() // Unreachable: an exited task is never resumed.
}

pub enum SpawnError {
    Denied(Denied),
    ChildPending,
    NoMemory(&'static str),
    Failed(&'static str),
}

pub fn spawn(kind: TaskKind) -> Result<u32, SpawnError> {
    security::check(Op::Spawn, None, None).map_err(SpawnError::Denied)?;
    reap_exited();
    interrupts::without(|| unsafe {
        let slot = (*addr_of!(STATES)).iter().position(|state| *state == State::Free)
            .ok_or(SpawnError::Failed("task table is full (8 tasks)"))?;
        let pid = NEXT_PID;
        let next_pid = pid.checked_add(1).ok_or(SpawnError::Failed("PID space is exhausted"))?;
        let domain = (*addr_of!(TASKS))[CURRENT].domain;
        security::charge(Op::Spawn, 1, STACK_FRAMES as u32).map_err(SpawnError::Denied)?;
        let Some(stack) = crate::with_frames(|frames| frames.allocate_contiguous(STACK_FRAMES)) else {
            security::release(domain, 1, STACK_FRAMES as u32);
            return Err(SpawnError::NoMemory("no free physical frames for a stack"));
        };
        // Object reuse: a new stack never shows a previous owner's data.
        for offset in (0..STACK_BYTES).step_by(8) {
            write_volatile((stack + offset) as *mut u64, 0);
        }
        let entry: extern "C" fn(u64) -> ! = match kind {
            TaskKind::Spin => spin_main,
            TaskKind::Beat => beat_main,
            TaskKind::Once => once_main,
        };
        COUNTERS[slot].store(0, Ordering::Relaxed);
        let context = prepare(stack, STACK_BYTES, entry, slot as u64);
        let (states, tasks) = table();
        tasks[slot] = Task { pid, name: kind.name(), context, stack, owns_stack: true, cpu_ticks: 0, domain,
            charged_domain: domain, root: crate::paging::kernel_root(), user: false,
            paused: false, waiting_child: 0, frames: STACK_FRAMES as u32 };
        states[slot] = State::Ready;
        NEXT_PID = next_pid;
        Ok(pid)
    })
}

/// Spawn a ring 3 image into a distinct address space. The caller's Spawn
/// gate and the destination User quota are intentionally separate.
pub(crate) fn spawn_user(image: &crate::executable::Image<'_>, args: &[u8],
    task_name: &'static str, image_name: &str) -> Result<u32, SpawnError> {
    security::check(Op::Spawn, None, None).map_err(SpawnError::Denied)?;
    reap_exited();
    interrupts::without(|| unsafe {
        process::check_child_capacity()?;
        let slot = (*addr_of!(STATES)).iter().position(|state| *state == State::Free)
            .ok_or(SpawnError::Failed("task table is full (8 tasks)"))?;
        let pid = NEXT_PID;
        let next_pid = pid.checked_add(1).ok_or(SpawnError::Failed("PID space is exhausted"))?;
        let frames = STACK_FRAMES as u32 + crate::usermem::SPACE_FRAMES as u32;
        security::charge_domain(Domain::User, Op::Spawn, 1, frames).map_err(SpawnError::Denied)?;
        let Some(stack) = crate::with_frames(|allocator| allocator.allocate_contiguous(STACK_FRAMES)) else {
            security::release(Domain::User, 1, frames);
            return Err(SpawnError::NoMemory("no free physical frames for a kernel stack"));
        };
        core::ptr::write_bytes(stack as *mut u8, 0, STACK_BYTES as usize);
        let space = match crate::usermem::AddressSpace::create(image, args) {
            Ok(space) => space,
            Err(message) => {
                let freed = crate::with_frames(|allocator| allocator.free_contiguous(stack, STACK_FRAMES));
                assert!(freed.is_ok(), "new kernel stack could not be freed");
                security::release(Domain::User, 1, frames);
                return Err(if message == "not enough contiguous process frames" {
                    SpawnError::NoMemory(message)
                } else { SpawnError::Failed(message) });
            }
        };
        write_volatile(stack as *mut u64, CANARY);
        // CPU ring transition uses RSP0, not this initial Frame's address.
        let frame = (stack + STACK_BYTES - size_of::<Frame>() as u64) as *mut Frame;
        let (argptr, arglen) = space.argument();
        write_volatile(frame, Frame::user(space.entry(), space.initial_rsp(), argptr, arglen));
        let root = space.root();
        let parent = (*addr_of!(TASKS))[CURRENT].pid;
        process::install(slot, pid, parent, image_name, space);
        let (states, tasks) = table();
        tasks[slot] = Task { pid, name: task_name, context: frame as u64, stack, owns_stack: true,
            cpu_ticks: 0, domain: Domain::User, charged_domain: Domain::User, root, user: true,
            paused: false, waiting_child: 0, frames };
        states[slot] = State::Ready;
        COUNTERS[slot].store(0, Ordering::Relaxed);
        NEXT_PID = next_pid;
        Ok(pid)
    })
}

pub enum KillError {
    Denied(Denied),
    Failed(&'static str),
}

/// The trusted shell may stop ring 3 execution, subject to the same MAC
/// authority as kill. Kernel demonstration tasks and terminal PIDs refuse it.
fn suspension(pid: u32, paused: bool) -> Result<&'static str, KillError> {
    interrupts::without(|| unsafe {
        let slot = (0..MAX_TASKS).find(|&slot| (*addr_of!(STATES))[slot] != State::Free
            && (*addr_of!(TASKS))[slot].pid == pid)
            .ok_or(KillError::Failed("no such task"))?;
        let victim = (*addr_of!(TASKS))[slot];
        if slot == CURRENT || !victim.user {
            return Err(KillError::Failed("only another user process can be suspended"));
        }
        if (*addr_of!(STATES))[slot] == State::Exited {
            return Err(KillError::Failed("process has already exited"));
        }
        security::check(Op::Kill, Some(victim.domain), Some(pid as u64)).map_err(KillError::Denied)?;
        if victim.paused == paused {
            return Err(KillError::Failed(if paused { "process is already stopped" } else { "process is not stopped" }));
        }
        (*addr_of_mut!(TASKS))[slot].paused = paused;
        Ok(victim.name)
    })
}

pub fn pause(pid: u32) -> Result<&'static str, KillError> { suspension(pid, true) }
pub fn resume(pid: u32) -> Result<&'static str, KillError> { suspension(pid, false) }

pub fn kill(pid: u32) -> Result<&'static str, KillError> {
    interrupts::without(|| unsafe {
        let slot = (0..MAX_TASKS)
            .find(|&slot| (*addr_of!(STATES))[slot] != State::Free && (*addr_of!(TASKS))[slot].pid == pid)
            .ok_or(KillError::Failed("no such task"))?;
        if slot == CURRENT {
            return Err(KillError::Failed("a task cannot kill itself"));
        }
        if slot == SHELL {
            return Err(KillError::Failed("the shell cannot be killed"));
        }
        if (*addr_of!(STATES))[slot] == State::Exited {
            return Err(KillError::Failed("task has already exited"));
        }
        let victim = (*addr_of!(TASKS))[slot];
        // The idle task is labelled kernel, so the policy refuses it.
        security::check(Op::Kill, Some(victim.domain), Some(pid as u64)).map_err(KillError::Denied)?;
        // The shell is running this command, so the victim is not on the CPU.
        let name = victim.name;
        if victim.user {
            process::finish(slot, victim.cpu_ticks, ExitReason::Killed);
            crate::user_syscalls::revoke(victim.pid);
        }
        let (states, tasks) = table();
        release(states, tasks, slot);
        Ok(name)
    })
}

/// The pid whose stack contains `address`. Callers hold IF=0 across this
/// check and the matching frame operation.
pub fn stack_owner(address: u64) -> Option<u32> {
    unsafe {
        let (states, tasks) = table();
        (0..MAX_TASKS)
            .find(|&slot| states[slot] != State::Free && tasks[slot].owns_stack
                && (tasks[slot].stack..tasks[slot].stack + STACK_BYTES).contains(&address))
            .map(|slot| tasks[slot].pid)
    }
}

/// Call `each` for every task, from a snapshot taken under CLI.
pub fn list(mut each: impl FnMut(&Info)) {
    let mut snapshot: [Option<Info>; MAX_TASKS] = [const { None }; MAX_TASKS];
    interrupts::without(|| unsafe {
        let (states, tasks) = table();
        for slot in 0..MAX_TASKS {
            if states[slot] == State::Free {
                continue;
            }
            let task = &tasks[slot];
            snapshot[slot] = Some(Info {
                pid: task.pid,
                name: task.name,
                domain: task.domain,
                state: task_state(states[slot], task, slot == CURRENT),
                cpu_ticks: task.cpu_ticks,
                counter: if task.owns_stack && !task.user { Some(COUNTERS[slot].load(Ordering::Relaxed)) } else { None },
                stack: task.stack,
                owns_stack: task.owns_stack,
                user: task.user,
                frames: task.frames,
            });
        }
    });
    for info in snapshot.iter().flatten() {
        each(info);
    }
}

extern "C" fn idle_main(_: u64) -> ! {
    loop {
        interrupts::wait();
    }
}

extern "C" fn spin_main(slot: u64) -> ! {
    loop {
        COUNTERS[slot as usize].fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn beat_main(slot: u64) -> ! {
    loop {
        sleep_until(interrupts::ticks() + interrupts::TIMER_HZ / 10);
        COUNTERS[slot as usize].fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn once_main(slot: u64) -> ! {
    sleep_until(interrupts::ticks() + interrupts::TIMER_HZ * 3 / 10);
    COUNTERS[slot as usize].store(1, Ordering::Relaxed);
    exit()
}
