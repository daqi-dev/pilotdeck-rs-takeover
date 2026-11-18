//! Single-authority revisioned context state ledger (`TC-02`, `TC-03`, `TC-05`).
//!
//! Provides [`AuthoritativeContextLedger`] with:
//! - Explicit support and versioned envelope mapping for all five PilotDeck
//!   context operations (`PrepareForModel`, `ApplyToolResults`,
//!   `RecoverFromModelError`, `CaptureTurn`, `TryAutoCompact`).
//! - Monotonic `revision: u64` optimistic concurrency via `expected_revision`.
//! - Idempotent `transaction_id` tracking paired with domain-separated SHA-256
//!   request digest verification (same digest returns the original
//!   [`LedgerReceipt`]; different digest fails closed with
//!   [`LedgerError::IdempotencyConflict`]).
//! - Atomic `consumed_inputs`, `replacement`, `facts`, and `compaction` commits
//!   without silent overwrites or partial state transitions.
//! - Fail-closed tool-pair integrity verification via
//!   [`pilotdeck_context_core::tool_pair`] on structured replacement/compaction
//!   transitions, while keeping opaque binary payloads lossless (`TC-03`).
//! - Bidirectional [`pilotdeck_context_runtime::history::History`] adapter.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use agentcpm_iface::types::{Error, ErrorCategory, SessionId};
use pilotdeck_context_core::tool_pair::{
    collect_tool_call_ids, collect_tool_result_ids, is_direct_tool_result,
    is_media_reference_with_id, strip_unpaired_tool_calls, strip_unpaired_tool_results,
};
use pilotdeck_context_core::wire::{block_type, content, is_block, is_role, role, string_field};
use pilotdeck_context_runtime::boundary_codec;
use pilotdeck_context_runtime::history::{
    Commit, History, HistoryError, HistoryFuture, Payload, Receipt, View,
};
use pilotdeck_context_runtime::operations::Operation;
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Current protocol version for versioned context control envelopes (`TC-02`).
pub const CONTEXT_LEDGER_PROTOCOL_VERSION: u32 = 1;

/// Default content type used for empty `boundary_codec` initial state.
pub const BOUNDARY_CONTENT_TYPE: &str = "application/vnd.pilotdeck.context.boundary.v1";

/// Maximum payload size accepted by the authoritative context ledger (8 MiB).
pub const MAX_LEDGER_PAYLOAD_BYTES: usize = boundary_codec::MAX_BYTES;

/// Maximum length of a `transaction_id` or consumed input identifier.
const MAX_ID_BYTES: usize = 256;

const LEDGER_DIGEST_DOMAIN: &[u8] = b"pilotdeck.context.ledger.v1";

/// Typed enum mirroring all five [`pilotdeck_context_runtime::operations::Operation`] variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextOperationKind {
    PrepareForModel,
    ApplyToolResults,
    RecoverFromModelError,
    CaptureTurn,
    TryAutoCompact,
}

impl ContextOperationKind {
    /// Convert from [`Operation`].
    pub fn from_runtime(operation: Operation) -> Self {
        match operation {
            Operation::PrepareForModel => Self::PrepareForModel,
            Operation::ApplyToolResults => Self::ApplyToolResults,
            Operation::RecoverFromModelError => Self::RecoverFromModelError,
            Operation::CaptureTurn => Self::CaptureTurn,
            Operation::TryAutoCompact => Self::TryAutoCompact,
        }
    }

    /// Convert to [`Operation`].
    pub fn to_runtime(self) -> Operation {
        match self {
            Self::PrepareForModel => Operation::PrepareForModel,
            Self::ApplyToolResults => Operation::ApplyToolResults,
            Self::RecoverFromModelError => Operation::RecoverFromModelError,
            Self::CaptureTurn => Operation::CaptureTurn,
            Self::TryAutoCompact => Operation::TryAutoCompact,
        }
    }

    /// Stable wire name matching [`Operation::name`].
    pub fn as_str(self) -> &'static str {
        self.to_runtime().name()
    }

    /// All five supported context operations in canonical order.
    pub const fn all() -> [Self; 5] {
        [
            Self::PrepareForModel,
            Self::ApplyToolResults,
            Self::RecoverFromModelError,
            Self::CaptureTurn,
            Self::TryAutoCompact,
        ]
    }
}

impl From<Operation> for ContextOperationKind {
    fn from(value: Operation) -> Self {
        Self::from_runtime(value)
    }
}

impl From<ContextOperationKind> for Operation {
    fn from(value: ContextOperationKind) -> Self {
        value.to_runtime()
    }
}

/// Versioned request control metadata bound into the ledger transaction digest (`TC-02`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextControl {
    pub protocol_version: u32,
    pub route: Option<String>,
    pub protocol: Option<String>,
    pub preview_only: bool,
    pub manual_force: bool,
    pub max_context_tokens: Option<u64>,
    pub reserved_output_tokens: Option<u64>,
}

impl ContextControl {
    /// Validate that control metadata declares the exact supported protocol version (`TC-02`).
    pub fn validate(&self) -> Result<(), LedgerError> {
        if self.protocol_version != CONTEXT_LEDGER_PROTOCOL_VERSION {
            return Err(LedgerError::InvalidTransition(format!(
                "unsupported context control protocol_version: {}",
                self.protocol_version
            )));
        }
        Ok(())
    }
}

impl Default for ContextControl {
    fn default() -> Self {
        Self {
            protocol_version: CONTEXT_LEDGER_PROTOCOL_VERSION,
            route: None,
            protocol: None,
            preview_only: false,
            manual_force: false,
            max_context_tokens: None,
            reserved_output_tokens: None,
        }
    }
}

/// Versioned atomic state transition request for [`AuthoritativeContextLedger`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerTransition {
    pub transaction_id: String,
    pub expected_revision: u64,
    pub operation: Operation,
    pub control: ContextControl,
    pub replacement: Option<Payload>,
    pub consumed_inputs: Vec<String>,
    pub facts: Vec<Payload>,
    pub compaction: Option<Payload>,
}

impl LedgerTransition {
    /// Construct a transition with default v1 control metadata.
    pub fn new(
        transaction_id: impl Into<String>,
        expected_revision: u64,
        operation: Operation,
    ) -> Self {
        Self {
            transaction_id: transaction_id.into(),
            expected_revision,
            operation,
            control: ContextControl::default(),
            replacement: None,
            consumed_inputs: Vec::new(),
            facts: Vec::new(),
            compaction: None,
        }
    }

    pub fn with_control(mut self, control: ContextControl) -> Self {
        self.control = control;
        self
    }

    pub fn with_replacement(mut self, replacement: Payload) -> Self {
        self.replacement = Some(replacement);
        self
    }

    pub fn with_consumed_inputs(mut self, consumed_inputs: Vec<String>) -> Self {
        self.consumed_inputs = consumed_inputs;
        self
    }

    pub fn with_facts(mut self, facts: Vec<Payload>) -> Self {
        self.facts = facts;
        self
    }

    pub fn with_compaction(mut self, compaction: Payload) -> Self {
        self.compaction = Some(compaction);
        self
    }
}

