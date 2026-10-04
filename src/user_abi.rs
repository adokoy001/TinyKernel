//! int 0x80 ABI: RAX number; RDI, RSI, RDX arguments; signed i64 RAX result.
//! Fixed bounds are validated before dereferencing pointers or file effects.
pub const MAX_IO: usize = 256;
pub const MAX_SLEEP_MS: u64 = 60_000;
pub const EINVAL: i64 = -22;
pub const EFAULT: i64 = -14;
pub const EACCES: i64 = -13;
pub const EBADF: i64 = -9;
pub const ESTALE: i64 = -116;
pub const ENOSYS: i64 = -38;
pub const ENOSPC: i64 = -28;
pub const EIO: i64 = -5;
pub const ENOENT: i64 = -2;
pub const EOVERFLOW: i64 = -75;
pub const EAGAIN: i64 = -11;
pub fn io_length(raw: u64) -> Result<usize, i64> { if raw > MAX_IO as u64 { Err(EINVAL) } else { Ok(raw as usize) } }
pub fn name_length(raw: u64) -> Result<usize, i64> { if raw == 0 || raw > 47 { Err(EINVAL) } else { Ok(raw as usize) } }
pub fn rights(raw: u64) -> Result<u8, i64> { if raw != 0 && raw <= 3 { Ok(raw as u8) } else { Err(EINVAL) } }
pub fn sleep_millis(raw: u64) -> Result<u64, i64> { if raw > MAX_SLEEP_MS { Err(EINVAL) } else { Ok(raw) } }
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn wide_registers_do_not_truncate_into_valid_arguments() {
        assert_eq!(io_length(256), Ok(256)); assert_eq!(io_length(257), Err(EINVAL));
        assert_eq!(io_length(u64::MAX), Err(EINVAL)); assert_eq!(name_length(1 << 32), Err(EINVAL));
        assert_eq!(rights(0x1_0000_0001), Err(EINVAL)); assert_eq!(rights(0), Err(EINVAL));
        assert_eq!(rights(3), Ok(3)); assert_eq!(sleep_millis(60_001), Err(EINVAL));
        assert_eq!(sleep_millis(60_000), Ok(60_000));
    }
}
