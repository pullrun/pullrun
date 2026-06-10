use std::fmt;
use std::str::FromStr;

pub mod node;
pub mod store;

pub use node::{ArchivedDagNode, DagNode, NodeKind};
pub use store::{ArchivedNodeGuard, MmapStore, StoreError};

use serde::{Deserialize, Serialize};

/// A content-addressed digest: SHA-256 hash stored as a fixed-size
/// byte array. More efficient and type-safe than the previous
/// `type Digest = String` alias.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// All-zero bytes sentinel — never a valid SHA-256 digest.
    pub const ZERO: Digest = Digest([0u8; 32]);

    /// Create a digest from a hex string.
    pub fn from_hex(s: &str) -> Result<Self, String> {
        let s = s.strip_prefix("sha256:").unwrap_or(s);
        let bytes = hex::decode(s).map_err(|e| format!("invalid hex digest: {e}"))?;
        if bytes.len() != 32 {
            return Err(format!("digest must be 32 bytes, got {}", bytes.len()));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Digest(arr))
    }

    /// Format this digest as a hex string.
    pub fn as_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Compute the SHA-256 digest of arbitrary data.
    pub fn compute(data: &[u8]) -> Self {
        use sha2::Digest as Sha256Digest;
        let mut hasher = sha2::Sha256::new();
        sha2::digest::Digest::update(&mut hasher, data);
        let result = hasher.finalize();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&result);
        Digest(arr)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.as_hex())
    }
}

impl FromStr for Digest {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl From<[u8; 32]> for Digest {
    fn from(bytes: [u8; 32]) -> Self {
        Digest(bytes)
    }
}

impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.as_hex(), serializer)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(deserializer)?;
        Digest::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// rkyv archive support: archive as `[u8; 32]` (inline, zero-copy).
use rkyv::{Archive, Serialize as RSerialize, Deserialize as RDeserialize};

/// The archived form of `Digest` is `Archived<[u8; 32]>`, which is `[u8; 32]`.
/// `#[derive(Archive)]` on a `Digest([u8; 32])` would produce
/// `ArchivedDigest([u8; 32])`, but all the rest of the code addresses
/// archived digests through the parent `DagNode`'s edges field, which
/// currently is `Vec<ArchivedString>`. To keep the change minimal,
/// we implement Archive manually so the archived type is `[u8; 32]`,
/// allowing existing code that reads edges to work with `&[u8; 32]`.
impl Archive for Digest {
    type Archived = [u8; 32];
    type Resolver = ();

    unsafe fn resolve(&self, _pos: usize, _resolver: Self::Resolver, out: *mut Self::Archived) {
        out.write(self.0);
    }
}

impl<R: rkyv::Fallible + ?Sized> RSerialize<R> for Digest {
    fn serialize(&self, _serializer: &mut R) -> Result<Self::Resolver, <R as rkyv::Fallible>::Error> {
        Ok(())
    }
}

impl<D: rkyv::Fallible + ?Sized> RDeserialize<Digest, D> for [u8; 32] {
    fn deserialize(&self, _deserializer: &mut D) -> Result<Digest, <D as rkyv::Fallible>::Error> {
        Ok(Digest(*self))
    }
}

/// Files/directory entries below this threshold are stored inline
/// in the DAG node rather than as separate blob files.
pub const SMALL_FILE_THRESHOLD: u64 = 4096;

