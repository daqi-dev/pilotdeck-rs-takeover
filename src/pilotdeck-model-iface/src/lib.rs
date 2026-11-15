//! Trusted PilotDeck model session adapter for the public kernel interface.
#![forbid(unsafe_code)]

mod event;
mod session;
mod sse;
pub use session::PilotDeckModelFactory;
