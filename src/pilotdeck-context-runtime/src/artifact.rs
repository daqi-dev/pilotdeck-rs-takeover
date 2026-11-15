//! Host-managed immutable spill publication. There is no filesystem fallback.
use std::{future::Future, pin::Pin};
pub type ArtifactFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ArtifactError>> + Send + 'a>>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactReceipt {
    pub reference: String,
    pub owner: String,
    pub context: String,
    pub generation: u64,
    pub digest: Vec<u8>,
    pub length: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactError {
    Unsupported,
    Denied,
    Conflict,
    Failed(String),
    /// Publication may have happened; callers must reconcile by key.
    Unknown(String),
}
/// Implementations bind scope, enforce exclusive no-replace publication, and
/// return stable receipts. Keys are opaque operation-scoped deduplication keys,
/// not caller-selected filesystem paths. Unsupported exclusive publication is
/// an error; it must never fall back to overwrite or exists-then-write.
pub trait ArtifactAccess: Send + Sync {
    fn publish_new<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
        bytes: &'a [u8],
    ) -> ArtifactFuture<'a, ArtifactReceipt>;
    fn copy_new<'a>(
        &'a self,
        key: &'a str,
        source_ref: &'a str,
    ) -> ArtifactFuture<'a, ArtifactReceipt>;
    fn read<'a>(&'a self, reference: &'a str, limit: u64) -> ArtifactFuture<'a, Vec<u8>>;
    fn status<'a>(&'a self, key: &'a str) -> ArtifactFuture<'a, Option<ArtifactReceipt>>;
}
