//! `agentcpm-pd-client`: shared CLI client state machine for target resolution,
//! credential lookup, `pd-connect/1` over `/v1/ws`, local Gateway auto-start,
//! subscription credit tracking, and idempotency key persistence.

pub fn client_crate_name() -> &'static str {
    "agentcpm-pd-client"
}
