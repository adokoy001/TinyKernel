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
}

impl Task {
    const EMPTY: Task = Task { pid: 0, name: "", context: 0, stack: 0, owns_stack: false, cpu_ticks: 0, domain: Domain::Kernel };
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
pub struct Info {
    pub pid: u32,
    pub name: &'static str,
    pub domain: Domain,
    pub state: &'static str,
    pub cpu_ticks: u64,
    pub counter: Option<u64>,
    pub stack: u64,
    pub owns_stack: bool,
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
    tasks[SHELL] = Task { pid: 1, name: "shell", stack: BOOT_STACK_BASE, domain: Domain::Admin, ..Task::EMPTY };
    states[SHELL] = State::Ready;
    let base = addr_of!(IDLE_STACK) as u64;
    let context = prepare(base, size_of::<IdleStack>() as u64, idle_main, 0);
    tasks[IDLE] = Task { pid: 0, name: "idle", context, stack: base, ..Task::EMPTY };
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
        let next = match prefer {
            Some(slot) if states[slot] == State::Ready => slot,
            _ => sched::next(states, current, IDLE),
        };
        CURRENT = next;
        tasks[next].context as *mut Frame
    }
}

unsafe fn release(states: &mut [State; MAX_TASKS], tasks: &mut [Task; MAX_TASKS], slot: usize) {
    if tasks[slot].owns_stack {
        let freed = crate::with_frames(|frames| frames.free_contiguous(tasks[slot].stack, STACK_FRAMES));
        assert!(freed.is_ok(), "task stack frames were not allocated");
    }
    tasks[slot] = Task::EMPTY;
    states[slot] = State::Free;
}

/// Timer IRQ: account the tick, wake sleepers, and preempt.
pub fn on_timer(frame: *mut Frame, now: u64) -> *mut Frame {
    unsafe {
        let (states, tasks) = table();
        tasks[CURRENT].cpu_ticks += 1;
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
    Failed(&'static str),
}

pub fn spawn(kind: TaskKind) -> Result<u32, SpawnError> {
    security::check(Op::Spawn, None, None).map_err(SpawnError::Denied)?;
    interrupts::without(|| unsafe {
        let (states, tasks) = table();
        let slot = states.iter().position(|state| *state == State::Free)
            .ok_or(SpawnError::Failed("task table is full (8 tasks)"))?;
        let stack = crate::with_frames(|frames| frames.allocate_contiguous(STACK_FRAMES))
            .ok_or(SpawnError::Failed("no free physical frames for a stack"))?;
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
        let pid = NEXT_PID;
        NEXT_PID += 1;
        let context = prepare(stack, STACK_BYTES, entry, slot as u64);
        let domain = tasks[CURRENT].domain;
        tasks[slot] = Task { pid, name: kind.name(), context, stack, owns_stack: true, cpu_ticks: 0, domain };
        states[slot] = State::Ready;
        Ok(pid)
    })
}

pub enum KillError {
    Denied(Denied),
    Failed(&'static str),
}

pub fn kill(pid: u32) -> Result<&'static str, KillError> {
    interrupts::without(|| unsafe {
        let (states, tasks) = table();
        let slot = (0..MAX_TASKS)
            .find(|&slot| states[slot] != State::Free && tasks[slot].pid == pid)
            .ok_or(KillError::Failed("no such task"))?;
        if slot == CURRENT {
            return Err(KillError::Failed("a task cannot kill itself"));
        }
        // The idle task is labelled kernel, so the policy refuses it.
        security::check(Op::Kill, Some(tasks[slot].domain), Some(pid as u64)).map_err(KillError::Denied)?;
        // The shell is running this command, so the victim is not on the CPU.
        let name = tasks[slot].name;
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
                state: if slot == CURRENT { "running" } else { states[slot].name() },
                cpu_ticks: task.cpu_ticks,
                counter: if task.owns_stack { Some(COUNTERS[slot].load(Ordering::Relaxed)) } else { None },
                stack: task.stack,
                owns_stack: task.owns_stack,
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
