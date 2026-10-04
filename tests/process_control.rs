//! Host boundary tests against the production process ownership/result code.
//! Hardware switching and page mappings are substituted here; QEMU exercises
//! their real implementations. These tests force completion eviction, invalid
//! output spans, slot reuse and allocation failures deterministically.

#![allow(dead_code)]

use std::sync::Mutex;

#[path = "../src/mac.rs"]
mod mac;
#[path = "../src/process.rs"]
mod process;

mod interrupts {
    #[derive(Default)]
    pub struct Frame { pub rax: u64 }
    pub fn without<T>(f: impl FnOnce() -> T) -> T { f() }
}

mod fs { pub const MAX_NAME: usize = 47; pub const MAX_FILE_SIZE: usize = 4096; }
mod user_abi { pub const EFAULT: i64 = -14; }
mod executable {
    pub const ARGS_MAX: usize = 128;
    pub struct Image<'a>(&'a [u8]);
    pub struct Error;
    impl Error { pub fn message(&self) -> &'static str { "invalid image" } }
    impl<'a> Image<'a> {
        pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> { Ok(Self(bytes)) }
    }
}
mod user_images {
    pub struct Program { pub name: &'static str, pub bytes: &'static [u8] }
    pub static PROGRAMS: [Program; 1] = [Program { name: "hello", bytes: &[1] }];
}
mod storage {
    pub struct StorageError;
    pub fn read_for_domain(_: super::mac::Domain, _: &str, bytes: &mut [u8]) -> Result<usize, StorageError> {
        bytes[0] = 1;
        Ok(1)
    }
}

mod security {
    use super::mac::{Domain, Op};
    #[derive(Clone, Copy)]
    pub struct Denied;
    pub static mut FRAMES: u32 = 0;
    pub static mut CAPABILITY_DENIALS: u32 = 0;
    pub fn check(_: Op, _: Option<Domain>, _: Option<u64>) -> Result<(), Denied> { Ok(()) }
    pub fn charge_domain(domain: Domain, _: Op, _: u32, frames: u32) -> Result<(), Denied> {
        assert_eq!(domain, Domain::User);
        unsafe {
            if FRAMES + frames > 24 { return Err(Denied); }
            FRAMES += frames;
        }
        Ok(())
    }
    pub fn release(domain: Domain, _: u32, frames: u32) {
        assert_eq!(domain, Domain::User);
        unsafe { FRAMES = FRAMES.checked_sub(frames).unwrap(); }
    }
    pub fn deny_capability(_: Op, _: Option<Domain>, _: Option<u64>) {
        unsafe { CAPABILITY_DENIALS += 1; }
    }
}

mod usermem {
    use std::cell::UnsafeCell;
    pub const SPACE_FRAMES: usize = 8;
    pub const HEAP_MAX_PAGES: usize = 8;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum HeapError { TooLarge, OutOfMemory }
    pub static mut FAIL_NEXT_GROWTH: bool = false;
    pub struct AddressSpace { bytes: Box<UnsafeCell<[u8; 4096]>>, pages: usize }
    impl AddressSpace {
        pub fn create(_: &super::executable::Image<'_>, _: &[u8]) -> Result<Self, &'static str> {
            Ok(Self { bytes: Box::new(UnsafeCell::new([0; 4096])), pages: 0 })
        }
        pub fn base(&self) -> u64 { self.bytes.get() as u64 }
        pub fn heap_pages(&self) -> usize { self.pages }
        pub fn owned_frames(&self) -> usize { SPACE_FRAMES + self.pages }
        pub fn resize_heap(&mut self, pages: usize) -> Result<(), HeapError> {
            if pages > HEAP_MAX_PAGES { return Err(HeapError::TooLarge); }
            unsafe {
                if pages > self.pages && FAIL_NEXT_GROWTH {
                    FAIL_NEXT_GROWTH = false;
                    return Err(HeapError::OutOfMemory);
                }
            }
            self.pages = pages;
            Ok(())
        }
        pub fn validate_write(&self, pointer: u64, count: usize) -> Result<(), &'static str> {
            let end = pointer.checked_add(count as u64).ok_or("overflow")?;
            if pointer < self.base() || pointer >= self.base() + 4096 || end > self.base() + 4096 {
                Err("unmapped span")
            } else { Ok(()) }
        }
        pub fn copy_to_user(&self, pointer: u64, bytes: &[u8]) -> Result<(), &'static str> {
            self.validate_write(pointer, bytes.len())?;
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len()); }
            Ok(())
        }
        pub fn destroy(self) {}
    }
}

