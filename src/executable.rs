//! Strict, bounded original Tane executable format. No relocations or ELF ABI.
pub const HEADER_BYTES: usize = 32;
pub const FILE_MAX: usize = 4096;
pub const PAGE_BYTES: usize = 4096;
pub const CODE_BASE: usize = 0x4000_0000;
pub const DATA_BASE: usize = 0x4000_1000;
pub const STACK_BASE: usize = 0x4000_4000;
pub const STACK_BYTES: usize = 8192;
pub const ARGS_MAX: usize = 128;
pub const MAGIC: &[u8; 8] = b"TANEEXE\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error { Header, Version, Flags, Length, Entry, Data }
impl Error {
    pub fn message(self) -> &'static str { match self {
        Self::Header => "invalid Tane executable header",
        Self::Version => "unsupported Tane executable version",
        Self::Flags => "unsupported Tane executable flags",
        Self::Length => "invalid Tane executable length",
        Self::Entry => "entry is outside executable code",
        Self::Data => "data and BSS exceed one page",
    } }
}

#[derive(Clone, Copy, Debug)]
pub struct Image<'a> { code: &'a [u8], data: &'a [u8], bss: usize, entry: usize }
impl<'a> Image<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() < HEADER_BYTES || &bytes[..8] != MAGIC { return Err(Error::Header); }
        let short = |at| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let word = |at| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        if short(8) != 1 { return Err(Error::Version); }
        if short(10) != 0 { return Err(Error::Flags); }
        if short(12) as usize != HEADER_BYTES || short(14) != 0 { return Err(Error::Header); }
        let (code, data, bss, entry) = (word(16), word(20), word(24), word(28));
        if bytes.len() > FILE_MAX || code == 0 || code > PAGE_BYTES || data > PAGE_BYTES ||
            HEADER_BYTES.checked_add(code).and_then(|n| n.checked_add(data)) != Some(bytes.len()) {
            return Err(Error::Length);
        }
        if bss > PAGE_BYTES || data.checked_add(bss).is_none_or(|n| n > PAGE_BYTES) { return Err(Error::Data); }
        if entry >= code { return Err(Error::Entry); }
        Ok(Self { code: &bytes[HEADER_BYTES..HEADER_BYTES + code], data: &bytes[HEADER_BYTES + code..], bss, entry })
    }
    pub fn code(self) -> &'a [u8] { self.code }
    pub fn data(self) -> &'a [u8] { self.data }
    pub fn bss_len(self) -> usize { self.bss }
    pub fn entry_offset(self) -> usize { self.entry }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(code: u32, data: u32, bss: u32, entry: u32) -> std::vec::Vec<u8> {
        let mut bytes = std::vec![0; HEADER_BYTES + code as usize + data as usize];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        bytes[12..14].copy_from_slice(&(HEADER_BYTES as u16).to_le_bytes());
        for (at, n) in [(16,code),(20,data),(24,bss),(28,entry)] { bytes[at..at+4].copy_from_slice(&n.to_le_bytes()); }
        bytes
    }
    #[test] fn parse_sections_and_entry() {
        let mut bytes = file(3,2,7,1); bytes[32..].copy_from_slice(&[0x90,0x90,0xc3,4,5]);
        let image = Image::parse(&bytes).unwrap();
        assert_eq!(image.code(), &[0x90,0x90,0xc3]); assert_eq!(image.data(), &[4,5]);
        assert_eq!(image.bss_len(),7); assert_eq!(image.entry_offset(),1);
    }
    #[test] fn exact_file_boundary() {
        assert!(Image::parse(&file((FILE_MAX-HEADER_BYTES) as u32,0,0,0)).is_ok());
        assert_eq!(Image::parse(&file((FILE_MAX-HEADER_BYTES+1) as u32,0,0,0)).unwrap_err(),Error::Length);
        let mut bytes=file(1,0,0,0); bytes.push(0);
        assert_eq!(Image::parse(&bytes).unwrap_err(),Error::Length);
    }
    #[test] fn all_header_fields_are_checked() {
        for offset in [0,7,12,14,15] { let mut bytes=file(1,0,0,0); bytes[offset]^=1; assert_eq!(Image::parse(&bytes).unwrap_err(),Error::Header); }
        let mut bytes=file(1,0,0,0); bytes[8]=2; assert_eq!(Image::parse(&bytes).unwrap_err(),Error::Version);
        let mut bytes=file(1,0,0,0); bytes[10]=1; assert_eq!(Image::parse(&bytes).unwrap_err(),Error::Flags);
        for len in 0..HEADER_BYTES { assert_eq!(Image::parse(&bytes[..len]).unwrap_err(),Error::Header); }
    }
    #[test] fn entry_must_be_inside_nonempty_code() {
        assert_eq!(Image::parse(&file(0,1,0,0)).unwrap_err(),Error::Length);
        assert_eq!(Image::parse(&file(1,0,0,1)).unwrap_err(),Error::Entry);
        assert_eq!(Image::parse(&file(1,0,0,u32::MAX)).unwrap_err(),Error::Entry);
    }
    #[test] fn data_bss_cannot_cross_page() {
        assert!(Image::parse(&file(1,1,4095,0)).is_ok());
        assert_eq!(Image::parse(&file(1,1,4096,0)).unwrap_err(),Error::Data);
        assert_eq!(Image::parse(&file(1,0,u32::MAX,0)).unwrap_err(),Error::Data);
    }
    #[test] fn malformed_lengths_never_slice_outside_input() {
        let mut bytes=file(1,0,0,0);
        for field in [16,20] { bytes[field..field+4].copy_from_slice(&u32::MAX.to_le_bytes()); assert_eq!(Image::parse(&bytes).unwrap_err(),Error::Length); }
    }
}
