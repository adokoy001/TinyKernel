//! ATA (IDE) disk driver: primary channel, master drive, 28-bit LBA, PIO
//! transfers with polling. The drive's interrupt is disabled (nIEN), so no
//! IRQ 14 handler is needed. Every wait is bounded; a missing or stuck
//! drive becomes an error instead of a hang.

use crate::fs::{BlockDevice, DiskError, Sector};
use core::arch::asm;

const DATA: u16 = 0x1f0;
const SECTOR_COUNT: u16 = 0x1f2;
const LBA_LOW: u16 = 0x1f3;
const LBA_MID: u16 = 0x1f4;
const LBA_HIGH: u16 = 0x1f5;
const DRIVE: u16 = 0x1f6;
const STATUS: u16 = 0x1f7;
const COMMAND: u16 = 0x1f7;
const CONTROL: u16 = 0x3f6;

const BSY: u8 = 0x80;
const DRQ: u8 = 0x08;
const ERR: u8 = 0x01;
const DF: u8 = 0x20;

const IDENTIFY: u8 = 0xec;
const READ_SECTORS: u8 = 0x20;
const WRITE_SECTORS: u8 = 0x30;
const CACHE_FLUSH: u8 = 0xe7;
const POLL_LIMIT: u32 = 1_000_000;

pub struct Ata {
    sectors: u32,
    pub model: [u8; 40],
}

unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    asm!("in ax, dx", in("dx") port, out("ax") value, options(nomem, nostack, preserves_flags));
    value
}

unsafe fn outw(port: u16, value: u16) {
    asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
}

/// About 400 ns: reading the alternate status port four times.
unsafe fn settle() {
    for _ in 0..4 {
        crate::inb(CONTROL);
    }
}

unsafe fn wait_not_busy() -> Result<u8, DiskError> {
    for _ in 0..POLL_LIMIT {
        let status = crate::inb(STATUS);
        if status & BSY == 0 {
            return Ok(status);
        }
    }
    Err(DiskError::Timeout)
}

unsafe fn wait_data() -> Result<(), DiskError> {
    for _ in 0..POLL_LIMIT {
        let status = crate::inb(STATUS);
        if status & BSY == 0 {
            if status & (ERR | DF) != 0 {
                return Err(DiskError::Device);
            }
            if status & DRQ != 0 {
                return Ok(());
            }
        }
    }
    Err(DiskError::Timeout)
}

impl Ata {
    /// Probe the primary master. `None` when no ATA disk answers.
    pub fn detect() -> Option<Ata> {
        unsafe {
            crate::outb(CONTROL, 0x02); // nIEN: no interrupts from the drive.
            crate::outb(DRIVE, 0xa0);
            settle();
            if crate::inb(STATUS) == 0xff {
                return None; // Floating bus: no controller or no drive.
            }
            for port in [SECTOR_COUNT, LBA_LOW, LBA_MID, LBA_HIGH] {
                crate::outb(port, 0);
            }
            crate::outb(COMMAND, IDENTIFY);
            if crate::inb(STATUS) == 0 {
                return None;
            }
            wait_not_busy().ok()?;
            if crate::inb(LBA_MID) != 0 || crate::inb(LBA_HIGH) != 0 {
                return None; // ATAPI or SATA signature, not a plain ATA disk.
            }
            wait_data().ok()?;
            let mut words = [0u16; 256];
            for word in words.iter_mut() {
                *word = inw(DATA);
            }
            let sectors = words[60] as u32 | (words[61] as u32) << 16;
            let mut model = [0u8; 40];
            for (index, word) in words[27..47].iter().enumerate() {
                model[index * 2] = (word >> 8) as u8;
                model[index * 2 + 1] = *word as u8;
            }
            (sectors > 0).then_some(Ata { sectors, model })
        }
    }

    pub fn model(&self) -> &str {
        core::str::from_utf8(&self.model).unwrap_or("?").trim_end()
    }

    unsafe fn select(&self, lba: u32, command: u8) -> Result<(), DiskError> {
        if lba >= self.sectors || lba >= 1 << 28 {
            return Err(DiskError::Device);
        }
        wait_not_busy()?;
        crate::outb(DRIVE, 0xe0 | ((lba >> 24) & 0x0f) as u8);
        settle();
        crate::outb(SECTOR_COUNT, 1);
        crate::outb(LBA_LOW, lba as u8);
        crate::outb(LBA_MID, (lba >> 8) as u8);
        crate::outb(LBA_HIGH, (lba >> 16) as u8);
        crate::outb(COMMAND, command);
        settle();
        Ok(())
    }
}

impl BlockDevice for Ata {
    fn sectors(&self) -> u32 {
        self.sectors
    }

    fn read(&mut self, lba: u32, buffer: &mut Sector) -> Result<(), DiskError> {
        unsafe {
            self.select(lba, READ_SECTORS)?;
            wait_data()?;
            for pair in buffer.chunks_mut(2) {
                pair.copy_from_slice(&inw(DATA).to_le_bytes());
            }
        }
        Ok(())
    }

    fn write(&mut self, lba: u32, buffer: &Sector) -> Result<(), DiskError> {
        unsafe {
            self.select(lba, WRITE_SECTORS)?;
            wait_data()?;
            for pair in buffer.chunks(2) {
                outw(DATA, u16::from_le_bytes([pair[0], pair[1]]));
            }
            // Only report success once the drive has the data on stable storage.
            crate::outb(COMMAND, CACHE_FLUSH);
            settle();
            if wait_not_busy()? & (ERR | DF) != 0 {
                return Err(DiskError::Device);
            }
        }
        Ok(())
    }
}
