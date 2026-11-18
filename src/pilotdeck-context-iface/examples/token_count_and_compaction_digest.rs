//! Minimal usage of the context-iface token counter and compaction digest
//! projections.
//!
//! Run with `cargo run -p pilotdeck-context-iface --example token_count_and_compaction_digest`.
#![forbid(unsafe_code)]

use pilotdeck_context_iface::{PilotDeckTokenCounter, compute_compaction_digest};
use pilotdeck_protocol::JsString;

fn main() {
    // 1) Count tokens with the PilotDeck tokenizer projection.
    let counter = PilotDeckTokenCounter::new();
    let tokens = counter.count_js_string(&JsString::from("hello from context-iface"));
    println!("tokens: {tokens}");

    // 2) Derive the deterministic compaction digest used to pin compacted
    //    context state (KIND_COMPACTION / SNAPSHOT_CONTENT_TYPE profile).
    let digest = compute_compaction_digest(7, b"boundary snapshot", b"compacted messages");
    println!("compaction digest: {digest}");
}
