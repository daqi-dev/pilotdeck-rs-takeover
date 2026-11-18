//! Public Kernel ifACE context adapters (`iface/context`, `iface/store`, `iface/types`)
//! and revisioned authoritative context state ledger for PilotDeck.
//!
//! # Honest Scope & Boundary
//! - [`PilotDeckTokenCounter`] bridges `pilotdeck_tokenizer::count_tokens` (`o200k_base`)
//!   to [`agentcpm_iface::context::TokenCounter`].
//! - [`AuthoritativeContextLedger`] provides single-authority optimistic concurrency
//!   (`expected_revision`), idempotent `transaction_id` + SHA-256 request digest
//!   verification, atomic input consumption, tool-pair integrity checks via
//!   `pilotdeck_context_core::tool_pair`, and a `pilotdeck_context_runtime::history::History`
//!   adapter across all five PilotDeck context operations (`TC-02`, `TC-03`, `TC-05`).
//! - [`PilotDeckContextContract`] and [`PilotDeckCompactionPersistence`] bridge
//!   [`agentcpm_iface::store::StoreContract`] and `pilotdeck_context_core` Snip/Micro
//!   read-only projections cleanly without mutating authority during read.
//! - End-to-end non-Recovery Gateway Context replacement still lives in
//!   `pilotdeck-gateway`; this crate adapts the public `agentcpm-iface` contracts and
//!   does not replace Gateway's full five-operation runtime host wiring on its own.
#![forbid(unsafe_code)]

pub mod contract;
pub mod state_authority;
pub mod token_counter;

pub use contract::{
    KIND_COMPACTION, KIND_CONTEXT_INPUT, KIND_TOOL_REGISTERED, PilotDeckCompactionPersistence,
    PilotDeckContextContract, SNAPSHOT_CONTENT_TYPE, compute_compaction_digest,
};
pub use state_authority::{
    AuthoritativeContextLedger, CONTEXT_LEDGER_PROTOCOL_VERSION, ContextControl,
    ContextOperationKind, LedgerAuditEntry, LedgerError, LedgerReceipt, LedgerTransition,
    LedgerView, validate_tool_pair_integrity,
};
pub use token_counter::{MESSAGES_PER_TURN_OVERHEAD, PilotDeckTokenCounter};
