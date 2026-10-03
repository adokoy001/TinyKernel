//! Physical memory: the BIOS E820 map and a bitmap allocator of 4 KiB frames.
//! Pure logic without hardware access, shared by the kernel and host tests.

pub const FRAME_SIZE: u64 = 4096;
pub const E820_USABLE: u32 = 1;

/// One BIOS INT 15h, EAX=E820h entry as the boot sector stored it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct E820Entry {
    pub base: u64,
    pub length: u64,
    pub kind: u32,
    pub attributes: u32,
}

impl E820Entry {
    pub fn end(&self) -> u64 {
        self.base.saturating_add(self.length)
    }

    /// ACPI 3.0 extended attributes: bit 0 clear means "ignore this entry".
    pub fn ignored(&self) -> bool {
        self.attributes & 1 == 0 || self.length == 0
    }

    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            1 => "usable",
            2 => "reserved",
            3 => "ACPI reclaimable",
            4 => "ACPI NVS",
            5 => "bad memory",
            _ => "other",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FreeError {
    Misaligned,
    NotManaged,
    AlreadyFree,
}

impl FreeError {
    pub fn message(&self) -> &'static str {
        match self {
            FreeError::Misaligned => "address is not 4 KiB aligned",
            FreeError::NotManaged => "not a managed RAM frame",
            FreeError::AlreadyFree => "frame is already free",
        }
    }
}

/// Covers frames 0..WORDS*64, i.e. addresses below `LIMIT`. A set bit in
/// `usable` marks RAM handed to the allocator; a set bit in `free` marks one
/// of those frames as available. Both bitmaps start zeroed (in BSS).
pub struct FrameAllocator<const WORDS: usize> {
    usable: [u64; WORDS],
    free: [u64; WORDS],
    usable_frames: usize,
    free_frames: usize,
    next: usize,
}

impl<const WORDS: usize> FrameAllocator<WORDS> {
    pub const LIMIT: u64 = (WORDS * 64) as u64 * FRAME_SIZE;
    const FRAMES: usize = WORDS * 64;

    pub const fn new() -> Self {
        Self { usable: [0; WORDS], free: [0; WORDS], usable_frames: 0, free_frames: 0, next: 0 }
    }

    /// Hand usable RAM in `[low, LIMIT)` to the allocator. Any overlapping
    /// entry that is not usable wins, so a conflicting map never yields
    /// firmware-owned memory.
    pub fn init(&mut self, map: &[E820Entry], low: u64) {
        self.usable.fill(0);
        for entry in map.iter().filter(|e| !e.ignored() && e.kind == E820_USABLE) {
            let start = align_up(entry.base.max(low));
            let end = align_down(entry.end().min(Self::LIMIT));
            Self::set(&mut self.usable, start, end, true);
        }
        for entry in map.iter().filter(|e| !e.ignored() && e.kind != E820_USABLE) {
            let start = align_down(entry.base.min(Self::LIMIT));
            let end = align_up(entry.end().min(Self::LIMIT));
            Self::set(&mut self.usable, start, end, false);
        }
        self.free.copy_from_slice(&self.usable);
        self.usable_frames = self.usable.iter().map(|w| w.count_ones() as usize).sum();
        self.free_frames = self.usable_frames;
        self.next = 0;
    }

    fn set(bits: &mut [u64; WORDS], start: u64, end: u64, value: bool) {
        let mut frame = (start / FRAME_SIZE) as usize;
        while (frame as u64) * FRAME_SIZE < end {
            if value { bits[frame / 64] |= 1 << (frame % 64); } else { bits[frame / 64] &= !(1 << (frame % 64)); }
            frame += 1;
        }
    }

    fn bit(bits: &[u64; WORDS], frame: usize) -> bool {
        bits[frame / 64] & (1 << (frame % 64)) != 0
    }

    pub fn usable_frames(&self) -> usize {
        self.usable_frames
    }

    pub fn free_frames(&self) -> usize {
        self.free_frames
    }

