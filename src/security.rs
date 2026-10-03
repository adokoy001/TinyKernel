//! The kernel's single MAC enforcement point and its audit log.

use crate::interrupts;
use crate::mac::{self, AuditLog, Domain, Op, Record};
use crate::tasks;
use core::ptr::addr_of_mut;

pub const AUDIT_RECORDS: usize = 16;

static mut AUDIT: AuditLog<AUDIT_RECORDS> = AuditLog::new();

/// A request the policy refused. It has already been audited.
#[derive(Clone, Copy)]
pub struct Denied {
    pub subject: Domain,
    pub op: Op,
    pub object: Option<Domain>,
}

/// Check the current task's request against the policy and audit any
/// denial. Safe to call inside a CLI section; callers that look up the
/// object do so in the same section, so the label cannot change meanwhile.
pub fn check(op: Op, object: Option<Domain>, target: Option<u64>) -> Result<(), Denied> {
    interrupts::without(|| {
        let (pid, _) = tasks::current();
        let subject = tasks::current_domain();
        if mac::allowed(subject, op, object) {
            return Ok(());
        }
        let record = Record { tick: interrupts::ticks(), pid, subject, op, object, target };
        unsafe { (*addr_of_mut!(AUDIT)).record(record); }
        Err(Denied { subject, op, object })
    })
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
