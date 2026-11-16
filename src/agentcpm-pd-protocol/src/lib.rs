//! `agentcpm-pd-protocol`: zero-IO, pure safe Rust contract types, `{nodeId}-{uuidv7}`
//! global identity, `pd-connect/1` JSON-RPC wire frames, and envelope mapping (`WP1a`).

pub mod contract;
pub mod envelope;
pub mod errors;
pub mod framing;
pub mod id;
pub mod wire;