/// Receipt emitted once a transition commits (or replayed idempotently on identical digest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerReceipt {
    pub transaction_id: String,
    pub request_digest: Vec<u8>,
    pub request_digest_hex: String,
    pub revision: u64,
    pub operation: ContextOperationKind,
    pub consumed_inputs: Vec<String>,
    pub event_cursor: String,
}

impl From<LedgerReceipt> for Receipt {
    fn from(receipt: LedgerReceipt) -> Self {
        Self {
            transaction_id: receipt.transaction_id,
            request_digest: receipt.request_digest,
            revision: receipt.revision,
            consumed_inputs: receipt.consumed_inputs,
            event_cursor: receipt.event_cursor,
        }
    }
}

/// Immutable audit record for each committed revision in [`AuthoritativeContextLedger`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerAuditEntry {
    pub revision: u64,
    pub transaction_id: String,
    pub request_digest_hex: String,
    pub operation: ContextOperationKind,
    pub control: ContextControl,
    pub replaced: bool,
    pub compaction_updated: bool,
    pub consumed_inputs: Vec<String>,
    pub facts_count: usize,
}

/// Read-only snapshot of [`AuthoritativeContextLedger`] state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerView {
    pub session_id: SessionId,
    pub revision: u64,
    pub payload: Payload,
    pub last_receipt: Option<LedgerReceipt>,
    pub latest_compaction: Option<Payload>,
    pub consumed_inputs: BTreeSet<String>,
    pub facts: Vec<Payload>,
    pub audit_log: Vec<LedgerAuditEntry>,
}

/// Fail-closed rejection errors returned by [`AuthoritativeContextLedger`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerError {
    RevisionMismatch {
        expected: u64,
        actual: u64,
    },
    IdempotencyConflict {
        transaction_id: String,
        expected_digest: String,
        actual_digest: String,
    },
    InputAlreadyConsumed {
        input_id: String,
    },
    ToolPairIntegrity(String),
    InvalidPayload(String),
    InvalidTransition(String),
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionMismatch { expected, actual } => {
                write!(
                    f,
                    "CONTEXT_REVISION_MISMATCH: expected revision {expected}, actual revision {actual}"
                )
            }
            Self::IdempotencyConflict {
                transaction_id,
                expected_digest,
                actual_digest,
            } => {
                write!(
                    f,
                    "CONTEXT_IDEMPOTENCY_CONFLICT: transaction {transaction_id:?} replayed with digest {actual_digest} != committed {expected_digest}"
                )
            }
            Self::InputAlreadyConsumed { input_id } => {
                write!(
                    f,
                    "CONTEXT_INPUT_ALREADY_CONSUMED: input {input_id:?} was already consumed"
                )
            }
            Self::ToolPairIntegrity(detail) => {
                write!(f, "CONTEXT_TOOL_PAIR_CORRUPT: {detail}")
            }
            Self::InvalidPayload(detail) => {
                write!(f, "CONTEXT_INVALID_PAYLOAD: {detail}")
            }
            Self::InvalidTransition(detail) => {
                write!(f, "CONTEXT_INVALID_TRANSITION: {detail}")
            }
        }
    }
}

impl std::error::Error for LedgerError {}

impl LedgerError {
    /// Map to public [`agentcpm_iface::types::Error`] with stable app reason token (`-32000`).
    pub fn to_iface_error(&self) -> Error {
        let reason = match self {
            Self::RevisionMismatch { .. } => "CONTEXT_REVISION_MISMATCH",
            Self::IdempotencyConflict { .. } => "CONTEXT_IDEMPOTENCY_CONFLICT",
            Self::InputAlreadyConsumed { .. } => "CONTEXT_INPUT_ALREADY_CONSUMED",
            Self::ToolPairIntegrity(_) => "CONTEXT_TOOL_PAIR_CORRUPT",
            Self::InvalidPayload(_) => "CONTEXT_INVALID_PAYLOAD",
            Self::InvalidTransition(_) => "CONTEXT_INVALID_TRANSITION",
        };
        Error::app(ErrorCategory::Protocol, reason, self.to_string())
    }
}

impl From<LedgerError> for HistoryError {
    fn from(error: LedgerError) -> Self {
        match error {
            LedgerError::RevisionMismatch { .. }
            | LedgerError::IdempotencyConflict { .. }
            | LedgerError::InputAlreadyConsumed { .. } => Self::Conflict,
            LedgerError::ToolPairIntegrity(msg)
            | LedgerError::InvalidPayload(msg)
            | LedgerError::InvalidTransition(msg) => Self::Invalid(msg),
        }
    }
}

struct LedgerState {
    revision: u64,
    payload: Payload,
    last_receipt: Option<LedgerReceipt>,
    latest_compaction: Option<Payload>,
    consumed_inputs: BTreeSet<String>,
    facts: Vec<Payload>,
    receipts: BTreeMap<String, LedgerReceipt>,
    audit_log: Vec<LedgerAuditEntry>,
}

struct LedgerInner {
    session_id: SessionId,
    state: Mutex<LedgerState>,
}

/// Single-authority revisioned context state ledger (`TC-02`, `TC-03`, `TC-05`).
#[derive(Clone)]
pub struct AuthoritativeContextLedger {
    inner: Arc<LedgerInner>,
}

impl AuthoritativeContextLedger {
    /// Create a new authoritative ledger at revision `0` with an empty canonical message list.
    pub fn new(session_id: impl Into<SessionId>) -> Self {
        let empty = Payload {
            profile: boundary_codec::PROFILE.to_string(),
            content_type: BOUNDARY_CONTENT_TYPE.to_string(),
            bytes: boundary_codec::encode(&JsValue::Array(Vec::new()))
                .expect("empty JsValue array always encodes"),
        };
        Self {
            inner: Arc::new(LedgerInner {
                session_id: session_id.into(),
                state: Mutex::new(LedgerState {
                    revision: 0,
                    payload: empty,
                    last_receipt: None,
                    latest_compaction: None,
                    consumed_inputs: BTreeSet::new(),
                    facts: Vec::new(),
                    receipts: BTreeMap::new(),
                    audit_log: Vec::new(),
                }),
            }),
        }
    }

    /// Create a ledger seeded at revision `0` with a validated initial payload.
    pub fn with_initial_payload(
        session_id: impl Into<SessionId>,
        initial: Payload,
    ) -> Result<Self, LedgerError> {
        validate_payload_integrity(&initial, true)?;
        let ledger = Self::new(session_id);
        {
            let mut state = ledger
                .inner
                .state
                .lock()
                .map_err(|_| LedgerError::InvalidTransition("ledger lock poisoned".into()))?;
            state.payload = initial;
        }
        Ok(ledger)
    }

    /// Session identifier bound to this authoritative ledger.
    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    /// All five PilotDeck context operations are supported by this authority.
    pub fn supports_operation(&self, operation: Operation) -> bool {
        matches!(
            operation,
            Operation::PrepareForModel
                | Operation::ApplyToolResults
                | Operation::RecoverFromModelError
                | Operation::CaptureTurn
                | Operation::TryAutoCompact
        )
    }