    #[cfg(test)]
    pub fn is_free(&self, address: u64) -> bool {
        address < Self::LIMIT && Self::bit(&self.free, (address / FRAME_SIZE) as usize)
    }

    pub fn allocate(&mut self) -> Option<u64> {
        self.allocate_contiguous(1)
    }

    /// Next-fit search for `count` physically adjacent free frames.
    pub fn allocate_contiguous(&mut self, count: usize) -> Option<u64> {
        if count == 0 || count > self.free_frames {
            return None;
        }
        for origin in [self.next, 0] {
            let mut frame = origin;
            let mut run = 0;
            while frame < Self::FRAMES {
                if frame % 64 == 0 && self.free[frame / 64] == 0 {
                    frame += 64; // Skip a whole word of unavailable frames.
                    run = 0;
                    continue;
                }
                run = if Self::bit(&self.free, frame) { run + 1 } else { 0 };
                frame += 1;
                if run == count {
                    let first = frame - count;
                    for taken in first..frame {
                        self.free[taken / 64] &= !(1 << (taken % 64));
                    }
                    self.free_frames -= count;
                    self.next = frame % Self::FRAMES;
                    return Some(first as u64 * FRAME_SIZE);
                }
            }
        }
        None
    }

    pub fn free(&mut self, address: u64) -> Result<(), FreeError> {
        self.free_contiguous(address, 1)
    }

    /// Validate every frame first, so a bad request changes nothing.
    pub fn free_contiguous(&mut self, address: u64, count: usize) -> Result<(), FreeError> {
        if address % FRAME_SIZE != 0 {
            return Err(FreeError::Misaligned);
        }
        let first = (address / FRAME_SIZE) as usize;
        if address >= Self::LIMIT || count > Self::FRAMES - first {
            return Err(FreeError::NotManaged);
        }
        for frame in first..first + count {
            if !Self::bit(&self.usable, frame) {
                return Err(FreeError::NotManaged);
            }
            if Self::bit(&self.free, frame) {
                return Err(FreeError::AlreadyFree);
            }
        }
        for frame in first..first + count {
            self.free[frame / 64] |= 1 << (frame % 64);
        }
        self.free_frames += count;
        Ok(())
    }
}

fn align_up(address: u64) -> u64 {
    address.saturating_add(FRAME_SIZE - 1) & !(FRAME_SIZE - 1)
}

fn align_down(address: u64) -> u64 {
    address & !(FRAME_SIZE - 1)
}

#[cfg(test)]
mod tests {
    use super::{E820Entry, FrameAllocator, FreeError, FRAME_SIZE};

    const MIB: u64 = 1 << 20;

    fn entry(base: u64, length: u64, kind: u32) -> E820Entry {
        E820Entry { base, length, kind, attributes: 1 }
    }

    // The map SeaBIOS reports for `qemu-system-x86_64 -m 64M`.
    fn qemu_64m() -> [E820Entry; 6] {
        [
            entry(0, 0x9fc00, 1),
            entry(0x9fc00, 0x400, 2),
            entry(0xf0000, 0x10000, 2),
            entry(0x100000, 0x3ee0000, 1),
            entry(0x3fe0000, 0x20000, 2),
            entry(0xfffc0000, 0x40000, 2),
        ]
    }

    #[test]
    fn qemu_map_above_one_mib() {
        let mut frames = Box::new(FrameAllocator::<4096>::new());
        frames.init(&qemu_64m(), MIB);
        assert_eq!(frames.usable_frames(), 0x3ee0000 / 4096);
        assert_eq!(frames.free_frames(), frames.usable_frames());
        assert!(!frames.is_free(0x9e000));
        assert!(frames.is_free(MIB));
        assert!(frames.is_free(0x3fdf000));
        assert!(!frames.is_free(0x3fe0000));
    }

