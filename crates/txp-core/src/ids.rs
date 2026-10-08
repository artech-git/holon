//! Identifiers shared by every layer: transactions, participants and log positions.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Transaction identifier. 128 random bits; rendered as 32 hex chars.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TxId(pub u128);

impl Serialize for TxId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{:032x}", self.0))
    }
}
impl<'de> Deserialize<'de> for TxId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        TxId::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("bad txid {s:?}")))
    }
}

impl TxId {
    /// Generate a fresh id from the OS entropy pool.
    ///
    /// Reading `/dev/urandom` keeps this crate free of an RNG dependency; in
    /// simulation the caller can construct ids deterministically via `TxId(n)`.
    pub fn generate() -> TxId {
        use std::io::Read;
        let mut buf = [0u8; 16];
        let mut f = std::fs::File::open("/dev/urandom").expect("open /dev/urandom");
        f.read_exact(&mut buf).expect("read /dev/urandom");
        TxId(u128::from_be_bytes(buf))
    }

    /// Parse the hexadecimal form produced by `Display`. Surrounding
    /// whitespace is ignored; leading zeros are optional.
    pub fn parse(s: &str) -> Option<TxId> {
        u128::from_str_radix(s.trim(), 16).ok().map(TxId)
    }
}

impl fmt::Display for TxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}
impl fmt::Debug for TxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TxId({:032x})", self.0)
    }
}

/// Stable identity of a participant, persisted in the decision log.
/// Convention: `<kind>:<name>`, e.g. `fs:site`, `proc:build`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct ParticipantId(pub String);

impl ParticipantId {
    /// Wrap a `<kind>:<name>` string.
    pub fn new(s: impl Into<String>) -> Self {
        ParticipantId(s.into())
    }
    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Log sequence number. Monotonic within one log; in Phase 4 it becomes
/// the Raft log index.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Debug, Default)]
#[serde(transparent)]
pub struct Lsn(pub u64);

impl Lsn {
    /// The position before any record. A fresh log hands out `ZERO.next()` first.
    pub const ZERO: Lsn = Lsn(0);
    /// The position immediately after this one.
    pub fn next(self) -> Lsn {
        Lsn(self.0 + 1)
    }
}
impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