    /// Read a consistent, non-mutating snapshot of the authoritative ledger state.
    pub fn snapshot(&self) -> LedgerView {
        let state = self
            .inner
            .state
            .lock()
            .expect("authoritative context ledger lock poisoned");
        LedgerView {
            session_id: self.inner.session_id.clone(),
            revision: state.revision,
            payload: state.payload.clone(),
            last_receipt: state.last_receipt.clone(),
            latest_compaction: state.latest_compaction.clone(),
            consumed_inputs: state.consumed_inputs.clone(),
            facts: state.facts.clone(),
            audit_log: state.audit_log.clone(),
        }
    }

    /// Lookup a previously committed transaction receipt without mutating state.
    pub fn lookup_transaction(&self, transaction_id: &str) -> Option<LedgerReceipt> {
        let state = self
            .inner
            .state
            .lock()
            .expect("authoritative context ledger lock poisoned");
        state.receipts.get(transaction_id).cloned()
    }

    /// Query whether an input identifier has been atomically consumed.
    pub fn is_input_consumed(&self, input_id: &str) -> bool {
        let state = self
            .inner
            .state
            .lock()
            .expect("authoritative context ledger lock poisoned");
        state.consumed_inputs.contains(input_id)
    }

    /// Atomically validate and apply a versioned operation transition.
    pub fn commit_transition(
        &self,
        transition: LedgerTransition,
    ) -> Result<LedgerReceipt, LedgerError> {
        if transition.transaction_id.is_empty() || transition.transaction_id.len() > MAX_ID_BYTES {
            return Err(LedgerError::InvalidTransition(
                "transaction_id must be 1..=256 bytes".into(),
            ));
        }
        transition.control.validate()?;
        if !self.supports_operation(transition.operation) {
            return Err(LedgerError::InvalidTransition(format!(
                "unsupported operation: {}",
                transition.operation.name()
            )));
        }

        let request_digest = compute_transition_digest(&self.inner.session_id, &transition);
        let request_digest_hex = hex_encode(&request_digest);

        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| LedgerError::InvalidTransition("ledger lock poisoned".into()))?;

        // Idempotent replay check MUST precede revision check: a replayed committed
        // transaction sees `state.revision > expected_revision`.
        if let Some(existing) = state.receipts.get(&transition.transaction_id) {
            if existing.request_digest == request_digest {
                return Ok(existing.clone());
            }
            return Err(LedgerError::IdempotencyConflict {
                transaction_id: transition.transaction_id,
                expected_digest: existing.request_digest_hex.clone(),
                actual_digest: request_digest_hex,
            });
        }

        if transition.expected_revision != state.revision {
            return Err(LedgerError::RevisionMismatch {
                expected: transition.expected_revision,
                actual: state.revision,
            });
        }

        if transition.control.preview_only
            && (transition.replacement.is_some()
                || transition.compaction.is_some()
                || !transition.consumed_inputs.is_empty()
                || !transition.facts.is_empty())
        {
            return Err(LedgerError::InvalidTransition(
                "preview_only transition cannot mutate authoritative ledger".into(),
            ));
        }

        let mut seen_inputs = HashSet::with_capacity(transition.consumed_inputs.len());
        for input_id in &transition.consumed_inputs {
            if input_id.is_empty() || input_id.len() > MAX_ID_BYTES {
                return Err(LedgerError::InvalidTransition(
                    "consumed_inputs entries must be 1..=256 bytes".into(),
                ));
            }
            if !seen_inputs.insert(input_id.as_str()) {
                return Err(LedgerError::InvalidTransition(format!(
                    "duplicate consumed input in transaction: {input_id:?}"
                )));
            }
            if state.consumed_inputs.contains(input_id) {
                return Err(LedgerError::InputAlreadyConsumed {
                    input_id: input_id.clone(),
                });
            }
        }

        if let Some(replacement) = &transition.replacement {
            validate_payload_integrity(replacement, true)?;
        }
        if let Some(compaction) = &transition.compaction {
            validate_payload_integrity(compaction, true)?;
        }
        for fact in &transition.facts {
            validate_payload_integrity(fact, false)?;
        }

        let next_revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| LedgerError::InvalidTransition("revision overflow".into()))?;
        let operation = ContextOperationKind::from_runtime(transition.operation);
        let replaced = transition.replacement.is_some();
        let compaction_updated = transition.compaction.is_some();
        let facts_count = transition.facts.len();

        if let Some(replacement) = transition.replacement {
            state.payload = replacement;
        }
        if let Some(compaction) = transition.compaction {
            state.latest_compaction = Some(compaction);
        }
        for input_id in &transition.consumed_inputs {
            state.consumed_inputs.insert(input_id.clone());
        }
        state.facts.extend(transition.facts);
        state.revision = next_revision;

        let receipt = LedgerReceipt {
            transaction_id: transition.transaction_id.clone(),
            request_digest,
            request_digest_hex: request_digest_hex.clone(),
            revision: next_revision,
            operation,
            consumed_inputs: transition.consumed_inputs.clone(),
            event_cursor: format!("rev-{next_revision}"),
        };
        state.last_receipt = Some(receipt.clone());
        state
            .receipts
            .insert(transition.transaction_id.clone(), receipt.clone());
        state.audit_log.push(LedgerAuditEntry {
            revision: next_revision,
            transaction_id: transition.transaction_id,
            request_digest_hex,
            operation,
            control: transition.control,
            replaced,
            compaction_updated,
            consumed_inputs: transition.consumed_inputs,
            facts_count,
        });

        Ok(receipt)
    }
}

impl History for AuthoritativeContextLedger {
    fn read(&self) -> HistoryFuture<'_, View> {
        let snapshot = self.snapshot();
        Box::pin(async move {
            Ok(View {
                revision: snapshot.revision,
                payload: snapshot.payload,
                receipt: snapshot.last_receipt.map(Receipt::from),
            })
        })
    }

    fn commit(&self, request: Commit) -> HistoryFuture<'_, Receipt> {
        let operation = infer_operation(&request);
        let transition = LedgerTransition {
            transaction_id: request.transaction_id,
            expected_revision: request.expected_revision,
            operation,
            control: ContextControl::default(),
            replacement: request.replacement,
            consumed_inputs: request.consumed_inputs,
            facts: request.facts,
            compaction: request.compaction,
        };
        let result = self
            .commit_transition(transition)
            .map(Receipt::from)
            .map_err(HistoryError::from);
        Box::pin(async move { result })
    }

    fn lookup<'a>(&'a self, transaction_id: &'a str) -> HistoryFuture<'a, Option<Receipt>> {
        let receipt = self.lookup_transaction(transaction_id).map(Receipt::from);
        Box::pin(async move { Ok(receipt) })
    }
}

fn infer_operation(request: &Commit) -> Operation {
    if request.compaction.is_some() {
        Operation::TryAutoCompact
    } else if !request.consumed_inputs.is_empty() {
        Operation::ApplyToolResults
    } else if request.replacement.is_some() {
        Operation::PrepareForModel
    } else {
        Operation::CaptureTurn
    }
}