    #[test]
    fn map_is_clipped_aligned_and_reserved_entries_win() {
        let mut frames = Box::new(FrameAllocator::<4>::new()); // 256 frames: 1 MiB.
        frames.init(&[
            entry(0x10800, 0x2000, 1),          // Partial frames shrink to 0x11000..0x12000.
            entry(0x20000, 0x10000, 1),         // 16 frames, one overlapped below.
            entry(0x25800, 0x100, 2),           // Reserved inside usable RAM.
            entry(0x80000, 0x100000, 1),        // Runs past LIMIT.
            E820Entry { base: 0x40000, length: 0x1000, kind: 1, attributes: 0 }, // Ignored.
            entry(0, 0x1000, 1),                // Below `low`.
        ], 0x10000);
        assert_eq!(frames.usable_frames(), 1 + 15 + 128);
        assert!(frames.is_free(0x11000));
        assert!(!frames.is_free(0x10000));
        assert!(!frames.is_free(0x12000));
        assert!(!frames.is_free(0x25000));
        assert!(!frames.is_free(0x40000));
        assert!(frames.is_free(0xff000));
        assert!(!frames.is_free(0));
    }

    #[test]
    fn allocation_is_unique_until_exhausted_then_reuses_freed_frames() {
        let mut frames = Box::new(FrameAllocator::<1>::new()); // 64 frames.
        frames.init(&[entry(0x10000, 0x4000, 1)], 0);
        let a = frames.allocate().unwrap();
        let b = frames.allocate().unwrap();
        let c = frames.allocate().unwrap();
        let d = frames.allocate().unwrap();
        assert_eq!([a, b, c, d], [0x10000, 0x11000, 0x12000, 0x13000]);
        assert_eq!(frames.allocate(), None);
        assert_eq!(frames.free_frames(), 0);
        assert_eq!(frames.free(b), Ok(()));
        assert_eq!(frames.allocate(), Some(b));
    }

    #[test]
    fn contiguous_runs_skip_holes() {
        let mut frames = Box::new(FrameAllocator::<2>::new());
        frames.init(&[entry(0, 128 * FRAME_SIZE, 1), entry(2 * FRAME_SIZE, 1, 2)], 0);
        assert_eq!(frames.allocate_contiguous(4), Some(3 * FRAME_SIZE));
        assert_eq!(frames.allocate_contiguous(2), Some(7 * FRAME_SIZE));
        assert_eq!(frames.allocate_contiguous(2), Some(9 * FRAME_SIZE));
        assert_eq!(frames.allocate_contiguous(0), None);
        assert_eq!(frames.allocate_contiguous(200), None);
        // A run may span the boundary between two bitmap words.
        assert_eq!(frames.allocate_contiguous(60), Some(11 * FRAME_SIZE));
        assert_eq!(frames.free_contiguous(3 * FRAME_SIZE, 4), Ok(()));
        assert_eq!(frames.free_frames(), 127 - 4 - 60);
    }

    #[test]
    fn invalid_frees_change_nothing() {
        let mut frames = Box::new(FrameAllocator::<1>::new());
        frames.init(&[entry(0x1000, 0x3000, 1)], 0);
        let first = frames.allocate_contiguous(2).unwrap();
        assert_eq!(first, 0x1000);
        assert_eq!(frames.free(0x1001), Err(FreeError::Misaligned));
        assert_eq!(frames.free(0), Err(FreeError::NotManaged));
        assert_eq!(frames.free(64 * FRAME_SIZE), Err(FreeError::NotManaged));
        assert_eq!(frames.free(u64::MAX & !(FRAME_SIZE - 1)), Err(FreeError::NotManaged));
        assert_eq!(frames.free(0x3000), Err(FreeError::AlreadyFree));
        // The second frame is already free, so the first must stay allocated.
        assert_eq!(frames.free_contiguous(0x2000, 2), Err(FreeError::AlreadyFree));
        assert!(!frames.is_free(0x2000));
        assert_eq!(frames.free_frames(), 1);
        assert_eq!(frames.free_contiguous(0x1000, 2), Ok(()));
        assert_eq!(frames.free(0x1000), Err(FreeError::AlreadyFree));
        assert_eq!(frames.free_frames(), 3);
    }
}
