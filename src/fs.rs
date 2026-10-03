//! TaneFS: a small filesystem on any 512-byte-sector block device.
//! Pure logic without hardware access, shared by the kernel (ATA disk) and
//! host tests (an in-memory disk). Access control is not decided here: the
//! kernel asks `security` before calling any operation that reads or changes
//! a file, using the label this module stores with every file.
//!
//! Layout:
//!   LBA 0       superblock: magic, version, geometry, header checksum
//!   LBA 1..=4   file table: 32 entries of 64 bytes
//!   LBA 8..     data: file slot N owns 8 sectors (4 KiB) at 8 + 8 * N
//!
//! Writes put the data down before the table entry that describes it, and
//! always rewrite the whole 4 KiB slot (zero padded); deletes clear the
//! entry and then zero the slot. Each entry keeps an FNV-1a checksum of the
//! contents, so a damaged file is reported instead of returned.

pub const SECTOR: usize = 512;
pub const MAX_FILES: usize = 32;
pub const SLOT_SECTORS: u32 = 8;
pub const MAX_FILE_SIZE: usize = SLOT_SECTORS as usize * SECTOR;
pub const MAX_NAME: usize = 47;
const MAGIC: [u8; 8] = *b"TANEFS1\0";
const VERSION: u32 = 1;
const TABLE_LBA: u32 = 1;
const TABLE_SECTORS: u32 = 4;
const DATA_LBA: u32 = 8;
pub const REQUIRED_SECTORS: u32 = DATA_LBA + MAX_FILES as u32 * SLOT_SECTORS;
const ENTRY_BYTES: usize = 64;

pub type Sector = [u8; SECTOR];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskError {
    Timeout,
    Device,
}

pub trait BlockDevice {
    fn sectors(&self) -> u32;
    fn read(&mut self, lba: u32, buffer: &mut Sector) -> Result<(), DiskError>;
    fn write(&mut self, lba: u32, buffer: &Sector) -> Result<(), DiskError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    Disk(DiskError),
    TooSmall,
    NotFormatted,
    BadName,
    NotFound,
    Exists,
    Full,
    TooLarge,
    Corrupt,
}

impl FsError {
    pub fn message(self) -> &'static str {
        match self {
            FsError::Disk(DiskError::Timeout) => "disk timed out",
            FsError::Disk(DiskError::Device) => "disk reported an error",
            FsError::TooSmall => "disk is too small for TaneFS",
            FsError::NotFormatted => "disk is not formatted (admin: format)",
            FsError::BadName => "name must be 1-47 of A-Z a-z 0-9 . _ -",
            FsError::NotFound => "no such file",
            FsError::Exists => "file exists",
            FsError::Full => "file table is full",
            FsError::TooLarge => "file would exceed 4096 bytes",
            FsError::Corrupt => "checksum mismatch: file contents are damaged",
        }
    }
}

impl From<DiskError> for FsError {
    fn from(error: DiskError) -> Self {
        FsError::Disk(error)
    }
}

/// One file table entry. `label` is the MAC domain index of its creator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub used: bool,
    pub label: u8,
    pub size: u32,
    pub checksum: u32,
    /// Times the contents were written; shows that a rewrite happened.
    pub generation: u32,
    name_len: u8,
    name: [u8; MAX_NAME],
}

impl Entry {
    const EMPTY: Entry = Entry { used: false, label: 0, size: 0, checksum: 0, generation: 0, name_len: 0, name: [0; MAX_NAME] };

    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("?")
    }

    fn encode(&self, out: &mut [u8]) {
        out.fill(0);
        out[0] = self.used as u8;
        out[1] = self.label;
        out[2] = self.name_len;
        out[4..8].copy_from_slice(&self.size.to_le_bytes());
        out[8..12].copy_from_slice(&self.checksum.to_le_bytes());
        out[12..16].copy_from_slice(&self.generation.to_le_bytes());
        out[16..16 + MAX_NAME].copy_from_slice(&self.name);
    }

    fn decode(raw: &[u8]) -> Result<Entry, FsError> {
        let word = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let mut entry = Entry { used: raw[0] == 1, label: raw[1], size: word(4), checksum: word(8),
                                generation: word(12), name_len: raw[2], name: [0; MAX_NAME] };
        entry.name.copy_from_slice(&raw[16..16 + MAX_NAME]);
        if raw[0] > 1 || (entry.used && (entry.size as usize > MAX_FILE_SIZE
            || !valid_name(&entry.name[..(entry.name_len as usize).min(MAX_NAME)]) || entry.name_len as usize > MAX_NAME)) {
            return Err(FsError::Corrupt);
        }
        Ok(entry)
    }
}

pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME
        && name.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// FNV-1a, 32 bit.