fn compute_transition_digest(session_id: &str, transition: &LedgerTransition) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(LEDGER_DIGEST_DOMAIN);
    hash_str(&mut hasher, session_id);
    hash_str(&mut hasher, &transition.transaction_id);
    hasher.update(transition.expected_revision.to_le_bytes());
    hash_str(&mut hasher, transition.operation.name());
    hasher.update(transition.control.protocol_version.to_le_bytes());
    hash_opt_str(&mut hasher, transition.control.route.as_deref());
    hash_opt_str(&mut hasher, transition.control.protocol.as_deref());
    hasher.update([u8::from(transition.control.preview_only)]);
    hasher.update([u8::from(transition.control.manual_force)]);
    hash_opt_u64(&mut hasher, transition.control.max_context_tokens);
    hash_opt_u64(&mut hasher, transition.control.reserved_output_tokens);
    hash_opt_payload(&mut hasher, transition.replacement.as_ref());
    hasher.update((transition.consumed_inputs.len() as u64).to_le_bytes());
    for input in &transition.consumed_inputs {
        hash_str(&mut hasher, input);
    }
    hasher.update((transition.facts.len() as u64).to_le_bytes());
    for fact in &transition.facts {
        hash_payload(&mut hasher, fact);
    }
    hash_opt_payload(&mut hasher, transition.compaction.as_ref());
    hasher.finalize().to_vec()
}

