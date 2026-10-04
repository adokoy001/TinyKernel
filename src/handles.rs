//! Bounded, process-scoped file capabilities. Tokens are boot-local and never
//! recycled. Rights are copied at open and cannot be enlarged by a later call.
//! A capability pins an owned file identity, not an untrusted pathname loan.

pub const READ: u8 = 1;
pub const WRITE: u8 = 2;
pub const MAX_HANDLES: usize = 4;
pub const MAX_NAME: usize = 47;
pub const fn valid_rights(rights: u8) -> bool { rights != 0 && rights & !(READ | WRITE) == 0 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub slot: u8,
    pub label: u8,
    pub size: u32,
    pub generation: u32,
    pub checksum: u32,
    pub epoch: u64,
}

impl Identity {
    const EMPTY: Self = Self { slot: 0, label: 0, size: 0, generation: 0, checksum: 0, epoch: 0 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error { Full, InvalidRights, InvalidName, BadHandle, WrongOwner, Exhausted }

pub struct TokenIssuer { next: u64 }
impl TokenIssuer {
    pub const fn new() -> Self { Self { next: 1 } }
    fn issue(&mut self) -> Result<u64, Error> {
        if self.next > i64::MAX as u64 { return Err(Error::Exhausted); }
        let token = self.next;
        self.next += 1;
        Ok(token)
    }
}

#[derive(Clone, Copy)]
pub struct Capability {
    token: u64,
    rights: u8,
    name: [u8; MAX_NAME],
    name_len: u8,
    pub identity: Identity,
    pub cursor: usize,
}
impl Capability {
    const EMPTY: Self = Self { token: 0, rights: 0, name: [0; MAX_NAME], name_len: 0, identity: Identity::EMPTY, cursor: 0 };
    pub fn name(&self) -> &str { core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("") }
    pub fn permits(&self, requested: u8) -> bool { valid_rights(requested) && self.rights & requested == requested }
}

pub struct Capabilities { owner: u32, entries: [Capability; MAX_HANDLES] }
impl Capabilities {
    pub const fn new() -> Self { Self { owner: 0, entries: [Capability::EMPTY; MAX_HANDLES] } }
    pub fn reset(&mut self, pid: u32) { self.owner = pid; self.entries.fill(Capability::EMPTY); }
    pub fn owner(&self) -> u32 { self.owner }
    pub fn free_slot(&self, pid: u32) -> Result<usize, Error> {
        if pid == 0 || pid != self.owner { return Err(Error::WrongOwner); }
        self.entries.iter().position(|cap| cap.token == 0).ok_or(Error::Full)
    }
    /// Reserve a unique token before any create operation. A failed open can
    /// burn a token, but can never recycle one or perform I/O when slots are full.
    pub fn reserve(&self, pid: u32, issuer: &mut TokenIssuer) -> Result<u64, Error> { self.free_slot(pid)?; issuer.issue() }
    pub fn install(&mut self, pid: u32, token: u64, rights: u8, name: &str, identity: Identity) -> Result<(), Error> {
        if !valid_rights(rights) { return Err(Error::InvalidRights); }
        if token == 0 || token > i64::MAX as u64 { return Err(Error::BadHandle); }
        if name.is_empty() || name.len() > MAX_NAME || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) { return Err(Error::InvalidName); }
        if self.entries.iter().any(|cap| cap.token == token) { return Err(Error::BadHandle); }
        let slot = self.free_slot(pid)?;
        let cap = &mut self.entries[slot];
        *cap = Capability { token, rights, name_len: name.len() as u8, identity, ..Capability::EMPTY };
        cap.name[..name.len()].copy_from_slice(name.as_bytes());
        Ok(())
    }
    pub fn get(&self, pid: u32, token: u64) -> Result<Capability, Error> {
        if pid == 0 || pid != self.owner { return Err(Error::WrongOwner); }
        self.entries.iter().find(|cap| cap.token == token && token != 0).copied().ok_or(Error::BadHandle)
    }
    pub fn update(&mut self, pid: u32, token: u64, identity: Identity, cursor: usize) -> Result<(), Error> {
        self.get(pid, token)?;
        let cap = self.entries.iter_mut().find(|cap| cap.token == token).ok_or(Error::BadHandle)?;
        cap.identity = identity; cap.cursor = cursor;
        Ok(())
    }
    pub fn close(&mut self, pid: u32, token: u64) -> Result<(), Error> {
        self.get(pid, token)?;
        let cap = self.entries.iter_mut().find(|cap| cap.token == token).ok_or(Error::BadHandle)?;
        *cap = Capability::EMPTY;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const FILE: Identity = Identity { slot: 2, label: 2, size: 3, generation: 1, checksum: 99, epoch: 7 };
    fn opened(caps: &mut Capabilities, issuer: &mut TokenIssuer, pid: u32, rights: u8) -> u64 {
        let token = caps.reserve(pid, issuer).unwrap();
        caps.install(pid, token, rights, "user.txt", FILE).unwrap(); token
    }
    #[test] fn rights_are_immutable_and_unknown_bits_invalid() {
        let mut caps = Capabilities::new(); caps.reset(17);
        let token = opened(&mut caps, &mut TokenIssuer::new(), 17, READ);
        let cap = caps.get(17, token).unwrap();
        assert!(cap.permits(READ)); assert!(!cap.permits(WRITE)); assert!(!cap.permits(READ | WRITE));
        assert!(!cap.permits(0)); assert!(!cap.permits(4)); assert!(!valid_rights(0)); assert!(!valid_rights(5));
        caps.update(17, token, FILE, 3).unwrap(); assert!(!caps.get(17, token).unwrap().permits(WRITE));
    }
    #[test] fn cross_process_theft_and_recycled_slots_do_not_work() {
        let mut issuer = TokenIssuer::new();
        let mut one = Capabilities::new(); one.reset(17);
        let mut two = Capabilities::new(); two.reset(18);
        let a = opened(&mut one, &mut issuer, 17, READ);
        let b = opened(&mut two, &mut issuer, 18, READ);
        assert_ne!(a, b);
        assert!(matches!(two.get(18, a), Err(Error::BadHandle)));
        assert!(matches!(one.get(18, a), Err(Error::WrongOwner)));
        one.close(17, a).unwrap();
        let c = opened(&mut one, &mut issuer, 17, WRITE); assert_ne!(a, c);
        assert!(matches!(one.get(17, a), Err(Error::BadHandle)));
        one.reset(19); assert!(matches!(one.get(19, c), Err(Error::BadHandle)));
    }
    #[test] fn capacity_checked_before_token_or_file_effects() {
        let mut issuer = TokenIssuer::new(); let mut caps = Capabilities::new(); caps.reset(1);
        for _ in 0..MAX_HANDLES { opened(&mut caps, &mut issuer, 1, READ); }
        let next = issuer.next;
        assert_eq!(caps.reserve(1, &mut issuer), Err(Error::Full)); assert_eq!(issuer.next, next);
    }
    #[test] fn token_exhaustion_never_wraps_or_returns_negative_tokens() {
        let mut issuer = TokenIssuer { next: i64::MAX as u64 };
        assert_eq!(issuer.issue(), Ok(i64::MAX as u64));
        assert_eq!(issuer.issue(), Err(Error::Exhausted));
        assert_eq!(issuer.issue(), Err(Error::Exhausted));
    }
}
