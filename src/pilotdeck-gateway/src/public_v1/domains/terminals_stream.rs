//! `terminals.output` stream source (t015 PR-3): the retained byte ring
//! observed through byte cursors.
//!
//! The source is the `terminals.output` data plane registered with the
//! generic subscription pump (`ws.rs`'s [`StreamSource`]). Frames are the
//! contract's four result types: `bytes` (chunked at ≤512 KiB so the
//! serialized base64 `sub.event` stays far under the 1 MiB wire cap),
//! `gap` (the ring evicted the requested window — the consumer is told
//! explicitly and continues from `earliestAvailableCursor`, never a silent
//! skip), `exit` (terminal ended; its `exitCode`/`exitSignal` follow the
//! final retained byte), and the conn-level heartbeat covers liveness.
//!
//! A cursor whose `outputEpoch` does not match the terminal is rejected at
//! open with `CURSOR_INVALID`; one behind the retained window at open is
//! rejected with `CURSOR_EXPIRED` (mirroring `run.events`' open-time
//! rejection), while the same condition mid-stream surfaces as the in-band
//! `gap` frame. A terminal with no live handle (the gateway restarted under
//! it) reports its `lost`/terminal facts through the `exit` frame and ends
//! the stream.

use std::sync::Arc;

use agentcpm_pd_protocol::contract::EventEnvelope;
use agentcpm_pd_protocol::errors::{CODE_CURSOR_EXPIRED, ContractError, EffectStatus, RetrySpec};
use agentcpm_pd_protocol::wire::{
    JSONRPC_VERSION, JsonRpcNotification, SubEventNotification, WIRE_SUB_EVENT,
};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::public_v1::PublicV1State;
use crate::public_v1::domains::terminals::{
    StoredTerminal, bytes_frame, exit_frame, gap_frame, read_cursor, stream_cursor,
};
use crate::public_v1::domains::terminals_pty::{ExitFacts, OUTPUT_FRAME_BYTES, live_terminal};
use crate::public_v1::ws::{StreamFetch, StreamFrame, StreamSource};

pub(crate) struct TerminalOutputSource {
    instance_id: String,
    row: StoredTerminal,
    sub_id: String,
    terminal_id: String,
    offset: u64,
    done: bool,
    wake: Arc<Notify>,
}

