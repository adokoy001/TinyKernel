//! Storage: the ATA disk, TaneFS on it, and the access checks for files.
//! Every operation that names a file asks `security` first, with the label
//! stored in that file's entry; new files take the creator's domain and
//! count against its file limit. All public operations run with interrupts
//! disabled, serializing the single ATA controller and filesystem against
//! preempted shell calls and user syscalls. Callbacks must never yield.

use crate::ata::Ata;
use crate::fs::{FileSystem, FsError, MAX_FILE_SIZE};
use crate::mac::{self, Domain, Op};
pub use crate::plans::{PlannedKind, Snapshot};
use crate::security::{self, Denied};
use crate::{interrupts, tasks};
use crate::handles::Identity;
use core::ptr::addr_of_mut;

enum Disk {
    Absent,
    Unformatted(Ata, FsError),
    Mounted(FileSystem<Ata>),
}

static mut DISK: Disk = Disk::Absent;
// Interrupts serialize storage changes. This boot-local stamp covers every
// mount and attempted mutation, including partial failures and slot reuse.
// It deliberately also invalidates a plan when an unrelated file changes.
static mut REVISION: u64 = 0;
// Boot-local per-slot mutation epochs distinguish deletion/recreation even
// if the on-disk 32-bit generation wraps or starts again at one.
static mut EPOCHS: [u64; crate::fs::MAX_FILES] = [0; crate::fs::MAX_FILES];

pub enum StorageError {
    NoDisk,
    Denied(Denied),
    Fs(FsError),
    Stale,
    IdentityExhausted,
}

impl StorageError {
    /// A mutating operation failed while accessing the disk. Some sectors
    /// may have crossed the durable journal commit point. The caller cannot
    /// distinguish old/new from this error; remount completes a valid journal.
    pub fn commit_unknown(&self) -> bool {
        matches!(self, StorageError::Fs(FsError::Disk(_)))
    }
}

pub fn revision() -> u64 {
    interrupts::without(|| unsafe { *addr_of_mut!(REVISION) })
}

fn advance_revision() {
    unsafe {
        let revision = &mut *addr_of_mut!(REVISION);
        // Saturation never makes plans reusable: plans reject this stamp.
        *revision = revision.saturating_add(1);
    }
}

fn invalidate_failed_write(error: FsError) {
    if matches!(error, FsError::Disk(_) | FsError::NeedsRecovery) {
        let old = core::mem::replace(disk(), Disk::Absent);
        *disk() = match old {
            Disk::Mounted(fs) => Disk::Unformatted(fs.into_device(), error),
            other => other,
        };
    }
}

impl From<FsError> for StorageError {
    fn from(error: FsError) -> Self {
        StorageError::Fs(error)
    }
}

impl From<Denied> for StorageError {
    fn from(denied: Denied) -> Self {
        StorageError::Denied(denied)
    }
}

fn disk() -> &'static mut Disk {
    unsafe { &mut *addr_of_mut!(DISK) }
}

fn mounted() -> Result<&'static mut FileSystem<Ata>, StorageError> {
    match disk() {
        Disk::Mounted(fs) => Ok(fs),
        Disk::Unformatted(_, error) => Err(StorageError::Fs(*error)),
        Disk::Absent => Err(StorageError::NoDisk),
    }
}

/// The MAC label a file entry stores, as a domain. Unknown values read as
/// kernel, which no rule allows.
fn label(stored: u8) -> Domain {
    match stored {
        1 => Domain::Admin,
        2 => Domain::User,
        _ => Domain::Kernel,
    }
}

/// Probe the disk and mount TaneFS if it is there.
fn init_locked() {
    advance_revision();
    *disk() = match Ata::detect() {
        None => Disk::Absent,
        Some(ata) => match FileSystem::mount(ata) {
            Ok(fs) => Disk::Mounted(fs),
            Err((error, ata)) => Disk::Unformatted(ata, error),
        },
    };
}

/// ATA identification copied out of the critical section; no disk borrow
/// escapes while another task may format or replace the filesystem.
#[derive(Clone, Copy)]
pub struct Model([u8; 40]);
impl Model {
    pub fn as_str(&self) -> &str { core::str::from_utf8(&self.0).unwrap_or("?").trim_end() }
}
impl core::fmt::Display for Model {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.write_str(self.as_str()) }
}
pub enum Status {
    Absent,
    Unformatted { model: Model, sectors: u32, error: FsError },
    Mounted { model: Model, sectors: u32, files: usize, version: u32, read_only: bool, replayed: bool },
}