mod tasks {
    use super::{interrupts::Frame, mac::Domain, process, security};
    use std::ptr::{addr_of, addr_of_mut};
    pub const MAX_TASKS: usize = 8;
    pub const STACK_FRAMES: usize = 4;
    pub enum SpawnError { Denied(security::Denied), ChildPending, Failed(&'static str) }
    pub enum KillError { Denied(security::Denied), Failed(&'static str) }
    #[derive(Clone, Copy)]
    pub struct Info { pub pid: u32, pub state: &'static str, pub cpu_ticks: u64, pub frames: u32 }
    #[derive(Clone, Copy)]
    struct Task { pid: u32, active: bool, terminal: bool, waiting: u32, context: u64, frames: u32 }
    impl Task { const EMPTY: Self = Self { pid: 0, active: false, terminal: false, waiting: 0, context: 0, frames: 0 }; }
    static mut TASKS: [Task; MAX_TASKS] = [Task::EMPTY; MAX_TASKS];
    static mut CURRENT: usize = 0;
    static mut NEXT_PID: u32 = 100;
    pub fn current_slot() -> usize { unsafe { CURRENT } }
    pub fn is_user() -> bool { unsafe { CURRENT >= 2 && (*addr_of!(TASKS))[CURRENT].active } }
    pub fn current() -> (u32, &'static str) { unsafe { ((*addr_of!(TASKS))[CURRENT].pid, "host") } }
    pub fn current_domain() -> Domain { if is_user() { Domain::User } else { Domain::Admin } }
    pub fn current_charge_owner() -> Domain { Domain::User }
    pub fn set_current_frames(frames: u32) { unsafe { (*addr_of_mut!(TASKS))[CURRENT].frames = frames; } }
    pub fn select(slot: usize) { unsafe { CURRENT = slot; } }
    pub fn slot_info(slot: usize) -> Option<Info> {
        let task = unsafe { (*addr_of!(TASKS))[slot] };
        task.active.then_some(Info { pid: task.pid,
            state: if task.waiting != 0 { "waiting" } else { "ready" }, cpu_ticks: 0, frames: task.frames })
    }
    pub fn install(slot: usize, parent_pid: u32) -> u32 {
        let image = super::executable::Image::parse(&[1]).unwrap_or_else(|_| panic!("image"));
        let space = super::usermem::AddressSpace::create(&image, &[]).unwrap();
        unsafe {
            assert!(!(*addr_of!(TASKS))[slot].active);
            let pid = NEXT_PID;
            NEXT_PID += 1;
            security::charge_domain(Domain::User, super::mac::Op::Spawn, 1, 12).unwrap_or_else(|_| panic!("quota"));
            process::install(slot, pid, parent_pid, "host", space);
            (*addr_of_mut!(TASKS))[slot] = Task { pid, active: true, terminal: false, waiting: 0, context: 0, frames: 12 };
            pid
        }
    }
    pub fn spawn_user(_: &super::executable::Image<'_>, _: &[u8], _: &'static str, _: &str) -> Result<u32, SpawnError> {
        process::check_child_capacity()?;
        let slot = unsafe { (2..MAX_TASKS).find(|&slot| !(*addr_of!(TASKS))[slot].active) }.unwrap();
        Ok(install(slot, current().0))
    }
    pub fn wait_child_current(frame: *mut Frame, child: u32) -> *mut Frame {
        unsafe {
            let task = &mut (*addr_of_mut!(TASKS))[CURRENT];
            task.waiting = child;
            task.context = frame as u64;
        }
        std::ptr::null_mut() // a different frame is selected by the fake switch
    }
    pub fn wake_child_wait(parent: u32, child: u32, result: i64) {
        unsafe {
            for task in (*addr_of_mut!(TASKS)).iter_mut() {
                if task.active && !task.terminal && task.pid == parent && task.waiting == child {
                    (*(task.context as *mut Frame)).rax = result as u64;
                    task.waiting = 0;
                    break;
                }
            }
        }
    }
    pub fn finish(slot: usize, reason: process::ExitReason) {
        unsafe {
            process::finish(slot, 0, reason);
            (*addr_of_mut!(TASKS))[slot].terminal = true;
        }
    }
    pub fn reap_exited() {
        unsafe {
            for slot in 2..MAX_TASKS {
                let task = (*addr_of!(TASKS))[slot];
                if task.active && task.terminal {
                    process::reap(slot);
                    security::release(Domain::User, 1, task.frames);
                    (*addr_of_mut!(TASKS))[slot] = Task::EMPTY;
                }
            }
        }
    }
    pub fn kill(pid: u32) -> Result<&'static str, KillError> {
        let slot = unsafe { (2..MAX_TASKS).find(|&slot| (*addr_of!(TASKS))[slot].active && (*addr_of!(TASKS))[slot].pid == pid) }
            .ok_or(KillError::Failed("missing"))?;
        finish(slot, process::ExitReason::Killed);
        reap_exited();
        Ok("host")
    }
    pub fn clear() {
        select(0);
        for slot in 2..MAX_TASKS {
            let task = unsafe { (*addr_of!(TASKS))[slot] };
            if task.active && !task.terminal { finish(slot, process::ExitReason::Exit(0)); }
        }
        reap_exited();
    }
}

static TEST_LOCK: Mutex<()> = Mutex::new(());
struct Environment;
impl Environment {
    fn new() -> Self {
        tasks::clear();
        unsafe { security::CAPABILITY_DENIALS = 0; usermem::FAIL_NEXT_GROWTH = false; }
        assert_eq!(unsafe { security::FRAMES }, 0);
        Self
    }
    fn parent(&self) -> u32 { tasks::select(0); let pid = tasks::install(2, 1); tasks::select(2); pid }
    fn child(&self, parent: u32) -> u32 { tasks::install(3, parent) }
    fn pointer(&self) -> u64 { process::current_space().unwrap().base() + 32 }
}
impl Drop for Environment { fn drop(&mut self) { tasks::clear(); } }
fn words(pointer: u64) -> [u64; 5] {
    let mut result = [0; 5];
    for (index, word) in result.iter_mut().enumerate() {
        let bytes = unsafe { std::slice::from_raw_parts((pointer + index as u64 * 8) as *const u8, 8) };
        *word = u64::from_le_bytes(bytes.try_into().unwrap());
    }
    result
}

#[test]
fn completed_child_survives_global_result_eviction_and_is_consumed_once() {
    let _lock = TEST_LOCK.lock().unwrap();
    let env = Environment::new();
    let parent = env.parent();
    let child = env.child(parent);
    tasks::finish(3, process::ExitReason::Exit(-22));
    tasks::reap_exited();
    assert!(matches!(process::spawn_builtin("hello", ""), Err(tasks::SpawnError::ChildPending)));
    // Ring 0 creates unrelated children until the shell's result log evicts it.
    for _ in 0..process::KEPT_RESULTS + 2 {
        tasks::select(0);
        tasks::install(3, 1);
        tasks::finish(3, process::ExitReason::Exit(7));
        tasks::reap_exited();
    }
    tasks::select(2);
    assert!(matches!(process::info(child), Err(process::ReadError::Missing)));
    let pointer = env.pointer();
    let mut frame = interrupts::Frame::default();
    assert!(process::wait_child(&mut frame, child, pointer).is_ok());
    assert_eq!(frame.rax, child as u64);
    assert_eq!(words(pointer), [child as u64, 0, (-22i64) as u64, 0, 0]);
    assert!(matches!(process::wait_child(&mut frame, child, pointer), Err(process::ChildError::NotChild)));
    assert!(process::spawn_builtin("hello", "").is_ok());
}

#[test]
fn invalid_full_span_never_consumes_completed_child() {
    let _lock = TEST_LOCK.lock().unwrap();
    let env = Environment::new();
    let parent = env.parent();
    let child = env.child(parent);
    tasks::finish(3, process::ExitReason::Killed);
    tasks::reap_exited();
    let base = process::current_space().unwrap().base();
    let mut frame = interrupts::Frame { rax: 991 };
    for pointer in [0, u64::MAX - 10, base + 4096 - 39] {
        assert!(matches!(process::wait_child(&mut frame, child, pointer), Err(process::ChildError::InvalidPointer)));
        assert_eq!(frame.rax, 991);
    }
    let pointer = env.pointer();
    assert!(process::wait_child(&mut frame, child, pointer).is_ok());
    assert_eq!(words(pointer), [child as u64, 2, 0, 0, 0]);
}

#[test]
fn blocked_wait_wakes_with_fault_details_before_parent_runs() {
    let _lock = TEST_LOCK.lock().unwrap();
    let env = Environment::new();
    let parent = env.parent();
    let child = env.child(parent);
    let pointer = env.pointer();
    let mut frame = interrupts::Frame { rax: 991 };
    assert!(process::wait_child(&mut frame, child, pointer).unwrap_or_else(|_| panic!("wait")).is_null());
    assert_eq!(tasks::slot_info(2).unwrap().state, "waiting");
    assert_eq!(frame.rax, 991);
    tasks::select(3);
    tasks::finish(3, process::ExitReason::Fault { vector: 14, error: 7, rip: 123, address: 456 });
    assert_eq!(frame.rax, child as u64);
    assert_eq!(tasks::slot_info(2).unwrap().state, "ready");
    assert_eq!(words(pointer), [child as u64, 1, 0, 14, 7]);
    tasks::select(2);
    assert!(matches!(process::wait_child(&mut frame, child, pointer), Err(process::ChildError::NotChild)));
}

#[test]
fn parent_exit_detaches_child_and_reused_slot_gets_no_completion() {
    let _lock = TEST_LOCK.lock().unwrap();
    let env = Environment::new();
    let parent = env.parent();
    let child = env.child(parent);
    let pointer = env.pointer();
    let mut frame = interrupts::Frame { rax: 991 };
    assert!(process::wait_child(&mut frame, child, pointer).is_ok());
    tasks::select(0);
    tasks::finish(2, process::ExitReason::Killed);
    tasks::reap_exited();
    let replacement = tasks::install(2, 1);
    assert_ne!(replacement, parent);
    tasks::select(3);
    tasks::finish(3, process::ExitReason::Exit(7));
    assert_eq!(frame.rax, 991);
    tasks::select(2);
    let pointer = env.pointer();
    assert!(matches!(process::wait_child(&mut frame, child, pointer), Err(process::ChildError::NotChild)));
}

#[test]
fn foreign_pid_and_completed_child_never_gain_kill_authority() {
    let _lock = TEST_LOCK.lock().unwrap();
    let env = Environment::new();
    let parent = env.parent();
    tasks::select(0);
    let foreign = tasks::install(3, 1);
    tasks::select(2);
    assert!(matches!(process::kill_child(foreign), Err(process::ChildError::NotChild)));
    assert_eq!(tasks::slot_info(3).unwrap().pid, foreign);
    assert_eq!(unsafe { security::CAPABILITY_DENIALS }, 1);
    tasks::select(0);
    tasks::finish(3, process::ExitReason::Exit(0));
    tasks::reap_exited();
    tasks::select(2);
    let child = env.child(parent);
    assert!(process::kill_child(child).is_ok());
    assert!(matches!(process::kill_child(child), Err(process::ChildError::AlreadyExited)));
    let pointer = env.pointer();
    let mut frame = interrupts::Frame::default();
    assert!(process::wait_child(&mut frame, child, pointer).is_ok());
    assert_eq!(words(pointer), [child as u64, 2, 0, 0, 0]);
}

#[test]
fn heap_failure_rolls_back_quota_and_shrink_updates_live_frame_metadata() {
    let _lock = TEST_LOCK.lock().unwrap();
    let env = Environment::new();
    let parent = env.parent();
    assert_eq!(process::resize_current(4).unwrap_or_else(|_| panic!("resize")), 4);
    assert_eq!(unsafe { security::FRAMES }, 16);
    unsafe { usermem::FAIL_NEXT_GROWTH = true; }
    assert!(matches!(process::resize_current(8), Err(process::HeapResizeError::Memory(usermem::HeapError::OutOfMemory))));
    assert_eq!(unsafe { security::FRAMES }, 16);
    assert_eq!(process::current_space().unwrap().heap_pages(), 4);
    assert_eq!(tasks::slot_info(2).unwrap().frames, 16);
    assert!(process::resize_current(2).is_ok());
    let info = process::info(parent).unwrap_or_else(|_| panic!("info"));
    assert_eq!(info.frames, 14);
    assert_eq!(info.heap_pages, 2);
    assert_eq!(unsafe { security::FRAMES }, 14);
    assert!(matches!(process::resize_current(9), Err(process::HeapResizeError::Memory(usermem::HeapError::TooLarge))));
    assert_eq!(unsafe { security::FRAMES }, 14);
    assert!(process::resize_current(0).is_ok());
    env.child(parent);
    unsafe { usermem::FAIL_NEXT_GROWTH = true; }
    assert!(matches!(process::resize_current(1), Err(process::HeapResizeError::Denied(_))));
    assert_eq!(unsafe { security::FRAMES }, 24);
    assert_eq!(process::current_space().unwrap().heap_pages(), 0);
    assert_eq!(tasks::slot_info(2).unwrap().frames, 12);
    // The allocator was never invoked after the quota gate refused growth.
    assert!(unsafe { usermem::FAIL_NEXT_GROWTH });
}
