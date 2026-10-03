//! Mandatory access control. Every task carries a domain label; objects
//! (tasks, frames from `alloc`, files) carry the label of the task that
//! created them. The whole policy is the `POLICY` table below: a request is
//! allowed only if some row matches it. The kernel never decides access
//! anywhere else, and `sec` prints this same table, so what is shown is
//! what is enforced. No subject can change the table, and a domain can only
//! be lowered (admin -> user), never raised, until the machine reboots.
//! Pure logic without hardware access, shared by the kernel and host tests.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    /// Kernel-internal tasks such as idle. No command may act on them.
    Kernel,
    /// The shell at boot: may manage the machine.
    Admin,
    /// A lowered shell: may only manage its own objects.
    User,
}

impl Domain {
    pub fn name(self) -> &'static str {
        match self {
            Domain::Kernel => "kernel",
            Domain::Admin => "admin",
            Domain::User => "user",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

/// The kind of object an operation acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    System,
    Memory,
    Task,
    File,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::System => "system",
            Class::Memory => "memory",
            Class::Task => "task",
            Class::File => "file",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Halt,
    Reboot,
    Fault,
    ReadAudit,
    Format,
    Alloc,
    Free,
    Spawn,
    Kill,
    Create,
    Read,
    Write,
    Delete,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Halt => "halt",
            Op::Reboot => "reboot",
            Op::Fault => "fault",
            Op::ReadAudit => "audit",
            Op::Format => "format",
            Op::Alloc => "alloc",
            Op::Free => "free",
            Op::Spawn => "spawn",
            Op::Kill => "kill",
            Op::Create => "create",
            Op::Read => "read",
            Op::Write => "write",
            Op::Delete => "delete",
        }
    }

    pub fn class(self) -> Class {
        match self {
            Op::Halt | Op::Reboot | Op::Fault | Op::ReadAudit | Op::Format => Class::System,
            Op::Alloc | Op::Free => Class::Memory,
            Op::Spawn | Op::Kill => Class::Task,
            Op::Create | Op::Read | Op::Write | Op::Delete => Class::File,
        }
    }

    /// Whether the operation acts on an existing, labelled object.
    pub fn has_object(self) -> bool {
        matches!(self, Op::Free | Op::Kill | Op::Read | Op::Write | Op::Delete)
    }
}

/// One allow rule: `subject` may perform `ops`. For operations with an
/// object, its label must be one of `objects`; creating operations (alloc,
/// spawn, create) have no object yet, and label the new one with `subject`.
pub struct Rule {
    pub subject: Domain,
    pub class: Class,
    pub ops: &'static [Op],
    pub objects: &'static [Domain],
}

use Domain::{Admin, User};

/// The policy. Anything not allowed here is denied, including every request
/// by or on a `kernel` object.
pub const POLICY: [Rule; 7] = [
    Rule { subject: Admin, class: Class::System, ops: &[Op::Halt, Op::Reboot, Op::Fault, Op::ReadAudit, Op::Format], objects: &[] },
    Rule { subject: Admin, class: Class::Memory, ops: &[Op::Alloc, Op::Free], objects: &[Admin, User] },
    Rule { subject: Admin, class: Class::Task, ops: &[Op::Spawn, Op::Kill], objects: &[Admin, User] },
    Rule { subject: Admin, class: Class::File, ops: &[Op::Create, Op::Read, Op::Write, Op::Delete], objects: &[Admin, User] },
    Rule { subject: User, class: Class::Memory, ops: &[Op::Alloc, Op::Free], objects: &[User] },
    Rule { subject: User, class: Class::Task, ops: &[Op::Spawn, Op::Kill], objects: &[User] },
    Rule { subject: User, class: Class::File, ops: &[Op::Create, Op::Read, Op::Write, Op::Delete], objects: &[User] },
    // User has no System row: no halt, reboot, fault, audit or format.
];

/// `object` is the label of the object acted on; it must be `Some` exactly
/// for operations that have an object.
pub fn allowed(subject: Domain, op: Op, object: Option<Domain>) -> bool {
    if op.has_object() != object.is_some() {
        return false;
    }
    POLICY.iter().any(|rule| {
        rule.subject == subject
            && rule.class == op.class()
            && rule.ops.contains(&op)
            && object.map_or(true, |label| rule.objects.contains(&label))
    })
}

/// Domains may only be lowered.
pub fn may_transition(from: Domain, to: Domain) -> bool {
    matches!((from, to), (Domain::Admin, Domain::User))
}

/// Why a request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// No `POLICY` row allows it.
    Policy,
    /// The policy allows it, but the domain's resource limit is used up.
    Quota,
}

/// One denied request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub tick: u64,
    pub pid: u32,
    pub subject: Domain,
    pub op: Op,
    pub object: Option<Domain>,
    /// The pid, address or file slot the request named, if any.
    pub target: Option<u64>,
    pub reason: Reason,
}

