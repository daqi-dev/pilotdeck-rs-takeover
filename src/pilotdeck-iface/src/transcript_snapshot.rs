//! Verify one original Gateway transcript snapshot before exposing its values.
//! The caller must obtain `start` and pages from the same trusted Host authority;
//! these public receipts and business strings never mint or replace that authority.
use agentcpm_scheduler_protocol::{Failure, Result, context_history as h};
use pilotdeck_protocol::{JsValue, boundary_codec};
use std::collections::BTreeSet;

const MAX_ENTRIES: u64 = 100_001;
const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const MAX_SNAPSHOT_NODES: usize = 1_000_000;
fn denied(message: &'static str) -> Failure {
    Failure::new("GATEWAY_TRANSCRIPT_PROTOCOL", message)
}
fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}
fn add_nodes(value: &JsValue, previous: usize) -> Result<usize> {
    let mut count = previous;
    let mut pending = vec![value];
    while let Some(node) = pending.pop() {
        count = count
            .checked_add(1)
            .ok_or_else(|| denied("Gateway transcript node count overflow"))?;
        if count > MAX_SNAPSHOT_NODES {
            return Err(denied("Gateway transcript node capacity exceeded"));
        }
        match node {
            JsValue::Array(items) => pending.extend(items.iter()),
            JsValue::Object(object) => {
                pending.extend(object.entries().iter().map(|(_, value)| value))
            }
            _ => {}
        }
    }
    Ok(count)
}

