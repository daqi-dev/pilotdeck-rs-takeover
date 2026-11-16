//! Public Kernel ifACE [`ContextContract`] and [`CompactionPersistenceContract`]
//! adapters backed by [`StoreContract`], [`AuthoritativeContextLedger`],
//! `pilotdeck-context-core`, and `pilotdeck-tokenizer`.
//!
//! # Honest Boundary
//! This module bridges [`agentcpm_iface::store::StoreContract`],
//! [`AuthoritativeContextLedger`], and `pilotdeck_context_core` projections cleanly.
//! End-to-end non-Recovery Gateway Context replacement and full five-operation
//! host orchestration still live in `pilotdeck-gateway`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use agentcpm_iface::BoxFuture;
use agentcpm_iface::context::{
    CompactionCommit, CompactionPersistenceContract, ContextContract, ContextSnapshot,
    LoadedCompaction,
};
use agentcpm_iface::store::{EventRecord, StoreContract};
use agentcpm_iface::types::{CallMeta, Cursor, Error, ErrorCategory, SessionId, TraceId, TurnId};
use pilotdeck_context_core::wire::{content, field, is_block, role, string_field};
use pilotdeck_context_core::{
    MicroCompactionEngine, MicroCompactionEngineOptions, MicroCompactionInput, SnipCallOptions,
    SnipEngine, SnipEngineOptions,
};
use pilotdeck_model_core::codec::projection::flatten_tool_result_block_text;
use pilotdeck_protocol::{JsString, JsValue, stringify_json};
use pilotdeck_tokenizer::{TokenizeError, count_tokens};
use serde_json::Value;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use crate::state_authority::{
    AuthoritativeContextLedger, extract_messages_from_payload, hex_encode,
};
use crate::token_counter::MESSAGES_PER_TURN_OVERHEAD;

/// Declared media type for [`PilotDeckContextContract::assemble`] snapshots.
pub const SNAPSHOT_CONTENT_TYPE: &str = "application/vnd.pilotdeck.context-snapshot+json";

/// Event kind used when recording registered capabilities/tools in `StoreContract`.
pub const KIND_TOOL_REGISTERED: &str = "tool.registered";

/// Event kind used for write-ahead trace persistence (`ContextContract::trace_input`).
pub const KIND_CONTEXT_INPUT: &str = "context.input";

/// Event kind used for compaction persistence (`CompactionPersistenceContract::commit`).
pub const KIND_COMPACTION: &str = "context.compaction";

/// Event kinds inspected when rebuilding declared capabilities from session metadata.
const KIND_SESSION_SPEC: &str = "session.spec";
const KIND_SESSION_METADATA: &str = "session.metadata";
const KIND_CONTEXT_CAPABILITIES: &str = "context.capabilities";

/// Maximum compaction boundary blob size (4 MiB).
pub const MAX_BOUNDARY_BYTES: usize = 4 * 1024 * 1024;

/// Maximum compaction messages blob size (8 MiB).
pub const MAX_MESSAGES_BYTES: usize = 8 * 1024 * 1024;

/// Maximum write-ahead trace input payload size (8 MiB).
pub const MAX_TRACE_BYTES: usize = 8 * 1024 * 1024;

const READ_PAGE: usize = 256;
const DEFAULT_MAX_EVENTS: usize = 512;
const COMPACTION_SCHEMA_VERSION: u64 = 1;
const COMPACTION_DIGEST_DOMAIN: &[u8] = b"pilotdeck.compaction.v1";
const MULTIMEDIA_FALLBACK_TOKENS: usize = 2_000;

/// Compute the deterministic domain-separated SHA-256 hex digest over
/// `turn`, `boundary`, and `messages`.
pub fn compute_compaction_digest(turn: u64, boundary: &[u8], messages: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(COMPACTION_DIGEST_DOMAIN);
    hasher.update(turn.to_le_bytes());
    hasher.update((boundary.len() as u64).to_le_bytes());
    hasher.update(boundary);
    hasher.update((messages.len() as u64).to_le_bytes());
    hasher.update(messages);
    hex_encode(&hasher.finalize())
}

/// PilotDeck adapter implementing [`agentcpm_iface::context::ContextContract`].
///
/// Reads session events from an injected [`StoreContract`] and (when attached)
/// reads the session's [`AuthoritativeContextLedger`], applying configured
/// `pilotdeck_context_core` micro/snip projections into a deterministic JSON
/// snapshot without mutating authoritative state during `assemble`.
pub struct PilotDeckContextContract {
    store: Arc<dyn StoreContract>,
    ledgers: Mutex<BTreeMap<SessionId, AuthoritativeContextLedger>>,
    capabilities: Vec<(String, String)>,
    micro_options: Option<MicroCompactionEngineOptions>,
    snip_options: Option<SnipEngineOptions>,
    snip_call_options: SnipCallOptions,
    max_events: usize,
}

impl PilotDeckContextContract {
    /// Construct a context contract adapter backed by `store`.
    pub fn new(store: Arc<dyn StoreContract>) -> Self {
        Self {
            store,
            ledgers: Mutex::new(BTreeMap::new()),
            capabilities: Vec::new(),
            micro_options: None,
            snip_options: None,
            snip_call_options: SnipCallOptions::default(),
            max_events: DEFAULT_MAX_EVENTS,
        }
    }

    /// Bind an [`AuthoritativeContextLedger`] for its session (chainable builder).
    pub fn with_ledger(self, ledger: AuthoritativeContextLedger) -> Self {
        self.register_ledger(ledger);
        self
    }