fn status_locked() -> Status {
    use crate::fs::BlockDevice;
    match disk() {
        Disk::Absent => Status::Absent,
        Disk::Unformatted(ata, error) => Status::Unformatted { model: Model(ata.model), sectors: ata.sectors(), error: *error },
        Disk::Mounted(fs) => {
            let files = fs.entries().count();
            let ata = fs.device();
            Status::Mounted { model: Model(ata.model), sectors: ata.sectors(), files,
                version: fs.version(), read_only: fs.read_only(),
                replayed: matches!(fs.recovery(), crate::fs::Recovery::Replayed { .. }) }
        }
    }
}

/// Erase the disk and write an empty TaneFS (admin only).
fn format_locked() -> Result<(), StorageError> {
    security::check(Op::Format, None, None)?;
    let ata = match core::mem::replace(disk(), Disk::Absent) {
        Disk::Absent => return Err(StorageError::NoDisk),
        Disk::Unformatted(ata, _) => ata,
        Disk::Mounted(fs) => fs.into_device(),
    };
    advance_revision();
    match FileSystem::format(ata) {
        Ok(fs) => {
            *disk() = Disk::Mounted(fs);
            Ok(())
        }
        Err(error) => {
            // The drive stays known but unusable until a format succeeds.
            *disk() = match Ata::detect() {
                Some(ata) => Disk::Unformatted(ata, error),
                None => Disk::Absent,
            };
            Err(error.into())
        }
    }
}

pub struct Listing<'a> {
    pub slot: usize,
    pub name: &'a str,
    pub label: Domain,
    pub size: u32,
    pub generation: u32,
}

/// Files the current domain may read. Others are not shown (and, since
/// nothing was requested of them, not audited).
fn list_locked(mut each: impl FnMut(&Listing)) -> Result<(), StorageError> {
    let fs = mounted()?;
    let subject = tasks::current_domain();
    for (slot, entry) in fs.entries() {
        let label = label(entry.label);
        if mac::allowed(subject, Op::Read, Some(label)) {
            each(&Listing { slot, name: entry.name(), label, size: entry.size, generation: entry.generation });
        }
    }
    Ok(())
}

fn read_locked(name: &str, buffer: &mut [u8; MAX_FILE_SIZE]) -> Result<usize, StorageError> {
    let fs = mounted()?;
    let (slot, entry) = fs.find(name).ok_or(FsError::NotFound)?;
    security::check(Op::Read, Some(label(entry.label)), Some(slot as u64))?;
    Ok(fs.read(slot, buffer)?)
}

/// Obtain a checked, policy-filtered target identity without changing disk.
/// A failed read is an unavailable target, never an absent target. Existing
/// data is checksum-verified before its metadata is used for a plan.
fn snapshot_locked(name: &str) -> Result<Snapshot, StorageError> {
    if !crate::fs::valid_name(name.as_bytes()) {
        return Err(FsError::BadName.into());
    }
    let fs = mounted()?;
    match fs.find(name) {
        None => Ok(Snapshot::Absent),
        Some((slot, entry)) => {
            let domain = label(entry.label);
            security::check(Op::Read, Some(domain), Some(slot as u64))?;
            let mut buffer = [0u8; MAX_FILE_SIZE];
            fs.read(slot, &mut buffer)?;
            Ok(Snapshot::Exists { slot, label: domain, size: entry.size,
                generation: entry.generation, checksum: entry.checksum })
        }
    }
}

