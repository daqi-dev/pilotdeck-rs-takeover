//! Pure decisions and stream state shared by model execution hosts.
//!
//! The host owns HTTP attempts, authentication, proxy selection, audit order,
//! abort/timer scheduling and reading ambient configuration. This module only
//! consumes explicit inputs; it neither performs nor schedules another attempt.
mod checkpoint;
pub mod errors;
mod guard;
mod retry;
pub mod sse;

pub use checkpoint::{Checkpoint, continuation_request};
pub use guard::StreamGuard;
pub use retry::{retry_decide, retry_decide_request};

#[cfg(test)]
mod tests;