pub fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0x811c_9dc5, |hash, &byte| (hash ^ byte as u32).wrapping_mul(0x0100_0193))
}

pub struct FileSystem<D: BlockDevice> {
    device: D,
    table: [Entry; MAX_FILES],
}

impl<D: BlockDevice> FileSystem<D> {
    /// Write an empty filesystem over the whole metadata and data area.
    pub fn format(mut device: D) -> Result<Self, FsError> {
        if device.sectors() < REQUIRED_SECTORS {
            return Err(FsError::TooSmall);
        }
        let zero = [0u8; SECTOR];
        // Data first, superblock last: a half-formatted disk does not mount.
        for lba in (1..REQUIRED_SECTORS).rev() {
            device.write(lba, &zero)?;
        }
        let mut sector = [0u8; SECTOR];
        sector[0..8].copy_from_slice(&MAGIC);
        let fields = [VERSION, MAX_FILES as u32, DATA_LBA, SLOT_SECTORS, device.sectors()];
        for (index, value) in fields.iter().enumerate() {
            sector[8 + index * 4..12 + index * 4].copy_from_slice(&value.to_le_bytes());
        }
        let sum = checksum(&sector[..28]);
        sector[28..32].copy_from_slice(&sum.to_le_bytes());
        device.write(0, &sector)?;
        Ok(Self { device, table: [Entry::EMPTY; MAX_FILES] })
    }

    pub fn mount(mut device: D) -> Result<Self, (FsError, D)> {
        let mut sector = [0u8; SECTOR];
        if let Err(error) = device.read(0, &mut sector) {
            return Err((error.into(), device));
        }
        let word = |at: usize| u32::from_le_bytes([sector[at], sector[at + 1], sector[at + 2], sector[at + 3]]);
        if sector[0..8] != MAGIC || word(28) != checksum(&sector[..28]) || word(8) != VERSION
            || word(12) != MAX_FILES as u32 || word(16) != DATA_LBA || word(20) != SLOT_SECTORS {
            return Err((FsError::NotFormatted, device));
        }
        let mut table = [Entry::EMPTY; MAX_FILES];
        for index in 0..TABLE_SECTORS {
            if let Err(error) = device.read(TABLE_LBA + index, &mut sector) {
                return Err((error.into(), device));
            }
            for (offset, raw) in sector.chunks(ENTRY_BYTES).enumerate() {
                match Entry::decode(raw) {
                    Ok(entry) => table[index as usize * (SECTOR / ENTRY_BYTES) + offset] = entry,
                    Err(error) => return Err((error, device)),
                }
            }
        }
        Ok(Self { device, table })
    }

    pub fn into_device(self) -> D {
        self.device
    }

    pub fn device(&self) -> &D {
        &self.device
    }

    pub fn entries(&self) -> impl Iterator<Item = (usize, &Entry)> {
        self.table.iter().enumerate().filter(|(_, entry)| entry.used)
    }

    pub fn find(&self, name: &str) -> Option<(usize, Entry)> {
        self.entries().find(|(_, entry)| entry.name() == name).map(|(slot, entry)| (slot, *entry))
    }

    pub fn count_label(&self, label: u8) -> u32 {
        self.entries().filter(|(_, entry)| entry.label == label).count() as u32
    }

    /// Read a file into `buffer`; returns its size. Verifies the checksum.
    pub fn read(&mut self, slot: usize, buffer: &mut [u8; MAX_FILE_SIZE]) -> Result<usize, FsError> {
        let entry = self.entry(slot)?;
        let mut sector = [0u8; SECTOR];
        for index in 0..SLOT_SECTORS {
            self.device.read(Self::data_lba(slot) + index, &mut sector)?;
            buffer[index as usize * SECTOR..(index as usize + 1) * SECTOR].copy_from_slice(&sector);
        }
        let size = entry.size as usize;
        if checksum(&buffer[..size]) != entry.checksum {
            return Err(FsError::Corrupt);
        }
        Ok(size)
    }

    /// Create a new file labelled `label`; returns its slot.
    pub fn create(&mut self, name: &str, label: u8, data: &[u8]) -> Result<usize, FsError> {
        if !valid_name(name.as_bytes()) {
            return Err(FsError::BadName);
        }
        if self.find(name).is_some() {
            return Err(FsError::Exists);
        }
        if data.len() > MAX_FILE_SIZE {
            return Err(FsError::TooLarge);
        }
        let slot = self.table.iter().position(|entry| !entry.used).ok_or(FsError::Full)?;
        let mut entry = Entry { used: true, label, name_len: name.len() as u8, ..Entry::EMPTY };
        entry.name[..name.len()].copy_from_slice(name.as_bytes());
        self.store(slot, entry, data)?;
        Ok(slot)
    }