/// Check the exact planned operation and available capacity without writes.
/// Apply still calls the ordinary write/remove gate after this preflight.
fn preflight_locked(name: &str, kind: PlannedKind, payload_len: usize) -> Result<(), StorageError> {
    if !crate::fs::valid_name(name.as_bytes()) {
        return Err(FsError::BadName.into());
    }
    let fs = mounted()?;
    let result: Result<(), StorageError> = match (kind, fs.find(name)) {
        (PlannedKind::Remove, None) => Err(FsError::NotFound.into()),
        (PlannedKind::Remove, Some((slot, entry))) => {
            security::check(Op::Delete, Some(label(entry.label)), Some(slot as u64))?;
            Ok(())
        }
        (PlannedKind::Write | PlannedKind::Append, Some((slot, entry))) => {
            security::check(Op::Write, Some(label(entry.label)), Some(slot as u64))?;
            let before = if kind == PlannedKind::Append { entry.size as usize } else { 0 };
            if payload_len > MAX_FILE_SIZE || before > MAX_FILE_SIZE - payload_len {
                return Err(FsError::TooLarge.into());
            }
            Ok(())
        }
        (PlannedKind::Write | PlannedKind::Append, None) => {
            security::check(Op::Create, None, None)?;
            let subject = tasks::current_domain();
            security::check_file_quota(fs.count_label(subject.index() as u8))?;
            if payload_len > MAX_FILE_SIZE { return Err(FsError::TooLarge.into()); }
            if fs.entries().count() == crate::fs::MAX_FILES { return Err(FsError::Full.into()); }
            Ok(())
        }
    };
    result?;
    if fs.read_only() { Err(FsError::ReadOnly.into()) } else { Ok(()) }
}

/// Write (or with `append`, extend) a file, creating it if needed.
fn write_locked(name: &str, data: &[u8], append: bool) -> Result<bool, StorageError> {
    let fs = mounted()?;
    let result = match fs.find(name) {
        Some((slot, entry)) => {
            security::check(Op::Write, Some(label(entry.label)), Some(slot as u64))?;
            advance_revision();
            stamp(slot);
            if append { fs.append(slot, data) } else { fs.overwrite(slot, data) }.map(|_| false)
        }
        None => {
            security::check(Op::Create, None, None)?;
            let subject = tasks::current_domain();
            security::check_file_quota(fs.count_label(subject.index() as u8))?;
            advance_revision();
            fs.create(name, subject.index() as u8, data).map(|slot| { stamp(slot); true })
        }
    };
    if let Err(error) = result { invalidate_failed_write(error); }
    result.map_err(StorageError::from)
}

fn remove_locked(name: &str) -> Result<(), StorageError> {
    let fs = mounted()?;
    let (slot, entry) = fs.find(name).ok_or(FsError::NotFound)?;
    security::check(Op::Delete, Some(label(entry.label)), Some(slot as u64))?;
    advance_revision();
    stamp(slot);
    let result = fs.delete(slot);
    if let Err(error) = result { invalidate_failed_write(error); }
    result.map_err(StorageError::from)
}

/// Files labelled `domain`, or `None` without a mounted disk.
fn files_owned_locked(domain: Domain) -> Option<u32> {
    match disk() {
        Disk::Mounted(fs) => Some(fs.count_label(domain.index() as u8)),
        _ => None,
    }
}


fn stamp(slot: usize) {
    unsafe { (*addr_of_mut!(EPOCHS))[slot] = *addr_of_mut!(REVISION); }
}

// The *_locked helpers never escape a mutable filesystem reference.
// Nesting interrupts::without is safe: each call restores its prior IF.
pub fn init() { interrupts::without(init_locked) }
pub fn status() -> Status { interrupts::without(status_locked) }
pub fn format() -> Result<(), StorageError> { interrupts::without(format_locked) }
pub fn list(each: impl FnMut(&Listing)) -> Result<(), StorageError> { interrupts::without(|| list_locked(each)) }
pub fn read(name: &str, buffer: &mut [u8; MAX_FILE_SIZE]) -> Result<usize, StorageError> { interrupts::without(|| read_locked(name, buffer)) }
pub fn snapshot(name: &str) -> Result<Snapshot, StorageError> { interrupts::without(|| snapshot_locked(name)) }
pub fn preflight(name: &str, kind: PlannedKind, payload_len: usize) -> Result<(), StorageError> { interrupts::without(|| preflight_locked(name, kind, payload_len)) }
pub fn write(name: &str, data: &[u8], append: bool) -> Result<bool, StorageError> { interrupts::without(|| write_locked(name, data, append)) }
pub fn remove(name: &str) -> Result<(), StorageError> { interrupts::without(|| remove_locked(name)) }
pub fn files_owned(domain: Domain) -> Option<u32> { interrupts::without(|| files_owned_locked(domain)) }

