//! TaneFS: a bounded original filesystem on 512-byte-sector block devices.
//! Version 2 mutations use a checksummed, ordered redo journal. Version 1
//! volumes stay readable without automatic migration; explicit upgrade stages
//! new integrity metadata and publishes v2 without touching file data/labels.
//! The caller serializes the device. Flush must honor the drive's durability
//! contract; checksums detect accidental tears, not hostile forgery.
//!
//! LBA 0 superblock; 1..=4 table; 5 table checksums; 8..=263 file data;
//! 264 commit header; 265..=274 staged data (8), table (1), checksums (1).

pub const SECTOR: usize = 512;
pub const MAX_FILES: usize = 32;
pub const SLOT_SECTORS: u32 = 8;
pub const MAX_FILE_SIZE: usize = SLOT_SECTORS as usize * SECTOR;
pub const MAX_NAME: usize = 47;
const MAGIC_V1: [u8; 8] = *b"TANEFS1\0";
const MAGIC_V2: [u8; 8] = *b"TANEFS2\0";
const TABLE_LBA: u32 = 1;
const TABLE_SECTORS: u32 = 4;
const CHECKS_LBA: u32 = 5;
const DATA_LBA: u32 = 8;
pub const LEGACY_REQUIRED_SECTORS: u32 = DATA_LBA + MAX_FILES as u32 * SLOT_SECTORS;
pub const JOURNAL_LBA: u32 = LEGACY_REQUIRED_SECTORS;
const PAYLOAD_SECTORS: u32 = SLOT_SECTORS + 2;
pub const REQUIRED_SECTORS: u32 = JOURNAL_LBA + 1 + PAYLOAD_SECTORS;
const ENTRY_BYTES: usize = 64;
const JOURNAL_MAGIC: [u8; 8] = *b"TANEJNL2";
const CHECKS_MAGIC: [u8; 8] = *b"TANECHK2";
const COMMITTED: u32 = 0x434f_4d54;

pub type Sector = [u8; SECTOR];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskError { Timeout, Device }

pub trait BlockDevice {
    fn sectors(&self) -> u32;
    fn read(&mut self, lba: u32, buffer: &mut Sector) -> Result<(), DiskError>;
    fn write(&mut self, lba: u32, buffer: &Sector) -> Result<(), DiskError>;
    /// Wait until all preceding writes are durable under the device contract.
    fn flush(&mut self) -> Result<(), DiskError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    Disk(DiskError), TooSmall, NotFormatted, BadName, NotFound, Exists,
    Full, TooLarge, Corrupt, ReadOnly, NeedsRecovery,
}
impl FsError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Disk(DiskError::Timeout) => "disk timed out",
            Self::Disk(DiskError::Device) => "disk reported an error",
            Self::TooSmall => "disk is too small for TaneFS",
            Self::NotFormatted => "disk is not formatted (admin: format)",
            Self::BadName => "name must be 1-47 of A-Z a-z 0-9 . _ -",
            Self::NotFound => "no such file", Self::Exists => "file exists",
            Self::Full => "file table is full", Self::TooLarge => "file would exceed 4096 bytes",
            Self::Corrupt => "filesystem checksum or metadata is damaged",
            Self::ReadOnly => "legacy TaneFS v1 is read-only (admin: file upgrade)",
            Self::NeedsRecovery => "interrupted disk operation: remount required",
        }
    }
}
impl From<DiskError> for FsError { fn from(error: DiskError) -> Self { Self::Disk(error) } }

