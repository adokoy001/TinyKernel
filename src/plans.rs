//! One reviewed, literal filesystem mutation. This module performs no I/O.
//! The runtime records a policy-checked target and storage revision, shows
//! the saved command, then checks both again before a one-use apply.
//! Revisions are boot-local and conservative: any filesystem mutation or
//! remount invalidates the plan, even when the target looks identical.

use crate::fs::{self, MAX_FILE_SIZE, MAX_NAME};
use crate::mac::{Domain, Op};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlannedKind {
    Write,
    Append,
    Remove,
}

impl PlannedKind {
    pub fn name(self) -> &'static str {
        match self { Self::Write => "write", Self::Append => "append", Self::Remove => "remove" }
    }
}

/// This identity is supplied only after the storage access/checksum gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Snapshot {
    Absent,
    Exists { slot: usize, label: Domain, size: u32, generation: u32, checksum: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    NoPlan,
    InvalidName,
    PayloadTooLarge,
    RemoveHasPayload,
    IdMismatch,
    DomainChanged,
    RevisionChanged,
    TargetChanged,
    UnknownState,
    RevisionExhausted,
    IdExhausted,
}

impl PlanError {
    pub fn message(self) -> &'static str {
        match self {
            Self::NoPlan => "no pending plan",
            Self::InvalidName => "plan target must be a valid TaneFS name",
            Self::PayloadTooLarge => "plan payload exceeds 4096 bytes",
            Self::RemoveHasPayload => "remove plan cannot have a payload",
            Self::IdMismatch => "plan ID does not match; plan consumed",
            Self::DomainChanged => "plan creator domain does not match the current domain",
            Self::RevisionChanged => "storage changed since plan; plan consumed",
            Self::TargetChanged => "target identity changed since plan; plan consumed",
            Self::UnknownState => "target state could not be verified; plan consumed",
            Self::RevisionExhausted => "storage revision exhausted; planning disabled",
            Self::IdExhausted => "plan IDs exhausted; planning disabled",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanView<'a> {
    pub id: u64,
    pub kind: PlannedKind,
    pub name: &'a str,
    pub payload: &'a [u8],
    pub creator: Domain,
    pub revision: u64,
    pub before: Snapshot,
}

impl PlanView<'_> {
    /// The mutation gate; storage also verifies read access for snapshots.
    pub fn required_op(self) -> Op {
        match (self.kind, self.before) {
            (PlannedKind::Remove, _) => Op::Delete,
            (_, Snapshot::Absent) => Op::Create,
            (_, Snapshot::Exists { .. }) => Op::Write,
        }
    }
}

/// Store this bounded buffer statically in the runtime, not on a task stack.
pub struct Plan {
    active: bool,
    next_id: u64,
    id: u64,
    kind: PlannedKind,
    name: [u8; MAX_NAME],
    name_len: usize,
    payload: [u8; MAX_FILE_SIZE],
    payload_len: usize,
    creator: Domain,
    revision: u64,
    before: Snapshot,
}

impl Plan {
    pub const fn new() -> Self {
        Self { active: false, next_id: 1, id: 0, kind: PlannedKind::Write,
            name: [0; MAX_NAME], name_len: 0, payload: [0; MAX_FILE_SIZE], payload_len: 0,
            creator: Domain::Kernel, revision: 0, before: Snapshot::Absent }
    }

    /// Replace any previous plan after validating the literal arguments.
    /// Failed staging leaves the old plan available for review/discard.
    pub fn stage(&mut self, kind: PlannedKind, name: &str, payload: &[u8], creator: Domain,
        revision: u64, before: Snapshot) -> Result<u64, PlanError>
    {
        if !fs::valid_name(name.as_bytes()) { return Err(PlanError::InvalidName); }
        if payload.len() > MAX_FILE_SIZE { return Err(PlanError::PayloadTooLarge); }
        if kind == PlannedKind::Remove && !payload.is_empty() { return Err(PlanError::RemoveHasPayload); }
        if revision == u64::MAX { return Err(PlanError::RevisionExhausted); }
        let next = self.next_id.checked_add(1).ok_or(PlanError::IdExhausted)?;
        self.name.fill(0);
        self.name[..name.len()].copy_from_slice(name.as_bytes());
        self.payload.fill(0);
        self.payload[..payload.len()].copy_from_slice(payload);
        self.name_len = name.len();
        self.payload_len = payload.len();
        self.id = self.next_id;
        self.next_id = next;
        self.kind = kind;
        self.creator = creator;
        self.revision = revision;
        self.before = before;
        self.active = true;
        Ok(self.id)
    }

