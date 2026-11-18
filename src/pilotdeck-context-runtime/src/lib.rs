//! Product Context runtime extraction in progress.
//! Only explicitly exported components are implemented; the five-operation
//! runtime and host adapters are not yet wired.
pub mod compaction;
pub mod history;
pub mod instructions;
pub mod memory;

pub mod artifact;
pub mod cache;
pub mod host;
pub mod locale;
pub mod operations;
pub mod prepare;
pub mod prompt;
mod serialization;
pub mod tool_budget;

pub mod boundary_codec;
