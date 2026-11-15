//! PilotDeck model execution foundations, with no ambient network authority.
//!
//! The original network arbitration and model transport types are shared through
//! Gateway re-exports. This first step does not contain an async
//! model driver, a socket connector, credential loading, proxy or CA policy.
//! Node-compatible ordering is private to the product runtime contract; it is
//! not an addition to the ifACE transport protocol.
#![forbid(unsafe_code)]

pub mod error;
pub mod network;
pub mod transport;

pub use error::ModelStreamError;
pub use transport::{
    BodyFailure, ModelBody, ModelHttpTransport, TransportFailure, TransportFuture, TransportResult,
};