    /// Register or replace the [`AuthoritativeContextLedger`] for its session.
    pub fn register_ledger(&self, ledger: AuthoritativeContextLedger) {
        let mut guard = self
            .ledgers
            .lock()
            .expect("context contract ledger map poisoned");
        guard.insert(ledger.session_id().to_string(), ledger);
    }

    /// Declare baseline `(name, version)` capabilities exposed by this context.
    pub fn with_capabilities(mut self, capabilities: Vec<(String, String)>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Enable non-mutating `MicroCompactionEngine` projection during `assemble`.
    pub fn with_micro_options(mut self, options: MicroCompactionEngineOptions) -> Self {
        self.micro_options = Some(options);
        self
    }

    /// Enable non-mutating `SnipEngine` projection during `assemble`.
    pub fn with_snip_options(mut self, options: SnipEngineOptions) -> Self {
        self.snip_options = Some(options);
        self
    }

    /// Configure call options passed to `SnipEngine::snip` during `assemble`.
    pub fn with_snip_call_options(mut self, options: SnipCallOptions) -> Self {
        self.snip_call_options = options;
        self
    }

    /// Bound the number of trailing store events embedded in assembled snapshots.
    pub fn with_max_events(mut self, max_events: usize) -> Self {
        self.max_events = max_events;
        self
    }

    fn ledger_for(&self, session: &str) -> Option<AuthoritativeContextLedger> {
        let guard = self
            .ledgers
            .lock()
            .expect("context contract ledger map poisoned");
        guard.get(session).cloned()
    }
}

impl ContextContract for PilotDeckContextContract {
    fn assemble(
        &self,
        session: &SessionId,
        turn: TurnId,
    ) -> BoxFuture<'_, Result<ContextSnapshot, Error>> {
        let session = session.clone();
        let store = Arc::clone(&self.store);
        let ledger = self.ledger_for(&session);
        let base_caps = self.capabilities.clone();
        let micro_options = self.micro_options.clone();
        let snip_options = self.snip_options.clone();
        let snip_call_options = self.snip_call_options.clone();
        let max_events = self.max_events;

        Box::pin(async move {
            validate_session_id(&session)?;
            let events = read_all_validated(&*store, &session, "CONTEXT_STORE_CORRUPT").await?;
            let caps_versions = collect_declared_capabilities(&base_caps, &events);
            let cap_names: Vec<String> =
                caps_versions.iter().map(|(name, _)| name.clone()).collect();

            let start = events.len().saturating_sub(max_events);
            let event_items: Vec<Value> = events[start..]
                .iter()
                .map(|event| {
                    let mut item = serde_json::json!({
                        "cursor": event.cursor,
                        "kind": event.kind,
                    });
                    match event.payload_str() {
                        Ok(text) => item["text"] = Value::String(text.to_string()),
                        Err(_) => {
                            item["bytes_len"] = Value::from(event.payload.len());
                            item["sha256"] = Value::String(sha256_hex(&event.payload));
                        }
                    }
                    item
                })
                .collect();

            let (revision, ledger_json) = if let Some(ledger) = ledger {
                let view = ledger.snapshot();
                let projection = project_ledger_view(
                    &view,
                    micro_options.as_ref(),
                    snip_options.as_ref(),
                    &snip_call_options,
                )?;
                (view.revision, Some(projection))
            } else {
                (0, None)
            };

            let mut snapshot_doc = serde_json::json!({
                "profile": "pilotdeck.context.snapshot.v1",
                "session": session,
                "turn": turn,
                "revision": revision,
                "has_ledger": ledger_json.is_some(),
                "events": event_items,
                "capabilities": cap_names,
            });
            if let Some(ledger_doc) = ledger_json
                && let Value::Object(ref mut map) = snapshot_doc
            {
                map.insert("ledger".to_string(), ledger_doc);
            }

            let payload_bytes = serde_json::to_vec(&snapshot_doc)
                .map_err(|err| internal_app("CONTEXT_SNAPSHOT_ENCODE", err.to_string()))?;

            Ok(
                ContextSnapshot::new(session, turn, payload_bytes, caps_versions)
                    .with_content_type(Some(SNAPSHOT_CONTENT_TYPE)),
            )
        })
    }