/// One file entry. The storage layer enforces its creator's MAC label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub used: bool, pub label: u8, pub size: u32, pub checksum: u32,
    pub generation: u32, name_len: u8, name: [u8; MAX_NAME],
}
impl Entry {
    const EMPTY: Self = Self { used: false, label: 0, size: 0, checksum: 0, generation: 0, name_len: 0, name: [0; MAX_NAME] };
    pub fn name(&self) -> &str { core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("?") }
    fn encode(&self, out: &mut [u8]) {
        out.fill(0); out[0] = self.used as u8; out[1] = self.label; out[2] = self.name_len;
        out[4..8].copy_from_slice(&self.size.to_le_bytes());
        out[8..12].copy_from_slice(&self.checksum.to_le_bytes());
        out[12..16].copy_from_slice(&self.generation.to_le_bytes());
        out[16..16 + MAX_NAME].copy_from_slice(&self.name);
    }
    fn decode(raw: &[u8]) -> Result<Self, FsError> {
        let mut entry = Self { used: raw[0] == 1, label: raw[1], size: word(raw, 4), checksum: word(raw, 8),
            generation: word(raw, 12), name_len: raw[2], name: [0; MAX_NAME] };
        entry.name.copy_from_slice(&raw[16..16 + MAX_NAME]);
        if raw[0] > 1 || raw[3] != 0 || raw[63] != 0 { return Err(FsError::Corrupt); }
        if !entry.used {
            if raw.iter().any(|&byte| byte != 0) { return Err(FsError::Corrupt); }
        } else if entry.size as usize > MAX_FILE_SIZE || entry.name_len as usize > MAX_NAME
            || !valid_name(&entry.name[..(entry.name_len as usize).min(MAX_NAME)])
            || entry.name[entry.name_len as usize..].iter().any(|&byte| byte != 0) {
            return Err(FsError::Corrupt);
        }
        Ok(entry)
    }
}
fn word(data: &[u8], at: usize) -> u32 { u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) }
fn put_word(data: &mut [u8], at: usize, value: u32) { data[at..at + 4].copy_from_slice(&value.to_le_bytes()); }
pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME
        && name.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
fn hash_more(hash: u32, data: &[u8]) -> u32 {
    data.iter().fold(hash, |hash, &byte| (hash ^ byte as u32).wrapping_mul(0x0100_0193))
}
pub fn checksum(data: &[u8]) -> u32 { hash_more(0x811c_9dc5, data) }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery { Clean, Replayed { slot: usize } }

pub struct FileSystem<D: BlockDevice> {
    device: D, table: [Entry; MAX_FILES], table_sums: [u32; TABLE_SECTORS as usize],
    version: u32, recovery: Recovery, poisoned: bool,
}

/// A complete replacement or a bounded patch over already verified contents.
/// Patch can extend with zeros or truncate, and never needs a 4 KiB scratch.
enum Image<'a> {
    Whole(&'a [u8]),
    Patch { offset: usize, data: &'a [u8], old_size: usize, new_size: usize },
    Zero,
}
impl Image<'_> {
    fn len(&self) -> usize {
        match self { Self::Whole(data) => data.len(), Self::Patch { new_size, .. } => *new_size, Self::Zero => 0 }
    }
}
impl<D: BlockDevice> FileSystem<D> {
    /// Explicit destructive format, not an atomic mutation of an old volume.
    /// Invalidate the old header durably before touching live metadata/data.
    /// A crash may leave an unformatted disk; it must not mount partial format.
    pub fn format(mut device: D) -> Result<Self, FsError> {
        if device.sectors() < REQUIRED_SECTORS { return Err(FsError::TooSmall); }
        let zero = [0u8; SECTOR];
        device.write(0, &zero)?; device.flush()?;
        for lba in 1..REQUIRED_SECTORS { device.write(lba, &zero)?; }
        let empty_sum = checksum(&zero);
        let sums = [empty_sum; TABLE_SECTORS as usize];
        let mut sector = [0u8; SECTOR];
        Self::encode_checks(&sums, &mut sector);
        device.write(CHECKS_LBA, &sector)?; device.flush()?;
        Self::encode_superblock(device.sectors(), &mut sector);
        device.write(0, &sector)?; device.flush()?;
        Ok(Self { device, table: [Entry::EMPTY; MAX_FILES], table_sums: sums,
            version: 2, recovery: Recovery::Clean, poisoned: false })
    }