/// The `terminals.output` factory registered with the pump.
pub(crate) fn terminal_output_source(
    state: &Arc<PublicV1State>,
    input: &Value,
    sub_id: &str,
) -> Result<Box<dyn StreamSource>, ContractError> {
    let store = state.runtime.store();
    let terminal_id = input["terminalId"].as_str().unwrap_or_default().to_owned();
    let row = store.terminal_row(&terminal_id)?;
    let live = live_terminal(&terminal_id);
    let offset = read_cursor(input, &row, live.as_ref())?;
    // An open-time cursor behind the retained window is rejected like
    // run.events' expired afterCursor: -32000 CURSOR_EXPIRED, with the gap
    // descriptor in details so the consumer can restart from the earliest
    // retained cursor.
    let earliest = live
        .as_ref()
        .map(|h| h.ring_positions().0)
        .unwrap_or(row.earliest_output_offset as u64);
    if offset < earliest {
        let mut err = ContractError::new(
            CODE_CURSOR_EXPIRED,
            format!(
                "cursor byteOffset `{offset}` is behind the retained window (earliest `{earliest}`); re-open from the earliest available cursor"
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        );
        err.details = Some(json!({
            "gap": {
                "kind": "gap",
                "requestedCursor": stream_cursor(&row, offset),
                "earliestCursor": stream_cursor(&row, earliest),
                "recovery": "restart_query",
            }
        }));
        return Err(err);
    }
    let wake = live
        .as_ref()
        .map(|h| h.notify.clone())
        .unwrap_or_else(|| Arc::new(Notify::new()));
    Ok(Box::new(TerminalOutputSource {
        instance_id: store.meta().instance_id.clone(),
        row,
        sub_id: sub_id.to_owned(),
        terminal_id,
        offset,
        done: false,
        wake,
    }))
}

impl StreamSource for TerminalOutputSource {
    fn fetch_next(&mut self, batch: usize) -> Result<StreamFetch, ContractError> {
        if self.done {
            return Ok(StreamFetch {
                frames: Vec::new(),
                stream_ended: true,
            });
        }
        let mut frames: Vec<StreamFrame> = Vec::new();
        if let Some(handle) = live_terminal(&self.terminal_id) {
            let (earliest, _latest) = handle.ring_positions();
            if self.offset < earliest {
                let frame = gap_frame(&self.row, self.offset, earliest);
                self.offset = earliest;
                frames.push(self.frame(frame));
            }
            while frames.len() < batch {
                match handle.copy_output(self.offset, OUTPUT_FRAME_BYTES) {
                    Some(chunk) if chunk.is_empty() => break,
                    Some(chunk) => {
                        let next = self.offset + chunk.len() as u64;
                        let frame = bytes_frame(&self.row, self.offset, next, &chunk);
                        self.offset = next;
                        frames.push(self.frame(frame));
                    }
                    None => {
                        // The ring moved under us mid-batch (a concurrent
                        // eviction): report the gap, continue from earliest.
                        let (earliest_now, _) = handle.ring_positions();
                        let frame = gap_frame(&self.row, self.offset, earliest_now);
                        self.offset = earliest_now;
                        frames.push(self.frame(frame));
                    }
                }
            }
            // Exit facts are recorded only after the leader is reaped and
            // the master drained, so `latest` is final here.
            if let Some(facts) = handle.exit_facts() {
                let (_, latest) = handle.ring_positions();
                if self.offset >= latest {
                    let frame = exit_frame(&self.row, self.offset, &facts);
                    self.done = true;
                    frames.push(self.frame(frame));
                    return Ok(StreamFetch {
                        frames,
                        stream_ended: true,
                    });
                }
            }
            Ok(StreamFetch {
                frames,
                stream_ended: false,
            })
        } else {
            // No live handle: the terminal predates this gateway boot (lost)
            // or its runtime is already gone — report the durable facts and
            // end the stream.
            let facts = ExitFacts {
                exit_code: self.row.exit_code.map(|c| c as i32),
                exit_signal: self.row.exit_signal.clone(),
            };
            let frame = exit_frame(&self.row, self.offset, &facts);
            self.done = true;
            frames.push(self.frame(frame));
            Ok(StreamFetch {
                frames,
                stream_ended: true,
            })
        }
    }

    fn wake(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    fn oversized_recovery_hint(&self) -> &'static str {
        "re-open from earliestAvailableCursor after the reported gap"
    }
}

impl TerminalOutputSource {
    /// Wraps one result frame into a serialized `sub.event` notification.
    fn frame(&self, data: Value) -> StreamFrame {
        let offset = data["cursor"]["byteOffset"].as_u64().unwrap_or(self.offset);
        let cursor = stream_cursor(&self.row, offset);
        let notif = JsonRpcNotification {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: WIRE_SUB_EVENT.to_owned(),
            params: serde_json::to_value(SubEventNotification {
                subscription_id: self.sub_id.clone(),
                event: EventEnvelope {
                    version: "agentcpm-pd.event/1".to_owned(),
                    instance_id: self.instance_id.clone(),
                    run_id: None,
                    operation_id: None,
                    subscription_id: Some(self.sub_id.clone()),
                    cursor: cursor.clone(),
                    event_type: "terminals.output".to_owned(),
                    data: data.as_object().cloned().unwrap_or_default(),
                },
            })
            .unwrap_or(Value::Null),
        };
        let text = serde_json::to_string(&notif).unwrap_or_default();
        let estimated_bytes = text.len() + 128;
        StreamFrame {
            text,
            cursor,
            estimated_bytes,
        }
    }
}