    fn rebuild_capabilities(
        &self,
        session: &SessionId,
    ) -> BoxFuture<'_, Result<Vec<String>, Error>> {
        let session = session.clone();
        let store = Arc::clone(&self.store);
        let base_caps = self.capabilities.clone();
        Box::pin(async move {
            validate_session_id(&session)?;
            let events = read_all_validated(&*store, &session, "CONTEXT_STORE_CORRUPT").await?;
            let caps = collect_declared_capabilities(&base_caps, &events);
            Ok(caps.into_iter().map(|(name, _)| name).collect())
        })
    }

    fn trace_input(
        &self,
        session: &SessionId,
        turn: TurnId,
        meta: &CallMeta,
        payload: &[u8],
    ) -> BoxFuture<'_, Result<TraceId, Error>> {
        let session = session.clone();
        let meta = meta.clone();
        let payload = payload.to_vec();
        let store = Arc::clone(&self.store);

        Box::pin(async move {
            validate_session_id(&session)?;
            if payload.len() > MAX_TRACE_BYTES {
                return Err(Error::new(
                    ErrorCategory::Protocol,
                    -32602,
                    format!(
                        "CONTEXT_TRACE_TOO_LARGE: payload={}B exceeds {MAX_TRACE_BYTES}B",
                        payload.len()
                    ),
                ));
            }
            // Write-ahead invariant: refuse to append on top of a corrupt checkpoint anchor.
            store.verify_checkpoint(&session).await?;

            let payload_sha256 = sha256_hex(&payload);
            let payload_utf8 = std::str::from_utf8(&payload).ok().map(str::to_owned);
            let record = serde_json::json!({
                "version": 1,
                "session": meta.session_id,
                "requested_session": session,
                "call_id": meta.call_id,
                "meta_turn": meta.turn,
                "turn": turn,
                "payload_len": payload.len(),
                "payload_sha256": payload_sha256,
                "payload_utf8": payload_utf8,
                "payload_hex": hex_encode(&payload),
            });
            let bytes = serde_json::to_vec(&record)
                .map_err(|err| internal_app("CONTEXT_TRACE_ENCODE", err.to_string()))?;

            let cursor = store
                .append(EventRecord::new(session.clone(), KIND_CONTEXT_INPUT, bytes))
                .await?;
            if cursor == 0 {
                return Err(internal_app(
                    "CONTEXT_TRACE_STORE",
                    "store returned invalid cursor 0 for trace_input",
                ));
            }

            let session_tag = &sha256_hex(session.as_bytes())[..8];
            Ok(format!(
                "trace-{session_tag}-{turn}-{cursor}-{}",
                &payload_sha256[..12]
            ))
        })
    }
}

/// PilotDeck adapter implementing [`agentcpm_iface::context::CompactionPersistenceContract`]
/// over [`agentcpm_iface::store::StoreContract`].
///
/// # Honest Boundary
/// Persists and fail-closed verifies compaction boundaries and compressed message
/// blobs in `StoreContract`. End-to-end non-Recovery Gateway Context replacement
/// still lives in `pilotdeck-gateway`.
pub struct PilotDeckCompactionPersistence {
    store: Arc<dyn StoreContract>,
}

impl PilotDeckCompactionPersistence {
    /// Construct a compaction persistence adapter backed by `store`.
    pub fn new(store: Arc<dyn StoreContract>) -> Self {
        Self { store }
    }
}

impl CompactionPersistenceContract for PilotDeckCompactionPersistence {
    fn commit(
        &self,
        session: &SessionId,
        turn: u64,
        boundary: &[u8],
        messages: &[u8],
    ) -> BoxFuture<'_, Result<CompactionCommit, Error>> {
        let session = session.clone();
        let boundary = boundary.to_vec();
        let messages = messages.to_vec();
        let store = Arc::clone(&self.store);

        Box::pin(async move {
            validate_session_id(&session)?;
            if boundary.len() > MAX_BOUNDARY_BYTES || messages.len() > MAX_MESSAGES_BYTES {
                return Err(Error::new(
                    ErrorCategory::Protocol,
                    -32602,
                    format!(
                        "COMPACTION_TOO_LARGE: boundary={}B messages={}B (limits {MAX_BOUNDARY_BYTES}/{MAX_MESSAGES_BYTES})",
                        boundary.len(),
                        messages.len()
                    ),
                ));
            }

            store.verify_checkpoint(&session).await?;

            let digest = compute_compaction_digest(turn, &boundary, &messages);
            if digest.len() != 64 || digest != compute_compaction_digest(turn, &boundary, &messages)
            {
                return Err(internal_app(
                    "COMPACTION_DIGEST_ERROR",
                    "computed compaction digest failed self-check",
                ));
            }

            let header = serde_json::json!({
                "version": COMPACTION_SCHEMA_VERSION,
                "turn": turn,
                "digest": digest,
                "boundary_len": boundary.len(),
                "messages_len": messages.len(),
            })
            .to_string();

            let mut payload =
                Vec::with_capacity(header.len() + 1 + boundary.len() + messages.len());
            payload.extend_from_slice(header.as_bytes());
            payload.push(b'\n');
            payload.extend_from_slice(&boundary);
            payload.extend_from_slice(&messages);

            let cursor = store
                .append(EventRecord::new(session.clone(), KIND_COMPACTION, payload))
                .await?;
            if cursor == 0 {
                return Err(internal_app(
                    "COMPACTION_STORE_ERROR",
                    "store returned invalid cursor 0 for compaction commit",
                ));
            }

            Ok(CompactionCommit::new(
                session,
                turn,
                digest,
                cursor,
                boundary.len(),
                messages.len(),
            ))
        })
    }

    fn latest(
        &self,
        session: &SessionId,
    ) -> BoxFuture<'_, Result<Option<CompactionCommit>, Error>> {
        let session = session.clone();
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            validate_session_id(&session)?;
            let events = read_all_validated(&*store, &session, "COMPACTION_CORRUPT").await?;
            let Some(event) = events.iter().rev().find(|e| e.kind == KIND_COMPACTION) else {
                return Ok(None);
            };
            let (commit, _, _) = parse_compaction_event(&session, event)?;
            Ok(Some(commit))
        })
    }

    fn load(&self, session: &SessionId) -> BoxFuture<'_, Result<Option<LoadedCompaction>, Error>> {
        let session = session.clone();
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            validate_session_id(&session)?;
            let events = read_all_validated(&*store, &session, "COMPACTION_CORRUPT").await?;
            let Some(event) = events.iter().rev().find(|e| e.kind == KIND_COMPACTION) else {
                return Ok(None);
            };
            let (commit, boundary, messages) = parse_compaction_event(&session, event)?;
            Ok(Some(LoadedCompaction::new(commit, boundary, messages)))
        })
    }
}