/// A one-shot accumulator. It verifies fixed snapshot metadata, every receipt
/// and exact opaque payload before returning decoded UTF-16-preserving values.
/// Kernel authorization is external: the expected start is correspondence data.
pub struct GatewayTranscriptSnapshot {
    start: h::GatewayStartReceipt,
    upper_cursor: Option<u64>,
    total: Option<u64>,
    next_offset: u64,
    previous_cursor: u64,
    previous_revision: u64,
    terminal_seen: bool,
    attempts: BTreeSet<String>,
    bytes: usize,
    nodes: usize,
    entries: Vec<(u64, JsValue)>,
}
impl GatewayTranscriptSnapshot {
    pub fn new(start: h::GatewayStartReceipt) -> Result<Self> {
        if start.event_cursor == 0
            || start.revision == 0
            || !valid_id(&start.session)
            || !valid_id(&start.reserved_input_ref)
            || start.origin.input_ref != start.reserved_input_ref
            || !valid_id(&start.origin.principal)
            || start.origin.turn == 0
            || start.origin.generation == 0
            || start.loop_binding.kind != agentcpm_scheduler_protocol::Kind::Loop
            || !valid_id(&start.loop_binding.id)
            || !valid_id(&start.loop_binding.revision)
            || !valid_id(&start.loop_context)
            || !valid_id(&start.producer_contract)
            || !valid_id(&start.producer_instance)
            || !valid_digest(&start.event_digest)
            || start.receipt_digest != h::gateway_start_digest(&start)?
        {
            return Err(denied("original Gateway start receipt differs"));
        }
        let bytes = serde_json::to_vec(&start)
            .map_err(|e| Failure::external("ENCODE", e.to_string()))?
            .len();
        if bytes > MAX_SNAPSHOT_BYTES {
            return Err(denied("Gateway transcript start exceeds capacity"));
        }
        Ok(Self {
            start,
            upper_cursor: None,
            total: None,
            next_offset: 0,
            previous_cursor: 0,
            previous_revision: 0,
            terminal_seen: false,
            attempts: BTreeSet::new(),
            bytes,
            nodes: 0,
            entries: Vec::new(),
        })
    }
    pub fn push(&mut self, page: h::GatewayTranscriptPage) -> Result<()> {
        if self.total.is_some_and(|total| self.next_offset == total)
            || page.start != self.start
            || page.offset != self.next_offset
            || page.total == 0
            || page.total > MAX_ENTRIES
            || page.upper_cursor < self.start.event_cursor
            || self
                .upper_cursor
                .is_some_and(|value| value != page.upper_cursor)
            || self.total.is_some_and(|value| value != page.total)
            || page.entries.len() > 256
            || page.entries.is_empty() && page.offset < page.total
        {
            return Err(denied(
                "Gateway transcript page or snapshot identity differs",
            ));
        }
        let end = page
            .offset
            .checked_add(page.entries.len() as u64)
            .ok_or_else(|| denied("Gateway transcript offset overflow"))?;
        if end > page.total {
            return Err(denied("Gateway transcript page exceeds snapshot"));
        }
        // Validate the entire page before mutating the accumulated snapshot.
        let mut cursor = self.previous_cursor;
        let mut revision = self.previous_revision;
        let mut terminal = self.terminal_seen;
        let mut bytes = self.bytes;
        let mut nodes = self.nodes;
        let mut new_attempts = BTreeSet::new();
        let mut values = Vec::with_capacity(page.entries.len());
        for (index, entry) in page.entries.iter().enumerate() {
            let ordinal = page.offset + index as u64;
            if entry.ordinal != ordinal
                || entry.event_cursor <= cursor
                || entry.event_cursor > page.upper_cursor
                || entry.event.profile != boundary_codec::PROFILE
                || terminal
            {
                return Err(denied(
                    "Gateway transcript order, profile or terminal differs",
                ));
            }
            let event_digest = h::gateway_event_digest(&entry.event)?;
            let receipt_bytes = if ordinal == 0 {
                if entry.receipt.is_some()
                    || entry.event_cursor != self.start.event_cursor
                    || event_digest != self.start.event_digest
                {
                    return Err(denied("Gateway start event differs from its receipt"));
                }
                revision = self.start.revision;
                0
            } else {
                let receipt = entry
                    .receipt
                    .as_ref()
                    .ok_or_else(|| denied("Gateway transcript receipt missing"))?;
                if receipt.start_digest != self.start.receipt_digest
                    || !valid_id(&receipt.attempt_id)
                    || self.attempts.contains(&receipt.attempt_id)
                    || !new_attempts.insert(receipt.attempt_id.clone())
                    || receipt.ordinal != ordinal
                    || receipt.event_cursor != entry.event_cursor
                    || receipt.revision <= revision
                    || receipt.event_digest != event_digest
                    || receipt.receipt_digest != h::gateway_transcript_digest(receipt)?
                    || receipt.terminal_cursor == Some(0)
                {
                    return Err(denied("Gateway transcript receipt differs"));
                }
                revision = receipt.revision;
                terminal = receipt.terminal_cursor.is_some();
                serde_json::to_vec(receipt)
                    .map_err(|e| Failure::external("ENCODE", e.to_string()))?
                    .len()
            };
            bytes = bytes
                .checked_add(receipt_bytes)
                .and_then(|n| n.checked_add(entry.event.bytes.len()))
                .ok_or_else(|| denied("Gateway transcript snapshot size overflow"))?;
            if bytes > MAX_SNAPSHOT_BYTES {
                return Err(denied("Gateway transcript snapshot exceeds capacity"));
            }
            let value = boundary_codec::decode(&entry.event.bytes)
                .map_err(|_| denied("Gateway transcript bytes are invalid"))?;
            nodes = add_nodes(&value, nodes)?;
            values.push((entry.event_cursor, value));
            cursor = entry.event_cursor;
        }
        self.upper_cursor = Some(page.upper_cursor);
        self.total = Some(page.total);
        self.next_offset = end;
        self.previous_cursor = cursor;
        self.previous_revision = revision;
        self.terminal_seen = terminal;
        self.bytes = bytes;
        self.nodes = nodes;
        self.attempts.extend(new_attempts);
        self.entries.extend(values);
        Ok(())
    }
    pub fn finish(self) -> Result<Vec<JsValue>> {
        if self.total != Some(self.next_offset) {
            return Err(denied("Gateway transcript snapshot is incomplete"));
        }
        Ok(self.entries.into_iter().map(|(_, value)| value).collect())
    }
    #[cfg(feature = "host-composition")]
    fn finish_ordered(self) -> Result<(Vec<(u64, JsValue)>, usize)> {
        if self.total != Some(self.next_offset) {
            return Err(denied("Gateway transcript snapshot is incomplete"));
        }
        Ok((self.entries, self.bytes))
    }
}