    pub fn id(&self) -> Option<u64> { if self.active { Some(self.id) } else { None } }

    fn view(&self) -> PlanView<'_> {
        // stage accepts ASCII filesystem names; the initial empty buffer is
        // never exposed because public view methods require an active plan.
        PlanView { id: self.id, kind: self.kind,
            name: core::str::from_utf8(&self.name[..self.name_len]).unwrap_or(""),
            payload: &self.payload[..self.payload_len], creator: self.creator,
            revision: self.revision, before: self.before }
    }

    /// Internal inspection. Display commands must use show_for so lowering
    /// a task domain cannot reveal a saved higher-domain file's metadata.
    pub fn show(&self) -> Option<PlanView<'_>> { if self.active { Some(self.view()) } else { None } }

    pub fn show_for(&self, domain: Domain) -> Result<PlanView<'_>, PlanError> {
        if !self.active { return Err(PlanError::NoPlan); }
        if domain != self.creator { return Err(PlanError::DomainChanged); }
        Ok(self.view())
    }

    pub fn discard(&mut self) {
        self.active = false;
        self.name.fill(0);
        self.payload.fill(0);
        self.name_len = 0;
        self.payload_len = 0;
    }

    /// Consume the pending plan before evaluating this attempt. `None`
    /// means the runtime could not verify the target (permission, disk or
    /// checksum failure); it must never be converted to Snapshot::Absent.
    /// A successful return authorizes just one call of the ordinary gated
    /// storage operation. It does not claim that that I/O will commit.
    pub fn begin_apply(&mut self, id: u64, domain: Domain, revision: u64,
        observed: Option<Snapshot>) -> Result<PlanView<'_>, PlanError>
    {
        if !self.active { return Err(PlanError::NoPlan); }
        self.active = false;
        if id != self.id { return Err(PlanError::IdMismatch); }
        if domain != self.creator { return Err(PlanError::DomainChanged); }
        if revision == u64::MAX || self.revision == u64::MAX { return Err(PlanError::RevisionExhausted); }
        if revision != self.revision { return Err(PlanError::RevisionChanged); }
        let observed = observed.ok_or(PlanError::UnknownState)?;
        if observed != self.before { return Err(PlanError::TargetChanged); }
        Ok(self.view())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{BlockDevice, DiskError, FileSystem, Sector, REQUIRED_SECTORS};

    #[derive(Clone)]
    struct MemoryDisk(Vec<Sector>);
    impl MemoryDisk {
        fn new() -> Self { Self(vec![[0; fs::SECTOR]; REQUIRED_SECTORS as usize]) }
    }
    impl BlockDevice for MemoryDisk {
        fn sectors(&self) -> u32 { self.0.len() as u32 }
        fn read(&mut self, lba: u32, buffer: &mut Sector) -> Result<(), DiskError> {
            *buffer = *self.0.get(lba as usize).ok_or(DiskError::Device)?;
            Ok(())
        }
        fn write(&mut self, lba: u32, buffer: &Sector) -> Result<(), DiskError> {
            *self.0.get_mut(lba as usize).ok_or(DiskError::Device)? = *buffer;
            Ok(())
        }
    }
    fn identity(fs: &mut FileSystem<MemoryDisk>, name: &str) -> Snapshot {
        match fs.find(name) {
            None => Snapshot::Absent,
            Some((slot, entry)) => {
                let mut data = [0; MAX_FILE_SIZE];
                fs.read(slot, &mut data).unwrap();
                Snapshot::Exists { slot, label: Domain::Admin, size: entry.size,
                    generation: entry.generation, checksum: entry.checksum }
            }
        }
    }
    fn file() -> (FileSystem<MemoryDisk>, Snapshot) {
        let mut fs = FileSystem::format(MemoryDisk::new()).unwrap();
        fs.create("note", Domain::Admin.index() as u8, b"before").unwrap();
        let before = identity(&mut fs, "note");
        (fs, before)
    }
    fn staged(before: Snapshot, revision: u64) -> Plan {
        let mut plan = Plan::new();
        plan.stage(PlannedKind::Write, "note", b"after", Domain::Admin, revision, before).unwrap();
        plan
    }

    #[test]
    fn valid_apply_is_one_use_and_keeps_literal_payload() {
        let mut plan = Plan::new();
        let literal = b"$(reboot); | write other \"quoted\"";
        let id = plan.stage(PlannedKind::Append, "note", literal, Domain::Admin, 7, Snapshot::Absent).unwrap();
        let view = plan.show_for(Domain::Admin).unwrap();
        assert_eq!(view.payload, literal);
        assert_eq!(view.required_op(), Op::Create);
        let view = plan.begin_apply(id, Domain::Admin, 7, Some(Snapshot::Absent)).unwrap();
        assert_eq!(view.name, "note");
        assert_eq!(view.payload, literal);
        assert_eq!(plan.id(), None);
        assert_eq!(plan.begin_apply(id, Domain::Admin, 7, Some(Snapshot::Absent)), Err(PlanError::NoPlan));
    }

    #[test]
    fn domain_change_hides_review_and_consumes_apply() {
        let (_, before) = file();
        let mut plan = staged(before, 3);
        assert_eq!(plan.show_for(Domain::User), Err(PlanError::DomainChanged));
        assert_eq!(plan.begin_apply(1, Domain::User, 3, Some(before)), Err(PlanError::DomainChanged));
        assert_eq!(plan.id(), None);
    }

    #[test]
    fn generation_change_is_detected_even_without_revision_change() {
        let (mut fs, before) = file();
        let mut plan = staged(before, 3);
        let slot = fs.find("note").unwrap().0;
        fs.overwrite(slot, b"before").unwrap();
        let after = identity(&mut fs, "note");
        assert_ne!(before, after);
        assert_eq!(plan.begin_apply(1, Domain::Admin, 3, Some(after)), Err(PlanError::TargetChanged));
    }

    #[test]
    fn delete_recreate_same_slot_contents_generation_is_not_an_aba() {
        let (mut fs, before) = file();
        let mut plan = staged(before, 3);
        let slot = fs.find("note").unwrap().0;
        fs.delete(slot).unwrap();
        assert_eq!(fs.create("note", Domain::Admin.index() as u8, b"before").unwrap(), slot);
        let after = identity(&mut fs, "note");
        // TaneFS resets generations on reuse: target metadata alone is not
        // an identity. The storage revision rejects the exact ABA case.
        assert_eq!(before, after);
        assert_eq!(plan.begin_apply(1, Domain::Admin, 5, Some(after)), Err(PlanError::RevisionChanged));
    }

    #[test]
    fn format_and_remount_same_target_is_not_an_aba() {
        let (fs, before) = file();
        let mut plan = staged(before, 3);
        let mut fs = FileSystem::format(fs.into_device()).unwrap();
        fs.create("note", Domain::Admin.index() as u8, b"before").unwrap();
        let mut fs = match FileSystem::mount(fs.into_device()) { Ok(fs) => fs, Err(_) => panic!("mount failed") };
        assert_eq!(identity(&mut fs, "note"), before);
        assert_eq!(plan.begin_apply(1, Domain::Admin, 6, Some(before)), Err(PlanError::RevisionChanged));
    }

    #[test]
    fn absent_then_created_cannot_be_overwritten_by_creation_plan() {
        let (mut fs, _) = file();
        let mut plan = Plan::new();
        plan.stage(PlannedKind::Write, "new", b"after", Domain::Admin, 3, Snapshot::Absent).unwrap();
        assert_eq!(identity(&mut fs, "new"), Snapshot::Absent);
        fs.create("new", Domain::Admin.index() as u8, b"other").unwrap();
        let new = identity(&mut fs, "new");
        assert_eq!(plan.begin_apply(1, Domain::Admin, 3, Some(new)), Err(PlanError::TargetChanged));
    }

    #[test]
    fn unrelated_mutation_or_failed_mutation_revision_invalidates() {
        let (_, before) = file();
        let mut plan = staged(before, 3);
        assert_eq!(plan.begin_apply(1, Domain::Admin, 4, Some(before)), Err(PlanError::RevisionChanged));
        assert_eq!(plan.id(), None);
    }

    #[test]
    fn unknown_state_is_never_treated_as_absent() {
        let mut plan = staged(Snapshot::Absent, 3);
        assert_eq!(plan.begin_apply(1, Domain::Admin, 3, None), Err(PlanError::UnknownState));
        assert_eq!(plan.id(), None);
    }

    #[test]
    fn every_target_metadata_field_is_checked() {
        let (_, before) = file();
        let Snapshot::Exists { slot, label, size, generation, checksum } = before else { unreachable!() };
        let changes = [Snapshot::Absent,
            Snapshot::Exists { slot: slot + 1, label, size, generation, checksum },
            Snapshot::Exists { slot, label: Domain::User, size, generation, checksum },
            Snapshot::Exists { slot, label, size: size + 1, generation, checksum },
            Snapshot::Exists { slot, label, size, generation: generation + 1, checksum },
            Snapshot::Exists { slot, label, size, generation, checksum: checksum ^ 1 }];
        for changed in changes {
            let mut plan = staged(before, 3);
            assert_eq!(plan.begin_apply(1, Domain::Admin, 3, Some(changed)), Err(PlanError::TargetChanged));
        }
    }

    #[test]
    fn replacement_ids_and_wrong_id_cannot_reuse_a_plan() {
        let mut plan = staged(Snapshot::Absent, 3);
        assert_eq!(plan.stage(PlannedKind::Remove, "note", b"", Domain::Admin, 3, Snapshot::Absent), Ok(2));
        assert_eq!(plan.begin_apply(1, Domain::Admin, 3, Some(Snapshot::Absent)), Err(PlanError::IdMismatch));
        assert_eq!(plan.begin_apply(2, Domain::Admin, 3, Some(Snapshot::Absent)), Err(PlanError::NoPlan));
    }

    #[test]
    fn bounds_and_invalid_arguments_do_not_replace_reviewed_plan() {
        let mut plan = staged(Snapshot::Absent, 3);
        assert_eq!(plan.stage(PlannedKind::Write, "a b", b"", Domain::Admin, 3, Snapshot::Absent), Err(PlanError::InvalidName));
        assert_eq!(plan.stage(PlannedKind::Write, "note", &[0; MAX_FILE_SIZE + 1], Domain::Admin, 3, Snapshot::Absent), Err(PlanError::PayloadTooLarge));
        assert_eq!(plan.stage(PlannedKind::Remove, "note", b"x", Domain::Admin, 3, Snapshot::Absent), Err(PlanError::RemoveHasPayload));
        assert_eq!(plan.id(), Some(1));
        plan.stage(PlannedKind::Write, &"a".repeat(MAX_NAME), &[b'x'; MAX_FILE_SIZE], Domain::Admin, 3, Snapshot::Absent).unwrap();
        assert_eq!(plan.show().unwrap().payload.len(), MAX_FILE_SIZE);
    }

    #[test]
    fn revision_and_id_exhaustion_never_wrap() {
        let mut plan = Plan::new();
        assert_eq!(plan.stage(PlannedKind::Write, "note", b"x", Domain::Admin, u64::MAX, Snapshot::Absent), Err(PlanError::RevisionExhausted));
        plan.next_id = u64::MAX;
        assert_eq!(plan.stage(PlannedKind::Write, "note", b"x", Domain::Admin, 3, Snapshot::Absent), Err(PlanError::IdExhausted));
        assert_eq!(plan.id(), None);
        plan.next_id = 1;
        plan.stage(PlannedKind::Write, "note", b"x", Domain::Admin, u64::MAX - 1, Snapshot::Absent).unwrap();
        assert_eq!(plan.begin_apply(1, Domain::Admin, u64::MAX, Some(Snapshot::Absent)), Err(PlanError::RevisionExhausted));
    }

    #[test]
    fn discard_clears_saved_literal_data_and_preserves_id_sequence() {
        let mut plan = staged(Snapshot::Absent, 3);
        plan.discard();
        assert_eq!(plan.show(), None);
        assert!(plan.payload.iter().all(|b| *b == 0));
        assert!(plan.name.iter().all(|b| *b == 0));
        assert_eq!(plan.stage(PlannedKind::Write, "note", b"x", Domain::Admin, 3, Snapshot::Absent), Ok(2));
    }
}
