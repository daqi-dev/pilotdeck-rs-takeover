//! PilotDeck model execution foundations, with no ambient network authority.
//!
//! Shares the original async model driver, network arbitration, body readers and
//! redirect policy. Product services supply credentials, audit storage, date
//! parsing and dispatch; this crate contains no socket connector or CA policy.
//! Node-compatible ordering is private to the product runtime contract; it is
//! not an addition to the ifACE transport protocol.
#![forbid(unsafe_code)]

pub mod driver_transport;
pub mod error;
pub mod fetch_redirect;
pub mod headers;
pub mod http;
pub mod network;
mod providers;
pub mod services;
pub mod transport;

pub use error::ModelStreamError;
pub use transport::{
    BodyFailure, ModelBody, ModelHttpTransport, TransportFailure, TransportFuture, TransportResult,
};
