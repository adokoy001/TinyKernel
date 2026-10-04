//! A fixed, sparse user address space with independent page tables.
//!
//! Kernel callers validate the entire requested range before copying through
//! the supervisor physical mapping. A hostile pointer cannot make an early
//! portion of a write succeed before a later page fails validation.

pub const PAGE_BYTES: u64 = 4096;
pub const CODE_BASE: u64 = 0x4000_0000;
pub const DATA_BASE: u64 = 0x4000_1000;
pub const STACK_BASE: u64 = 0x4000_4000;
pub const STACK_TOP: u64 = 0x4000_6000;
pub const ARGS_MAX: usize = 128;
pub const SPACE_FRAMES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access { Read, Write }

/// Check all bytes of a user range. Empty spans still need a mapped pointer;
/// this keeps malformed pointers from being accepted by a zero-byte syscall.
pub fn validate_span(pointer: u64, length: usize, access: Access) -> Result<(), &'static str> {
    let end = pointer.checked_add(length as u64).ok_or("user range overflows")?;
    // The whole layout is in the low canonical half. Checking the layout also
    // rejects high canonical kernel pointers and the noncanonical hole.
    let code = pointer >= CODE_BASE && pointer < DATA_BASE;
    let data = pointer >= DATA_BASE && pointer < DATA_BASE + PAGE_BYTES;
    let stack = pointer >= STACK_BASE && pointer < STACK_TOP;
    if code {
        if access == Access::Write { return Err("user page is read-only"); }
        if end > DATA_BASE + PAGE_BYTES { return Err("user range crosses an unmapped page"); }
    } else if data {
        if end > DATA_BASE + PAGE_BYTES { return Err("user range crosses an unmapped page"); }
    } else if stack {
        if end > STACK_TOP { return Err("user range crosses an unmapped page"); }
    } else {
        return Err("user pointer is unmapped");
    }
    Ok(())
}

/// The physical frame holding this virtual page, relative to the allocation.
/// This is consulted only after `validate_span` has authorized the full span.
fn physical_offset(pointer: u64) -> u64 {
    if pointer < DATA_BASE {
        4 * PAGE_BYTES + (pointer - CODE_BASE)
    } else if pointer < DATA_BASE + PAGE_BYTES {
        5 * PAGE_BYTES + (pointer - DATA_BASE)
    } else {
        6 * PAGE_BYTES + (pointer - STACK_BASE)
    }
}

#[cfg(not(test))]
mod owned {
    use super::*;
    use core::ptr::{copy_nonoverlapping, write_bytes};

    const PRESENT: u64 = 1;
    const WRITABLE: u64 = 1 << 1;
    const USER: u64 = 1 << 2;
    const NO_EXECUTE: u64 = 1 << 63;
    const TABLE_FLAGS: u64 = PRESENT | WRITABLE | USER;

    /// Owns four page-table frames, one code page, one data page and two stack
    /// pages. It cannot be copied; teardown happens after CR3 and RSP leave it.
    pub struct AddressSpace {
        base: u64,
        entry: u64,
        argument_len: usize,
    }

