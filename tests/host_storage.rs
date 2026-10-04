#![allow(dead_code)]
//! Real storage gates and TaneFS against a mock ATA device. Hardware calls
//! assert IF=0, so this also catches public APIs accidentally escaping the
//! storage critical section. Pure-module tests remain independently useful.
#[path = "../src/fs.rs"] mod fs;
#[path = "../src/mac.rs"] mod mac;
#[path = "../src/plans.rs"] mod plans;
#[path = "../src/handles.rs"] mod handles;
#[path = "../src/resources.rs"] mod resources;
#[path = "../src/security.rs"] mod security;
#[path = "../src/storage.rs"] mod storage;

mod interrupts {
    use std::cell::Cell;
    thread_local! { static ENABLED: Cell<bool> = const { Cell::new(true) }; }
    pub fn enabled() -> bool { ENABLED.with(Cell::get) }
    pub fn without<R>(f: impl FnOnce() -> R) -> R {
        let previous = ENABLED.with(|iflag| iflag.replace(false));
        let result = f();
        ENABLED.with(|iflag| iflag.set(previous));
        result
    }
    pub fn ticks() -> u64 { 123 }
}
mod tasks {
    use crate::mac::Domain;
    use std::cell::Cell;
    thread_local! { static DOMAIN: Cell<Domain> = const { Cell::new(Domain::Admin) }; }
    pub fn current_domain() -> Domain { DOMAIN.with(Cell::get) }
    pub fn current() -> (u32, &'static str) { (7, "storage-test") }
    pub fn domain(domain: Domain) { DOMAIN.with(|value| value.set(domain)); }
}
mod ata {
    use crate::fs::{BlockDevice, DiskError, Sector, REQUIRED_SECTORS};
    use std::sync::atomic::{AtomicUsize, Ordering};
    pub static READS: AtomicUsize = AtomicUsize::new(0);
    pub static WRITES: AtomicUsize = AtomicUsize::new(0);
    pub struct Ata { pub model: [u8; 40], data: Box<[Sector]> }
    impl Ata {
        pub fn detect() -> Option<Self> {
            assert!(!crate::interrupts::enabled(), "ATA detect must be serialized");
            let mut model = [b' '; 40]; model[..8].copy_from_slice(b"MOCK ATA");
            Some(Self { model, data: vec![[0; 512]; REQUIRED_SECTORS as usize].into_boxed_slice() })
        }
    }
    impl BlockDevice for Ata {
        fn sectors(&self) -> u32 { REQUIRED_SECTORS }
        fn read(&mut self, lba: u32, buffer: &mut Sector) -> Result<(), DiskError> {
            assert!(!crate::interrupts::enabled(), "ATA reads must be serialized");
            READS.fetch_add(1, Ordering::SeqCst);
            *buffer = *self.data.get(lba as usize).ok_or(DiskError::Device)?;
            Ok(())
        }
        fn write(&mut self, lba: u32, buffer: &Sector) -> Result<(), DiskError> {
            assert!(!crate::interrupts::enabled(), "ATA writes must be serialized");
            WRITES.fetch_add(1, Ordering::SeqCst);
            *self.data.get_mut(lba as usize).ok_or(DiskError::Device)? = *buffer;
            Ok(())
        }
    }
}

#[test]
fn real_storage_identity_access_quota_and_serialization() {
    use handles::{READ, WRITE};
    use mac::Domain::{Admin, User};
    use std::sync::atomic::Ordering::SeqCst;
    use storage::StorageError;
    assert!(interrupts::enabled());
    storage::init(); storage::format().unwrap_or_else(|_| panic!("format failed"));
    assert!(interrupts::enabled(), "critical section restores prior IF");
    let owned_model = match storage::status() {
        storage::Status::Mounted { model, .. } => model,
        _ => panic!("mock disk should be mounted"),
    };
    assert_eq!(owned_model.as_str(), "MOCK ATA");
    tasks::domain(User);
    let first = storage::open_capability("shared", READ | WRITE).unwrap_or_else(|_| panic!("create"));
    let second = storage::open_capability("shared", READ).unwrap_or_else(|_| panic!("read open"));
    assert_eq!(first, second);
    let after = storage::append_capability("shared", first, b"payload").unwrap_or_else(|_| panic!("append"));
    assert_ne!(after, first);
    let mut bytes = [0u8; fs::MAX_FILE_SIZE];
    let reads = ata::READS.load(SeqCst);
    assert!(matches!(storage::read_capability("shared", second, &mut bytes), Err(StorageError::Stale)));
    assert_eq!(ata::READS.load(SeqCst), reads, "stale handle rejected before disk reads");
    assert_eq!(storage::read_capability("shared", after, &mut bytes).ok(), Some(7));
    assert_eq!(&bytes[..7], b"payload");
    // WRITE open must not truncate a live file.
    assert_eq!(storage::open_capability("shared", WRITE).ok(), Some(after));
    assert_eq!(storage::read("shared", &mut bytes).ok(), Some(7));

    // Two generations really reset to one across deletion/recreation, while
    // the independent kernel epoch still makes the previous handle stale.
    let old = storage::open_capability("reuse", READ | WRITE).unwrap_or_else(|_| panic!("create reuse"));
    storage::remove("reuse").unwrap_or_else(|_| panic!("remove reuse"));
    let recreated = storage::open_capability("reuse", READ | WRITE).unwrap_or_else(|_| panic!("recreate reuse"));
    assert_eq!(recreated.slot, old.slot); assert_eq!(recreated.generation, old.generation);
    assert_eq!(recreated.label, old.label); assert_eq!(recreated.checksum, old.checksum);
    assert_ne!(recreated.epoch, old.epoch);
    assert!(matches!(storage::read_capability("reuse", old, &mut bytes), Err(StorageError::Stale)));

    let before = ata::WRITES.load(SeqCst);
    assert!(matches!(storage::open_capability("bad/name", WRITE), Err(StorageError::Fs(fs::FsError::BadName))));
    assert!(matches!(storage::open_capability("bad-rights", 0x81), Err(StorageError::Fs(fs::FsError::BadName))));
    assert!(matches!(storage::read("missing", &mut bytes), Err(StorageError::Fs(fs::FsError::NotFound))));
    assert_eq!(ata::WRITES.load(SeqCst), before, "invalid calls produce no writes");

    tasks::domain(Admin);
    storage::write("secret", b"admin executable", false).unwrap_or_else(|_| panic!("admin file"));
    let before = ata::READS.load(SeqCst);
    assert!(matches!(storage::read_for_domain(User, "secret", &mut bytes), Err(StorageError::Denied(_))));
    assert_eq!(ata::READS.load(SeqCst), before, "destination gate precedes executable reads");
    tasks::domain(User);
    let before = ata::WRITES.load(SeqCst);
    assert!(matches!(storage::open_capability("secret", READ), Err(StorageError::Denied(_))));
    assert!(matches!(storage::open_capability("secret", WRITE), Err(StorageError::Denied(_))));
    assert_eq!(ata::WRITES.load(SeqCst), before, "MAC refusal preserves admin file");
    // shared/reuse already consume two User slots; exactly six more fit.
    for n in 0..6 { storage::open_capability(&format!("quota{n}"), WRITE).unwrap_or_else(|_| panic!("quota fill")); }
    let before = ata::WRITES.load(SeqCst);
    assert!(matches!(storage::open_capability("overflow", WRITE), Err(StorageError::Denied(_))));
    assert_eq!(ata::WRITES.load(SeqCst), before, "quota checked before creating metadata");
    let mut names = Vec::new();
    storage::list(|entry| {
        assert!(!interrupts::enabled(), "listing callback stays serialized");
        interrupts::without(|| assert!(!interrupts::enabled()));
        assert!(!interrupts::enabled(), "nested critical section must not re-enable IF");
        names.push(entry.name.to_owned());
    }).unwrap_or_else(|_| panic!("list"));
    assert!(!names.iter().any(|name| name == "secret" || name == "overflow"));
    assert!(interrupts::enabled());
    let mut records = Vec::new();
    security::audit_records(|_, record| records.push(*record));
    assert!(records.iter().any(|r| r.subject == User && r.op == mac::Op::Read && r.object == Some(Admin)));
    assert!(records.iter().any(|r| r.subject == User && r.op == mac::Op::Create && r.reason == mac::Reason::Quota));
    tasks::domain(Admin);
    storage::format().unwrap_or_else(|_| panic!("reformat"));
    tasks::domain(User);
    let fresh = storage::open_capability("shared", READ | WRITE).unwrap_or_else(|_| panic!("fresh shared"));
    assert_ne!(fresh.epoch, first.epoch);
    assert!(matches!(storage::read_capability("shared", first, &mut bytes), Err(StorageError::Stale)));
    assert_eq!(owned_model.as_str(), "MOCK ATA", "status owns its model across replacement");
    assert!(interrupts::enabled());
}
