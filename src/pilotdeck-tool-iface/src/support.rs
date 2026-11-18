//! Shared plumbing for the Kernel tool adapter modules: JSON byte encoding
//! and the unsupported-abort contract behavior.

use agentcpm_iface::{
    BoxFuture,
    types::{ERR_APP, Error, ErrorCategory},
};
use pilotdeck_protocol::{JsValue, stringify_json};

/// Encode a JSON value as UTF-8 bytes, failing closed with `message`.
pub(crate) fn json_bytes(value: &JsValue, message: &str) -> Result<Vec<u8>, Error> {
    stringify_json(value)
        .ok()
        .flatten()
        .map(String::into_bytes)
        .ok_or_else(|| Error::new(ErrorCategory::Internal, ERR_APP, message))
}

/// Synchronous adapters cannot be preempted; report unsupported abort with the
/// adapter-specific `message` (`-32085`).
pub(crate) fn unsupported_abort(message: &'static str) -> BoxFuture<'static, Result<(), Error>> {
    Box::pin(async move { Err(Error::new(ErrorCategory::Execution, -32085, message)) })
}
