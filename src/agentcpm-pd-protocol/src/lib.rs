//! `agentcpm-pd-protocol`: zero-IO, pure safe Rust contract types, `{nodeId}-{uuidv7}`
//! global identity, `pd-connect/1` JSON-RPC wire frames, envelope mapping (`WP1a`),
//! 285-operation embedded registry, and Draft 2020-12 schema validator (`WP1b`).

pub mod contract;
pub mod envelope;
pub mod errors;
pub mod framing;
pub mod id;
pub mod registry;
pub mod schema_validator;
pub mod wire;