    /// Replace a file's contents, keeping its name and label.
    pub fn overwrite(&mut self, slot: usize, data: &[u8]) -> Result<(), FsError> {
        let entry = self.entry(slot)?;
        if data.len() > MAX_FILE_SIZE {
            return Err(FsError::TooLarge);
        }
        self.store(slot, entry, data)
    }

    /// Append to a file, after verifying what is already there.
    pub fn append(&mut self, slot: usize, data: &[u8]) -> Result<(), FsError> {
        let mut buffer = [0u8; MAX_FILE_SIZE];
        let size = self.read(slot, &mut buffer)?;
        if size + data.len() > MAX_FILE_SIZE {
            return Err(FsError::TooLarge);
        }
        buffer[size..size + data.len()].copy_from_slice(data);
        let entry = self.entry(slot)?;
        self.store(slot, entry, &buffer[..size + data.len()])
    }

    pub fn delete(&mut self, slot: usize) -> Result<(), FsError> {
        self.entry(slot)?;
        self.table[slot] = Entry::EMPTY;
        self.write_table_sector(slot)?;
        // Object reuse on disk: the next owner of this slot finds zeros.
        let zero = [0u8; SECTOR];
        for index in 0..SLOT_SECTORS {
            self.device.write(Self::data_lba(slot) + index, &zero)?;
        }
        Ok(())
    }

    fn entry(&self, slot: usize) -> Result<Entry, FsError> {
        self.table.get(slot).filter(|entry| entry.used).copied().ok_or(FsError::NotFound)
    }

    fn data_lba(slot: usize) -> u32 {
        DATA_LBA + slot as u32 * SLOT_SECTORS
    }

    fn store(&mut self, slot: usize, mut entry: Entry, data: &[u8]) -> Result<(), FsError> {
        let mut sector = [0u8; SECTOR];
        for index in 0..SLOT_SECTORS as usize {
            sector.fill(0);
            let start = (index * SECTOR).min(data.len());
            let end = ((index + 1) * SECTOR).min(data.len());
            sector[..end - start].copy_from_slice(&data[start..end]);
            self.device.write(Self::data_lba(slot) + index as u32, &sector)?;
        }
        entry.size = data.len() as u32;
        entry.checksum = checksum(data);
        entry.generation = entry.generation.wrapping_add(1);
        self.table[slot] = entry;
        self.write_table_sector(slot)
    }

