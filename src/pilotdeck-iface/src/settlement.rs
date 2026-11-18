//! Original accepted notification suffix retained after execution authority ends.
//!
//! This is data only. It never publishes, calls a model/tool, or grants a
//! cancellation exception to the running Controller. The Native worker seals
//! it after its actual completion and the Kernel's settlement receipt owns any
//! later ordinary publication.
use crate::port_helpers::invalid;
use pilotdeck_agent::{
    ports::{CallControl, CancelCause, PortError, PortResult},
    turn::TurnEventPort,
};
use pilotdeck_protocol::{model::BoundarySnapshot, stringify_json};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub const PROFILE: &str = "pilotdeck.settlement/1";

/// Original product data carried by an independently authenticated Kernel
/// settlement. Decoding this value proves neither authority nor an outcome.
pub struct ProductSettlement {
    pub events_json: Vec<String>,
    pub result_json: Option<String>,
}

pub fn decode(value: &Value) -> Result<ProductSettlement, &'static str> {
    let object = value
        .as_object()
        .ok_or("product settlement must be an object")?;
    if object.get("profile").and_then(Value::as_str) != Some(PROFILE)
        || object
            .keys()
            .any(|key| !["profile", "events_json", "result_json"].contains(&key.as_str()))
    {
        return Err("unknown product settlement profile or field");
    }
    let events_json = object
        .get("events_json")
        .and_then(Value::as_array)
        .ok_or("product settlement lacks its ordered events")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or("product settlement event must retain JSON text")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let result_json = object
        .get("result_json")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or("product settlement result must retain JSON text")
        })
        .transpose()?;
    Ok(ProductSettlement {
        events_json,
        result_json,
    })
}

#[derive(Default)]
struct Tail {
    events: Vec<String>,
    bytes: usize,
    sealed: bool,
}

/// Read-only after `seal`; not a proof of outcome or resource release.
#[derive(Clone, Default)]
pub(crate) struct SettlementTail(Arc<Mutex<Tail>>);
impl SettlementTail {
    pub(crate) fn seal(&self) -> Option<Value> {
        let mut tail = self.0.lock().unwrap_or_else(|p| p.into_inner());
        tail.sealed = true;
        (!tail.events.is_empty()).then(|| json!({"profile":PROFILE,"events_json":tail.events}))
    }
}

