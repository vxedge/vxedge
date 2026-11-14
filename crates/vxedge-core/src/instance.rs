//! The instance slot a run holds on its host (`ARCHITECTURE.md` §6.0.5).
//!
//! Several runs share one host, and every host object a run creates — its bridge, its ports, its
//! nodes' namespaces and VMs, its 5G pods — carries the run's slot in its name. Two copies of one
//! vehicle therefore get different names, and a name still depends on nothing but its own device
//! and its slot, so a script can know it before the run starts.
//!
//! A slot is one character, `0`–`9` then `a`–`z`: one byte is what the kernel's 15-byte
//! interface-name limit can spare.

use std::fmt;

/// One run's slot on its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instance(u8);

/// What a slot may be, in the order a free one is chosen.
const SLOTS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("instance `{0}` is not a slot: a slot is one character, 0-9 or a-z")]
pub struct InstanceError(pub String);

impl Instance {
    /// The lowest slot, the one a host's first run takes.
    pub const FIRST: Instance = Instance(b'0');

    /// Every slot, lowest first.
    pub fn all() -> impl Iterator<Item = Instance> {
        SLOTS.iter().map(|b| Instance(*b))
    }

    /// The slot a one-character string names.
    pub fn parse(s: &str) -> Result<Self, InstanceError> {
        match s.as_bytes() {
            [b] if SLOTS.contains(b) => Ok(Instance(*b)),
            _ => Err(InstanceError(s.to_string())),
        }
    }

    /// The slot a character names.
    pub fn from_char(c: char) -> Option<Self> {
        u8::try_from(c)
            .ok()
            .filter(|b| SLOTS.contains(b))
            .map(Instance)
    }

    pub fn as_char(self) -> char {
        char::from(self.0)
    }
}

impl fmt::Display for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_char())
    }
}

impl std::str::FromStr for Instance {
    type Err = InstanceError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Instance::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_is_one_digit_or_lowercase_letter() {
        assert_eq!(Instance::parse("0").unwrap(), Instance::FIRST);
        assert_eq!(Instance::parse("z").unwrap().as_char(), 'z');
        for bad in ["", "00", "A", "-", " ", "é"] {
            assert!(Instance::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn there_are_36_slots_lowest_first() {
        let all: Vec<char> = Instance::all().map(Instance::as_char).collect();
        assert_eq!(all.len(), 36);
        assert_eq!(all.first(), Some(&'0'));
        assert_eq!(all.last(), Some(&'z'));
        assert_eq!(Instance::all().next(), Some(Instance::FIRST));
    }
}
