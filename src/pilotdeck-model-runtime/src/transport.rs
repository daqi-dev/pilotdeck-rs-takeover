//! HTTP exchange boundary without a connection implementation.
use bytes::Bytes;
use http_body_util::{Full, combinators::BoxBody};
use hyper::{Request, Response};

/// Existing Node ERR_STRING_TOO_LONG text; shared with the host redirect/body adapter.
pub const STRING_TOO_LONG: &str = "Cannot create a string longer than 0x1fffffe8 characters";

/// A failed exchange before response headers (undici `TypeError: fetch failed`).
#[derive(Clone, Debug)]
pub struct TransportFailure {
    /// `error.cause.code` as undici reports it (e.g. `ECONNREFUSED`).
    pub cause_code: Option<&'static str>,
    pub message: String,
}
impl TransportFailure {
    pub fn fetch_failed(cause_code: Option<&'static str>) -> Self {
        Self {
            cause_code,
            message: "fetch failed".into(),
        }
    }
}
/// A response body read failure. `Terminated` is undici's
/// `TypeError: terminated` (socket failure or body idle timeout);
/// `StringTooLong` is the V8 error replay of a drained over-limit shape body
/// on the last candidate (real TS `response.text()`); `Decode` is the same
/// `TypeError: terminated` whose `error.cause` holds the content codec's
/// rejection text (Node zlib: "incorrect header check" for a corrupt gzip
/// member). The `cause` stays out of `Display` because the corpus records
/// the error's own message only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyFailure {
    Terminated,
    StringTooLong,
    Decode(&'static str),
}
impl std::fmt::Display for BodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Terminated | Self::Decode(_) => f.write_str("terminated"),
            Self::StringTooLong => f.write_str(STRING_TOO_LONG),
        }
    }
}
impl std::error::Error for BodyFailure {}

pub type ModelBody = BoxBody<Bytes, BodyFailure>;
pub type TransportResult = Result<Response<ModelBody>, TransportFailure>;

/// Shared event source with the provider transport's response type.
pub type TransportFuture = crate::network::ExchangeFuture<TransportResult>;

/// One HTTP exchange. Dropping the future or the response body closes the
/// connection; nothing is detached.
pub trait ModelHttpTransport: Send + Sync {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture;
}
