//! Global `{nodeId}-{uuidv7}` identity generation and contract ID validation.
//!
//! Aligns wire identity shape with Agent Kernel (`{node_id}-{uuidv7}`) using pure
//! safe Rust without cross-workspace path dependencies so `unsafe_boundary` stays clean.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Canonical UUID string length (`8-4-4-4-12` = 36 ASCII bytes).
pub const UUID_LEN: usize = 36;
/// Maximum allowed contract ID length (`$defs.id`).
pub const MAX_ID_LEN: usize = 256;
/// Maximum allowed `nodeId` prefix length.
pub const MAX_NODE_ID_LEN: usize = 64;

/// Validated node identifier matching `^[a-z0-9-]{1,64}$`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(String);

impl NodeId {
    /// Parses and validates a `nodeId` against `^[a-z0-9-]{1,64}$`.
    pub fn new(raw: impl Into<String>) -> Result<Self, &'static str> {
        let s = raw.into();
        if !is_valid_node_id(&s) {
            return Err("nodeId must match ^[a-z0-9-]{1,64}$");
        }
        Ok(Self(s))
    }

    /// Returns the default local node identifier (`"local"`).
    pub fn local() -> Self {
        Self("local".to_owned())
    }

    /// Returns the string slice representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validates a `nodeId` string against `^[a-z0-9-]{1,64}$`.
pub fn is_valid_node_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_NODE_ID_LEN
        && raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Validates a public contract ID (`$defs.id`: `^[A-Za-z0-9][A-Za-z0-9._~-]*$`, len `1..=256`).
pub fn is_valid_contract_id(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_ID_LEN {
        return false;
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
}

/// Returns current wall-clock Unix epoch milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Formats an RFC 9562 UUIDv7 string from 48-bit timestamp, 12-bit monotonic seq, and 62-bit entropy.
pub fn format_uuid_v7(ts_ms: u64, rand_a: u16, rand_b: u64) -> String {
    let ms = ts_ms & 0x0000_FFFF_FFFF_FFFF;
    let ra = u64::from(rand_a & 0x0FFF);
    let rb = rand_b & 0x3FFF_FFFF_FFFF_FFFF;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (ms >> 16) & 0xFFFF_FFFF,
        ms & 0xFFFF,
        0x7000 | ra,
        0x8000 | ((rb >> 48) & 0x3FFF),
        rb & 0xFFFF_FFFF_FFFF
    )
}

/// Validates whether `raw` is a `{nodeId}-{uuidv7}` global ID (length <= 256).
pub fn parse_global_id(raw: &str) -> Option<(&str, &str)> {
    if raw.len() > MAX_ID_LEN || raw.len() <= UUID_LEN + 1 {
        return None;
    }
    let split = raw.len() - UUID_LEN - 1;
    if raw.as_bytes().get(split) != Some(&b'-') {
        return None;
    }
    let node = &raw[..split];
    let uuid = &raw[split + 1..];
    if !is_valid_node_id(node) || !is_canonical_uuid_v7(uuid) {
        return None;
    }
    Some((node, uuid))
}

/// Checks canonical lowercase hyphenated UUIDv7 shape (`8-4-4-4-12`, version `7`, RFC 4122/9562 variant).
pub fn is_canonical_uuid_v7(uuid: &str) -> bool {
    let b = uuid.as_bytes();
    if b.len() != UUID_LEN {
        return false;
    }
    for (idx, byte) in b.iter().enumerate() {
        if matches!(idx, 8 | 13 | 18 | 23) {
            if *byte != b'-' {
                return false;
            }
        } else if !(byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)) {
            return false;
        }
    }
    b[14] == b'7' && matches!(b[19], b'8' | b'9' | b'a' | b'b')
}

/// Thread-safe `{nodeId}-{uuidv7}` global ID generator.
#[derive(Debug)]
pub struct IdGenerator {
    node: NodeId,
    seed: u64,
    counter: AtomicU64,
}

impl IdGenerator {
    /// Creates a generator seeded from OS entropy via `std::collections::hash_map::RandomState`.
    pub fn new(node: NodeId) -> Self {
        Self::with_seed(node, entropy_seed())
    }

    /// Creates a deterministic generator for reproducible tests.
    pub fn with_seed(node: NodeId, seed: u64) -> Self {
        Self {
            node,
            seed,
            counter: AtomicU64::new(0),
        }
    }

    /// Returns the configured node prefix.
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// Generates the next global ID using wall-clock time.
    pub fn next_id(&self) -> String {
        self.next_at(now_ms())
    }

    /// Generates the next global ID at a specific millisecond timestamp.
    pub fn next_at(&self, ts_ms: u64) -> String {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let rand_a = (seq & 0x0FFF) as u16;
        let rand_b = splitmix64(self.seed ^ ts_ms.rotate_left(17) ^ seq);
        format!("{}-{}", self.node, format_uuid_v7(ts_ms, rand_a, rand_b))
    }
}

fn splitmix64(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn entropy_seed() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(now_ms());
    hasher.write_u32(std::process::id());
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_validation_matches_contract() {
        assert!(NodeId::new("local").is_ok());
        assert!(NodeId::new("ws-01-a").is_ok());
        assert!(NodeId::new("").is_err());
        assert!(NodeId::new("UPPER").is_err());
        assert!(NodeId::new("bad_underscore").is_err());
        assert!(NodeId::new("a".repeat(65)).is_err());
    }

    #[test]
    fn generator_produces_monotonic_valid_global_ids() {
        let id_gen = IdGenerator::with_seed(NodeId::new("local").unwrap(), 42);
        let a = id_gen.next_at(1_731_800_000_000);
        let b = id_gen.next_at(1_731_800_000_000);
        let c = id_gen.next_at(1_731_800_000_005);
        assert_ne!(a, b);
        assert!(a < b);
        assert!(b < c);
        assert!(is_valid_contract_id(&a));
        assert_eq!(parse_global_id(&a).unwrap().0, "local");
        assert!(parse_global_id(&b).is_some());
        assert!(parse_global_id("missing-uuid").is_none());
        assert!(parse_global_id("local-01933600-0000-4000-8000-000000000000").is_none());
    }
}