/// Keeps the newest `N` denials and the total count.
pub struct AuditLog<const N: usize> {
    records: [Option<Record>; N],
    next: usize,
    total: u64,
}

impl<const N: usize> AuditLog<N> {
    pub const fn new() -> Self {
        Self { records: [None; N], next: 0, total: 0 }
    }

    pub fn record(&mut self, record: Record) {
        self.records[self.next] = Some(record);
        self.next = (self.next + 1) % N;
        self.total += 1;
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// Kept records, oldest first, numbered from 1 since boot.
    pub fn for_each(&self, mut each: impl FnMut(u64, &Record)) {
        let kept = self.total.min(N as u64);
        for index in 0..kept {
            let slot = (self.next + N - kept as usize + index as usize) % N;
            if let Some(record) = &self.records[slot] {
                each(self.total - kept + index + 1, record);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{allowed, may_transition, AuditLog, Domain, Op, Reason, Record, POLICY};
    use Domain::*;

    const ALL_OPS: [Op; 13] = [Op::Halt, Op::Reboot, Op::Fault, Op::ReadAudit, Op::Format, Op::Alloc, Op::Free,
                               Op::Spawn, Op::Kill, Op::Create, Op::Read, Op::Write, Op::Delete];
    const OBJECTS: [Option<Domain>; 4] = [None, Some(Kernel), Some(Admin), Some(User)];

    #[test]
    fn admin_manages_the_machine_but_never_kernel_objects() {
        for op in ALL_OPS.iter().filter(|op| !op.has_object()) {
            assert!(allowed(Admin, *op, None), "{op:?}");
        }
        for op in ALL_OPS.iter().filter(|op| op.has_object()) {
            assert!(allowed(Admin, *op, Some(Admin)), "{op:?}");
            assert!(allowed(Admin, *op, Some(User)), "{op:?}");
            assert!(!allowed(Admin, *op, Some(Kernel)), "{op:?}");
        }
    }

    #[test]
    fn user_only_touches_user_objects_and_no_system_operation() {
        for op in [Op::Alloc, Op::Spawn, Op::Create] {
            assert!(allowed(User, op, None), "{op:?}");
        }
        for op in [Op::Halt, Op::Reboot, Op::Fault, Op::ReadAudit, Op::Format] {
            assert!(!allowed(User, op, None), "{op:?}");
        }
        for op in ALL_OPS.iter().filter(|op| op.has_object()) {
            assert!(allowed(User, *op, Some(User)), "{op:?}");
            assert!(!allowed(User, *op, Some(Admin)), "{op:?}");
            assert!(!allowed(User, *op, Some(Kernel)), "{op:?}");
        }
    }

    #[test]
    fn kernel_subjects_and_mismatched_objects_are_denied() {
        for op in ALL_OPS {
            for object in OBJECTS {
                assert!(!allowed(Kernel, op, object));
            }
        }
        // Operations without an object never accept one, and vice versa.
        for subject in [Admin, User] {
            assert!(!allowed(subject, Op::Alloc, Some(User)));
            assert!(!allowed(subject, Op::Kill, None));
            assert!(!allowed(subject, Op::Read, None));
        }
    }

    #[test]
    fn every_rule_is_consistent_with_its_class() {
        for rule in POLICY.iter() {
            assert_ne!(rule.subject, Kernel, "kernel subjects get no rule");
            assert!(!rule.objects.contains(&Kernel), "kernel objects are never allowed");
            for op in rule.ops {
                assert_eq!(op.class(), rule.class, "{op:?} listed under {:?}", rule.class);
            }
        }
    }

    #[test]
    fn domains_only_go_down() {
        assert!(may_transition(Admin, User));
        for (from, to) in [(User, Admin), (User, User), (Admin, Admin), (Admin, Kernel), (User, Kernel), (Kernel, Admin)] {
            assert!(!may_transition(from, to), "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn audit_log_keeps_the_newest_records_in_order() {
        let mut log = AuditLog::<3>::new();
        let record = |tick| Record { tick, pid: 1, subject: User, op: Op::Halt, object: None, target: None, reason: Reason::Policy };
        let collect = |log: &AuditLog<3>| {
            let mut seen = Vec::new();
            log.for_each(|number, record| seen.push((number, record.tick)));
            seen
        };
        assert!(collect(&log).is_empty());
        log.record(record(10));
        log.record(record(20));
        assert_eq!(collect(&log), [(1, 10), (2, 20)]);
        for tick in [30, 40, 50] {
            log.record(record(tick));
        }
        assert_eq!(log.total(), 5);
        assert_eq!(collect(&log), [(3, 30), (4, 40), (5, 50)]);
    }
}
