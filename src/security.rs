//! The kernel's single gate for every request: first the access policy
//! (`mac::POLICY`: may this domain do it at all?), then the resource limits
//! (`resources`: does the domain still have the tasks, frames, files or
//! CPU share for it?). Both kinds of refusal go to one audit log.

use crate::interrupts;
use crate::mac::{self, AuditLog, Domain, Op, Reason, Record};
use crate::resources::{self, Accounts, Exceeded, Usage};
use crate::tasks;
use core::ptr::addr_of_mut;

pub const AUDIT_RECORDS: usize = 16;

static mut AUDIT: AuditLog<AUDIT_RECORDS> = AuditLog::new();
static mut ACCOUNTS: Accounts = Accounts::new();

/// A refused request. It has already been audited.
#[derive(Clone, Copy)]
pub struct Denied {
    pub subject: Domain,
    pub op: Op,
    pub object: Option<Domain>,
    /// `Some` when the policy allowed it but a resource limit did not.
    pub quota: Option<Exceeded>,
}

fn deny_as(subject: Domain, op: Op, object: Option<Domain>, target: Option<u64>, quota: Option<Exceeded>) -> Denied {
    let (pid, _) = tasks::current();
    let reason = if quota.is_some() { Reason::Quota } else { Reason::Policy };
    let record = Record { tick: interrupts::ticks(), pid, subject, op, object, target, reason };
    unsafe { (*addr_of_mut!(AUDIT)).record(record); }
    Denied { subject, op, object, quota }
}

/// Check the current task's request against the policy and audit any
/// denial. Safe to call inside a CLI section; callers that look up the
/// object do so in the same section, so the label cannot change meanwhile.
pub fn check(op: Op, object: Option<Domain>, target: Option<u64>) -> Result<(), Denied> {
    interrupts::without(|| {
        if mac::allowed(tasks::current_domain(), op, object) {
            return Ok(());
        }
        Err(deny_as(tasks::current_domain(), op, object, target, None))
    })
}

/// Reserve tasks and frames for the current task's domain, after `check`
/// allowed `op`. All or nothing.
pub fn charge(op: Op, tasks: u32, frames: u32) -> Result<(), Denied> {
    interrupts::without(|| {
        let domain = tasks::current_domain();
        unsafe { (*addr_of_mut!(ACCOUNTS)).charge(domain, tasks, frames) }
            .map_err(|exceeded| deny_as(domain, op, None, None, Some(exceeded)))
    })
}

/// Check a destination domain as well as the current caller. The caller's
/// PID remains attached to a refusal; the subject is the checked domain.
pub fn check_domain(subject: Domain, op: Op, object: Option<Domain>, target: Option<u64>) -> Result<(), Denied> {
    interrupts::without(|| {
        if mac::allowed(subject, op, object) { Ok(()) }
        else { Err(deny_as(subject, op, object, target, None)) }
    })
}

/// Charge the immutable child domain, rather than inheriting the privileged
/// creator's account. Callers authorize Spawn before this all-or-nothing step.
pub fn charge_domain(domain: Domain, op: Op, tasks: u32, frames: u32) -> Result<(), Denied> {
    interrupts::without(|| unsafe { (*addr_of_mut!(ACCOUNTS)).charge(domain, tasks, frames) }
        .map_err(|exceeded| deny_as(domain, op, None, None, Some(exceeded))))
}

/// A process tried to use a revoked, foreign or insufficient file handle.
/// This gate is distinct from domain MAC; tokens convey an immutable subset
/// of file authority even when the domain policy would allow an operation.
pub fn deny_capability(op: Op, object: Option<Domain>, target: Option<u64>) {
    interrupts::without(|| {
        let (pid, _) = tasks::current();
        let record = Record { tick: interrupts::ticks(), pid,
            subject: tasks::current_domain(), op, object, target,
            reason: Reason::Capability };
        unsafe { (*addr_of_mut!(AUDIT)).record(record); }
    });
}

/// Return resources to the domain that was charged for them.
pub fn release(domain: Domain, tasks: u32, frames: u32) {
    interrupts::without(|| unsafe { (*addr_of_mut!(ACCOUNTS)).release(domain, tasks, frames) })
}

/// Refuse a new file when the domain already has its limit of files.
pub fn check_file_quota(files_owned: u32) -> Result<(), Denied> {
    interrupts::without(|| {
        resources::check_files(tasks::current_domain(), files_owned)
            .map_err(|exceeded| deny_as(tasks::current_domain(), Op::Create, None, None, Some(exceeded)))
    })
}

/// Account one timer tick (from the timer IRQ).
pub fn tick(domain: Option<Domain>) {
    unsafe { (*addr_of_mut!(ACCOUNTS)).tick(domain) }
}

pub fn over_cpu_share(domain: Domain) -> bool {
    unsafe { (*addr_of_mut!(ACCOUNTS)).over_cpu_share(domain) }
}

pub fn usage(domain: Domain) -> Usage {
    interrupts::without(|| unsafe { (*addr_of_mut!(ACCOUNTS)).usage(domain) })
}

pub fn denials() -> u64 {
    interrupts::without(|| unsafe { (*addr_of_mut!(AUDIT)).total() })
}

/// Copy the kept records under CLI, then hand them out for printing.
pub fn audit_records(mut each: impl FnMut(u64, &Record)) {
    let mut copy: [Option<(u64, Record)>; AUDIT_RECORDS] = [None; AUDIT_RECORDS];
    interrupts::without(|| unsafe {
        let mut index = 0;
        (*addr_of_mut!(AUDIT)).for_each(|number, record| {
            copy[index] = Some((number, *record));
            index += 1;
        });
    });
    for (number, record) in copy.iter().flatten() {
        each(*number, record);
    }
}