    pub fn mount(mut device: D) -> Result<Self, (FsError, D)> {
        match Self::mount_inner(&mut device) {
            Ok((table, table_sums, version, recovery)) => Ok(Self { device, table, table_sums, version, recovery, poisoned: false }),
            Err(error) => Err((error, device)),
        }
    }
    fn mount_inner(device: &mut D) -> Result<([Entry; MAX_FILES], [u32; 4], u32, Recovery), FsError> {
        let mut sector = [0u8; SECTOR]; device.read(0, &mut sector)?;
        let version = word(&sector, 8);
        let legacy = sector[..8] == MAGIC_V1 && version == 1;
        let modern = sector[..8] == MAGIC_V2 && version == 2;
        if !(legacy || modern) || word(&sector, 28) != checksum(&sector[..28])
            || word(&sector, 12) != MAX_FILES as u32 || word(&sector, 16) != DATA_LBA
            || word(&sector, 20) != SLOT_SECTORS { return Err(FsError::NotFormatted); }
        let required = if modern { REQUIRED_SECTORS } else { LEGACY_REQUIRED_SECTORS };
        if device.sectors() < required || word(&sector, 24) < required || word(&sector, 24) > device.sectors() {
            return Err(FsError::TooSmall);
        }
        let recovery = if modern {
            if word(&sector, 32) != JOURNAL_LBA || word(&sector, 36) != PAYLOAD_SECTORS
                || word(&sector, 40) != CHECKS_LBA || sector[44..508].iter().any(|&b| b != 0)
                || word(&sector, 508) != checksum(&sector[..508]) { return Err(FsError::Corrupt); }
            Self::recover(device)?
        } else { Recovery::Clean };
        let mut sums = [0u32; 4];
        if modern { device.read(CHECKS_LBA, &mut sector)?; sums = Self::decode_checks(&sector)?; }
        let mut table = [Entry::EMPTY; MAX_FILES];
        for index in 0..TABLE_SECTORS {
            device.read(TABLE_LBA + index, &mut sector)?;
            if modern && checksum(&sector) != sums[index as usize] { return Err(FsError::Corrupt); }
            for (offset, raw) in sector.chunks(ENTRY_BYTES).enumerate() {
                let entry = Entry::decode(raw)?;
                if entry.used && table.iter().any(|other| other.used && other.name() == entry.name()) { return Err(FsError::Corrupt); }
                table[index as usize * (SECTOR / ENTRY_BYTES) + offset] = entry;
            }
        }
        Ok((table, sums, version, recovery))
    }
    pub fn into_device(self) -> D { self.device }
    pub fn device(&self) -> &D { &self.device }
    pub fn version(&self) -> u32 { self.version }
    pub fn read_only(&self) -> bool { self.version == 1 }
    pub fn recovery(&self) -> Recovery { self.recovery }
    pub fn sync(&mut self) -> Result<(), FsError> {
        self.ensure_usable()?;
        match self.device.flush() {
            Ok(()) => Ok(()),
            Err(error) => { self.poisoned = true; Err(error.into()) }
        }
    }
    pub fn entries(&self) -> impl Iterator<Item = (usize, &Entry)> {
        self.table.iter().enumerate().filter(|(_, entry)| entry.used)
    }
    pub fn find(&self, name: &str) -> Option<(usize, Entry)> {
        self.entries().find(|(_, entry)| entry.name() == name).map(|(slot, entry)| (slot, *entry))
    }
    pub fn count_label(&self, label: u8) -> u32 { self.entries().filter(|(_, e)| e.label == label).count() as u32 }
    fn ensure_usable(&self) -> Result<(), FsError> { if self.poisoned { Err(FsError::NeedsRecovery) } else { Ok(()) } }
    fn ensure_writable(&self) -> Result<(), FsError> {
        self.ensure_usable()?; if self.read_only() { Err(FsError::ReadOnly) } else { Ok(()) }
    }
    fn entry(&self, slot: usize) -> Result<Entry, FsError> {
        self.ensure_usable()?;
        self.table.get(slot).filter(|entry| entry.used).copied().ok_or(FsError::NotFound)
    }
    fn data_lba(slot: usize) -> u32 { DATA_LBA + slot as u32 * SLOT_SECTORS }
    fn table_lba(slot: usize) -> u32 { TABLE_LBA + (slot / (SECTOR / ENTRY_BYTES)) as u32 }

