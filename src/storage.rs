//! Storage: the ATA disk, TaneFS on it, and the access checks for files.
//! Every operation that names a file asks `security` first, with the label
//! stored in that file's entry; new files take the creator's domain and
//! count against its file limit. Only the shell task uses storage, so the
//! disk itself needs no lock.

use crate::ata::Ata;
use crate::fs::{FileSystem, FsError, MAX_FILE_SIZE};
use crate::mac::{self, Domain, Op};
pub use crate::plans::{PlannedKind, Snapshot};
use crate::security::{self, Denied};
use crate::tasks;
use core::ptr::addr_of_mut;

enum Disk {
    Absent,
    Unformatted(Ata, FsError),
    Mounted(FileSystem<Ata>),
}

static mut DISK: Disk = Disk::Absent;
// Only the shell task changes storage. This boot-local stamp covers every
// mount and attempted mutation, including partial failures and slot reuse.
// It deliberately also invalidates a plan when an unrelated file changes.
static mut REVISION: u64 = 0;

pub enum StorageError {
    NoDisk,
    Denied(Denied),
    Fs(FsError),
}

impl StorageError {
    /// A mutating operation failed while accessing the disk. Some sectors
    /// may already have changed; no rollback or atomic commit is promised.
    pub fn commit_unknown(&self) -> bool {
        matches!(self, StorageError::Fs(FsError::Disk(_)))
    }
}

pub fn revision() -> u64 {
    unsafe { *addr_of_mut!(REVISION) }
}

fn advance_revision() {
    unsafe {
        let revision = &mut *addr_of_mut!(REVISION);
        // Saturation never makes plans reusable: plans reject this stamp.
        *revision = revision.saturating_add(1);
    }
}

fn invalidate_failed_write(error: FsError) {
    if matches!(error, FsError::Disk(_)) {
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
pub fn init() {
    advance_revision();
    *disk() = match Ata::detect() {
        None => Disk::Absent,
        Some(ata) => match FileSystem::mount(ata) {
            Ok(fs) => Disk::Mounted(fs),
            Err((error, ata)) => Disk::Unformatted(ata, error),
        },
    };
}

pub enum Status<'a> {
    Absent,
    Unformatted { model: &'a str, sectors: u32, error: FsError },
    Mounted { model: &'a str, sectors: u32, files: usize },
}

pub fn status() -> Status<'static> {
    use crate::fs::BlockDevice;
    match disk() {
        Disk::Absent => Status::Absent,
        Disk::Unformatted(ata, error) => Status::Unformatted { model: ata.model(), sectors: ata.sectors(), error: *error },
        Disk::Mounted(fs) => {
            let files = fs.entries().count();
            let ata = fs.device();
            Status::Mounted { model: ata.model(), sectors: ata.sectors(), files }
        }
    }
}

/// Erase the disk and write an empty TaneFS (admin only).
pub fn format() -> Result<(), StorageError> {
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
pub fn list(mut each: impl FnMut(&Listing)) -> Result<(), StorageError> {
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

pub fn read(name: &str, buffer: &mut [u8; MAX_FILE_SIZE]) -> Result<usize, StorageError> {
    let fs = mounted()?;
    let (slot, entry) = fs.find(name).ok_or(FsError::NotFound)?;
    security::check(Op::Read, Some(label(entry.label)), Some(slot as u64))?;
    Ok(fs.read(slot, buffer)?)
}

/// Obtain a checked, policy-filtered target identity without changing disk.
/// A failed read is an unavailable target, never an absent target. Existing
/// data is checksum-verified before its metadata is used for a plan.
pub fn snapshot(name: &str) -> Result<Snapshot, StorageError> {
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
pub fn preflight(name: &str, kind: PlannedKind, payload_len: usize) -> Result<(), StorageError> {
    if !crate::fs::valid_name(name.as_bytes()) {
        return Err(FsError::BadName.into());
    }
    let fs = mounted()?;
    match (kind, fs.find(name)) {
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
    }
}

/// Write (or with `append`, extend) a file, creating it if needed.
pub fn write(name: &str, data: &[u8], append: bool) -> Result<bool, StorageError> {
    let fs = mounted()?;
    let result = match fs.find(name) {
        Some((slot, entry)) => {
            security::check(Op::Write, Some(label(entry.label)), Some(slot as u64))?;
            advance_revision();
            if append { fs.append(slot, data) } else { fs.overwrite(slot, data) }.map(|_| false)
        }
        None => {
            security::check(Op::Create, None, None)?;
            let subject = tasks::current_domain();
            security::check_file_quota(fs.count_label(subject.index() as u8))?;
            advance_revision();
            fs.create(name, subject.index() as u8, data).map(|_| true)
        }
    };
    if let Err(error) = result { invalidate_failed_write(error); }
    result.map_err(StorageError::from)
}

pub fn remove(name: &str) -> Result<(), StorageError> {
    let fs = mounted()?;
    let (slot, entry) = fs.find(name).ok_or(FsError::NotFound)?;
    security::check(Op::Delete, Some(label(entry.label)), Some(slot as u64))?;
    advance_revision();
    let result = fs.delete(slot);
    if let Err(error) = result { invalidate_failed_write(error); }
    result.map_err(StorageError::from)
}

/// Files labelled `domain`, or `None` without a mounted disk.
pub fn files_owned(domain: Domain) -> Option<u32> {
    match disk() {
        Disk::Mounted(fs) => Some(fs.count_label(domain.index() as u8)),
        _ => None,
    }
}
