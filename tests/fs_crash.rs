//! Power-cut harness: a drive cache is volatile; only flush establishes an
//! ordered durable barrier. Individual unflushed writes may also reach media.
//! This models devices honoring flush, not a proof about arbitrary hardware.
#[allow(dead_code)]
#[path = "../src/fs.rs"] mod fs;
use fs::{BlockDevice, DiskError, Entry, FileSystem, FsError, Recovery, Sector};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

#[derive(Clone, Copy, Debug)]
enum Event { Write(u32), Flush }
#[derive(Clone, Copy, Debug)]
enum Failure { Before, Persist, Prefix(usize), FlushSubset(usize) }
#[derive(Clone)]
struct State {
    durable: Vec<Sector>, cache: BTreeMap<u32, Sector>, events: Vec<Event>,
    cut: Option<usize>, failure: Failure, flip_stage_read: bool,
}
#[derive(Clone)]
struct Disk(Rc<RefCell<State>>);
impl Disk {
    fn new() -> Self { Self::from_image(vec![[0; fs::SECTOR]; 2048]) }
    fn from_image(durable: Vec<Sector>) -> Self {
        Self(Rc::new(RefCell::new(State { durable, cache: BTreeMap::new(), events: vec![], cut: None, failure: Failure::Before, flip_stage_read: false })))
    }
    fn cut(&self, at: usize, failure: Failure) { let mut s = self.0.borrow_mut(); s.events.clear(); s.cut = Some(at); s.failure = failure; }
    fn image(&self) -> Vec<Sector> { self.0.borrow().durable.clone() }
    fn reboot(&self) -> Self { Self::from_image(self.image()) }
    fn events(&self) -> Vec<Event> { self.0.borrow().events.clone() }
}
impl BlockDevice for Disk {
    fn sectors(&self) -> u32 { self.0.borrow().durable.len() as u32 }
    fn read(&mut self, lba: u32, output: &mut Sector) -> Result<(), DiskError> {
        let s = self.0.borrow();
        *output = *s.cache.get(&lba).or_else(|| s.durable.get(lba as usize)).ok_or(DiskError::Device)?;
        if s.flip_stage_read && !s.events.is_empty() && lba == 8 { output[3] ^= 0x41; }
        Ok(())
    }
    fn write(&mut self, lba: u32, bytes: &Sector) -> Result<(), DiskError> {
        let mut s = self.0.borrow_mut(); let at = s.events.len(); s.events.push(Event::Write(lba));
        if lba as usize >= s.durable.len() { return Err(DiskError::Device); }
        if s.cut == Some(at) {
            match s.failure {
                Failure::Persist => s.durable[lba as usize] = *bytes,
                Failure::Prefix(n) => s.durable[lba as usize][..n].copy_from_slice(&bytes[..n]),
                _ => {},
            }
            return Err(DiskError::Timeout);
        }
        s.cache.insert(lba, *bytes); Ok(())
    }
    fn flush(&mut self) -> Result<(), DiskError> {
        let mut s = self.0.borrow_mut(); let at = s.events.len(); s.events.push(Event::Flush);
        let cut = s.cut == Some(at);
        let limit = if cut { match s.failure { Failure::Persist => usize::MAX, Failure::FlushSubset(n) => n, _ => 0 } } else { usize::MAX };
        let writes: Vec<_> = s.cache.iter().take(limit).map(|(&lba, &bytes)| (lba, bytes)).collect();
        for (lba, bytes) in writes { s.durable[lba as usize] = bytes; }
        if cut { return Err(DiskError::Timeout); }
        s.cache.clear(); Ok(())
    }
}
fn mounted(disk: Disk) -> FileSystem<Disk> { FileSystem::mount(disk).map_err(|e| e.0).unwrap() }
type View = BTreeMap<String, (u8, u32, Vec<u8>)>;
fn view(fs: &mut FileSystem<Disk>) -> View {
    let entries: Vec<(usize, Entry)> = fs.entries().map(|(slot, entry)| (slot, *entry)).collect();
    let mut result = BTreeMap::new(); let mut bytes = [0u8; fs::MAX_FILE_SIZE];
    for (slot, entry) in entries {
        let length = fs.read(slot, &mut bytes).unwrap();
        assert!(bytes[length..].iter().all(|&byte| byte == 0), "zero padding in slot {slot}");
        result.insert(entry.name().to_owned(), (entry.label, entry.generation, bytes[..length].to_vec()));
    }
    result
}
fn fixture() -> Vec<Sector> {
    let mut fs = FileSystem::format(Disk::new()).unwrap();
    let contents: Vec<_> = (0..1537).map(|i| ((i * 37 + 11) % 251) as u8).collect();
    fs.create("victim", 1, &contents).unwrap();
    // Seven entries share one table sector: a target-sector tear must not
    // lose neighbors with distinct labels, names, generations and data.
    for i in 1..7 { fs.create(&format!("neighbor{i}"), if i % 2 == 0 { 1 } else { 2 }, &[i as u8; 73]).unwrap(); }
    fs.into_device().image()
}
#[derive(Clone, Copy, Debug)]
enum Operation { Create, Overwrite, Append, Delete, Rename, Truncate, Extend, WriteAt }
fn apply(fs: &mut FileSystem<Disk>, op: Operation) -> Result<(), FsError> {
    let slot = fs.find("victim").unwrap().0;
    match op {
        Operation::Create => fs.create("new", 2, &[0x97; 3001]).map(|_| ()),
        Operation::Overwrite => fs.overwrite(slot, &[0x31; 4096]),
        Operation::Append => fs.append(slot, &[0x54; 1300]),
        Operation::Delete => fs.delete(slot), Operation::Rename => fs.rename(slot, "renamed"),
        Operation::Truncate => fs.truncate(slot, 513), Operation::Extend => fs.truncate(slot, 4096),
        Operation::WriteAt => fs.write_at(slot, 1499, &[0x63; 1801]),
    }
}
fn cut_old_or_new(op: Operation, failures: &[Failure]) {
    let image = fixture(); let old = view(&mut mounted(Disk::from_image(image.clone())));
    let completed_disk = Disk::from_image(image.clone()); let mut completed = mounted(completed_disk.clone());
    apply(&mut completed, op).unwrap(); let new = view(&mut completed); let events = completed_disk.events();
    assert_eq!(events.len(), 28, "bounded write/flush sequence");
    assert_eq!(view(&mut mounted(completed_disk.reboot())), new, "acknowledged operation is durable");
    for (at, event) in events.iter().enumerate() {
        for &failure in failures {
            let disk = Disk::from_image(image.clone()); let mut fs = mounted(disk.clone()); disk.cut(at, failure);
            assert_eq!(apply(&mut fs, op), Err(FsError::Disk(DiskError::Timeout)), "{op:?} {at} {failure:?}");
            let reboot = disk.reboot();
            match FileSystem::mount(reboot.clone()) {
                Ok(mut fs) => {
                    let recovered = view(&mut fs);
                    assert!(recovered == old || recovered == new, "mixture: {op:?} {at} {event:?} {failure:?}");
                    assert_eq!(view(&mut mounted(reboot.reboot())), recovered, "recovery is idempotent");
                }
                Err((error, returned)) => {
                    let torn_marker = matches!(event, Event::Write(lba) if *lba == fs::JOURNAL_LBA)
                        && matches!(failure, Failure::Prefix(n) if n > 0 && n < 512);
                    assert!(torn_marker && error == FsError::Corrupt, "unexpected failed mount: {op:?} {at} {event:?} {failure:?} {error:?}");
                    assert_eq!(returned.image(), reboot.image(), "invalid marker does not replay any byte");
                }
            }
        }
    }
}
#[test]
fn every_write_and_flush_failure_has_complete_old_or_new_files() {
    for op in [Operation::Create, Operation::Overwrite, Operation::Append, Operation::Delete,
        Operation::Rename, Operation::Truncate, Operation::Extend, Operation::WriteAt] {
        cut_old_or_new(op, &[Failure::Before, Failure::Persist, Failure::FlushSubset(1), Failure::FlushSubset(5)]);
    }
}
#[test]
fn torn_sector_writes_recover_or_reject_torn_markers() {
    for op in [Operation::Create, Operation::Overwrite, Operation::Append, Operation::Delete,
        Operation::Rename, Operation::Truncate, Operation::Extend, Operation::WriteAt] {
        cut_old_or_new(op, &[Failure::Prefix(1), Failure::Prefix(16), Failure::Prefix(128), Failure::Prefix(256), Failure::Prefix(511)]);
    }
}
fn committed_image() -> (Vec<Sector>, View) {
    let image = fixture(); let mut full = mounted(Disk::from_image(image.clone()));
    apply(&mut full, Operation::Overwrite).unwrap(); let new = view(&mut full);
    let disk = Disk::from_image(image); let mut fs = mounted(disk.clone()); disk.cut(15, Failure::Before);
    assert_eq!(apply(&mut fs, Operation::Overwrite), Err(FsError::Disk(DiskError::Timeout)));
    (disk.image(), new)
}
#[test]
fn recovery_can_crash_at_each_write_and_flush_and_resume_again() {
    let (image, expected) = committed_image();
    let disk = Disk::from_image(image.clone()); let mut recovered = mounted(disk.clone());
    assert_eq!(recovered.recovery(), Recovery::Replayed { slot: 0 }); assert_eq!(view(&mut recovered), expected);
    let count = disk.events().len(); assert_eq!(count, 13);
    for at in 0..count {
        for failure in [Failure::Before, Failure::Persist, Failure::FlushSubset(1), Failure::FlushSubset(5)] {
            let disk = Disk::from_image(image.clone()); disk.cut(at, failure);
            assert_eq!(FileSystem::mount(disk.clone()).err().map(|e| e.0), Some(FsError::Disk(DiskError::Timeout)));
            let mut fs = mounted(disk.reboot()); assert_eq!(view(&mut fs), expected, "recovery {at} {failure:?}");
        }
    }
}
#[test]
fn committed_journal_corruption_is_rejected_before_any_home_write() {
    let (image, _) = committed_image();
    for lba in fs::JOURNAL_LBA..fs::REQUIRED_SECTORS {
        for at in [0, 28, 255, 508, 511] {
            let mut damaged = image.clone(); damaged[lba as usize][at] ^= 0x81;
            let disk = Disk::from_image(damaged); let original = disk.image();
            assert_eq!(FileSystem::mount(disk.clone()).err().map(|e| e.0), Some(FsError::Corrupt), "LBA {lba} offset {at}");
            assert!(disk.events().is_empty()); assert_eq!(disk.image(), original);
        }
    }
}
#[test]
fn checksummed_journal_bounds_are_validated_before_replay() {
    let (image, _) = committed_image();
    for (at, value) in [(8, 3u32), (12, 32), (16, 9), (20, 2), (24, 9), (504, 0)] {
        let mut damaged = image.clone(); let header = &mut damaged[fs::JOURNAL_LBA as usize];
        header[at..at + 4].copy_from_slice(&value.to_le_bytes()); let sum = fs::checksum(&header[..508]);
        header[508..].copy_from_slice(&sum.to_le_bytes());
        let disk = Disk::from_image(damaged);
        assert_eq!(FileSystem::mount(disk.clone()).err().map(|e| e.0), Some(FsError::Corrupt)); assert!(disk.events().is_empty());
    }
}
#[test]
fn home_metadata_torn_outside_transaction_is_detected() {
    let image = fixture();
    for (lba, at) in [(1, 65), (2, 0), (5, 8), (5, 511)] {
        let mut damaged = image.clone(); damaged[lba][at] ^= 1;
        assert_eq!(FileSystem::mount(Disk::from_image(damaged)).err().map(|e| e.0), Some(FsError::Corrupt));
    }
}
#[test]
fn failed_live_mount_requires_remount_instead_of_serving_stale_table() {
    let disk = Disk::from_image(fixture()); let mut fs = mounted(disk.clone()); disk.cut(17, Failure::Before);
    assert_eq!(apply(&mut fs, Operation::Overwrite), Err(FsError::Disk(DiskError::Timeout)));
    let events = disk.events().len(); let mut bytes = [0; fs::MAX_FILE_SIZE];
    assert_eq!(fs.read(0, &mut bytes), Err(FsError::NeedsRecovery));
    assert_eq!(fs.append(0, b"later"), Err(FsError::NeedsRecovery)); assert_eq!(disk.events().len(), events);
}
#[test]
fn legacy_volume_preserves_files_without_automatic_migration() {
    let mut image = fixture(); let expected = view(&mut mounted(Disk::from_image(image.clone())));
    image.truncate(fs::LEGACY_REQUIRED_SECTORS as usize);
    let header = &mut image[0]; header.fill(0); header[..8].copy_from_slice(b"TANEFS1\0");
    for (at, value) in [(8, 1u32), (12, 32), (16, 8), (20, 8), (24, fs::LEGACY_REQUIRED_SECTORS)] {
        header[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    let sum = fs::checksum(&header[..28]); header[28..32].copy_from_slice(&sum.to_le_bytes());
    let disk = Disk::from_image(image); let mut fs = mounted(disk.clone());
    assert_eq!(fs.version(), 1); assert!(fs.read_only()); assert_eq!(view(&mut fs), expected);
    assert_eq!(fs.overwrite(0, b"change"), Err(FsError::ReadOnly)); assert_eq!(fs.create("new", 2, b""), Err(FsError::ReadOnly));
    assert_eq!(fs.delete(0), Err(FsError::ReadOnly)); assert!(disk.events().is_empty());
}
#[test]
fn destructive_format_crashes_never_mount_partial_new_tables() {
    let image = fixture(); let old = view(&mut mounted(Disk::from_image(image.clone())));
    let disk = Disk::from_image(image.clone()); FileSystem::format(disk.clone()).unwrap(); let count = disk.events().len();
    for at in 0..count {
        for failure in [Failure::Before, Failure::Persist, Failure::FlushSubset(1), Failure::FlushSubset(100)] {
            let disk = Disk::from_image(image.clone()); disk.cut(at, failure);
            assert_eq!(FileSystem::format(disk.clone()).err(), Some(FsError::Disk(DiskError::Timeout)));
            match FileSystem::mount(disk.reboot()) {
                Ok(mut fs) => { let actual = view(&mut fs); assert!(actual == old || actual.is_empty(), "partial format {at} {failure:?}"); }
                Err((error, _)) => assert!(matches!(error, FsError::NotFormatted | FsError::Corrupt)),
            }
        }
    }
}
#[test]
fn range_operations_preserve_labels_bounds_and_zero_extensions() {
    let mut fs = mounted(Disk::from_image(fixture())); let (slot, original) = fs.find("victim").unwrap();
    fs.write_at(slot, 1600, b"XYZ").unwrap(); let mut bytes = [0; fs::MAX_FILE_SIZE];
    assert_eq!(fs.read(slot, &mut bytes).unwrap(), 1603); assert!(bytes[1537..1600].iter().all(|&b| b == 0));
    assert_eq!(&bytes[1600..1603], b"XYZ"); let mut short = [0; 11];
    assert_eq!(fs.read_at(slot, 1595, &mut short).unwrap(), 8); assert_eq!(&short[..8], b"\0\0\0\0\0XYZ");
    assert_eq!(fs.read_at(slot, usize::MAX, &mut short).unwrap(), 0);
    fs.truncate(slot, 7).unwrap(); fs.truncate(slot, 20).unwrap(); fs.read(slot, &mut bytes).unwrap();
    assert!(bytes[7..].iter().all(|&b| b == 0));
    fs.rename(slot, "new-name").unwrap(); assert!(fs.find("victim").is_none());
    let changed = fs.find("new-name").unwrap().1; assert_eq!(changed.label, original.label);
    assert_eq!(fs.rename(slot, "neighbor1"), Err(FsError::Exists));
    assert_eq!(fs.rename(slot, "bad/name"), Err(FsError::BadName));
    assert_eq!(fs.write_at(slot, usize::MAX, b"x"), Err(FsError::TooLarge)); assert_eq!(fs.truncate(slot, 4097), Err(FsError::TooLarge));
}
#[test]
fn short_reads_detect_corruption_outside_requested_range() {
    let mut image = fixture(); image[10][3] ^= 0x10;
    let mut fs = mounted(Disk::from_image(image)); let mut bytes = [0; 1];
    assert_eq!(fs.read_at(0, 0, &mut bytes), Err(FsError::Corrupt));
    assert_eq!(fs.append(0, b"x"), Err(FsError::Corrupt)); assert_eq!(fs.rename(0, "bad-data"), Err(FsError::Corrupt));
}

#[test]
fn source_changed_between_verification_and_staging_is_not_blessed() {
    let image = fixture(); let old = view(&mut mounted(Disk::from_image(image.clone())));
    for op in [Operation::Append, Operation::Rename, Operation::Truncate, Operation::Extend, Operation::WriteAt] {
        let disk = Disk::from_image(image.clone()); let mut fs = mounted(disk.clone());
        disk.0.borrow_mut().flip_stage_read = true;
        assert_eq!(apply(&mut fs, op), Err(FsError::Corrupt), "{op:?}");
        assert_eq!(view(&mut mounted(disk.reboot())), old);
    }
}
#[test]
fn torn_format_header_and_data_writes_never_publish_partial_format() {
    let image = fixture(); let old = view(&mut mounted(Disk::from_image(image.clone())));
    let disk = Disk::from_image(image.clone()); FileSystem::format(disk.clone()).unwrap(); let events = disk.events();
    for (at, event) in events.iter().enumerate() {
        if !matches!(event, Event::Write(_)) { continue; }
        for n in [1, 128, 256, 511] {
            let disk = Disk::from_image(image.clone()); disk.cut(at, Failure::Prefix(n));
            assert_eq!(FileSystem::format(disk.clone()).err(), Some(FsError::Disk(DiskError::Timeout)));
            match FileSystem::mount(disk.reboot()) {
                Ok(mut fs) => { let actual = view(&mut fs); assert!(actual == old || actual.is_empty()); }
                Err((error, _)) => assert!(matches!(error, FsError::NotFormatted | FsError::Corrupt)),
            }
        }
    }
}
#[test]
fn recovery_torn_home_sectors_replay_but_torn_clear_marker_fails_closed() {
    let (image, expected) = committed_image();
    let disk = Disk::from_image(image.clone()); mounted(disk.clone()); let events = disk.events();
    for (at, event) in events.iter().enumerate() {
        if !matches!(event, Event::Write(_)) { continue; }
        for n in [1, 128, 256, 511] {
            let disk = Disk::from_image(image.clone()); disk.cut(at, Failure::Prefix(n));
            assert_eq!(FileSystem::mount(disk.clone()).err().map(|e| e.0), Some(FsError::Disk(DiskError::Timeout)));
            match FileSystem::mount(disk.reboot()) {
                Ok(mut fs) => assert_eq!(view(&mut fs), expected),
                Err((error, _)) => assert!(matches!(event, Event::Write(lba) if *lba == fs::JOURNAL_LBA) && error == FsError::Corrupt),
            }
        }
    }
}
#[test]
fn explicit_sync_failures_poison_the_mount() {
    let disk = Disk::from_image(fixture()); let mut fs = mounted(disk.clone()); disk.cut(0, Failure::Before);
    assert_eq!(fs.sync(), Err(FsError::Disk(DiskError::Timeout)));
    assert_eq!(fs.sync(), Err(FsError::NeedsRecovery)); assert_eq!(fs.overwrite(0, b"no"), Err(FsError::NeedsRecovery));
    assert!(fs.device().sectors() >= fs::REQUIRED_SECTORS); assert!(!FsError::ReadOnly.message().is_empty());
}
fn legacy_image(expanded: bool) -> Vec<Sector> {
    let mut image = fixture();
    if !expanded { image.truncate(fs::LEGACY_REQUIRED_SECTORS as usize); }
    let header = &mut image[0]; header.fill(0); header[..8].copy_from_slice(b"TANEFS1\0");
    // Simulate an old volume whose underlying disk has subsequently grown.
    for (at, value) in [(8, 1u32), (12, 32), (16, 8), (20, 8), (24, fs::LEGACY_REQUIRED_SECTORS)] {
        header[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    let sum = fs::checksum(&header[..28]); header[28..32].copy_from_slice(&sum.to_le_bytes()); image
}
#[test]
fn explicit_upgrade_is_durable_and_preserves_every_file_byte_and_label() {
    let image = legacy_image(true); let disk = Disk::from_image(image.clone()); let mut fs = mounted(disk.clone());
    let expected = view(&mut fs); assert!(fs.read_only()); fs.upgrade().unwrap();
    assert_eq!(fs.version(), 2); assert!(!fs.read_only()); assert_eq!(view(&mut fs), expected);
    let after = disk.image(); assert_eq!(&after[1..5], &image[1..5]); assert_eq!(&after[6..264], &image[6..264]);
    assert_eq!(u32::from_le_bytes(after[0][24..28].try_into().unwrap()), 2048);
    let mut fs = mounted(disk.reboot()); assert_eq!(view(&mut fs), expected);
    fs.append(0, b"upgraded").unwrap(); assert!(view(&mut fs)["victim"].2.ends_with(b"upgraded"));
    let disk = fs.into_device(); let events = disk.events().len(); let mut fs = mounted(disk.clone());
    fs.upgrade().unwrap(); assert_eq!(disk.events().len(), events, "v2 upgrade is a no-op");
}
#[test]
fn every_upgrade_failure_preserves_home_files_and_old_or_new_mount() {
    let image = legacy_image(true); let expected = view(&mut mounted(Disk::from_image(image.clone())));
    let disk = Disk::from_image(image.clone()); mounted(disk.clone()).upgrade().unwrap(); let events = disk.events(); assert_eq!(events.len(), 6);
    for (at, event) in events.iter().enumerate() {
        for failure in [Failure::Before, Failure::Persist, Failure::FlushSubset(1), Failure::Prefix(1), Failure::Prefix(128), Failure::Prefix(511)] {
            let disk = Disk::from_image(image.clone()); let mut fs = mounted(disk.clone()); disk.cut(at, failure);
            assert_eq!(fs.upgrade(), Err(FsError::Disk(DiskError::Timeout)));
            assert_eq!(fs.upgrade(), Err(FsError::NeedsRecovery));
            let after = disk.image(); assert_eq!(&after[1..5], &image[1..5]); assert_eq!(&after[6..264], &image[6..264]);
            match FileSystem::mount(disk.reboot()) {
                Ok(mut fs) => { assert!(fs.version() == 1 || fs.version() == 2); assert_eq!(view(&mut fs), expected); }
                Err((error, _)) => assert!(matches!(event, Event::Write(0)) && matches!(failure, Failure::Prefix(_))
                    && matches!(error, FsError::NotFormatted | FsError::Corrupt)),
            }
        }
    }
}
#[test]
fn upgrade_preflight_refuses_small_or_damaged_volumes_without_writes() {
    let small = Disk::from_image(legacy_image(false)); let mut fs = mounted(small.clone());
    assert_eq!(fs.upgrade(), Err(FsError::TooSmall)); assert!(small.events().is_empty());
    let mut image = legacy_image(true); image[8][1] ^= 1;
    let disk = Disk::from_image(image); let mut fs = mounted(disk.clone());
    assert_eq!(fs.upgrade(), Err(FsError::Corrupt)); assert!(disk.events().is_empty());
    let disk = Disk::from_image(legacy_image(true)); let mut fs = mounted(disk.clone());
    disk.0.borrow_mut().durable[1][1] ^= 1;
    assert_eq!(fs.upgrade(), Err(FsError::Corrupt)); assert!(disk.events().is_empty());
}