    fn write_table_sector(&mut self, slot: usize) -> Result<(), FsError> {
        let per_sector = SECTOR / ENTRY_BYTES;
        let first = slot / per_sector * per_sector;
        let mut sector = [0u8; SECTOR];
        for (offset, raw) in sector.chunks_mut(ENTRY_BYTES).enumerate() {
            self.table[first + offset].encode(raw);
        }
        Ok(self.device.write(TABLE_LBA + (slot / per_sector) as u32, &sector)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemoryDisk {
        sectors: Vec<Sector>,
        writes: usize,
        fail_after: Option<usize>,
    }

    impl MemoryDisk {
        fn new(sectors: u32) -> Self {
            Self { sectors: vec![[0; SECTOR]; sectors as usize], writes: 0, fail_after: None }
        }
    }

    impl BlockDevice for MemoryDisk {
        fn sectors(&self) -> u32 {
            self.sectors.len() as u32
        }
        fn read(&mut self, lba: u32, buffer: &mut Sector) -> Result<(), DiskError> {
            *buffer = *self.sectors.get(lba as usize).ok_or(DiskError::Device)?;
            Ok(())
        }
        fn write(&mut self, lba: u32, buffer: &Sector) -> Result<(), DiskError> {
            if self.fail_after == Some(self.writes) {
                return Err(DiskError::Timeout);
            }
            self.writes += 1;
            *self.sectors.get_mut(lba as usize).ok_or(DiskError::Device)? = *buffer;
            Ok(())
        }
    }

    fn read_all(fs: &mut FileSystem<MemoryDisk>, name: &str) -> Result<Vec<u8>, FsError> {
        let (slot, _) = fs.find(name).ok_or(FsError::NotFound)?;
        let mut buffer = [0u8; MAX_FILE_SIZE];
        let size = fs.read(slot, &mut buffer)?;
        Ok(buffer[..size].to_vec())
    }

    #[test]
    fn unformatted_and_small_disks_are_refused() {
        assert_eq!(FileSystem::mount(MemoryDisk::new(2048)).err().map(|e| e.0), Some(FsError::NotFormatted));
        assert_eq!(FileSystem::format(MemoryDisk::new(REQUIRED_SECTORS - 1)).err(), Some(FsError::TooSmall));
    }

    #[test]
    fn files_and_labels_survive_a_remount() {
        let mut fs = FileSystem::format(MemoryDisk::new(2048)).unwrap();
        let a = fs.create("notes.txt", 1, b"admin secret").unwrap();
        let b = fs.create("todo", 2, b"user data").unwrap();
        fs.append(b, b" + more").unwrap();
        fs.overwrite(a, b"rewritten").unwrap();
        let mut fs = FileSystem::mount(fs.into_device()).map_err(|e| e.0).unwrap();
        assert_eq!(read_all(&mut fs, "notes.txt").unwrap(), b"rewritten");
        assert_eq!(read_all(&mut fs, "todo").unwrap(), b"user data + more");
        assert_eq!(fs.find("notes.txt").unwrap().1.label, 1);
        assert_eq!(fs.find("notes.txt").unwrap().1.generation, 2);
        assert_eq!(fs.count_label(2), 1);
        assert_eq!(fs.entries().count(), 2);
    }

    #[test]
    fn names_sizes_and_capacity_are_checked() {
        let mut fs = FileSystem::format(MemoryDisk::new(REQUIRED_SECTORS)).unwrap();
        for bad in ["", "a b", "x/y", "über", &"n".repeat(48)] {
            assert_eq!(fs.create(bad, 1, b""), Err(FsError::BadName), "{bad:?}");
        }
        assert!(fs.create(&"n".repeat(47), 1, b"").is_ok());
        assert_eq!(fs.create("big", 1, &[7; MAX_FILE_SIZE + 1]), Err(FsError::TooLarge));
        let full = fs.create("full", 1, &[7; MAX_FILE_SIZE]).unwrap();
        assert_eq!(fs.append(full, b"x"), Err(FsError::TooLarge));
        assert_eq!(fs.create("full", 2, b""), Err(FsError::Exists));
        for index in 2..MAX_FILES {
            fs.create(&format!("f{index}"), 2, b"").unwrap();
        }
        assert_eq!(fs.create("one-more", 2, b""), Err(FsError::Full));
    }

    #[test]
    fn damaged_contents_are_detected() {
        let mut fs = FileSystem::format(MemoryDisk::new(2048)).unwrap();
        let slot = fs.create("data", 1, b"important").unwrap();
        let mut disk = fs.into_device();
        disk.sectors[(DATA_LBA + slot as u32 * SLOT_SECTORS) as usize][3] ^= 0x20;
        let mut fs = FileSystem::mount(disk).map_err(|e| e.0).unwrap();
        assert_eq!(read_all(&mut fs, "data"), Err(FsError::Corrupt));
        // A damaged superblock means the disk no longer mounts.
        let mut disk = fs.into_device();
        disk.sectors[0][9] ^= 1;
        assert_eq!(FileSystem::mount(disk).err().map(|e| e.0), Some(FsError::NotFormatted));
    }

    #[test]
    fn deleted_slots_are_zeroed_and_reused_without_old_data() {
        let mut fs = FileSystem::format(MemoryDisk::new(2048)).unwrap();
        let slot = fs.create("old", 1, &[0xaa; 3000]).unwrap();
        fs.delete(slot).unwrap();
        assert!(fs.find("old").is_none());
        let disk = fs.into_device();
        let start = (DATA_LBA + slot as u32 * SLOT_SECTORS) as usize;
        assert!(disk.sectors[start..start + SLOT_SECTORS as usize].iter().all(|s| s.iter().all(|&b| b == 0)));
        let mut fs = FileSystem::mount(disk).map_err(|e| e.0).unwrap();
        assert_eq!(fs.create("new", 2, b"x").unwrap(), slot);
        let mut buffer = [0u8; MAX_FILE_SIZE];
        fs.read(slot, &mut buffer).unwrap();
        assert!(buffer[1..].iter().all(|&b| b == 0), "no bytes of the old file remain");
        assert_eq!(fs.delete(slot + 1), Err(FsError::NotFound));
    }

    #[test]
    fn an_interrupted_rewrite_is_reported_as_damage() {
        let mut fs = FileSystem::format(MemoryDisk::new(2048)).unwrap();
        let slot = fs.create("keep", 1, b"version one").unwrap();
        let mut disk = fs.into_device();
        disk.fail_after = Some(disk.writes + SLOT_SECTORS as usize); // all data written, table write fails
        let mut fs = FileSystem::mount(disk).map_err(|e| e.0).unwrap();
        assert_eq!(fs.overwrite(slot, b"version two"), Err(FsError::Disk(DiskError::Timeout)));
        let mut fs = FileSystem::mount(fs.into_device()).map_err(|e| e.0).unwrap();
        // The table still describes version one, whose checksum no longer
        // matches the new data: the damage is reported, not returned.
        assert_eq!(read_all(&mut fs, "keep"), Err(FsError::Corrupt));
    }
}