pub(crate) struct SettlementEvents {
    inner: Box<dyn TurnEventPort + Send>,
    control: CallControl,
    tail: SettlementTail,
}
impl SettlementEvents {
    pub(crate) fn wrap(
        inner: Box<dyn TurnEventPort + Send>,
        control: CallControl,
    ) -> (Box<dyn TurnEventPort + Send>, SettlementTail) {
        let tail = SettlementTail::default();
        (
            Box::new(Self {
                inner,
                control,
                tail: tail.clone(),
            }),
            tail,
        )
    }
    fn retain(&self, event: BoundarySnapshot) -> PortResult<()> {
        let encoded = stringify_json(event.value())
            .ok()
            .flatten()
            .ok_or_else(|| invalid("accepted settlement event is not JSON data"))?;
        let mut tail = self.tail.0.lock().unwrap_or_else(|p| p.into_inner());
        if tail.sealed {
            return Err(invalid(
                "settlement event arrived after completion was sealed",
            ));
        }
        // Count the actual JSON-string escaping in the outer snapshot, not
        // only the original event text. The whole response (result/envelope
        // included) still passes the SDK's independent frame check.
        let wire_bytes = serde_json::to_vec(&encoded)
            .map_err(|_| invalid("settlement event could not be encoded"))?
            .len();
        let bytes = tail
            .bytes
            .checked_add(wire_bytes)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| invalid("settlement events exceed the public Native frame limit"))?;
        let header_bytes = serde_json::to_vec(&json!({"profile":PROFILE,"events_json":[]}))
            .map_err(|_| invalid("settlement profile could not be encoded"))?
            .len();
        if bytes
            .checked_add(header_bytes)
            .is_none_or(|total| total > agentcpm_scheduler_protocol::provider::MAX_FRAME)
        {
            return Err(invalid(
                "settlement events exceed the public Native frame limit",
            ));
        }
        tail.bytes = bytes;
        tail.events.push(encoded);
        Ok(())
    }
}
impl TurnEventPort for SettlementEvents {
    fn drain_owned_activity(&mut self) {
        self.inner.drain_owned_activity();
    }
    fn emit(&mut self, event: BoundarySnapshot) -> PortResult<()> {
        // Ordinary production continues to require execution authority.
        self.control.check()?;
        self.inner.emit(event)
    }
    fn emit_accepted(&mut self, event: BoundarySnapshot) -> PortResult<()> {
        match self.control.check() {
            Ok(()) => {
                // Deliberately no retry or catch-to-retain here. A raced
                // deadline/failed demand call may already have published;
                // replaying it through settlement would duplicate delivery.
                self.inner.emit_accepted(event)
            }
            Err(PortError::Cancelled {
                cause: CancelCause::Requested(_) | CancelCause::Deadline,
            }) => self.retain(event),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_agent::ports::Cancellation;
    use pilotdeck_protocol::{JsValue, parse_json};
    use std::time::Instant;

    struct Sink {
        seen: Arc<Mutex<Vec<String>>>,
        fail: bool,
    }
    impl TurnEventPort for Sink {
        fn emit(&mut self, event: BoundarySnapshot) -> PortResult<()> {
            self.seen
                .lock()
                .unwrap()
                .push(stringify_json(event.value()).unwrap().unwrap());
            if self.fail {
                Err(invalid("original demand failed"))
            } else {
                Ok(())
            }
        }
    }
    fn control() -> CallControl {
        CallControl {
            cancellation: Cancellation::default(),
            deadline: None,
        }
    }
    fn boundary(raw: &str) -> BoundarySnapshot {
        BoundarySnapshot::capture(&parse_json(raw).unwrap())
    }
    fn fixture(
        control: CallControl,
        fail: bool,
    ) -> (
        Box<dyn TurnEventPort + Send>,
        SettlementTail,
        Arc<Mutex<Vec<String>>>,
    ) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (port, tail) = SettlementEvents::wrap(
            Box::new(Sink {
                seen: seen.clone(),
                fail,
            }),
            control,
        );
        (port, tail, seen)
    }
    #[test]
    fn aborted_payload_cannot_grant_settlement_authority_while_running() {
        let (mut port, tail, seen) = fixture(control(), false);
        port.emit_accepted(boundary(
            r#"{"type":"turn_completed","result":{"type":"aborted"}}"#,
        ))
        .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(tail.seal(), None);
    }
    #[test]
    fn cancel_and_deadline_retain_exact_original_suffix_after_delivered_prefix() {
        for deadline in [false, true] {
            let cancellation = Cancellation::default();
            let control = CallControl {
                cancellation: cancellation.clone(),
                deadline: None,
            };
            let (mut port, tail, seen) = fixture(control, false);
            let prefix = r#"{"type":"model_event","text":"prefix"}"#;
            port.emit_accepted(boundary(prefix)).unwrap();
            cancellation.cancel(if deadline {
                CancelCause::Deadline
            } else {
                CancelCause::Requested("parity_cancel".into())
            });
            let partial = r#"{"type":"assistant_message","message":{"content":[{"type":"text","text":"\ud800 tail"}]}}"#;
            let completed = r#"{"type":"turn_completed","result":{"type":"aborted","stopReason":"aborted_streaming"}}"#;
            for raw in [partial, completed] {
                port.emit_accepted(boundary(raw)).unwrap();
            }
            assert_eq!(seen.lock().unwrap().as_slice(), &[prefix]);
            let retained = tail.seal().unwrap();
            assert_eq!(retained["profile"], PROFILE);
            let values = retained["events_json"].as_array().unwrap();
            assert_eq!(values.len(), 2);
            assert_eq!(values[0], partial);
            assert_eq!(values[1], completed);
            // Negative sequence control: dropped/duplicated/reordered suffixes
            // fail the same exact full-value comparison, including UTF-16.
            let mut reversed = values.clone();
            reversed.reverse();
            assert_ne!(reversed, *values);
            assert_ne!(
                values[0].as_str().unwrap().as_bytes(),
                values[1].as_str().unwrap().as_bytes()
            );
            assert!(port.emit(boundary(r#"{"type":"new_effect"}"#)).is_err());
            assert!(port.emit_accepted(boundary(completed)).is_err());
        }
    }
    #[test]
    fn an_expired_clock_retains_data_without_fabricating_a_terminal() {
        let (mut port, tail, seen) = fixture(
            CallControl {
                cancellation: Cancellation::default(),
                deadline: Some(Instant::now()),
            },
            false,
        );
        port.emit_accepted(boundary(
            r#"{"type":"assistant_message","message":{"content":[]}}"#,
        ))
        .unwrap();
        assert!(seen.lock().unwrap().is_empty());
        let data = tail.seal().unwrap();
        assert!(data.get("state").is_none());
        assert!(data.get("outcome").is_none());
        assert_eq!(
            parse_json(data["events_json"][0].as_str().unwrap())
                .unwrap()
                .get("type"),
            Some(&JsValue::from("assistant_message"))
        );
    }
    #[test]
    fn failed_demand_does_not_retry_or_move_the_event_to_settlement() {
        let (mut port, tail, seen) = fixture(control(), true);
        let error = port
            .emit_accepted(boundary(r#"{"type":"turn_completed"}"#))
            .unwrap_err();
        assert!(
            matches!(error, PortError::InvalidResponse{message} if message.equals_str("original demand failed"))
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(tail.seal(), None);
    }
    #[test]
    fn hard_retirement_cannot_reuse_the_accepted_notification_exception() {
        for cause in [CancelCause::OwnerRetired, CancelCause::TransportClosed] {
            let control = control();
            control.cancellation.cancel(cause);
            let (mut port, tail, seen) = fixture(control, false);
            assert!(
                port.emit_accepted(boundary(
                    r#"{"type":"turn_completed","result":{"type":"aborted"}}"#
                ))
                .is_err()
            );
            assert!(seen.lock().unwrap().is_empty());
            assert_eq!(tail.seal(), None);
        }
    }
    #[test]
    fn outer_json_escaping_cannot_silently_overflow_the_native_frame_budget() {
        let limit = agentcpm_scheduler_protocol::provider::MAX_FRAME;
        let raw =
            serde_json::to_string(&json!({"type":"assistant_message","text":"\"".repeat(limit/3)}))
                .unwrap();
        assert!(raw.len() < limit);
        assert!(
            serde_json::to_vec(&raw).unwrap().len() > limit,
            "negative control must cross the actual outer wire boundary"
        );
        let control = control();
        control.cancellation.cancel(CancelCause::Deadline);
        let (mut port, tail, seen) = fixture(control, false);
        assert!(
            matches!(port.emit_accepted(boundary(&raw)),Err(PortError::InvalidResponse{message})
            if message.equals_str("settlement events exceed the public Native frame limit"))
        );
        assert_eq!(tail.seal(), None);
        assert!(seen.lock().unwrap().is_empty());
    }
}