/// Verify only files this subject can read. Hidden files contribute neither
/// names nor counts; the checksum is checked before any successful report.
pub fn check() -> Result<(usize, u64), StorageError> {
    interrupts::without(|| {
        let fs = mounted()?;
        let subject = tasks::current_domain();
        let mut buffer = [0u8; MAX_FILE_SIZE];
        let mut files = 0;
        let mut bytes = 0u64;
        for slot in 0..crate::fs::MAX_FILES {
            let entry = fs.entries().find(|(index, _)| *index == slot).map(|(_, entry)| *entry);
            if let Some(entry) = entry {
                if !mac::allowed(subject, Op::Read, Some(label(entry.label))) { continue; }
                security::check(Op::Read, Some(label(entry.label)), Some(slot as u64))?;
                bytes += fs.read(slot, &mut buffer)? as u64;
                files += 1;
            }
        }
        Ok((files, bytes))
    })
}
pub fn sync() -> Result<(), StorageError> {
    interrupts::without(|| {
        security::check(Op::Sync, None, None)?;
        let result = mounted()?.sync();
        if let Err(error) = result { invalidate_failed_write(error); }
        result.map_err(StorageError::from)
    })
}
pub fn upgrade() -> Result<(), StorageError> {
    interrupts::without(|| {
        security::check(Op::Format, None, None)?;
        let fs = mounted()?;
        if !fs.read_only() { return Ok(()); }
        advance_revision();
        let result = fs.upgrade();
        if let Err(error) = result { invalidate_failed_write(error); }
        result.map_err(StorageError::from)
    })
}
pub fn rename(old: &str, new: &str) -> Result<(), StorageError> {
    interrupts::without(|| {
        if !crate::fs::valid_name(old.as_bytes()) || !crate::fs::valid_name(new.as_bytes()) {
            return Err(FsError::BadName.into());
        }
        let fs = mounted()?;
        let (slot, entry) = fs.find(old).ok_or(FsError::NotFound)?;
        security::check(Op::Write, Some(label(entry.label)), Some(slot as u64))?;
        if old != new && fs.find(new).is_some() { return Err(FsError::Exists.into()); }
        if old == new { return Ok(()); }
        advance_revision(); stamp(slot);
        let result = fs.rename(slot, new);
        if let Err(error) = result { invalidate_failed_write(error); }
        result.map_err(StorageError::from)
    })
}
pub fn truncate(name: &str, size: usize) -> Result<(), StorageError> {
    interrupts::without(|| {
        if size > MAX_FILE_SIZE { return Err(FsError::TooLarge.into()); }
        let fs = mounted()?;
        let (slot, entry) = fs.find(name).ok_or(FsError::NotFound)?;
        security::check(Op::Write, Some(label(entry.label)), Some(slot as u64))?;
        if size == entry.size as usize { return Ok(()); }
        advance_revision(); stamp(slot);
        let result = fs.truncate(slot, size);
        if let Err(error) = result { invalidate_failed_write(error); }
        result.map_err(StorageError::from)
    })
}

/// Executable bytes must be readable by both the creator and the child
/// domain. An Admin-only file cannot become public by spawning a User task.
pub fn read_for_domain(destination: Domain, name: &str, buffer: &mut [u8; MAX_FILE_SIZE]) -> Result<usize, StorageError> {
    interrupts::without(|| {
        let fs = mounted()?;
        let (slot, entry) = fs.find(name).ok_or(FsError::NotFound)?;
        let object = Some(label(entry.label));
        security::check(Op::Read, object, Some(slot as u64))?;
        security::check_domain(destination, Op::Read, object, Some(slot as u64))?;
        Ok(fs.read(slot, buffer)?)
    })
}

fn identity_locked(name: &str) -> Result<Identity, StorageError> {
    if revision() == u64::MAX { return Err(StorageError::IdentityExhausted); }
    let fs = mounted()?;
    let (slot, entry) = fs.find(name).ok_or(FsError::NotFound)?;
    Ok(Identity { slot: slot as u8, label: entry.label, size: entry.size,
        generation: entry.generation, checksum: entry.checksum,
        epoch: unsafe { (*addr_of_mut!(EPOCHS))[slot] } })
}

