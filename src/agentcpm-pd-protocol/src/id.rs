//! Global `{nodeId}-{uuidv7}` identity generation and contract ID validation.
//!
//! Aligns wire identity shape with Agent Kernel (`{node_id}-{uuidv7}`) using pure
//! safe Rust without cross-workspace path dependencies so `unsafe_boundary` stays clean.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Canonical UUID string length (`8-4-4-4-12` = 36 ASCII bytes).
pub const UUID_LEN: usize = 36;
/// Maximum allowed contract ID length (`$defs.id`).
pub const MAX_ID_LEN: usize = 256;
/// Maximum allowed `nodeId` prefix length.
pub const MAX_NODE_ID_LEN: usize = 64;

/// Validated node identifier matching `^[a-z0-9][a-z0-9-]{0,63}$`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(String);

impl NodeId {
    /// Parses and validates a `nodeId` against `^[a-z0-9][a-z0-9-]{0,63}$`.
    pub fn new(raw: impl Into<String>) -> Result<Self, &'static str> {
        let s = raw.into();
        if !is_valid_node_id(&s) {
            return Err("nodeId must match ^[a-z0-9][a-z0-9-]{0,63}$");
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

/// Validates a `nodeId` string against `^[a-z0-9][a-z0-9-]{0,63}$`.
pub fn is_valid_node_id(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_NODE_ID_LEN {
        return false;
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
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

const UNINIT_CLOCK_STATE: u64 = u64::MAX;

#[derive(Debug)]
enum EntropySource {
    OsCsprng,
    DeterministicSeed(u64),
}

/// Thread-safe `{nodeId}-{uuidv7}` global ID generator.
#[derive(Debug)]
pub struct IdGenerator {
    node: NodeId,
    source: EntropySource,
    /// Packed `(last_ms << 12) | seq` (60-bit monotonic state; `u64::MAX` = uninitialized).
    clock_state: AtomicU64,
    deterministic_counter: AtomicU64,
}

impl IdGenerator {
    /// Creates a generator drawing fresh OS CSPRNG entropy (`getrandom`) for every ID.
    pub fn new(node: NodeId) -> Self {
        Self {
            node,
            source: EntropySource::OsCsprng,
            clock_state: AtomicU64::new(UNINIT_CLOCK_STATE),
            deterministic_counter: AtomicU64::new(0),
        }
    }

    /// Creates a deterministic generator for reproducible tests.
    pub fn with_seed(node: NodeId, seed: u64) -> Self {
        Self {
            node,
            source: EntropySource::DeterministicSeed(seed),
            clock_state: AtomicU64::new(UNINIT_CLOCK_STATE),
            deterministic_counter: AtomicU64::new(0),
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

    /// Generates the next strictly monotonic global ID at `ts_ms`.
    pub fn next_at(&self, ts_ms: u64) -> String {
        let input_ms = ts_ms & 0x0000_FFFF_FFFF_FFFF;
        let (effective_ms, rand_a) = loop {
            let prev = self.clock_state.load(Ordering::Relaxed);
            let (next_ms, next_seq) = if prev == UNINIT_CLOCK_STATE {
                (input_ms, 0u16)
            } else {
                let prev_ms = prev >> 12;
                let prev_seq = (prev & 0x0FFF) as u16;
                if input_ms > prev_ms {
                    (input_ms, 0u16)
                } else if prev_seq < 0x0FFF {
                    (prev_ms, prev_seq + 1)
                } else {
                    (prev_ms.wrapping_add(1) & 0x0000_FFFF_FFFF_FFFF, 0u16)
                }
            };
            let packed = (next_ms << 12) | u64::from(next_seq);
            if self
                .clock_state
                .compare_exchange_weak(prev, packed, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                break (next_ms, next_seq);
            }
        };

        let rand_b = match self.source {
            EntropySource::OsCsprng => os_random_u64(),
            EntropySource::DeterministicSeed(seed) => {
                let n = self.deterministic_counter.fetch_add(1, Ordering::Relaxed);
                splitmix64(
                    seed ^ effective_ms.rotate_left(17) ^ u64::from(rand_a) ^ n.rotate_left(29),
                )
            }
        };
        format!(
            "{}-{}",
            self.node,
            format_uuid_v7(effective_ms, rand_a, rand_b)
        )
    }
}

fn os_random_u64() -> u64 {
    let mut buf = [0u8; 8];
    getrandom::getrandom(&mut buf).expect("OS CSPRNG getrandom failed");
    u64::from_le_bytes(buf)
}

/// Generate a 256-bit OS CSPRNG lowercase hex token (64 hex chars).
pub fn generate_csprng_token_hex() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)?;
    let mut out = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    Ok(out)
}

fn splitmix64(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_validation_matches_contract() {
        assert!(NodeId::new("local").is_ok());
        assert!(NodeId::new("ws-01-a").is_ok());
        assert!(NodeId::new("0node").is_ok());
        assert!(NodeId::new("").is_err());
        assert!(NodeId::new("-leading").is_err());
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

    #[test]
    fn generator_preserves_monotonicity_across_4096_and_clock_rollback() {
        let id_gen = IdGenerator::new(NodeId::new("local").unwrap());
        let mut prev = id_gen.next_at(1_731_800_000_000);
        assert!(is_valid_contract_id(&prev));
        for _ in 0..4105 {
            let cur = id_gen.next_at(1_731_800_000_000);
            assert!(cur > prev, "expected {cur} > {prev}");
            assert!(parse_global_id(&cur).is_some());
            prev = cur;
        }
        // Clock rollback to an earlier timestamp still produces strictly larger IDs.
        let rolled = id_gen.next_at(1_731_700_000_000);
        assert!(rolled > prev, "expected {rolled} > {prev} after rollback");
    }

    #[test]
    fn csprng_token_hex_is_64_hex_chars_and_unique() {
        let a = generate_csprng_token_hex().expect("csprng token");
        let b = generate_csprng_token_hex().expect("csprng token");
        assert_eq!(a.len(), 64);
        assert_eq!(b.len(), 64);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, b);
    }
}
