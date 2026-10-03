//! Mandatory access control. Every task carries a domain label; objects
//! (tasks, frames from `alloc`) carry the label of the task that created
//! them. A fixed, compiled-in policy decides which operations a subject
//! domain may perform. No subject can change the policy, and a domain can
//! only be lowered (admin -> user), never raised, until the machine reboots.
//! Pure logic without hardware access, shared by the kernel and host tests.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    /// Kernel-internal tasks such as idle. No command may act on them.
    Kernel,
    /// The shell at boot: may manage the machine.
    Admin,
    /// A lowered shell: may only manage its own frames and tasks.
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Halt,
    Reboot,
    Fault,
    ReadAudit,
    Alloc,
    Free,
    Spawn,
    Kill,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Halt => "halt",
            Op::Reboot => "reboot",
            Op::Fault => "fault",
            Op::ReadAudit => "audit",
            Op::Alloc => "alloc",
            Op::Free => "free",
            Op::Spawn => "spawn",
            Op::Kill => "kill",
        }
    }
}

/// The policy. `object` is the label of the frame or task acted on, or
/// `None` for operations without an object. Anything not listed is denied.
pub fn allowed(subject: Domain, op: Op, object: Option<Domain>) -> bool {
    use Domain::*;
    use Op::*;
    matches!(
        (subject, op, object),
        (Admin, Halt | Reboot | Fault | ReadAudit | Alloc | Spawn, None)
            | (Admin, Free | Kill, Some(Admin | User))
            | (User, Alloc | Spawn, None)
            | (User, Free | Kill, Some(User))
    )
}

/// Domains may only be lowered.
pub fn may_transition(from: Domain, to: Domain) -> bool {
    matches!((from, to), (Domain::Admin, Domain::User))
}

/// One denied request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub tick: u64,
    pub pid: u32,
    pub subject: Domain,
    pub op: Op,
    pub object: Option<Domain>,
    /// The pid or address the request named, if any.
    pub target: Option<u64>,
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
    use super::{allowed, may_transition, AuditLog, Domain, Op, Record};
    use Domain::*;

    const ALL_OPS: [Op; 8] = [Op::Halt, Op::Reboot, Op::Fault, Op::ReadAudit, Op::Alloc, Op::Free, Op::Spawn, Op::Kill];
    const OBJECTS: [Option<Domain>; 4] = [None, Some(Kernel), Some(Admin), Some(User)];

    #[test]
    fn admin_manages_the_machine_but_never_kernel_objects() {
        for op in [Op::Halt, Op::Reboot, Op::Fault, Op::ReadAudit, Op::Alloc, Op::Spawn] {
            assert!(allowed(Admin, op, None), "{op:?}");
        }
        for op in [Op::Free, Op::Kill] {
            assert!(allowed(Admin, op, Some(Admin)));
            assert!(allowed(Admin, op, Some(User)));
            assert!(!allowed(Admin, op, Some(Kernel)));
            assert!(!allowed(Admin, op, None));
        }
    }

    #[test]
    fn user_only_touches_user_objects() {
        assert!(allowed(User, Op::Alloc, None));
        assert!(allowed(User, Op::Spawn, None));
        assert!(allowed(User, Op::Free, Some(User)));
        assert!(allowed(User, Op::Kill, Some(User)));
        for op in [Op::Halt, Op::Reboot, Op::Fault, Op::ReadAudit] {
            assert!(!allowed(User, op, None), "{op:?}");
        }
        for op in [Op::Free, Op::Kill] {
            assert!(!allowed(User, op, Some(Admin)));
            assert!(!allowed(User, op, Some(Kernel)));
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
        let record = |tick| Record { tick, pid: 1, subject: User, op: Op::Halt, object: None, target: None };
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