fn validate_session_id(session: &str) -> Result<(), Error> {
    if session.is_empty() {
        return Err(Error::new(
            ErrorCategory::Protocol,
            -32602,
            "CONTEXT_INVALID_SESSION: session id must not be empty",
        ));
    }
    Ok(())
}

fn internal_app(reason: &str, detail: impl AsRef<str>) -> Error {
    Error::app(ErrorCategory::Internal, reason, detail)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

fn project_ledger_view(
    view: &crate::state_authority::LedgerView,
    micro_options: Option<&MicroCompactionEngineOptions>,
    snip_options: Option<&SnipEngineOptions>,
    snip_call_options: &SnipCallOptions,
) -> Result<Value, Error> {
    let extracted = extract_messages_from_payload(&view.payload)
        .map_err(|err| internal_app("CONTEXT_LEDGER_DECODE", err.to_string()))?;

    let mut base = serde_json::json!({
        "revision": view.revision,
        "profile": view.payload.profile,
        "content_type": view.payload.content_type,
        "bytes_len": view.payload.bytes.len(),
        "sha256": sha256_hex(&view.payload.bytes),
        "consumed_inputs": view.consumed_inputs.iter().cloned().collect::<Vec<_>>(),
        "facts_count": view.facts.len(),
        "has_compaction": view.latest_compaction.is_some(),
    });

    if let Some(messages) = extracted {
        let mut working = messages;
        let mut micro_applied = false;
        let mut micro_rewritten = 0usize;
        if let Some(opts) = micro_options {
            let input = MicroCompactionInput::new(&working);
            let res = MicroCompactionEngine::new(opts.clone())
                .apply(
                    &input,
                    &|text| count_tokens(text),
                    &flatten_tool_result_block_text,
                )
                .map_err(|err| internal_app("CONTEXT_MICRO_PROJECTION", format!("{err:?}")))?;
            micro_applied = res.rewritten > 0;
            micro_rewritten = res.rewritten;
            working = res.messages;
        }

        let mut snip_applied = false;
        let mut turns_snipped = 0usize;
        if let Some(opts) = snip_options {
            let res = SnipEngine::new(opts.clone())
                .snip(
                    &working,
                    snip_call_options.clone(),
                    &estimate_wire_messages_tokens,
                )
                .map_err(|err| internal_app("CONTEXT_SNIP_PROJECTION", format!("{err:?}")))?;
            snip_applied = res.applied;
            turns_snipped = res.turns_snipped;
            working = res.messages;
        }

        let json_text = stringify_json(&JsValue::Array(working))
            .map_err(|err| internal_app("CONTEXT_JSON_ENCODE", err.to_string()))?
            .unwrap_or_else(|| "[]".to_string());
        let raw_messages = RawValue::from_string(json_text)
            .map_err(|err| internal_app("CONTEXT_JSON_ENCODE", err.to_string()))?;
        let messages_val = serde_json::to_value(raw_messages)
            .map_err(|err| internal_app("CONTEXT_JSON_ENCODE", err.to_string()))?;

        if let Value::Object(ref mut map) = base {
            map.insert("messages".to_string(), messages_val);
            map.insert(
                "projections".to_string(),
                serde_json::json!({
                    "micro_applied": micro_applied,
                    "micro_rewritten": micro_rewritten,
                    "snip_applied": snip_applied,
                    "turns_snipped": turns_snipped,
                }),
            );
        }
    } else if let Value::Object(ref mut map) = base {
        // Preserve opaque non-UTF-8 payloads losslessly in hex (`TC-03`).
        map.insert(
            "bytes_hex".to_string(),
            Value::String(hex_encode(&view.payload.bytes)),
        );
    }

    Ok(base)
}

fn estimate_wire_messages_tokens(messages: &[JsValue]) -> Result<usize, TokenizeError> {
    let mut total = 0usize;
    for msg in messages {
        total = total.saturating_add(MESSAGES_PER_TURN_OVERHEAD as usize);
        if let Some(r) = role(msg) {
            total = total.saturating_add(count_tokens(r)?);
        }
        for block in content(msg) {
            total = total.saturating_add(estimate_wire_block_tokens(block)?);
        }
    }
    Ok(total)
}

fn estimate_wire_block_tokens(block: &JsValue) -> Result<usize, TokenizeError> {
    if is_block(block, "text") || is_block(block, "thinking") {
        let key =
            if is_block(block, "thinking") && string_field(block, "reasoningContent").is_some() {
                "reasoningContent"
            } else {
                "text"
            };
        if let Some(text) = string_field(block, key) {
            return count_tokens(text);
        }
        return Ok(0);
    }
    if is_block(block, "tool_call") || is_block(block, "tool_use") {
        let mut units = string_field(block, "name")
            .map(|n| n.units().to_vec())
            .unwrap_or_default();
        let input = field(block, "input");
        if !matches!(input, JsValue::Undefined | JsValue::Null)
            && let Ok(Some(json)) = stringify_json(input)
        {
            units.extend(json.encode_utf16());
        }
        return count_tokens(&JsString::from_utf16(units));
    }
    if is_block(block, "tool_result") {
        return count_tokens(&flatten_tool_result_block_text(block));
    }
    if is_block(block, "tool_result_reference") || is_block(block, "media_reference") {
        if let Some(preview) = string_field(block, "preview") {
            return count_tokens(preview);
        }
        return Ok(0);
    }
    if is_block(block, "image") || is_block(block, "pdf") || is_block(block, "audio") {
        return Ok(MULTIMEDIA_FALLBACK_TOKENS);
    }
    if let Ok(Some(json)) = stringify_json(block) {
        return count_tokens(&JsString::from(json));
    }
    Ok(0)
}

fn collect_declared_capabilities(
    base_caps: &[(String, String)],
    events: &[EventRecord],
) -> Vec<(String, String)> {
    let mut ordered: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    let mut push_cap = |name: String, version: String| {
        if !name.is_empty() && seen.insert(name.clone()) {
            ordered.push((name, version));
        }
    };

    for (name, version) in base_caps {
        push_cap(name.clone(), version.clone());
    }

    for event in events {
        let Ok(text) = event.payload_str() else {
            continue;
        };
        if event.kind == KIND_TOOL_REGISTERED {
            if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(text) {
                if let Some(name) = obj.get("name").and_then(Value::as_str) {
                    let version = obj
                        .get("version")
                        .and_then(Value::as_str)
                        .unwrap_or("0.1.0")
                        .to_string();
                    push_cap(name.to_string(), version);
                }
            } else if let Some(name) = text.split('|').next().filter(|n| !n.is_empty()) {
                push_cap(name.to_string(), "0.1.0".to_string());
            }
        } else if matches!(
            event.kind.as_str(),
            KIND_SESSION_SPEC | KIND_SESSION_METADATA | KIND_CONTEXT_CAPABILITIES
        ) && let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(text)
            && let Some(Value::Array(items)) = obj.get("capabilities")
        {
            for item in items {
                match item {
                    Value::String(name) => push_cap(name.clone(), "0.1.0".to_string()),
                    Value::Object(cap_obj) => {
                        if let Some(name) = cap_obj.get("name").and_then(Value::as_str) {
                            let version = cap_obj
                                .get("version")
                                .and_then(Value::as_str)
                                .unwrap_or("0.1.0")
                                .to_string();
                            push_cap(name.to_string(), version);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    ordered
}

async fn read_all_validated(
    store: &dyn StoreContract,
    session: &SessionId,
    corrupt_reason: &str,
) -> Result<Vec<EventRecord>, Error> {
    let anchor = store.verify_checkpoint(session).await?;
    let mut all: Vec<EventRecord> = Vec::new();
    let mut from: Cursor = 1;
    loop {
        let page = store.read(session, from, READ_PAGE).await?;
        validate_page(session, from, READ_PAGE, &page, corrupt_reason)?;
        if page.is_empty() {
            ensure_anchor_coverage(
                anchor.as_ref(),
                all.last().map(|event| event.cursor),
                session,
                corrupt_reason,
            )?;
            return Ok(all);
        }
        let last = page
            .last()
            .map(|event| event.cursor)
            .ok_or_else(|| internal_app(corrupt_reason, "non-empty page has no last event"))?;
        all.extend(page);
        if last == u64::MAX {
            ensure_anchor_coverage(anchor.as_ref(), Some(last), session, corrupt_reason)?;
            return Ok(all);
        }
        from = last + 1;
    }
}

fn validate_page(
    session: &SessionId,
    from: Cursor,
    limit: usize,
    page: &[EventRecord],
    corrupt_reason: &str,
) -> Result<(), Error> {
    if page.len() > limit {
        return Err(internal_app(
            corrupt_reason,
            format!("store returned {} events for limit {limit}", page.len()),
        ));
    }
    let mut previous: Option<Cursor> = None;
    for event in page {
        if &event.session != session {
            return Err(internal_app(
                corrupt_reason,
                format!(
                    "page session {:?} != requested {:?}",
                    event.session, session
                ),
            ));
        }
        if event.cursor < from {
            return Err(internal_app(
                corrupt_reason,
                format!("page cursor {} before requested from {from}", event.cursor),
            ));
        }
        if let Some(prev) = previous
            && event.cursor <= prev
        {
            return Err(internal_app(
                corrupt_reason,
                format!("non-increasing page cursor {} after {prev}", event.cursor),
            ));
        }
        previous = Some(event.cursor);
    }
    Ok(())
}

fn ensure_anchor_coverage(
    anchor: Option<&(Cursor, String)>,
    seen: Option<Cursor>,
    session: &SessionId,
    corrupt_reason: &str,
) -> Result<(), Error> {
    if let Some((cursor, _)) = anchor {
        let seen = seen.unwrap_or(0);
        if seen < *cursor {
            return Err(internal_app(
                corrupt_reason,
                format!(
                    "checkpoint requires cursor {cursor}, but history ends at {seen} for session {session:?}"
                ),
            ));
        }
    }
    Ok(())
}

fn parse_compaction_event(
    session: &SessionId,
    event: &EventRecord,
) -> Result<(CompactionCommit, Vec<u8>, Vec<u8>), Error> {
    if &event.session != session {
        return Err(internal_app(
            "COMPACTION_CORRUPT",
            format!(
                "event session {:?} != requested {:?}",
                event.session, session
            ),
        ));
    }
    let newline = event
        .payload
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| internal_app("COMPACTION_CORRUPT", "missing header newline"))?;
    let header: Value = serde_json::from_slice(&event.payload[..newline])
        .map_err(|err| internal_app("COMPACTION_CORRUPT", format!("header is not JSON: {err}")))?;
    if !header.is_object() {
        return Err(internal_app(
            "COMPACTION_CORRUPT",
            "header must be a JSON object",
        ));
    }
    if let Some(version) = header.get("version")
        && version.as_u64() != Some(COMPACTION_SCHEMA_VERSION)
    {
        return Err(internal_app(
            "COMPACTION_CORRUPT",
            format!("unsupported compaction schema version: {version}"),
        ));
    }
    let turn = header
        .get("turn")
        .and_then(Value::as_u64)
        .ok_or_else(|| internal_app("COMPACTION_CORRUPT", "header.turn missing or invalid"))?;
    let digest = header
        .get("digest")
        .and_then(Value::as_str)
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| internal_app("COMPACTION_CORRUPT", "header.digest missing or invalid"))?
        .to_string();
    let boundary_len_u64 = header
        .get("boundary_len")
        .and_then(Value::as_u64)
        .ok_or_else(|| internal_app("COMPACTION_CORRUPT", "header.boundary_len missing"))?;
    let messages_len_u64 = header
        .get("messages_len")
        .and_then(Value::as_u64)
        .ok_or_else(|| internal_app("COMPACTION_CORRUPT", "header.messages_len missing"))?;

    let boundary_len = usize::try_from(boundary_len_u64)
        .map_err(|_| internal_app("COMPACTION_CORRUPT", "boundary_len exceeds usize"))?;
    let messages_len = usize::try_from(messages_len_u64)
        .map_err(|_| internal_app("COMPACTION_CORRUPT", "messages_len exceeds usize"))?;

    if boundary_len > MAX_BOUNDARY_BYTES {
        return Err(internal_app(
            "COMPACTION_CORRUPT",
            format!("boundary_len {boundary_len} exceeds {MAX_BOUNDARY_BYTES}"),
        ));
    }
    if messages_len > MAX_MESSAGES_BYTES {
        return Err(internal_app(
            "COMPACTION_CORRUPT",
            format!("messages_len {messages_len} exceeds {MAX_MESSAGES_BYTES}"),
        ));
    }

    let expected_len = boundary_len
        .checked_add(messages_len)
        .ok_or_else(|| internal_app("COMPACTION_CORRUPT", "length overflow"))?;
    let content_start = newline
        .checked_add(1)
        .ok_or_else(|| internal_app("COMPACTION_CORRUPT", "header offset overflow"))?;
    let actual_len = event
        .payload
        .len()
        .checked_sub(content_start)
        .ok_or_else(|| internal_app("COMPACTION_CORRUPT", "content starts beyond payload"))?;

    if actual_len != expected_len {
        return Err(internal_app(
            "COMPACTION_CORRUPT",
            format!("payload content len {actual_len} != declared {expected_len}"),
        ));
    }

    let boundary_end = content_start
        .checked_add(boundary_len)
        .ok_or_else(|| internal_app("COMPACTION_CORRUPT", "boundary offset overflow"))?;
    let boundary = event.payload[content_start..boundary_end].to_vec();
    let messages = event.payload[boundary_end..].to_vec();

    let recomputed = compute_compaction_digest(turn, &boundary, &messages);
    if recomputed != digest {
        return Err(internal_app(
            "COMPACTION_DIGEST_MISMATCH",
            format!("recorded={digest} recomputed={recomputed}"),
        ));
    }

    Ok((
        CompactionCommit::new(
            session.clone(),
            turn,
            digest,
            event.cursor,
            boundary_len,
            messages_len,
        ),
        boundary,
        messages,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_authority::LedgerTransition;
    use agentcpm_iface::types::NodeId;
    use pilotdeck_context_runtime::boundary_codec;
    use pilotdeck_context_runtime::history::Payload;
    use pilotdeck_context_runtime::operations::Operation;
    use pilotdeck_protocol::parse_json;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("store-backed future must resolve immediately"),
        }
    }

    #[derive(Default)]
    struct MemoryStore {
        sessions: Mutex<BTreeMap<SessionId, Vec<EventRecord>>>,
        corrupt_checkpoint: Mutex<Option<String>>,
    }

    impl MemoryStore {
        fn raw_append(&self, session: &SessionId, kind: &str, payload: Vec<u8>) -> Cursor {
            let mut sessions = self.sessions.lock().unwrap();
            let log = sessions.entry(session.clone()).or_default();
            let cursor = log.len() as Cursor + 1;
            let mut rec = EventRecord::new(session.clone(), kind, payload);
            rec.cursor = cursor;
            log.push(rec);
            cursor
        }

        fn tamper_last_event(&self, session: &SessionId, new_payload: Vec<u8>) {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(log) = sessions.get_mut(session)
                && let Some(last) = log.last_mut()
            {
                last.payload = new_payload;
            }
        }

        fn set_corrupt_checkpoint(&self, message: &str) {
            *self.corrupt_checkpoint.lock().unwrap() = Some(message.to_string());
        }
    }

    impl StoreContract for MemoryStore {
        fn append(&self, mut record: EventRecord) -> BoxFuture<'_, Result<Cursor, Error>> {
            let session = record.session.clone();
            Box::pin(async move {
                let mut sessions = self.sessions.lock().unwrap();
                let log = sessions.entry(session).or_default();
                let cursor = log.len() as Cursor + 1;
                record.cursor = cursor;
                log.push(record);
                Ok(cursor)
            })
        }

        fn read(
            &self,
            session: &SessionId,
            from: Cursor,
            limit: usize,
        ) -> BoxFuture<'_, Result<Vec<EventRecord>, Error>> {
            let session = session.clone();
            Box::pin(async move {
                let sessions = self.sessions.lock().unwrap();
                let mut out: Vec<EventRecord> = sessions
                    .get(&session)
                    .map(|log| {
                        log.iter()
                            .filter(|event| event.cursor >= from)
                            .take(if limit == 0 { usize::MAX } else { limit })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                out.sort_by_key(|event| event.cursor);
                Ok(out)
            })
        }

        fn last_cursor(&self, session: &SessionId) -> BoxFuture<'_, Result<Cursor, Error>> {
            let session = session.clone();
            Box::pin(async move {
                Ok(self
                    .sessions
                    .lock()
                    .unwrap()
                    .get(&session)
                    .map_or(0, |log| log.len() as Cursor))
            })
        }

        fn checkpoint(&self, _session: &SessionId) -> BoxFuture<'_, Result<String, Error>> {
            Box::pin(async { Ok("cp-1".to_string()) })
        }

        fn verify_checkpoint(
            &self,
            _session: &SessionId,
        ) -> BoxFuture<'_, Result<Option<(Cursor, String)>, Error>> {
            let err = self.corrupt_checkpoint.lock().unwrap().clone();
            Box::pin(async move {
                if let Some(msg) = err {
                    return Err(Error::new(ErrorCategory::Internal, -32000, msg));
                }
                Ok(None)
            })
        }

        fn create_session(&self, session: &SessionId) -> BoxFuture<'_, Result<(), Error>> {
            let session = session.clone();
            Box::pin(async move {
                self.sessions.lock().unwrap().entry(session).or_default();
                Ok(())
            })
        }

        fn contains_session(&self, session: &SessionId) -> BoxFuture<'_, Result<bool, Error>> {
            let session = session.clone();
            Box::pin(async move { Ok(self.sessions.lock().unwrap().contains_key(&session)) })
        }

        fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionId>, Error>> {
            Box::pin(async move { Ok(self.sessions.lock().unwrap().keys().cloned().collect()) })
        }

        fn node(&self) -> Option<&NodeId> {
            None
        }
    }

    fn meta(session: &str, call_id: &str, turn: u64) -> CallMeta {
        CallMeta {
            session_id: session.to_string(),
            call_id: call_id.to_string(),
            turn,
        }
    }

    #[test]
    fn context_contract_assemble_projects_without_mutating_ledger() {
        let store = Arc::new(MemoryStore::default());
        let session: SessionId = "s-proj".to_string();
        block_on(store.create_session(&session)).unwrap();
        store.raw_append(&session, KIND_TOOL_REGISTERED, b"read_file|{}|9".to_vec());
        store.raw_append(
            &session,
            KIND_SESSION_SPEC,
            br#"{"capabilities":[{"name":"custom.cap","version":"1.2.0"}]}"#.to_vec(),
        );

        let messages = parse_json(
            r#"[
                {"role":"user","content":[{"type":"text","text":"t1"}]},
                {"role":"assistant","content":[{"type":"tool_call","id":"c1","name":"read_file","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","toolCallId":"c1","content":[{"type":"text","text":"big-1"}]}]},
                {"role":"user","content":[{"type":"text","text":"t2"}]},
                {"role":"assistant","content":[{"type":"text","text":"a2"}]},
                {"role":"user","content":[{"type":"text","text":"t3"}]},
                {"role":"assistant","content":[{"type":"text","text":"a3"}]},
                {"role":"user","content":[{"type":"text","text":"t4"}]},
                {"role":"assistant","content":[{"type":"text","text":"a4"}]},
                {"role":"user","content":[{"type":"text","text":"t5"}]},
                {"role":"assistant","content":[{"type":"text","text":"a5"}]},
                {"role":"user","content":[{"type":"text","text":"t6"}]},
                {"role":"assistant","content":[{"type":"text","text":"a6"}]},
                {"role":"user","content":[{"type":"text","text":"t7"}]},
                {"role":"assistant","content":[{"type":"text","text":"a7"}]},
                {"role":"user","content":[{"type":"text","text":"t8"}]},
                {"role":"assistant","content":[{"type":"text","text":"a8"}]}
            ]"#,
        )
        .unwrap();
        let raw_payload = Payload {
            profile: boundary_codec::PROFILE.into(),
            content_type: "application/vnd.pilotdeck.context.boundary.v1".into(),
            bytes: boundary_codec::encode(&messages).unwrap(),
        };

        let ledger = AuthoritativeContextLedger::new(session.clone());
        let receipt = ledger
            .commit_transition(
                LedgerTransition::new("tx-seed", 0, Operation::PrepareForModel)
                    .with_replacement(raw_payload.clone()),
            )
            .unwrap();
        assert_eq!(receipt.revision, 1);

        let contract = PilotDeckContextContract::new(store)
            .with_ledger(ledger.clone())
            .with_capabilities(vec![("pilotdeck.core".into(), "0.1.0".into())])
            .with_micro_options(MicroCompactionEngineOptions {
                keep_latest: Some(0.0),
                trim_to_tokens: Some(64.0),
                ..Default::default()
            })
            .with_snip_options(SnipEngineOptions {
                keep_head_turns: Some(1.0),
                keep_tail_turns: Some(2.0),
                enabled: Some(true),
                ..Default::default()
            })
            .with_snip_call_options(SnipCallOptions {
                target_tokens: Some(32.0),
                target_total_tokens: None,
            });

        let snapshot = block_on(contract.assemble(&session, 4)).unwrap();
        assert_eq!(snapshot.session, session);
        assert_eq!(snapshot.turn, 4);
        assert_eq!(
            snapshot.content_type.as_deref(),
            Some(SNAPSHOT_CONTENT_TYPE)
        );
        assert_eq!(
            snapshot.capabilities_versions,
            vec![
                ("pilotdeck.core".into(), "0.1.0".into()),
                ("read_file".into(), "0.1.0".into()),
                ("custom.cap".into(), "1.2.0".into()),
            ]
        );

        let body: Value = serde_json::from_slice(&snapshot.bytes).unwrap();
        assert_eq!(body["revision"], 1);
        assert_eq!(body["ledger"]["projections"]["snip_applied"], true);

        // Authoritative ledger MUST NOT be mutated during read/assemble!
        let after = ledger.snapshot();
        assert_eq!(after.revision, 1);
        assert_eq!(after.payload, raw_payload);

        // rebuild_capabilities must match declared capability names consistently.
        let rebuilt = block_on(contract.rebuild_capabilities(&session)).unwrap();
        assert_eq!(
            rebuilt,
            vec![
                "pilotdeck.core".to_string(),
                "read_file".to_string(),
                "custom.cap".to_string()
            ]
        );
    }

    #[test]
    fn trace_input_records_write_ahead_and_returns_unique_deterministic_id() {
        let store = Arc::new(MemoryStore::default());
        let session: SessionId = "s-trace".to_string();
        block_on(store.create_session(&session)).unwrap();

        let contract = PilotDeckContextContract::new(store.clone());
        let id1 =
            block_on(contract.trace_input(&session, 1, &meta(&session, "c-1", 1), b"hello input"))
                .unwrap();
        let id2 =
            block_on(contract.trace_input(&session, 1, &meta(&session, "c-1", 1), b"hello input"))
                .unwrap();
        assert!(!id1.is_empty());
        assert_ne!(
            id1, id2,
            "sequential traces must have distinct cursor-bound ids"
        );

        // Binary non-UTF-8 input is also traced losslessly.
        let bin_id = block_on(contract.trace_input(
            &session,
            2,
            &meta(&session, "c-2", 2),
            &[0xff, 0x00, 0xfe],
        ))
        .unwrap();
        assert!(!bin_id.is_empty());

        let events = block_on(store.read(&session, 1, 10)).unwrap();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| e.kind == KIND_CONTEXT_INPUT));
        let third: Value = serde_json::from_slice(&events[2].payload).unwrap();
        assert!(third["payload_utf8"].is_null());
        assert_eq!(third["payload_hex"], "ff00fe");

        // Broken checkpoint fails closed without appending.
        store.set_corrupt_checkpoint("checkpoint mismatch");
        let err = block_on(contract.trace_input(&session, 3, &meta(&session, "c-3", 3), b"x"))
            .unwrap_err();
        assert!(err.message.contains("checkpoint mismatch"));
        assert_eq!(block_on(store.read(&session, 1, 10)).unwrap().len(), 3);
    }

    #[test]
    fn compaction_persistence_roundtrips_and_fails_closed_on_tamper_or_truncation() {
        let store = Arc::new(MemoryStore::default());
        let persistence = PilotDeckCompactionPersistence::new(store.clone());
        let session: SessionId = "s-compact".to_string();
        block_on(store.create_session(&session)).unwrap();

        assert!(block_on(persistence.latest(&session)).unwrap().is_none());
        assert!(block_on(persistence.load(&session)).unwrap().is_none());

        let boundary = b"\x00<compact-boundary turn=\"5\"/>\xff";
        let messages = b"[{\"role\":\"assistant\",\"content\":\"summary\"}]";
        let commit = block_on(persistence.commit(&session, 5, boundary, messages)).unwrap();
        assert_eq!(commit.session, session);
        assert_eq!(commit.turn, 5);
        assert_eq!(commit.boundary_len, boundary.len());
        assert_eq!(commit.messages_len, messages.len());
        assert_eq!(commit.digest.len(), 64);
        assert!(commit.cursor > 0);

        let latest = block_on(persistence.latest(&session)).unwrap().unwrap();
        assert_eq!(latest, commit);
        let loaded = block_on(persistence.load(&session)).unwrap().unwrap();
        assert_eq!(loaded.commit, commit);
        assert_eq!(loaded.boundary, boundary);
        assert_eq!(loaded.messages, messages);

        // Truncate payload by 1 byte -> must fail closed with Internal / -32000.
        let events = block_on(store.read(&session, 1, 10)).unwrap();
        let mut truncated = events[0].payload.clone();
        truncated.pop();
        store.tamper_last_event(&session, truncated);

        let err = block_on(persistence.latest(&session)).unwrap_err();
        assert_eq!(err.category, ErrorCategory::Internal);
        assert_eq!(err.code, -32000);
        assert!(err.message.contains("COMPACTION"));
        let load_err = block_on(persistence.load(&session)).unwrap_err();
        assert_eq!(load_err.category, ErrorCategory::Internal);
        assert_eq!(load_err.code, -32000);

        // Tamper 1 byte inside messages while preserving lengths -> digest mismatch fails closed.
        let mut tampered = events[0].payload.clone();
        let last_idx = tampered.len() - 1;
        tampered[last_idx] ^= 0x01;
        store.tamper_last_event(&session, tampered);

        let err = block_on(persistence.load(&session)).unwrap_err();
        assert_eq!(err.category, ErrorCategory::Internal);
        assert_eq!(err.code, -32000);
        assert!(err.message.contains("COMPACTION_DIGEST_MISMATCH"));
    }
}