    impl AddressSpace {
        pub fn create(image: &crate::executable::Image<'_>, argument: &[u8]) -> Result<Self, &'static str> {
            if !crate::paging::nx_enabled() { return Err("user processes require NX support"); }
            let code = image.code();
            let data = image.data();
            if code.is_empty() || code.len() > PAGE_BYTES as usize || image.entry_offset() >= code.len() {
                return Err("invalid executable code range");
            }
            if data.len().checked_add(image.bss_len()).filter(|size| *size <= PAGE_BYTES as usize).is_none() {
                return Err("invalid executable data range");
            }
            if argument.len() > ARGS_MAX { return Err("process argument is too long"); }
            let base = crate::with_frames(|frames| frames.allocate_contiguous(SPACE_FRAMES))
                .ok_or("not enough contiguous process frames")?;
            // Allocation owns these frames now, and all aliases stay supervisor
            // only. Clear reused data and page-table flags before publishing.
            unsafe {
                write_bytes(base as *mut u8, 0, SPACE_FRAMES * PAGE_BYTES as usize);
                let pml4 = base as *mut u64;
                let pdpt = (base + PAGE_BYTES) as *mut u64;
                let user_pd = (base + 2 * PAGE_BYTES) as *mut u64;
                let user_pt = (base + 3 * PAGE_BYTES) as *mut u64;
                pml4.add(0).write(base + PAGE_BYTES | TABLE_FLAGS);
                // U/S is clear here, so kernel identity pages stay supervisor
                // even though the common PML4 entry allows user descendants.
                pdpt.add(0).write(crate::paging::kernel_pd() | PRESENT | WRITABLE);
                pdpt.add(1).write(base + 2 * PAGE_BYTES | TABLE_FLAGS);
                user_pd.add(0).write(base + 3 * PAGE_BYTES | TABLE_FLAGS);
                user_pt.add(0).write(base + 4 * PAGE_BYTES | PRESENT | USER);
                user_pt.add(1).write(base + 5 * PAGE_BYTES | PRESENT | WRITABLE | USER | NO_EXECUTE);
                // Indices 2 and 3 deliberately remain absent: stack guard.
                user_pt.add(4).write(base + 6 * PAGE_BYTES | PRESENT | WRITABLE | USER | NO_EXECUTE);
                user_pt.add(5).write(base + 7 * PAGE_BYTES | PRESENT | WRITABLE | USER | NO_EXECUTE);
                copy_nonoverlapping(code.as_ptr(), (base + 4 * PAGE_BYTES) as *mut u8, code.len());
                copy_nonoverlapping(data.as_ptr(), (base + 5 * PAGE_BYTES) as *mut u8, data.len());
                copy_nonoverlapping(argument.as_ptr(), (base + 6 * PAGE_BYTES) as *mut u8, argument.len());
            }
            Ok(Self { base, entry: CODE_BASE + image.entry_offset() as u64, argument_len: argument.len() })
        }

        pub fn root(&self) -> u64 { self.base }
        pub fn entry(&self) -> u64 { self.entry }
        pub fn initial_rsp(&self) -> u64 { STACK_TOP - 8 }
        pub fn argument(&self) -> (u64, u64) { (STACK_BASE, self.argument_len as u64) }

        pub fn validate_read(&self, pointer: u64, length: usize) -> Result<(), &'static str> {
            validate_span(pointer, length, Access::Read)
        }

        pub fn validate_write(&self, pointer: u64, length: usize) -> Result<(), &'static str> {
            validate_span(pointer, length, Access::Write)
        }

        pub fn copy_from_user(&self, pointer: u64, output: &mut [u8]) -> Result<(), &'static str> {
            self.validate_read(pointer, output.len())?;
            let mut copied = 0;
            while copied < output.len() {
                let address = pointer + copied as u64;
                let count = (PAGE_BYTES as usize - (address % PAGE_BYTES) as usize).min(output.len() - copied);
                unsafe {
                    copy_nonoverlapping((self.base + physical_offset(address)) as *const u8,
                        output.as_mut_ptr().add(copied), count);
                }
                copied += count;
            }
            Ok(())
        }

        pub fn copy_to_user(&self, pointer: u64, input: &[u8]) -> Result<(), &'static str> {
            self.validate_write(pointer, input.len())?;
            let mut copied = 0;
            while copied < input.len() {
                let address = pointer + copied as u64;
                let count = (PAGE_BYTES as usize - (address % PAGE_BYTES) as usize).min(input.len() - copied);
                unsafe {
                    copy_nonoverlapping(input.as_ptr().add(copied),
                        (self.base + physical_offset(address)) as *mut u8, count);
                }
                copied += count;
            }
            Ok(())
        }

        /// The scheduler must switch to another address space and kernel stack
        /// before destroying this allocation, including its root frame.
        pub fn destroy(self) {
            unsafe { write_bytes(self.base as *mut u8, 0, SPACE_FRAMES * PAGE_BYTES as usize); }
            let freed = crate::with_frames(|frames| frames.free_contiguous(self.base, SPACE_FRAMES));
            assert!(freed.is_ok(), "process address-space frames were not allocated");
        }
    }
}

