//! The asset GUID — this crate's fundamental identity key.
//!
//! Stored as a `u128`; renders as Unity's 32-char lowercase hex (the form
//! written in `.meta` files and asset references). Encodes transparently
//! under bincode (a newtype struct emits just its field), so the on-disk
//! `asset-db.bin` layout is identical to the bare `u128` it replaces.

use std::fmt;
use std::num::ParseIntError;
use std::str::FromStr;

use bincode::{Decode, Encode};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct Guid(u128);

impl Guid {
    pub fn from_u128(raw: u128) -> Self {
        Self(raw)
    }

    pub fn as_u128(self) -> u128 {
        self.0
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// Parse from lowercase hex (Unity's `.meta` form). Accepts any valid hex
/// width; the canonical Unity guid is 32 chars.
impl FromStr for Guid {
    type Err = ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        u128::from_str_radix(s, 16).map(Self)
    }
}
