//! Complete pure provider request builders and stateful canonical event codecs.
//! HTTP, credentials, retries, permissions and session ownership belong to callers.
pub mod error;
pub mod js;
mod json_walk;
pub mod limits;
pub mod metadata;
pub mod projection;
pub mod property;
pub mod providers;
pub use error::CodecError;
pub use providers::{
    StreamState, build_model_request, chat_endpoint_candidates, normalize_provider_base_url,
    provider_headers, validate_model_request,
};
#[cfg(test)]
mod test_baseline;

mod request_started;
pub use request_started::request_started_event;

pub mod normalize_error;
