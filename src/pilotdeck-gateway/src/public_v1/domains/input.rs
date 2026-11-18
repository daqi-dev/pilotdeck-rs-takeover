//! `input.*` — CLI-local explicit input sources (`t007`).
//!
//! `input.submit`（`execution='local'`, capability `cli.capability.default-input`）
//! reads one explicit input source — `argument` text or stdin-until-EOF — and
//! submits a single run by calling `run.submit` as a client. The contract's
//! postcondition is explicit: the client only reads and calls `run.submit`;
//! no new backend stdin entry is created. The whole operation therefore lives
//! in the CLI (`agentcpm-pd-cli/src/local_ops/input.rs`); this gateway-side
//! file registers availability only (the `system.rs` precedent for CLI-local
//! lifecycle ops), so `system.capabilities` and the implemented-set gates stay
//! truthful about the operation existing in this product.
//!
//! `input.follow`（非 TUI 逐行多轮）belongs to `t010` and is intentionally
//! NOT registered here.
//!
//! The input admission itself is `run.submit`'s state machine: an open
//! session accepts the new run (queue admission, `QUEUE_PAUSED` refusal, and
//! once accepted the session worker drives it — queued runs wait for the
//! session, an idle session starts immediately). Nothing in this file
//! duplicates that.

use crate::public_v1::domains::DomainRegistry;

pub(super) fn register(reg: &mut DomainRegistry) {
    // Availability-only: `/v1/ws` refuses `execution='local'` operations
    // before the availability gate (`runtime.rs::dispatch`); the CLI local
    // table owns the real handler.
    reg.available("input.submit");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_submit_is_available_without_a_dispatch_handler() {
        let domains = DomainRegistry::global();
        assert!(
            domains.is_registered("input.submit"),
            "`input.submit` must be registered as implemented (availability-only)"
        );
        assert!(
            domains.handler_for("input.submit").is_none(),
            "`input.submit` is execution='local' and must not carry a /v1/ws handler"
        );
    }
}
