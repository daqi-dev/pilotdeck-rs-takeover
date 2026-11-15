//! Atomic history capability. Implementations are provided by the host owner.
//! This module deliberately contains no persistence implementation.
use std::{future::Future, pin::Pin};

pub type HistoryFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, HistoryError>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub profile: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub transaction_id: String,
    pub request_digest: Vec<u8>,
    pub revision: u64,
    pub consumed_inputs: Vec<String>,
    pub event_cursor: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub revision: u64,
    pub payload: Payload,
    pub receipt: Option<Receipt>,
}

/// All fields are committed as one owner transaction, or none are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub transaction_id: String,
    pub expected_revision: u64,
    pub replacement: Option<Payload>,
    pub consumed_inputs: Vec<String>,
    pub facts: Vec<Payload>,
    pub compaction: Option<Payload>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryError {
    Conflict,
    Invalid(String),
    Unavailable(String),
    /// Outcome cannot be established; callers must reconcile, not replay.
    Unknown(String),
}

/// Implementations bind identity and authority outside request payloads. A
/// transaction ID replayed with the same digest returns the original receipt;
/// replay with different content fails. A commit must atomically compare the
/// revision and persist replacement, input consumption, facts and receipt.
pub trait History: Send + Sync {
    fn read(&self) -> HistoryFuture<'_, View>;
    fn commit(&self, request: Commit) -> HistoryFuture<'_, Receipt>;
    fn lookup<'a>(&'a self, transaction_id: &'a str) -> HistoryFuture<'a, Option<Receipt>>;
}