/// Read through the actual root-held Host capability. Public receipts only
/// verify correspondence; the same original Host session authorizes each page.
#[cfg(feature = "host-composition")]
pub async fn read_gateway_session_history(
    source: &agentcpm_host::TrustedGatewaySessionHistory,
) -> Result<Vec<JsValue>> {
    let upper = source.upper_cursor();
    let total = source.total();
    if total > MAX_ENTRIES {
        return Err(denied("Gateway session start count exceeds capacity"));
    }
    let mut offset = 0u64;
    let mut starts = Vec::new();
    let mut previous = 0u64;
    let mut receipts_bytes = 0usize;
    while offset < total {
        let page = source.starts(offset, 256).await?;
        if page.upper_cursor != upper
            || page.total != total
            || page.offset != offset
            || page.entries.is_empty()
            || page.entries.len() > 256
        {
            return Err(denied("Gateway session index differs"));
        }
        let count = page.entries.len() as u64;
        offset = offset
            .checked_add(count)
            .ok_or_else(|| denied("Gateway session offset overflow"))?;
        if offset > total {
            return Err(denied("Gateway session index exceeds snapshot"));
        }
        for start in page.entries {
            if start.event_cursor <= previous || start.event_cursor > upper {
                return Err(denied("Gateway session start cursor differs"));
            }
            receipts_bytes = receipts_bytes
                .checked_add(
                    serde_json::to_vec(&start)
                        .map_err(|e| Failure::external("ENCODE", e.to_string()))?
                        .len(),
                )
                .ok_or_else(|| denied("Gateway session index size overflow"))?;
            if receipts_bytes > MAX_SNAPSHOT_BYTES {
                return Err(denied("Gateway session index exceeds capacity"));
            }
            previous = start.event_cursor;
            starts.push(start);
        }
    }
    let mut entries = Vec::new();
    let mut bytes = receipts_bytes;
    let mut nodes = 0usize;
    for start in starts {
        let mut turn = GatewayTranscriptSnapshot::new(start.clone())?;
        let mut next = 0u64;
        loop {
            let page = source.turn(&start, next, 256).await?;
            if page.upper_cursor != upper {
                return Err(denied("Gateway session upper cursor differs"));
            }
            let total = page.total;
            let count = page.entries.len() as u64;
            turn.push(page)?;
            next = next
                .checked_add(count)
                .ok_or_else(|| denied("Gateway turn offset overflow"))?;
            if next == total {
                break;
            }
        }
        let (turn_entries, turn_bytes) = turn.finish_ordered()?;
        bytes = bytes
            .checked_add(turn_bytes)
            .ok_or_else(|| denied("Gateway session size overflow"))?;
        if bytes > MAX_SNAPSHOT_BYTES
            || entries.len().saturating_add(turn_entries.len()) > MAX_ENTRIES as usize
        {
            return Err(denied("Gateway session snapshot exceeds capacity"));
        }
        for (_, value) in &turn_entries {
            nodes = add_nodes(value, nodes)?;
        }
        entries.extend(turn_entries);
    }
    entries.sort_unstable_by_key(|(cursor, _)| *cursor);
    if entries.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(denied("Gateway session event cursors overlap"));
    }
    Ok(entries.into_iter().map(|(_, value)| value).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_scheduler_protocol as p;
    use pilotdeck_protocol::JsString;

    fn fixture() -> (
        h::GatewayStartReceipt,
        h::GatewayTranscriptPage,
        h::GatewayTranscriptPage,
        Vec<JsValue>,
    ) {
        let values = vec![
            JsValue::String(JsString::from_utf16(vec![0xd800, 0, 0xdc00])),
            JsValue::Bool(true),
        ];
        let first = h::OpaquePayload {
            profile: boundary_codec::PROFILE.into(),
            bytes: boundary_codec::encode(&values[0]).unwrap(),
        };
        let second = h::OpaquePayload {
            profile: boundary_codec::PROFILE.into(),
            bytes: boundary_codec::encode(&values[1]).unwrap(),
        };
        let mut start = h::GatewayStartReceipt {
            session: "session".into(),
            reserved_input_ref: "run-input".into(),
            origin: p::InputOrigin {
                principal: "alice".into(),
                turn: 1,
                generation: 1,
                input_ref: "run-input".into(),
            },
            loop_binding: p::Binding {
                id: "loop".into(),
                revision: "rev".into(),
                kind: p::Kind::Loop,
                effect: p::Effect::Read,
                permissions: p::Permissions::calls([]),
            },
            loop_context: "pilotdeck.turn".into(),
            producer_contract: "trusted-gateway".into(),
            producer_instance: "instance".into(),
            event_digest: h::gateway_event_digest(&first).unwrap(),
            receipt_digest: String::new(),
            revision: 1,
            event_cursor: 1,
        };
        start.receipt_digest = h::gateway_start_digest(&start).unwrap();
        let mut receipt = h::GatewayTranscriptReceipt {
            start_digest: start.receipt_digest.clone(),
            attempt_id: "kernel-attempt".into(),
            ordinal: 1,
            terminal_cursor: Some(8),
            event_digest: h::gateway_event_digest(&second).unwrap(),
            revision: 3,
            event_cursor: 4,
            receipt_digest: String::new(),
        };
        receipt.receipt_digest = h::gateway_transcript_digest(&receipt).unwrap();
        let first_page = h::GatewayTranscriptPage {
            start: start.clone(),
            upper_cursor: 6,
            total: 2,
            offset: 0,
            entries: vec![h::GatewayTranscriptEntry {
                event_cursor: 1,
                ordinal: 0,
                receipt: None,
                event: first,
            }],
        };
        let second_page = h::GatewayTranscriptPage {
            start: start.clone(),
            upper_cursor: 6,
            total: 2,
            offset: 1,
            entries: vec![h::GatewayTranscriptEntry {
                event_cursor: 4,
                ordinal: 1,
                receipt: Some(receipt),
                event: second,
            }],
        };
        (start, first_page, second_page, values)
    }
    #[test]
    fn fixed_pages_verify_receipts_and_preserve_original_utf16() {
        let (start, first, second, values) = fixture();
        let mut reader = GatewayTranscriptSnapshot::new(start).unwrap();
        reader.push(first).unwrap();
        reader.push(second).unwrap();
        assert_eq!(reader.finish().unwrap(), values);
    }
    #[cfg(feature = "host-composition")]
    #[test]
    fn turn_decode_retains_original_history_cursors() {
        let (start, first, second, values) = fixture();
        let mut reader = GatewayTranscriptSnapshot::new(start).unwrap();
        reader.push(first).unwrap();
        reader.push(second).unwrap();
        let (entries, _) = reader.finish_ordered().unwrap();
        assert_eq!(
            entries,
            vec![(1, values[0].clone()), (4, values[1].clone())]
        );
    }
    #[test]
    fn rejects_mixed_cursor_forged_payload_and_incomplete_snapshot_without_partial_advance() {
        let (start, first, second, values) = fixture();
        let mut reader = GatewayTranscriptSnapshot::new(start.clone()).unwrap();
        reader.push(first.clone()).unwrap();
        let mut changed = second.clone();
        changed.upper_cursor += 1;
        assert!(reader.push(changed).is_err());
        let mut forged = second.clone();
        forged.entries[0].event.bytes[0] ^= 1;
        assert!(reader.push(forged).is_err());
        let mut bad_offset = second.clone();
        bad_offset.offset = 0;
        assert!(reader.push(bad_offset).is_err());
        reader.push(second).unwrap();
        assert_eq!(reader.finish().unwrap(), values);
        let mut incomplete = GatewayTranscriptSnapshot::new(start).unwrap();
        incomplete.push(first).unwrap();
        assert!(incomplete.finish().is_err());
    }
    #[test]
    fn rejects_false_start_and_any_event_after_terminal_receipt() {
        let (start, first, mut second, _) = fixture();
        let mut forged = start.clone();
        forged.origin.principal = "mallory".into();
        assert!(GatewayTranscriptSnapshot::new(forged).is_err());
        let mut reader = GatewayTranscriptSnapshot::new(start.clone()).unwrap();
        let mut first = first;
        first.total = 3;
        second.total = 3;
        reader.push(first).unwrap();
        reader.push(second.clone()).unwrap();
        let mut later = second;
        later.offset = 2;
        later.entries[0].ordinal = 2;
        later.entries[0].event_cursor = 5;
        assert!(reader.push(later).is_err());
        assert!(reader.finish().is_err());
    }
    #[test]
    fn rejects_duplicate_attempt_even_when_receipt_digests_are_self_consistent() {
        let (start, mut first, mut second, _) = fixture();
        first.total = 3;
        second.total = 3;
        let receipt = second.entries[0].receipt.as_mut().unwrap();
        receipt.terminal_cursor = None;
        receipt.receipt_digest = h::gateway_transcript_digest(receipt).unwrap();
        let mut reader = GatewayTranscriptSnapshot::new(start).unwrap();
        reader.push(first).unwrap();
        reader.push(second.clone()).unwrap();
        second.offset = 2;
        second.entries[0].ordinal = 2;
        second.entries[0].event_cursor = 5;
        let receipt = second.entries[0].receipt.as_mut().unwrap();
        receipt.ordinal = 2;
        receipt.revision = 4;
        receipt.event_cursor = 5;
        receipt.receipt_digest = h::gateway_transcript_digest(receipt).unwrap();
        assert!(reader.push(second).is_err());
    }
    #[test]
    fn a_late_invalid_entry_cannot_partially_advance_the_page() {
        let (start, first, second, values) = fixture();
        let mut combined = first.clone();
        combined.entries.extend(second.entries.clone());
        combined.entries[1].event.bytes[0] ^= 1;
        let mut reader = GatewayTranscriptSnapshot::new(start).unwrap();
        assert!(reader.push(combined).is_err());
        reader.push(first).unwrap();
        reader.push(second).unwrap();
        assert_eq!(reader.finish().unwrap(), values);
    }
}