#[cfg(not(test))]
pub use owned::AddressSpace;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cover_code_data_and_both_stack_pages() {
        assert_eq!(validate_span(CODE_BASE, 8192, Access::Read), Ok(()));
        assert_eq!(validate_span(STACK_BASE, 8192, Access::Read), Ok(()));
        assert_eq!(validate_span(DATA_BASE + 4095, 1, Access::Read), Ok(()));
        assert_eq!(validate_span(STACK_TOP - 1, 1, Access::Read), Ok(()));
    }

    #[test]
    fn writes_require_writable_pages_for_the_entire_span() {
        assert!(validate_span(CODE_BASE, 1, Access::Write).is_err());
        assert!(validate_span(DATA_BASE - 1, 2, Access::Write).is_err());
        assert_eq!(validate_span(DATA_BASE, 4096, Access::Write), Ok(()));
        assert_eq!(validate_span(STACK_BASE + 4095, 2, Access::Write), Ok(()));
    }

    #[test]
    fn crossing_guard_or_mapping_end_is_rejected_before_copy() {
        assert!(validate_span(DATA_BASE + 4095, 2, Access::Read).is_err());
        assert!(validate_span(CODE_BASE, 8193, Access::Read).is_err());
        assert!(validate_span(STACK_BASE - 1, 2, Access::Read).is_err());
        assert!(validate_span(STACK_TOP - 1, 2, Access::Write).is_err());
        assert!(validate_span(DATA_BASE, 3 * PAGE_BYTES as usize, Access::Write).is_err());
    }

    #[test]
    fn kernel_noncanonical_and_guard_pointers_are_rejected() {
        for pointer in [0, 0x10000, CODE_BASE - 1, DATA_BASE + PAGE_BYTES,
            STACK_BASE - PAGE_BYTES, STACK_TOP, 0x8000_0000_0000,
            0xffff_8000_0000_0000, u64::MAX] {
            assert!(validate_span(pointer, 1, Access::Read).is_err(), "{pointer:x}");
        }
    }

    #[test]
    fn arithmetic_wrap_and_extreme_lengths_are_rejected() {
        assert!(validate_span(u64::MAX, 2, Access::Read).is_err());
        assert!(validate_span(DATA_BASE, usize::MAX, Access::Write).is_err());
        assert!(validate_span(STACK_BASE, usize::MAX, Access::Read).is_err());
    }

    #[test]
    fn zero_length_does_not_authorize_an_invalid_pointer() {
        assert_eq!(validate_span(DATA_BASE, 0, Access::Write), Ok(()));
        assert_eq!(validate_span(CODE_BASE, 0, Access::Read), Ok(()));
        assert!(validate_span(CODE_BASE, 0, Access::Write).is_err());
        assert!(validate_span(STACK_TOP, 0, Access::Read).is_err());
        assert!(validate_span(0, 0, Access::Read).is_err());
    }

    #[test]
    fn physical_aliases_skip_tables_and_guard_without_sharing_pages() {
        assert_eq!(physical_offset(CODE_BASE), 4 * PAGE_BYTES);
        assert_eq!(physical_offset(DATA_BASE - 1), 5 * PAGE_BYTES - 1);
        assert_eq!(physical_offset(DATA_BASE), 5 * PAGE_BYTES);
        assert_eq!(physical_offset(STACK_BASE), 6 * PAGE_BYTES);
        assert_eq!(physical_offset(STACK_TOP - 1), 8 * PAGE_BYTES - 1);
    }
}