fn hash_str(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn hash_bytes(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn hash_opt_str(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(text) => {
            hasher.update([1]);
            hash_str(hasher, text);
        }
        None => hasher.update([0]),
    }
}

fn hash_opt_u64(hasher: &mut Sha256, value: Option<u64>) {
    match value {
        Some(n) => {
            hasher.update([1]);
            hasher.update(n.to_le_bytes());
        }
        None => hasher.update([0]),
    }
}

fn hash_payload(hasher: &mut Sha256, payload: &Payload) {
    hash_str(hasher, &payload.profile);
    hash_str(hasher, &payload.content_type);
    hash_bytes(hasher, &payload.bytes);
}

fn hash_opt_payload(hasher: &mut Sha256, payload: Option<&Payload>) {
    match payload {
        Some(item) => {
            hasher.update([1]);
            hash_payload(hasher, item);
        }
        None => hasher.update([0]),
    }
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn is_json_payload(payload: &Payload) -> bool {
    payload.content_type == "application/json"
        || payload.content_type.ends_with("+json")
        || payload.profile.ends_with(".json")
        || payload.profile.ends_with("+json")
}

fn decode_structured_payload(payload: &Payload) -> Result<Option<JsValue>, LedgerError> {
    if payload.profile == boundary_codec::PROFILE {
        let decoded = boundary_codec::decode(&payload.bytes).map_err(|err| {
            LedgerError::InvalidPayload(format!("boundary_codec decode failed: {err:?}"))
        })?;
        return Ok(Some(decoded));
    }
    if is_json_payload(payload) {
        let text = std::str::from_utf8(&payload.bytes).map_err(|err| {
            LedgerError::InvalidPayload(format!("JSON payload is not valid UTF-8: {err}"))
        })?;
        let parsed = parse_json(text).map_err(|err| {
            LedgerError::InvalidPayload(format!("JSON payload parse failed: {err}"))
        })?;
        return Ok(Some(parsed));
    }
    // Opaque binary profiles remain lossless (`TC-03`), without forced UTF-8 decoding.
    Ok(None)
}

pub(crate) fn extract_messages_from_payload(
    payload: &Payload,
) -> Result<Option<Vec<JsValue>>, LedgerError> {
    let Some(decoded) = decode_structured_payload(payload)? else {
        return Ok(None);
    };
    match &decoded {
        JsValue::Array(items) => Ok(Some(items.clone())),
        JsValue::Object(obj) => {
            if let Some(messages) = obj.get("messages").filter(|v| !v.is_undefined()) {
                let JsValue::Array(items) = messages else {
                    return Err(LedgerError::InvalidPayload(
                        "payload.messages must be an array".into(),
                    ));
                };
                return Ok(Some(items.clone()));
            }
            if let Some(kept) = obj.get("messagesToKeep").filter(|v| !v.is_undefined()) {
                let JsValue::Array(items) = kept else {
                    return Err(LedgerError::InvalidPayload(
                        "payload.messagesToKeep must be an array".into(),
                    ));
                };
                return Ok(Some(items.clone()));
            }
            Ok(None)
        }
        _ => Err(LedgerError::InvalidPayload(
            "structured payload must be a message array or object".into(),
        )),
    }
}

fn validate_payload_integrity(
    payload: &Payload,
    require_structured_container: bool,
) -> Result<(), LedgerError> {
    if payload.profile.is_empty() || payload.profile.len() > MAX_ID_BYTES {
        return Err(LedgerError::InvalidPayload(
            "payload.profile must be 1..=256 bytes".into(),
        ));
    }
    if payload.content_type.is_empty() || payload.content_type.len() > MAX_ID_BYTES {
        return Err(LedgerError::InvalidPayload(
            "payload.content_type must be 1..=256 bytes".into(),
        ));
    }
    if payload.bytes.len() > MAX_LEDGER_PAYLOAD_BYTES {
        return Err(LedgerError::InvalidPayload(format!(
            "payload exceeds {} byte limit ({})",
            MAX_LEDGER_PAYLOAD_BYTES,
            payload.bytes.len()
        )));
    }
    let Some(decoded) = decode_structured_payload(payload)? else {
        return Ok(());
    };
    match &decoded {
        JsValue::Array(items) => validate_tool_pair_integrity(items),
        JsValue::Object(obj) => {
            for key in ["messages", "messagesToKeep"] {
                if let Some(value) = obj.get(key).filter(|v| !v.is_undefined()) {
                    let JsValue::Array(items) = value else {
                        return Err(LedgerError::InvalidPayload(format!(
                            "payload.{key} must be an array"
                        )));
                    };
                    validate_tool_pair_integrity(items)?;
                }
            }
            Ok(())
        }
        _ if require_structured_container => Err(LedgerError::InvalidPayload(
            "structured context payload must be an array or object".into(),
        )),
        _ => Ok(()),
    }
}

/// Validate canonical tool-call / tool-result pairing on a wire message sequence.
///
/// Rejects orphaned `tool_call` / `tool_use` blocks, orphaned `tool_result` /
/// `tool_result_reference` / `media_reference` blocks, missing or empty IDs,
/// role/block mismatches, and out-of-order pairing before or across turns.
pub fn validate_tool_pair_integrity(messages: &[JsValue]) -> Result<(), LedgerError> {
    let mut canonical = Vec::with_capacity(messages.len());
    for (msg_idx, message) in messages.iter().enumerate() {
        canonical.push(canonicalize_and_validate_message(msg_idx, message)?);
    }

    let call_ids = collect_tool_call_ids(&canonical);
    let result_ids = collect_tool_result_ids(&canonical);

    for id in call_ids.iter() {
        if !result_ids.contains(id) {
            return Err(LedgerError::ToolPairIntegrity(format!(
                "orphaned tool_call/tool_use without matching tool_result: {}",
                display_js_string(id)
            )));
        }
    }
    for id in result_ids.iter() {
        if !call_ids.contains(id) {
            return Err(LedgerError::ToolPairIntegrity(format!(
                "orphaned tool_result without matching tool_call/tool_use: {}",
                display_js_string(id)
            )));
        }
    }

    for (msg_idx, message) in canonical.iter().enumerate() {
        for block in content(message) {
            if is_media_reference_with_id(block)
                && let Some(id) = string_field(block, "toolCallId")
                && !call_ids.contains(id)
            {
                return Err(LedgerError::ToolPairIntegrity(format!(
                    "message {msg_idx}: orphaned media_reference toolCallId {}",
                    display_js_string(id)
                )));
            }
        }
    }

    verify_sequential_pairing(&canonical)?;

    // Cross-check with `pilotdeck_context_core::tool_pair` stripping passes so
    // any block that `snip` / `tool_pair` considers dangling fails closed.
    let after_calls = strip_unpaired_tool_calls(&canonical, &result_ids);
    let paired_calls = collect_tool_call_ids(&after_calls);
    let after_results = strip_unpaired_tool_results(&after_calls, &paired_calls);
    if collect_tool_call_ids(&after_results).iter().count() != call_ids.iter().count()
        || collect_tool_result_ids(&after_results).iter().count() != result_ids.iter().count()
    {
        return Err(LedgerError::ToolPairIntegrity(
            "message sequence contains unpaired tool_call/tool_result blocks".into(),
        ));
    }

    Ok(())
}

fn canonicalize_and_validate_message(
    msg_idx: usize,
    message: &JsValue,
) -> Result<JsValue, LedgerError> {
    let Some(msg_obj) = message.as_object() else {
        return Err(LedgerError::ToolPairIntegrity(format!(
            "message {msg_idx} must be an object"
        )));
    };
    let Some(msg_role) = role(message).filter(|r| !r.is_empty()) else {
        return Err(LedgerError::ToolPairIntegrity(format!(
            "message {msg_idx} missing non-empty string role"
        )));
    };
    let is_assistant = msg_role.equals_str("assistant");
    let is_user = msg_role.equals_str("user");

    let Some(raw_content) = msg_obj.get("content") else {
        return Err(LedgerError::ToolPairIntegrity(format!(
            "message {msg_idx} missing content field"
        )));
    };
    match raw_content {
        JsValue::String(_) => Ok(message.clone()),
        JsValue::Array(blocks) => {
            let mut updated_blocks = Vec::with_capacity(blocks.len());
            for (block_idx, block) in blocks.iter().enumerate() {
                let Some(block_obj) = block.as_object() else {
                    return Err(LedgerError::ToolPairIntegrity(format!(
                        "message {msg_idx} block {block_idx} must be an object"
                    )));
                };
                let Some(kind) = block_type(block).filter(|k| !k.is_empty()) else {
                    return Err(LedgerError::ToolPairIntegrity(format!(
                        "message {msg_idx} block {block_idx} missing non-empty string type"
                    )));
                };
                if kind.equals_str("tool_call") || kind.equals_str("tool_use") {
                    if !is_assistant {
                        return Err(LedgerError::ToolPairIntegrity(format!(
                            "message {msg_idx} block {block_idx}: tool_call/tool_use must belong to assistant role"
                        )));
                    }
                    let Some(id) = string_field(block, "id").filter(|id| !is_blank_js(id)) else {
                        return Err(LedgerError::ToolPairIntegrity(format!(
                            "message {msg_idx} block {block_idx}: corrupted tool_call/tool_use missing id"
                        )));
                    };
                    let _ = id;
                    if kind.equals_str("tool_use") {
                        updated_blocks.push(with_field(block_obj, "type", "tool_call".into()));
                    } else {
                        updated_blocks.push(block.clone());
                    }
                } else if kind.equals_str("tool_result") || kind.equals_str("tool_result_reference")
                {
                    if !is_user {
                        return Err(LedgerError::ToolPairIntegrity(format!(
                            "message {msg_idx} block {block_idx}: tool_result must belong to user role"
                        )));
                    }
                    let call_id = extract_tool_result_id(msg_idx, block_idx, block)?;
                    let mut cloned = block_obj.clone();
                    if cloned.get("toolCallId").is_none() {
                        cloned.insert("toolCallId".into(), JsValue::String(call_id));
                    }
                    updated_blocks.push(JsValue::Object(cloned));
                } else if kind.equals_str("media_reference") {
                    if let Some(raw_id) = block_obj.get("toolCallId").filter(|v| !v.is_undefined())
                    {
                        if !is_user {
                            return Err(LedgerError::ToolPairIntegrity(format!(
                                "message {msg_idx} block {block_idx}: media_reference with toolCallId must belong to user role"
                            )));
                        }
                        let Some(id) = raw_id.as_string().filter(|id| !is_blank_js(id)) else {
                            return Err(LedgerError::ToolPairIntegrity(format!(
                                "message {msg_idx} block {block_idx}: media_reference has invalid toolCallId"
                            )));
                        };
                        let _ = id;
                    }
                    updated_blocks.push(block.clone());
                } else {
                    updated_blocks.push(block.clone());
                }
            }
            let mut cloned_msg = msg_obj.clone();
            cloned_msg.insert("content".into(), JsValue::Array(updated_blocks));
            Ok(JsValue::Object(cloned_msg))
        }
        _ => Err(LedgerError::ToolPairIntegrity(format!(
            "message {msg_idx} content must be an array or string"
        ))),
    }
}

fn extract_tool_result_id(
    msg_idx: usize,
    block_idx: usize,
    block: &JsValue,
) -> Result<JsString, LedgerError> {
    let obj = block.as_object().expect("validated object");
    let direct = match obj.get("toolCallId").filter(|v| !v.is_undefined()) {
        None => None,
        Some(value) => {
            let Some(id) = value.as_string().filter(|id| !is_blank_js(id)) else {
                return Err(LedgerError::ToolPairIntegrity(format!(
                    "message {msg_idx} block {block_idx}: corrupted tool_result toolCallId"
                )));
            };
            Some(id.clone())
        }
    };
    let anthropic = match obj.get("tool_use_id").filter(|v| !v.is_undefined()) {
        None => None,
        Some(value) => {
            let Some(id) = value.as_string().filter(|id| !is_blank_js(id)) else {
                return Err(LedgerError::ToolPairIntegrity(format!(
                    "message {msg_idx} block {block_idx}: corrupted tool_result tool_use_id"
                )));
            };
            Some(id.clone())
        }
    };
    match (direct, anthropic) {
        (Some(a), Some(b)) if a != b => Err(LedgerError::ToolPairIntegrity(format!(
            "message {msg_idx} block {block_idx}: conflicting toolCallId and tool_use_id"
        ))),
        (Some(id), _) | (None, Some(id)) => Ok(id),
        (None, None) => Err(LedgerError::ToolPairIntegrity(format!(
            "message {msg_idx} block {block_idx}: corrupted tool_result missing toolCallId"
        ))),
    }
}

fn verify_sequential_pairing(messages: &[JsValue]) -> Result<(), LedgerError> {
    let mut pending: Vec<JsString> = Vec::new();
    let mut seen_call_ids: HashSet<JsString> = HashSet::new();

    for (msg_idx, message) in messages.iter().enumerate() {
        if is_role(message, "assistant") {
            if !pending.is_empty() {
                return Err(LedgerError::ToolPairIntegrity(format!(
                    "message {msg_idx}: assistant turn arrived before pending tool_call results were provided"
                )));
            }
            for block in content(message) {
                if is_block(block, "tool_call")
                    && let Some(id) = string_field(block, "id")
                {
                    if !seen_call_ids.insert(id.clone()) {
                        return Err(LedgerError::ToolPairIntegrity(format!(
                            "message {msg_idx}: duplicate tool_call id {}",
                            display_js_string(id)
                        )));
                    }
                    pending.push(id.clone());
                }
            }
            continue;
        }

        if is_role(message, "user") {
            let blocks = content(message);
            let mut matched_any = false;
            for block in blocks {
                if is_direct_tool_result(block)
                    && let Some(id) = string_field(block, "toolCallId")
                {
                    let Some(pos) = pending.iter().position(|pending_id| pending_id == id) else {
                        return Err(LedgerError::ToolPairIntegrity(format!(
                            "message {msg_idx}: tool_result {} does not match a pending preceding tool_call",
                            display_js_string(id)
                        )));
                    };
                    pending.remove(pos);
                    matched_any = true;
                }
            }
            if !pending.is_empty() && !matched_any {
                return Err(LedgerError::ToolPairIntegrity(format!(
                    "message {msg_idx}: user message missing required tool_result for pending tool_call"
                )));
            }
            continue;
        }

        if !pending.is_empty() {
            return Err(LedgerError::ToolPairIntegrity(format!(
                "message {msg_idx}: non-user turn interrupted pending tool_call results"
            )));
        }
    }

    if let Some(dangling) = pending.first() {
        return Err(LedgerError::ToolPairIntegrity(format!(
            "trailing unpaired tool_call without tool_result: {}",
            display_js_string(dangling)
        )));
    }
    Ok(())
}

fn with_field(obj: &JsObject, key: &str, value: JsValue) -> JsValue {
    let mut cloned = obj.clone();
    cloned.insert(key.into(), value);
    JsValue::Object(cloned)
}

fn is_blank_js(value: &JsString) -> bool {
    value
        .units()
        .iter()
        .all(|u| pilotdeck_context_core::wire::is_js_whitespace(*u))
}

fn display_js_string(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("ledger future must resolve immediately"),
        }
    }

    fn json_payload(json: &str) -> Payload {
        Payload {
            profile: "pilotdeck.messages.json".into(),
            content_type: "application/json".into(),
            bytes: json.as_bytes().to_vec(),
        }
    }

    fn boundary_payload(value: &JsValue) -> Payload {
        Payload {
            profile: boundary_codec::PROFILE.into(),
            content_type: BOUNDARY_CONTENT_TYPE.into(),
            bytes: boundary_codec::encode(value).unwrap(),
        }
    }

    #[test]
    fn maps_all_five_operations_and_enforces_revision_cas() {
        let ledger = AuthoritativeContextLedger::new("sess-1");
        assert_eq!(ledger.snapshot().revision, 0);

        for (idx, op) in ContextOperationKind::all().into_iter().enumerate() {
            assert!(ledger.supports_operation(op.to_runtime()));
            let rev = idx as u64;
            let receipt = ledger
                .commit_transition(
                    LedgerTransition::new(format!("tx-{idx}"), rev, op.to_runtime()).with_control(
                        ContextControl {
                            route: Some("anthropic/claude".into()),
                            ..Default::default()
                        },
                    ),
                )
                .unwrap();
            assert_eq!(receipt.revision, rev + 1);
            assert_eq!(receipt.operation, op);
        }

        let err = ledger
            .commit_transition(LedgerTransition::new(
                "tx-stale",
                2,
                Operation::PrepareForModel,
            ))
            .unwrap_err();
        assert_eq!(
            err,
            LedgerError::RevisionMismatch {
                expected: 2,
                actual: 5
            }
        );
        assert_eq!(ledger.snapshot().revision, 5);
    }

    #[test]
    fn enforces_idempotency_replay_and_conflict() {
        let ledger = AuthoritativeContextLedger::new("sess-idem");
        let messages = r#"[{"role":"user","content":[{"type":"text","text":"hi"}]}]"#;
        let tx = LedgerTransition::new("tx-1", 0, Operation::PrepareForModel)
            .with_replacement(json_payload(messages))
            .with_consumed_inputs(vec!["in-1".into()]);
        let first = ledger.commit_transition(tx.clone()).unwrap();
        assert_eq!(first.revision, 1);

        // Advance revision further to ensure replay still returns original receipt.
        let _second = ledger
            .commit_transition(LedgerTransition::new("tx-2", 1, Operation::CaptureTurn))
            .unwrap();
        assert_eq!(ledger.snapshot().revision, 2);

        let replayed = ledger.commit_transition(tx).unwrap();
        assert_eq!(replayed, first);

        // Replaying same transaction_id with different content fails closed.
        let mut mutated = LedgerTransition::new("tx-1", 0, Operation::PrepareForModel)
            .with_replacement(json_payload(
                r#"[{"role":"user","content":[{"type":"text","text":"tampered"}]}]"#,
            ));
        mutated.consumed_inputs = vec!["in-1".into()];
        let err = ledger.commit_transition(mutated).unwrap_err();
        assert!(matches!(err, LedgerError::IdempotencyConflict { .. }));

        // Reusing an already consumed input under a new transaction_id fails atomically.
        let err = ledger
            .commit_transition(
                LedgerTransition::new("tx-3", 2, Operation::ApplyToolResults)
                    .with_consumed_inputs(vec!["in-1".into()]),
            )
            .unwrap_err();
        assert_eq!(
            err,
            LedgerError::InputAlreadyConsumed {
                input_id: "in-1".into()
            }
        );
        assert_eq!(ledger.snapshot().revision, 2);
    }

    #[test]
    fn rejects_corrupted_or_orphaned_tool_pairs_fail_closed() {
        let ledger = AuthoritativeContextLedger::new("sess-tools");

        // Valid tool_call + tool_result passes.
        let valid = r#"[
            {"role":"user","content":[{"type":"text","text":"read it"}]},
            {"role":"assistant","content":[{"type":"tool_call","id":"call_1","name":"read_file","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","toolCallId":"call_1","content":[{"type":"text","text":"ok"}]}]}
        ]"#;
        let r = ledger
            .commit_transition(
                LedgerTransition::new("tx-ok", 0, Operation::ApplyToolResults)
                    .with_replacement(json_payload(valid)),
            )
            .unwrap();
        assert_eq!(r.revision, 1);

        // Valid Anthropic tool_use + tool_use_id also passes.
        let valid_anthropic = r#"[
            {"role":"assistant","content":[{"type":"tool_use","id":"call_2","name":"read_file","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_2","content":[{"type":"text","text":"ok"}]}]}
        ]"#;
        let r2 = ledger
            .commit_transition(
                LedgerTransition::new("tx-ok-anthropic", 1, Operation::TryAutoCompact)
                    .with_compaction(json_payload(valid_anthropic)),
            )
            .unwrap();
        assert_eq!(r2.revision, 2);

        // Orphaned tool_use / tool_call / tool_result / wrong order / empty IDs fail closed.
        let bad_cases = [
            // Orphaned tool_use
            r#"[{"role":"assistant","content":[{"type":"tool_use","id":"orphan_1","name":"read_file","input":{}}]}]"#,
            // Orphaned tool_call
            r#"[{"role":"assistant","content":[{"type":"tool_call","id":"orphan_2","name":"read_file","input":{}}]}]"#,
            // Orphaned tool_result
            r#"[{"role":"user","content":[{"type":"tool_result","toolCallId":"missing","content":[]}]}]"#,
            // Out-of-order tool_result before tool_call
            r#"[
                {"role":"user","content":[{"type":"tool_result","toolCallId":"c1","content":[]}]},
                {"role":"assistant","content":[{"type":"tool_call","id":"c1","name":"read_file","input":{}}]}
            ]"#,
            // Empty tool_use id
            r#"[{"role":"assistant","content":[{"type":"tool_use","id":"","name":"read_file","input":{}}]}]"#,
            // Orphaned media_reference
            r#"[{"role":"user","content":[{"type":"media_reference","toolCallId":"ghost","preview":"p"}]}]"#,
        ];

        for (idx, bad) in bad_cases.iter().enumerate() {
            let err = ledger
                .commit_transition(
                    LedgerTransition::new(format!("tx-bad-{idx}"), 2, Operation::ApplyToolResults)
                        .with_replacement(json_payload(bad)),
                )
                .unwrap_err();
            assert!(
                matches!(err, LedgerError::ToolPairIntegrity(_)),
                "expected ToolPairIntegrity for case {idx}, got {err:?}"
            );
            // Revision and payload must remain untouched on failure.
            assert_eq!(ledger.snapshot().revision, 2);
        }
    }

    #[test]
    fn preserves_lossless_binary_and_lone_surrogate_payloads() {
        let ledger = AuthoritativeContextLedger::new("sess-lossless");

        // Opaque binary bytes (`TC-03`) are not forced through UTF-8 decoding.
        let raw_bytes = vec![0xff, 0x00, 0xfe, 0x80, 0x01, 0x02];
        let opaque = Payload {
            profile: "pilotdeck.opaque.bin.v1".into(),
            content_type: "application/octet-stream".into(),
            bytes: raw_bytes.clone(),
        };
        ledger
            .commit_transition(
                LedgerTransition::new("tx-bin", 0, Operation::PrepareForModel)
                    .with_replacement(opaque.clone()),
            )
            .unwrap();
        assert_eq!(ledger.snapshot().payload.bytes, raw_bytes);

        // `boundary_codec` payload preserves lone surrogates losslessly.
        let mut msg = JsObject::new();
        msg.insert("role".into(), "user".into());
        let mut blk = JsObject::new();
        blk.insert("type".into(), "text".into());
        blk.insert(
            "text".into(),
            JsValue::String(JsString::from_utf16(vec![0xd800, 0x0061])),
        );
        msg.insert("content".into(), JsValue::Array(vec![JsValue::Object(blk)]));
        let boundary = boundary_payload(&JsValue::Array(vec![JsValue::Object(msg)]));
        ledger
            .commit_transition(
                LedgerTransition::new("tx-utf16", 1, Operation::PrepareForModel)
                    .with_replacement(boundary.clone()),
            )
            .unwrap();
        assert_eq!(ledger.snapshot().payload, boundary);
    }

    #[test]
    fn history_trait_bridge_supports_read_commit_lookup_and_conflicts() {
        let ledger = AuthoritativeContextLedger::new("sess-history");
        let history: &dyn History = &ledger;

        let initial = block_on(history.read()).unwrap();
        assert_eq!(initial.revision, 0);
        assert!(initial.receipt.is_none());

        let commit = Commit {
            transaction_id: "hist-1".into(),
            expected_revision: 0,
            replacement: Some(json_payload(
                r#"[{"role":"user","content":[{"type":"text","text":"hello"}]}]"#,
            )),
            consumed_inputs: vec!["evt-1".into()],
            facts: vec![],
            compaction: None,
        };
        let receipt = block_on(history.commit(commit.clone())).unwrap();
        assert_eq!(receipt.revision, 1);
        assert_eq!(receipt.consumed_inputs, vec!["evt-1".to_string()]);
        assert_eq!(
            block_on(history.lookup("hist-1")).unwrap(),
            Some(receipt.clone())
        );
        assert_eq!(block_on(history.commit(commit)).unwrap(), receipt);

        // Stale revision fails with Conflict.
        let stale = Commit {
            transaction_id: "hist-2".into(),
            expected_revision: 0,
            replacement: None,
            consumed_inputs: vec![],
            facts: vec![],
            compaction: None,
        };
        assert_eq!(block_on(history.commit(stale)), Err(HistoryError::Conflict));
    }

    #[test]
    fn tc02_rejects_unknown_control_protocol_version_and_unknown_fields_without_downgrade() {
        let ledger = AuthoritativeContextLedger::new("sess-tc02");
        for bad_version in [0_u32, 2, 99, u32::MAX] {
            let control = ContextControl {
                protocol_version: bad_version,
                ..Default::default()
            };
            assert!(matches!(
                control.validate(),
                Err(LedgerError::InvalidTransition(_))
            ));
            let err = ledger
                .commit_transition(
                    LedgerTransition::new(
                        format!("tx-bad-v-{bad_version}"),
                        0,
                        Operation::PrepareForModel,
                    )
                    .with_control(control)
                    .with_consumed_inputs(vec![format!("in-{bad_version}")]),
                )
                .unwrap_err();
            assert!(matches!(err, LedgerError::InvalidTransition(_)));
            assert!(!ledger.is_input_consumed(&format!("in-{bad_version}")));
        }
        let view = ledger.snapshot();
        assert_eq!(view.revision, 0);
        assert!(view.audit_log.is_empty());
        assert!(view.last_receipt.is_none());

        // Serialized ContextControl rejects missing protocol_version or unknown downgrade fields.
        assert!(
            serde_json::from_str::<ContextControl>(
                r#"{"route":null,"protocol":null,"preview_only":false,"manual_force":false,"max_context_tokens":null,"reserved_output_tokens":null}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ContextControl>(
                r#"{"protocol_version":1,"route":null,"protocol":null,"preview_only":false,"manual_force":false,"max_context_tokens":null,"reserved_output_tokens":null,"legacy_fallback":true}"#
            )
            .is_err()
        );
    }

    #[test]
    fn tc03_preserves_non_utf8_binary_payload_across_all_five_operations_and_binds_digest() {
        let ledger = AuthoritativeContextLedger::new("sess-tc03-bin");
        let history: &dyn History = &ledger;
        let ops = [
            Operation::PrepareForModel,
            Operation::ApplyToolResults,
            Operation::RecoverFromModelError,
            Operation::CaptureTurn,
            Operation::TryAutoCompact,
        ];

        for (idx, op) in ops.into_iter().enumerate() {
            let raw_bytes = vec![0xff, 0x00, 0x80, 0xfe, 0xc3, 0x28, idx as u8, 0x7f];
            let binary_payload = Payload {
                profile: format!("pilotdeck.opaque.bin.v1.{}", op.name()),
                content_type: "application/octet-stream".into(),
                bytes: raw_bytes.clone(),
            };
            let fact_payload = Payload {
                profile: "pilotdeck.opaque.fact.v1".into(),
                content_type: "application/octet-stream".into(),
                bytes: vec![0x80, 0xff, 0x00, idx as u8],
            };
            let tx_id = format!("tx-bin-{idx}");
            let mut tx = LedgerTransition::new(tx_id.clone(), idx as u64, op)
                .with_replacement(binary_payload.clone())
                .with_facts(vec![fact_payload.clone()]);
            if op == Operation::TryAutoCompact {
                tx = tx.with_compaction(binary_payload.clone());
            }
            let receipt = ledger.commit_transition(tx.clone()).unwrap();
            assert_eq!(receipt.revision, idx as u64 + 1);

            let snap = ledger.snapshot();
            assert_eq!(snap.payload, binary_payload);
            assert_eq!(snap.payload.bytes, raw_bytes);
            assert_eq!(snap.facts.last(), Some(&fact_payload));
            if op == Operation::TryAutoCompact {
                assert_eq!(snap.latest_compaction.as_ref(), Some(&binary_payload));
            }
            let hist_view = block_on(history.read()).unwrap();
            assert_eq!(hist_view.payload.bytes, raw_bytes);

            // Replaying exact transition returns identical digest; mutating 1 bit conflicts.
            assert_eq!(ledger.commit_transition(tx.clone()).unwrap(), receipt);
            let mut flipped = binary_payload.clone();
            flipped.bytes[0] ^= 0x01;
            let conflict = ledger
                .commit_transition(
                    LedgerTransition::new(tx_id, idx as u64, op)
                        .with_replacement(flipped)
                        .with_facts(vec![fact_payload]),
                )
                .unwrap_err();
            assert!(matches!(conflict, LedgerError::IdempotencyConflict { .. }));
        }
        assert_eq!(ledger.snapshot().revision, 5);
    }

    #[test]
    fn tc05_apply_tool_results_and_capture_turn_concurrent_cas_atomic_append_last() {
        for op in [Operation::ApplyToolResults, Operation::CaptureTurn] {
            let ledger = AuthoritativeContextLedger::new(format!("sess-tc05-{}", op.name()));
            let base =
                json_payload(r#"[{"role":"user","content":[{"type":"text","text":"start"}]}]"#);
            ledger
                .commit_transition(
                    LedgerTransition::new("tx-init", 0, Operation::PrepareForModel)
                        .with_replacement(base.clone()),
                )
                .unwrap();

            let payload_a = json_payload(
                r#"[{"role":"user","content":[{"type":"text","text":"winner-or-loser-A"}]}]"#,
            );
            let payload_b = json_payload(
                r#"[{"role":"user","content":[{"type":"text","text":"winner-or-loser-B"}]}]"#,
            );
            let fact_a = Payload {
                profile: "pilotdeck.fact.v1".into(),
                content_type: "application/octet-stream".into(),
                bytes: vec![0xff, 0x01, 0x80],
            };
            let fact_b = Payload {
                profile: "pilotdeck.fact.v1".into(),
                content_type: "application/octet-stream".into(),
                bytes: vec![0xfe, 0x02, 0x80],
            };

            let (res_a, res_b) = std::thread::scope(|scope| {
                let l1 = ledger.clone();
                let l2 = ledger.clone();
                let pa = payload_a.clone();
                let pb = payload_b.clone();
                let fa = fact_a.clone();
                let fb = fact_b.clone();
                let a = scope.spawn(move || {
                    l1.commit_transition(
                        LedgerTransition::new("tx-race-a", 1, op)
                            .with_replacement(pa)
                            .with_consumed_inputs(vec!["turn-input-a".into()])
                            .with_facts(vec![fa]),
                    )
                });
                let b = scope.spawn(move || {
                    l2.commit_transition(
                        LedgerTransition::new("tx-race-b", 1, op)
                            .with_replacement(pb)
                            .with_consumed_inputs(vec!["turn-input-b".into()])
                            .with_facts(vec![fb]),
                    )
                });
                (a.join().unwrap(), b.join().unwrap())
            });

            let (winner, loser) = match (res_a, res_b) {
                (Ok(win), Err(lose)) => (win, lose),
                (Err(lose), Ok(win)) => (win, lose),
                other => panic!("expected one CAS winner and one loser for {op:?}: {other:?}"),
            };
            assert_eq!(winner.revision, 2);
            assert_eq!(winner.operation, ContextOperationKind::from_runtime(op));
            assert_eq!(
                loser,
                LedgerError::RevisionMismatch {
                    expected: 1,
                    actual: 2
                }
            );

            let snap = ledger.snapshot();
            assert_eq!(snap.revision, 2);
            assert_eq!(snap.facts.len(), 1);
            assert_eq!(snap.audit_log.len(), 2);
            if winner.transaction_id == "tx-race-a" {
                assert_eq!(snap.payload, payload_a);
                assert_eq!(snap.facts, vec![fact_a]);
                assert!(ledger.is_input_consumed("turn-input-a"));
                assert!(!ledger.is_input_consumed("turn-input-b"));
            } else {
                assert_eq!(snap.payload, payload_b);
                assert_eq!(snap.facts, vec![fact_b]);
                assert!(ledger.is_input_consumed("turn-input-b"));
                assert!(!ledger.is_input_consumed("turn-input-a"));
            }

            // Explicit stale revision retry after append-last also fails with zero mutation.
            let stale_err = ledger
                .commit_transition(
                    LedgerTransition::new("tx-stale-after-win", 1, op)
                        .with_consumed_inputs(vec!["turn-input-stale".into()])
                        .with_facts(vec![Payload {
                            profile: "pilotdeck.fact.v1".into(),
                            content_type: "application/octet-stream".into(),
                            bytes: vec![0xaa],
                        }]),
                )
                .unwrap_err();
            assert_eq!(
                stale_err,
                LedgerError::RevisionMismatch {
                    expected: 1,
                    actual: 2
                }
            );
            assert!(!ledger.is_input_consumed("turn-input-stale"));
            assert_eq!(ledger.snapshot().facts.len(), 1);
            assert_eq!(ledger.snapshot().revision, 2);
        }
    }
}
