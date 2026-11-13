//! Native LSP service and source-level provider boundary. A project generation
//! owns the shared registry; each session owns a separately bound registry.
mod service;
pub mod stdio;
mod types;
pub use service::{LspProviderRegistration, LspService};
pub use types::*;
