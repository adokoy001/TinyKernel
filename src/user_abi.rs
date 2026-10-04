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
pub const ENOMEM: i64 = -12;
pub const ECHILD: i64 = -10;
pub const EROFS: i64 = -30;
pub const SPAWN_BYTES: usize = 40;
pub const CHILD_STATUS_BYTES: usize = 40;

/// An owned, decoded request; pointer spans are checked separately before
/// any process or filesystem action. All register-width fields stay u64.
pub struct SpawnRequest {
    pub name: u64, pub name_len: usize, pub argument: u64,
    pub argument_len: usize, pub file: bool,
}
impl SpawnRequest {
    pub fn decode(bytes: &[u8; SPAWN_BYTES]) -> Result<Self, i64> {
        let word = |index: usize| {
            let mut value = [0u8; 8];
            value.copy_from_slice(&bytes[index * 8..index * 8 + 8]);
            u64::from_le_bytes(value)
        };
        let name_len = name_length(word(1))?;
        let argument_len = word(3);
        if argument_len > 128 || word(4) > 1 { return Err(EINVAL); }
        Ok(Self { name: word(0), name_len, argument: word(2),
            argument_len: argument_len as usize, file: word(4) == 1 })
    }
}
pub fn process_id(raw: u64) -> Result<u32, i64> {
    if raw == 0 || raw > u32::MAX as u64 { Err(EINVAL) } else { Ok(raw as u32) }
}
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
    #[test] fn spawn_descriptor_retains_pointer_width_and_rejects_wide_sizes_and_modes() {
        let request = |words: [u64; 5]| {
            let mut bytes = [0u8; SPAWN_BYTES];
            for (index, word) in words.iter().enumerate() {
                bytes[index * 8..index * 8 + 8].copy_from_slice(&word.to_le_bytes());
            }
            bytes
        };
        let decoded = SpawnRequest::decode(&request([u64::MAX, 47, 1 << 40, 128, 1])).unwrap();
        assert_eq!(decoded.name, u64::MAX); assert_eq!(decoded.argument, 1 << 40);
        assert_eq!(decoded.name_len, 47); assert_eq!(decoded.argument_len, 128); assert!(decoded.file);
        for words in [[0, 1 << 32, 0, 0, 0], [0, 0, 0, 0, 0],
            [0, 1, 0, 129, 0], [0, 1, 0, 1 << 32, 0], [0, 1, 0, 0, 1 << 32]] {
            assert!(matches!(SpawnRequest::decode(&request(words)), Err(EINVAL)));
        }
        assert_eq!(process_id(1 << 32), Err(EINVAL));
        assert_eq!(process_id(0), Err(EINVAL));
        assert_eq!(process_id(u32::MAX as u64), Ok(u32::MAX));
    }
}