/// Open performs MAC/quota checks before creating a missing WRITE file.
/// It never truncates an existing file. The returned identity is owned.
pub fn open_capability(name: &str, rights: u8) -> Result<Identity, StorageError> {
    interrupts::without(|| {
        if !crate::fs::valid_name(name.as_bytes()) { return Err(FsError::BadName.into()); }
        if !crate::handles::valid_rights(rights) { return Err(FsError::BadName.into()); }
        if revision() == u64::MAX { return Err(StorageError::IdentityExhausted); }
        let fs = mounted()?;
        match fs.find(name) {
            Some((slot, entry)) => {
                if rights & crate::handles::READ != 0 { security::check(Op::Read, Some(label(entry.label)), Some(slot as u64))?; }
                if rights & crate::handles::WRITE != 0 { security::check(Op::Write, Some(label(entry.label)), Some(slot as u64))?; }
            }
            None => {
                if rights & crate::handles::WRITE == 0 { return Err(FsError::NotFound.into()); }
                if revision() >= u64::MAX - 1 { return Err(StorageError::IdentityExhausted); }
                write_locked(name, &[], false)?;
            }
        }
        identity_locked(name)
    })
}

/// Rechecks the immutable handle's target and MAC on each use. The caller
/// has already validated a complete writable user span before disk I/O.
pub fn read_capability(name: &str, expected: Identity, buffer: &mut [u8; MAX_FILE_SIZE]) -> Result<usize, StorageError> {
    interrupts::without(|| {
        let actual = identity_locked(name).map_err(|error| if matches!(error, StorageError::Fs(FsError::NotFound)) { StorageError::Stale } else { error })?;
        if actual != expected { return Err(StorageError::Stale); }
        read_locked(name, buffer)
    })
}

/// Writes append at EOF and return a new identity to this one handle.
/// Other handles to this target become stale, including after slot reuse.
pub fn append_capability(name: &str, expected: Identity, data: &[u8]) -> Result<Identity, StorageError> {
    interrupts::without(|| {
        let actual = identity_locked(name).map_err(|error| if matches!(error, StorageError::Fs(FsError::NotFound)) { StorageError::Stale } else { error })?;
        if actual != expected { return Err(StorageError::Stale); }
        preflight_locked(name, PlannedKind::Append, data.len())?;
        if data.is_empty() { return Ok(actual); }
        if revision() >= u64::MAX - 1 { return Err(StorageError::IdentityExhausted); }
        write_locked(name, data, true)?;
        identity_locked(name)
    })
}

fn pinned_locked(name: &str, expected: Identity, op: Op) -> Result<Identity, StorageError> {
    let actual = identity_locked(name).map_err(|error|
        if matches!(error, StorageError::Fs(FsError::NotFound)) { StorageError::Stale } else { error })?;
    if actual != expected { return Err(StorageError::Stale); }
    security::check(op, Some(label(actual.label)), Some(actual.slot as u64))?;
    Ok(actual)
}
pub fn seek_capability(name: &str, expected: Identity, op: Op) -> Result<(), StorageError> {
    interrupts::without(|| pinned_locked(name, expected, op).map(|_| ()))
}
pub fn read_capability_at(name: &str, expected: Identity, offset: usize, buffer: &mut [u8]) -> Result<usize, StorageError> {
    interrupts::without(|| {
        let actual = pinned_locked(name, expected, Op::Read)?;
        Ok(mounted()?.read_at(actual.slot as usize, offset, buffer)?)
    })
}
pub fn write_capability_at(name: &str, expected: Identity, offset: usize, data: &[u8]) -> Result<Identity, StorageError> {
    interrupts::without(|| {
        if offset.checked_add(data.len()).filter(|&end| end <= MAX_FILE_SIZE).is_none() {
            return Err(FsError::TooLarge.into());
        }
        let actual = pinned_locked(name, expected, Op::Write)?;
        if data.is_empty() { return Ok(actual); }
        if revision() >= u64::MAX - 1 { return Err(StorageError::IdentityExhausted); }
        advance_revision(); stamp(actual.slot as usize);
        let result = mounted()?.write_at(actual.slot as usize, offset, data);
        if let Err(error) = result { invalidate_failed_write(error); }
        result?;
        identity_locked(name)
    })
}
pub fn truncate_capability(name: &str, expected: Identity, size: usize) -> Result<Identity, StorageError> {
    interrupts::without(|| {
        if size > MAX_FILE_SIZE { return Err(FsError::TooLarge.into()); }
        let actual = pinned_locked(name, expected, Op::Write)?;
        if size == actual.size as usize { return Ok(actual); }
        if revision() >= u64::MAX - 1 { return Err(StorageError::IdentityExhausted); }
        advance_revision(); stamp(actual.slot as usize);
        let result = mounted()?.truncate(actual.slot as usize, size);
        if let Err(error) = result { invalidate_failed_write(error); }
        result?;
        identity_locked(name)
    })
}
