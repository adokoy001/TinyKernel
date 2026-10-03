//! Storage: the ATA disk, TaneFS on it, and the access checks for files.
//! Every operation that names a file asks `security` first, with the label
//! stored in that file's entry; new files take the creator's domain and
//! count against its file limit. Only the shell task uses storage, so the
//! disk itself needs no lock.

use crate::ata::Ata;
use crate::fs::{FileSystem, FsError, MAX_FILE_SIZE};
use crate::mac::{self, Domain, Op};
use crate::security::{self, Denied};
use crate::tasks;
use core::ptr::addr_of_mut;

enum Disk {
    Absent,
    Unformatted(Ata, FsError),
    Mounted(FileSystem<Ata>),
}

static mut DISK: Disk = Disk::Absent;

pub enum StorageError {
    NoDisk,
    Denied(Denied),
    Fs(FsError),
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

/// Write (or with `append`, extend) a file, creating it if needed.
pub fn write(name: &str, data: &[u8], append: bool) -> Result<bool, StorageError> {
    let fs = mounted()?;
    match fs.find(name) {
        Some((slot, entry)) => {
            security::check(Op::Write, Some(label(entry.label)), Some(slot as u64))?;
            if append { fs.append(slot, data)? } else { fs.overwrite(slot, data)? }
            Ok(false)
        }
        None => {
            security::check(Op::Create, None, None)?;
            let subject = tasks::current_domain();
            security::check_file_quota(fs.count_label(subject.index() as u8))?;
            fs.create(name, subject.index() as u8, data)?;
            Ok(true)
        }
    }
}

pub fn remove(name: &str) -> Result<(), StorageError> {
    let fs = mounted()?;
    let (slot, entry) = fs.find(name).ok_or(FsError::NotFound)?;
    security::check(Op::Delete, Some(label(entry.label)), Some(slot as u64))?;
    Ok(fs.delete(slot)?)
}

/// Files labelled `domain`, or `None` without a mounted disk.
pub fn files_owned(domain: Domain) -> Option<u32> {
    match disk() {
        Disk::Mounted(fs) => Some(fs.count_label(domain.index() as u8)),
        _ => None,
    }
}