    /// Verify all original bytes before returning even a short range.
    fn verify_contents(&mut self, slot: usize, entry: Entry) -> Result<(), FsError> {
        let mut sector = [0u8; SECTOR]; let mut sum = checksum(&[]);
        for index in 0..SLOT_SECTORS as usize {
            self.device.read(Self::data_lba(slot) + index as u32, &mut sector)?;
            let start = (index * SECTOR).min(entry.size as usize);
            let end = ((index + 1) * SECTOR).min(entry.size as usize);
            sum = hash_more(sum, &sector[..end - start]);
        }
        if sum != entry.checksum { Err(FsError::Corrupt) } else { Ok(()) }
    }
    pub fn read(&mut self, slot: usize, buffer: &mut [u8; MAX_FILE_SIZE]) -> Result<usize, FsError> {
        let entry = self.entry(slot)?;
        let mut sector = [0u8; SECTOR];
        for index in 0..SLOT_SECTORS as usize {
            self.device.read(Self::data_lba(slot) + index as u32, &mut sector)?;
            buffer[index * SECTOR..(index + 1) * SECTOR].copy_from_slice(&sector);
        }
        if checksum(&buffer[..entry.size as usize]) != entry.checksum { return Err(FsError::Corrupt); }
        Ok(entry.size as usize)
    }
    /// Range reads verify every original byte, including bytes outside range.
    /// A caller must discard the output buffer when this returns an error.
    pub fn read_at(&mut self, slot: usize, offset: usize, buffer: &mut [u8]) -> Result<usize, FsError> {
        let entry = self.entry(slot)?; let size = entry.size as usize;
        let length = buffer.len().min(size.saturating_sub(offset));
        let mut sector = [0u8; SECTOR]; let mut sum = checksum(&[]);
        for index in 0..SLOT_SECTORS as usize {
            self.device.read(Self::data_lba(slot) + index as u32, &mut sector)?;
            let start = index * SECTOR;
            let size_in_sector = size.saturating_sub(start).min(SECTOR);
            sum = hash_more(sum, &sector[..size_in_sector]);
            if length != 0 {
                let from = start.max(offset); let end = (start + SECTOR).min(offset + length);
                if end > from { buffer[from - offset..end - offset].copy_from_slice(&sector[from - start..end - start]); }
            }
        }
        if sum != entry.checksum { return Err(FsError::Corrupt); }
        Ok(length)
    }
    pub fn create(&mut self, name: &str, label: u8, data: &[u8]) -> Result<usize, FsError> {
        self.ensure_writable()?;
        if !valid_name(name.as_bytes()) { return Err(FsError::BadName); }
        if self.find(name).is_some() { return Err(FsError::Exists); }
        if data.len() > MAX_FILE_SIZE { return Err(FsError::TooLarge); }
        let slot = self.table.iter().position(|entry| !entry.used).ok_or(FsError::Full)?;
        let mut entry = Entry { used: true, label, name_len: name.len() as u8, ..Entry::EMPTY };
        entry.name[..name.len()].copy_from_slice(name.as_bytes());
        self.transaction(slot, entry, Image::Whole(data))?; Ok(slot)
    }
    pub fn overwrite(&mut self, slot: usize, data: &[u8]) -> Result<(), FsError> {
        self.ensure_writable()?; let entry = self.entry(slot)?;
        if data.len() > MAX_FILE_SIZE { return Err(FsError::TooLarge); }
        self.transaction(slot, entry, Image::Whole(data))
    }
    pub fn append(&mut self, slot: usize, data: &[u8]) -> Result<(), FsError> {
        self.ensure_writable()?; let entry = self.entry(slot)?;
        let size = entry.size as usize;
        let new_size = size.checked_add(data.len()).filter(|&n| n <= MAX_FILE_SIZE).ok_or(FsError::TooLarge)?;
        self.verify_contents(slot, entry)?;
        self.transaction(slot, entry, Image::Patch { offset: size, data, old_size: size, new_size })
    }
    pub fn write_at(&mut self, slot: usize, offset: usize, data: &[u8]) -> Result<(), FsError> {
        self.ensure_writable()?; let entry = self.entry(slot)?;
        let end = offset.checked_add(data.len()).filter(|&n| n <= MAX_FILE_SIZE).ok_or(FsError::TooLarge)?;
        if data.is_empty() { return Ok(()); }
        self.verify_contents(slot, entry)?;
        self.transaction(slot, entry, Image::Patch { offset, data, old_size: entry.size as usize, new_size: end.max(entry.size as usize) })
    }
    pub fn truncate(&mut self, slot: usize, size: usize) -> Result<(), FsError> {
        self.ensure_writable()?; let entry = self.entry(slot)?;
        if size > MAX_FILE_SIZE { return Err(FsError::TooLarge); }
        self.verify_contents(slot, entry)?;
        self.transaction(slot, entry, Image::Patch { offset: 0, data: &[], old_size: entry.size as usize, new_size: size })
    }
    pub fn rename(&mut self, slot: usize, name: &str) -> Result<(), FsError> {
        self.ensure_writable()?; let mut entry = self.entry(slot)?;
        if !valid_name(name.as_bytes()) { return Err(FsError::BadName); }
        if let Some((other, _)) = self.find(name) { if other != slot { return Err(FsError::Exists); } }
        if entry.name() == name { return Ok(()); }
        self.verify_contents(slot, entry)?;
        entry.name.fill(0); entry.name[..name.len()].copy_from_slice(name.as_bytes()); entry.name_len = name.len() as u8;
        self.transaction(slot, entry, Image::Patch { offset: 0, data: &[], old_size: entry.size as usize, new_size: entry.size as usize })
    }
    pub fn delete(&mut self, slot: usize) -> Result<(), FsError> {
        self.ensure_writable()?; self.entry(slot)?; self.transaction(slot, Entry::EMPTY, Image::Zero)
    }

