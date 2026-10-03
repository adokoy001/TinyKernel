//! A bounded literal variable store. Values are data, never command source.

pub const MAX_VARIABLES: usize = 8;
pub const MAX_NAME: usize = 24;
pub const MAX_VALUE: usize = 64;

#[derive(Clone, Copy)]
struct Variable {
    name: [u8; MAX_NAME], name_len: u8,
    value: [u8; MAX_VALUE], value_len: u8,
}

impl Variable {
    const EMPTY: Self = Self { name: [0; MAX_NAME], name_len: 0,
        value: [0; MAX_VALUE], value_len: 0 };
    fn name(&self) -> &str { core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("") }
    fn value(&self) -> &str { core::str::from_utf8(&self.value[..self.value_len as usize]).unwrap_or("") }
}

pub struct Variables { entries: [Variable; MAX_VARIABLES] }

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME &&
        name.bytes().enumerate().all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}

impl Variables {
    pub const fn new() -> Self { Self { entries: [Variable::EMPTY; MAX_VARIABLES] } }

    pub fn set(&mut self, name: &str, value: &str) -> Result<(), &'static str> {
        if !valid_name(name) { return Err("variable name must be an identifier of 1-24 bytes"); }
        if name == "STATUS" { return Err("STATUS is a read-only shell outcome"); }
        if value.len() > MAX_VALUE { return Err("variable value exceeds 64 bytes"); }
        let slot = self.entries.iter().position(|e| e.name_len > 0 && e.name() == name)
            .or_else(|| self.entries.iter().position(|e| e.name_len == 0))
            .ok_or("variable store is full (8 values)")?;
        let mut entry = Variable::EMPTY;
        entry.name[..name.len()].copy_from_slice(name.as_bytes());
        entry.name_len = name.len() as u8;
        entry.value[..value.len()].copy_from_slice(value.as_bytes());
        entry.value_len = value.len() as u8;
        self.entries[slot] = entry;
        Ok(())
    }

    pub fn lookup(&self, name: &str) -> Option<&str> {
        self.entries.iter().find(|e| e.name_len > 0 && e.name() == name).map(Variable::value)
    }

    pub fn unset(&mut self, name: &str) -> Result<(), &'static str> {
        if name == "STATUS" { return Err("STATUS is a read-only shell outcome"); }
        let slot = self.entries.iter().position(|e| e.name_len > 0 && e.name() == name)
            .ok_or("variable is not defined")?;
        self.entries[slot] = Variable::EMPTY;
        Ok(())
    }

    /// Enumerate occupied entries by ordinal; no unused values are exposed.
    pub fn get(&self, index: usize) -> Option<(&str, &str)> {
        self.entries.iter().filter(|e| e.name_len > 0).nth(index).map(|e| (e.name(), e.value()))
    }
    pub fn len(&self) -> usize { self.entries.iter().filter(|e| e.name_len > 0).count() }
    pub fn clear(&mut self) { self.entries.fill(Variable::EMPTY); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_values_and_empty_values_are_preserved() {
        let mut v = Variables::new();
        let literal = "a | file remove secrets; $(halt)\n\t'\"";
        v.set("payload", literal).unwrap();
        assert_eq!(v.lookup("payload"), Some(literal));
        v.set("empty", "").unwrap();
        assert_eq!(v.lookup("empty"), Some(""));
        assert_eq!(v.lookup("absent"), None);
        assert_eq!(v.get(1), Some(("empty", "")));
    }

    #[test]
    fn capacity_errors_preserve_existing_values_and_reuse_slots() {
        let mut v = Variables::new();
        for name in ["a", "b", "c", "d", "e", "f", "g", "h"] { v.set(name, name).unwrap(); }
        assert!(v.set("i", "new").is_err());
        v.set("a", "updated").unwrap();
        assert_eq!(v.lookup("a"), Some("updated"));
        assert!(v.set("a", &"x".repeat(65)).is_err());
        assert_eq!(v.lookup("a"), Some("updated"));
        v.unset("b").unwrap();
        v.set("i", "new").unwrap();
        assert_eq!(v.len(), 8);
    }

    #[test]
    fn identifier_limits_and_readonly_status_are_enforced() {
        let mut v = Variables::new();
        for name in ["", "1a", "a-b", "a b", "a|b", "é", "STATUS"] { assert!(v.set(name, "x").is_err()); }
        assert!(v.set(&"a".repeat(25), "x").is_err());
        v.set("_x9", "literal").unwrap();
        v.set(&"a".repeat(24), "boundary").unwrap();
        assert!(v.unset("STATUS").is_err());
    }

    #[test]
    fn lowering_session_can_erase_all_prior_data() {
        let mut v = Variables::new();
        v.set("secret", "admin data").unwrap();
        v.clear();
        assert_eq!(v.len(), 0);
        assert_eq!(v.lookup("secret"), None);
        assert_eq!(v.get(0), None);
        v.set("public", "user data").unwrap();
        assert_eq!(v.get(0), Some(("public", "user data")));
    }
}