    /// Explicit non-destructive v1 upgrade. Auxiliary metadata is made
    /// durable while the original v1 superblock remains readable. Only the
    /// final superblock write publishes v2; a torn publication is rejected.
    /// This does not promise availability under a torn superblock write.
    pub fn upgrade(&mut self) -> Result<(), FsError> {
        self.ensure_usable()?;
        if self.version == 2 { return Ok(()); }
        if self.device.sectors() < REQUIRED_SECTORS { return Err(FsError::TooSmall); }
        let result = self.upgrade_verified();
        if matches!(result, Err(FsError::Disk(_))) { self.poisoned = true; }
        result
    }
    fn upgrade_verified(&mut self) -> Result<(), FsError> {
        for slot in 0..MAX_FILES {
            if self.table[slot].used { self.verify_contents(slot, self.table[slot])?; }
        }
        self.upgrade_inner()
    }
    fn upgrade_inner(&mut self) -> Result<(), FsError> {
        let mut sector = [0u8; SECTOR]; let mut sums = [0u32; 4];
        for index in 0..TABLE_SECTORS as usize {
            self.device.read(TABLE_LBA + index as u32, &mut sector)?;
            for (offset, raw) in sector.chunks(ENTRY_BYTES).enumerate() {
                if Entry::decode(raw)? != self.table[index * (SECTOR / ENTRY_BYTES) + offset] { return Err(FsError::Corrupt); }
            }
            sums[index] = checksum(&sector);
        }
        Self::encode_checks(&sums, &mut sector);
        self.device.write(CHECKS_LBA, &sector)?; self.device.flush()?;
        sector.fill(0); self.device.write(JOURNAL_LBA, &sector)?; self.device.flush()?;
        Self::encode_superblock(self.device.sectors(), &mut sector);
        self.device.write(0, &sector)?; self.device.flush()?;
        self.table_sums = sums; self.version = 2; Ok(())
    }
    fn encode_superblock(sectors: u32, sector: &mut Sector) {
        sector.fill(0); sector[0..8].copy_from_slice(&MAGIC_V2);
        for (index, value) in [2, MAX_FILES as u32, DATA_LBA, SLOT_SECTORS, sectors].iter().enumerate() {
            put_word(sector, 8 + index * 4, *value);
        }
        let sum = checksum(&sector[..28]); put_word(sector, 28, sum);
        put_word(sector, 32, JOURNAL_LBA); put_word(sector, 36, PAYLOAD_SECTORS);
        put_word(sector, 40, CHECKS_LBA);
        let sum = checksum(&sector[..508]); put_word(sector, 508, sum);
    }
    fn encode_checks(sums: &[u32; 4], sector: &mut Sector) {
        sector.fill(0); sector[..8].copy_from_slice(&CHECKS_MAGIC);
        for (index, sum) in sums.iter().enumerate() { put_word(sector, 8 + index * 4, *sum); }
        let sum = checksum(&sector[..508]); put_word(sector, 508, sum);
    }
    fn decode_checks(sector: &Sector) -> Result<[u32; 4], FsError> {
        if sector[..8] != CHECKS_MAGIC || sector[24..508].iter().any(|&b| b != 0)
            || word(sector, 508) != checksum(&sector[..508]) { return Err(FsError::Corrupt); }
        Ok([word(sector, 8), word(sector, 12), word(sector, 16), word(sector, 20)])
    }
    fn encode_table(&self, slot: usize, entry: Entry, sector: &mut Sector) {
        let per_sector = SECTOR / ENTRY_BYTES; let first = slot / per_sector * per_sector;
        for (offset, raw) in sector.chunks_mut(ENTRY_BYTES).enumerate() {
            (if first + offset == slot { entry } else { self.table[first + offset] }).encode(raw);
        }
    }
    fn image_sector(&mut self, slot: usize, image: &Image<'_>, index: usize, sector: &mut Sector, old_sum: &mut u32) -> Result<(), FsError> {
        let start = index * SECTOR;
        sector.fill(0);
        match image {
            Image::Whole(data) => {
                let from = start.min(data.len()); let end = (start + SECTOR).min(data.len());
                sector[..end - from].copy_from_slice(&data[from..end]);
            }
            Image::Patch { offset, data, old_size, new_size } => {
                if start < *old_size {
                    self.device.read(Self::data_lba(slot) + index as u32, sector)?;
                    *old_sum = hash_more(*old_sum, &sector[..old_size.saturating_sub(start).min(SECTOR)]);
                }
                let keep = old_size.min(new_size).saturating_sub(start).min(SECTOR);
                sector[keep..].fill(0);
                let from = start.max(*offset); let end = (start + SECTOR).min(*offset + data.len());
                if end > from { sector[from - start..end - start].copy_from_slice(&data[from - offset..end - offset]); }
            }
            Image::Zero => {}
        }
        Ok(())
    }
    fn transaction(&mut self, slot: usize, entry: Entry, image: Image<'_>) -> Result<(), FsError> {
        let result = self.transaction_inner(slot, entry, image);
        if matches!(result, Err(FsError::Disk(_))) { self.poisoned = true; }
        result
    }
    fn transaction_inner(&mut self, slot: usize, mut entry: Entry, image: Image<'_>) -> Result<(), FsError> {
        let mut sector = [0u8; SECTOR];
        // A previous committed payload must never be overwritten until its
        // marker is durably cleared. Even a failed clear poisons this mount.
        self.device.write(JOURNAL_LBA, &sector)?; self.device.flush()?;
        let mut payload_sum = checksum(&[]); let mut file_sum = checksum(&[]); let mut old_sum = checksum(&[]);
        for index in 0..SLOT_SECTORS as usize {
            self.image_sector(slot, &image, index, &mut sector, &mut old_sum)?;
            let size = image.len().saturating_sub(index * SECTOR).min(SECTOR);
            file_sum = hash_more(file_sum, &sector[..size]); payload_sum = hash_more(payload_sum, &sector);
            self.device.write(JOURNAL_LBA + 1 + index as u32, &sector)?;
        }
        if matches!(image, Image::Patch { .. }) && old_sum != self.table[slot].checksum {
            return Err(FsError::Corrupt);
        }
        if entry.used { entry.size = image.len() as u32; entry.checksum = file_sum; entry.generation = entry.generation.wrapping_add(1); }
        self.encode_table(slot, entry, &mut sector);
        let table_index = (slot / (SECTOR / ENTRY_BYTES)) as usize;
        let mut sums = self.table_sums; sums[table_index] = checksum(&sector);
        payload_sum = hash_more(payload_sum, &sector);
        self.device.write(JOURNAL_LBA + 1 + SLOT_SECTORS, &sector)?;
        Self::encode_checks(&sums, &mut sector); payload_sum = hash_more(payload_sum, &sector);
        self.device.write(JOURNAL_LBA + 2 + SLOT_SECTORS, &sector)?; self.device.flush()?;
        sector.fill(0); sector[..8].copy_from_slice(&JOURNAL_MAGIC);
        for (at, value) in [(8, 2), (12, slot as u32), (16, PAYLOAD_SECTORS),
            (20, Self::table_lba(slot)), (24, Self::data_lba(slot)), (28, payload_sum), (504, COMMITTED)] {
            put_word(&mut sector, at, value);
        }
        let sum = checksum(&sector[..508]); put_word(&mut sector, 508, sum);
        self.device.write(JOURNAL_LBA, &sector)?; self.device.flush()?;
        // The durable marker is the commit decision; never clear it before
        // the entire home image has itself completed a durability barrier.
        Self::apply_payload(&mut self.device, slot)?;
        self.table[slot] = entry; self.table_sums = sums;
        Ok(())
    }
    fn apply_payload(device: &mut D, slot: usize) -> Result<(), FsError> {
        let mut sector = [0u8; SECTOR];
        for index in 0..PAYLOAD_SECTORS {
            device.read(JOURNAL_LBA + 1 + index, &mut sector)?;
            let home = if index < SLOT_SECTORS { Self::data_lba(slot) + index }
                else if index == SLOT_SECTORS { Self::table_lba(slot) } else { CHECKS_LBA };
            device.write(home, &sector)?;
        }
        device.flush()?;
        sector.fill(0); device.write(JOURNAL_LBA, &sector)?; device.flush()?;
        Ok(())
    }
    fn recover(device: &mut D) -> Result<Recovery, FsError> {
        let mut header = [0u8; SECTOR]; device.read(JOURNAL_LBA, &mut header)?;
        if header.iter().all(|&b| b == 0) { return Ok(Recovery::Clean); }
        let slot = word(&header, 12) as usize;
        if header[..8] != JOURNAL_MAGIC || word(&header, 8) != 2 || slot >= MAX_FILES
            || word(&header, 16) != PAYLOAD_SECTORS || word(&header, 20) != Self::table_lba(slot)
            || word(&header, 24) != Self::data_lba(slot) || word(&header, 504) != COMMITTED
            || header[32..504].iter().any(|&b| b != 0) || word(&header, 508) != checksum(&header[..508]) {
            return Err(FsError::Corrupt);
        }
        // Check every staged byte before any home write. Invalid committed
        // payloads are preserved as evidence and the mount fails closed.
        let mut sector = [0u8; SECTOR]; let mut sum = checksum(&[]); let mut file_sum = checksum(&[]);
        device.read(JOURNAL_LBA + 1 + SLOT_SECTORS, &mut sector)?;
        for raw in sector.chunks(ENTRY_BYTES) { Entry::decode(raw)?; }
        let table_sum = checksum(&sector);
        let offset = slot % (SECTOR / ENTRY_BYTES) * ENTRY_BYTES;
        let entry = Entry::decode(&sector[offset..offset + ENTRY_BYTES])?;
        for index in 0..PAYLOAD_SECTORS {
            device.read(JOURNAL_LBA + 1 + index, &mut sector)?; sum = hash_more(sum, &sector);
            if index < SLOT_SECTORS {
                let length = (entry.size as usize).saturating_sub(index as usize * SECTOR).min(SECTOR);
                file_sum = hash_more(file_sum, &sector[..length]);
                if sector[length..].iter().any(|&b| b != 0) { return Err(FsError::Corrupt); }
            } else if index == SLOT_SECTORS + 1 {
                let sums = Self::decode_checks(&sector)?;
                if sums[slot / (SECTOR / ENTRY_BYTES)] != table_sum { return Err(FsError::Corrupt); }
            }
        }
        if sum != word(&header, 28) || (entry.used && file_sum != entry.checksum) { return Err(FsError::Corrupt); }
        Self::apply_payload(device, slot)?;
        Ok(Recovery::Replayed { slot })
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
        fn flush(&mut self) -> Result<(), DiskError> { Ok(()) }
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
    fn an_interrupted_prepare_preserves_old_contents() {
        let mut fs = FileSystem::format(MemoryDisk::new(2048)).unwrap();
        let slot = fs.create("keep", 1, b"version one").unwrap();
        let mut disk = fs.into_device();
        disk.fail_after = Some(disk.writes + SLOT_SECTORS as usize); // staging fails before commit
        let mut fs = FileSystem::mount(disk).map_err(|e| e.0).unwrap();
        assert_eq!(fs.overwrite(slot, b"version two"), Err(FsError::Disk(DiskError::Timeout)));
        let mut fs = FileSystem::mount(fs.into_device()).map_err(|e| e.0).unwrap();
        assert_eq!(read_all(&mut fs, "keep").unwrap(), b"version one");
    }
}
