//! Public Kernel adapter for PilotDeck tools:
//! - [`TimeToolContract`]: exact `get_current_time` implementation with ReadOnly host ports.
//! - [`CatalogToolContract`]: multi-tool [`ToolContract`] + [`agentcpm_iface::tool::ToolStaging`]
//!   adapter with JSON Schema validation and TC-01/TC-04 governance.
//! - [`governance`]: [`VerifiedToolEnvironment`], [`PreAuthArgumentFinalizer`], and
//!   [`GovernedNestedToolInvoker`].
//! - [`workspace_tools`]: builtin `read_file`, `write_file`, `edit_file`,
//!   `glob`, `grep`, and `todo_write` registration over [`CatalogToolContract`].
//!
//! The embedding party supplies trusted, read-only, short and bounded local
//! clock/zone ports for `TimeToolContract`. They must not call hooks, nested
//! tools, remote services or other side-effecting code. This condition is part
//! of the ReadOnly binding. No session identity or service handle is accepted
//! from tool arguments.
//!
//! Successful `TimeToolContract` result bytes use the UTF-8 JSON profile
//! identified by [`OUTPUT_PROFILE`], with `content` and `data` as in the
//! product ToolOutput. Absent optional ToolOutput fields are omitted. Failure
//! bytes carry `code` and `message`, but the Kernel currently wraps not-ok
//! bytes as an execution error; this adapter does not claim end-to-end product
//! error transparency.
#![forbid(unsafe_code)]

pub mod catalog_contract;
pub mod governance;
pub mod workspace_tools;

pub use catalog_contract::{
    CatalogToolContract, CatalogToolFn, CatalogToolHandler, VerifiedToolContext,
};
pub use governance::{
    FinalizedToolCall, GovernedNestedToolInvoker, NestedToolInvoker, PassthroughArgumentFinalizer,
    PreAuthArgumentFinalizer, RESERVED_HOST_FIELDS, VerifiedToolEnvironment,
    finalize_and_lock_args,
};
pub use workspace_tools::{
    WORKSPACE_TOOL_NAMES, WorkspaceTodoStore, WorkspaceToolConfig, register_workspace_tools,
    workspace_catalog_entries, workspace_tool_contract,
};

use std::sync::Arc;

use agentcpm_iface::{
    BoxFuture,
    tool::{ToolCall, ToolContract, ToolEffect, ToolResult, ToolSpec},
    types::{CallId, Error, ErrorCategory},
};
use pilotdeck_protocol::{JsObject, JsValue, parse_json, stringify_json};
use pilotdeck_transport::system_clock::ZonedDateTimeParts;

pub const TOOL_NAME: &str = "get_current_time";
pub const OUTPUT_PROFILE: &str = "pilotdeck.time-tool-output.v1";

/// Trusted read-only local clock. Calls must be short, bounded and nonblocking.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> Result<f64, String>;
}

/// A trusted timezone lookup could not resolve the requested instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZoneError;

/// Trusted read-only local timezone access. Calls must be short and bounded;
/// implementations must not wait for remote services or invoke other tools.
pub trait ZoneResolver: Send + Sync {
    fn host_timezone(&self) -> String;
    fn parts_at(&self, timezone: &str, epoch_ms: i64) -> Result<ZonedDateTimeParts, ZoneError>;
}

/// One fixed implementation and its fixed trusted host ports.
pub struct TimeToolContract {
    clock: Arc<dyn Clock>,
    zones: Arc<dyn ZoneResolver>,
    spec: ToolSpec,
}

impl TimeToolContract {
    pub fn new(clock: Arc<dyn Clock>, zones: Arc<dyn ZoneResolver>) -> Self {
        let definition = pilotdeck_tool_core::definition();
        let description = definition
            .get("description")
            .and_then(JsValue::as_string)
            .expect("time definition description is a string")
            .to_utf8()
            .expect("time definition description is UTF-8");
        let parameters = json_bytes(
            definition
                .get("inputSchema")
                .expect("time definition schema"),
        )
        .expect("time definition schema is JSON");
        Self {
            clock,
            zones,
            spec: ToolSpec {
                name: TOOL_NAME.into(),
                description,
                parameters,
            },
        }
    }

    pub fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
}

fn json_bytes(value: &JsValue) -> Result<Vec<u8>, Error> {
    stringify_json(value)
        .ok()
        .flatten()
        .map(String::into_bytes)
        .ok_or_else(|| {
            Error::new(
                ErrorCategory::Internal,
                -32000,
                "time tool JSON encoding failed",
            )
        })
}

fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsValue {
    let mut result = JsObject::new();
    for (key, value) in fields {
        result.insert(key.into(), value);
    }
    JsValue::Object(result)
}

fn failure(code: &str, message: &str) -> Result<ToolResult, Error> {
    Ok(ToolResult {
        ok: false,
        bytes: json_bytes(&object([
            ("code", code.into()),
            ("message", message.into()),
        ]))?,
        unknown: false,
    })
}

fn valid_input(bytes: &[u8]) -> Option<JsValue> {
    let text = std::str::from_utf8(bytes).ok()?;
    let input = parse_json(text).ok()?;
    let fields = input.as_object()?;
    let timezone = fields.get("timezone");
    if fields.len() != usize::from(timezone.is_some())
        || timezone.is_some_and(|value| value.as_string().is_none())
    {
        return None;
    }
    Some(input)
}

impl ToolContract for TimeToolContract {
    fn effect(&self, tool: &str) -> ToolEffect {
        if tool == TOOL_NAME {
            ToolEffect::ReadOnly
        } else {
            ToolEffect::Unknown
        }
    }

    fn register(&self, spec: ToolSpec) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if spec == self.spec {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorCategory::Execution,
                    -32000,
                    "time tool spec does not match fixed implementation",
                ))
            }
        })
    }

    fn list(&self) -> BoxFuture<'_, Vec<ToolSpec>> {
        Box::pin(async move { vec![self.spec()] })
    }

    fn invoke(&self, call: ToolCall) -> BoxFuture<'_, Result<ToolResult, Error>> {
        Box::pin(async move {
            if call.tool != TOOL_NAME {
                return Err(Error::new(
                    ErrorCategory::Execution,
                    -32033,
                    format!("unknown tool: {}", call.tool),
                ));
            }
            let Some(input) = valid_input(&call.args) else {
                return failure("invalid_input", "Invalid get_current_time input");
            };
            let timezone = pilotdeck_tool_core::timezone(&input, || self.zones.host_timezone());
            let now = match self.clock.now_ms() {
                Ok(now) => now,
                Err(message) => return failure("tool_execution_failed", &message),
            };
            let output = match pilotdeck_tool_core::execute(timezone, now, |timezone, epoch_ms| {
                self.zones.parts_at(timezone, epoch_ms).map_err(|_| ())
            }) {
                Ok(output) => output,
                Err(message) => return failure("tool_execution_failed", &message),
            };
            Ok(ToolResult {
                ok: true,
                bytes: json_bytes(&object([
                    ("profile", OUTPUT_PROFILE.into()),
                    ("content", JsValue::Array(output.content)),
                    ("data", output.data),
                ]))?,
                unknown: false,
            })
        })
    }

    /// This synchronous, bounded implementation cannot be preempted. Kernel
    /// cancellation and deadlines remain owned and classified by the Kernel.
    fn abort(&self, _call: &CallId) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            Err(Error::new(
                ErrorCategory::Execution,
                -32085,
                "time tool does not support abort",
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_iface::types::CallMeta;
    use std::{
        future::Future,
        pin::pin,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll, Waker},
    };

    fn ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("time adapter must complete synchronously"),
        }
    }

    #[derive(Default)]
    struct Ports {
        clock: AtomicUsize,
        host: AtomicUsize,
        zone: AtomicUsize,
        order: Mutex<Vec<&'static str>>,
    }
    impl Clock for Ports {
        fn now_ms(&self) -> Result<f64, String> {
            self.clock.fetch_add(1, Ordering::SeqCst);
            self.order.lock().unwrap().push("clock");
            Ok(0.0)
        }
    }
    impl ZoneResolver for Ports {
        fn host_timezone(&self) -> String {
            self.host.fetch_add(1, Ordering::SeqCst);
            self.order.lock().unwrap().push("host");
            "UTC".into()
        }
        fn parts_at(
            &self,
            _timezone: &str,
            _epoch_ms: i64,
        ) -> Result<ZonedDateTimeParts, ZoneError> {
            self.zone.fetch_add(1, Ordering::SeqCst);
            self.order.lock().unwrap().push("zone");
            Ok(ZonedDateTimeParts {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
                weekday: 4,
                offset_seconds: 0,
            })
        }
    }
    fn call(name: &str, args: &[u8]) -> ToolCall {
        ToolCall::new(
            CallMeta {
                session_id: "s".into(),
                call_id: "c".into(),
                turn: 1,
            },
            name.into(),
            None,
            args.to_vec(),
        )
    }
    fn contract() -> (TimeToolContract, Arc<Ports>) {
        let ports = Arc::new(Ports::default());
        (TimeToolContract::new(ports.clone(), ports.clone()), ports)
    }

    #[test]
    fn fixed_spec_effect_and_abort_are_honest() {
        let (tool, _) = contract();
        let spec = tool.spec();
        assert_eq!(ready(tool.list()), vec![spec.clone()]);
        assert!(ready(tool.register(spec.clone())).is_ok());
        let mut modified = spec;
        modified.description.push('!');
        assert!(ready(tool.register(modified)).is_err());
        assert_eq!(ready(tool.list()), vec![tool.spec()]);
        assert_eq!(tool.effect(TOOL_NAME), ToolEffect::ReadOnly);
        assert_eq!(tool.effect("other"), ToolEffect::Unknown);
        assert_eq!(ready(tool.abort(&"c".into())).unwrap_err().code, -32085);
    }

    #[test]
    fn validates_before_any_trusted_port() {
        let (tool, ports) = contract();
        assert_eq!(
            ready(tool.invoke(call("other", b"{}"))).unwrap_err().code,
            -32033
        );
        for invalid in [
            &b"\xff"[..],
            &b"{"[..],
            &b"[]"[..],
            &b"{\"timezone\":null}"[..],
            &b"{\"extra\":1}"[..],
        ] {
            let result = ready(tool.invoke(call(TOOL_NAME, invalid))).unwrap();
            assert!(!result.ok && !result.unknown);
        }
        assert_eq!(ports.clock.load(Ordering::SeqCst), 0);
        assert_eq!(ports.host.load(Ordering::SeqCst), 0);
        assert_eq!(ports.zone.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn success_preserves_product_projections_and_fallback_order() {
        let (tool, ports) = contract();
        let result = ready(tool.invoke(call(TOOL_NAME, b"{}"))).unwrap();
        assert!(result.ok && !result.unknown);
        let value = parse_json(std::str::from_utf8(&result.bytes).unwrap()).unwrap();
        assert_eq!(value.get("profile"), Some(&JsValue::from(OUTPUT_PROFILE)));
        let data = value.get("data").unwrap();
        assert_eq!(
            data.get("iso"),
            Some(&JsValue::from("1970-01-01T00:00:00.000Z"))
        );
        let Some(JsValue::Array(content)) = value.get("content") else {
            panic!("content array")
        };
        assert_eq!(content[0].get("value"), Some(data));
        assert!(value.get("supplementalMessages").is_none());
        assert!(value.get("metadata").is_none());
        assert_eq!(*ports.order.lock().unwrap(), ["host", "clock", "zone"]);
        assert_eq!(
            (
                ports.host.load(Ordering::SeqCst),
                ports.clock.load(Ordering::SeqCst),
                ports.zone.load(Ordering::SeqCst)
            ),
            (1, 1, 1)
        );
    }

    #[test]
    fn resolver_failure_is_a_known_product_failure() {
        struct BadZone;
        impl ZoneResolver for BadZone {
            fn host_timezone(&self) -> String {
                "UTC".into()
            }
            fn parts_at(&self, _: &str, _: i64) -> Result<ZonedDateTimeParts, ZoneError> {
                Err(ZoneError)
            }
        }
        let ports = Arc::new(Ports::default());
        let tool = TimeToolContract::new(ports.clone(), Arc::new(BadZone));
        let result = ready(tool.invoke(call(TOOL_NAME, br#"{"timezone":"bad"}"#))).unwrap();
        assert!(!result.ok && !result.unknown);
        let value = parse_json(std::str::from_utf8(&result.bytes).unwrap()).unwrap();
        assert_eq!(
            value.get("code"),
            Some(&JsValue::from("tool_execution_failed"))
        );
        assert_eq!(
            value.get("message"),
            Some(&JsValue::from("Invalid timezone: bad"))
        );
        assert_eq!(ports.host.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn clock_error_and_timeclip_are_known_failures_without_zone_lookup() {
        struct FixedClock(Result<f64, &'static str>);
        impl Clock for FixedClock {
            fn now_ms(&self) -> Result<f64, String> {
                self.0.map_err(str::to_owned)
            }
        }
        for (now, expected) in [
            (Err("clock failed"), "clock failed"),
            (Ok(f64::INFINITY), "Invalid time value"),
        ] {
            let ports = Arc::new(Ports::default());
            let tool = TimeToolContract::new(Arc::new(FixedClock(now)), ports.clone());
            let result = ready(tool.invoke(call(TOOL_NAME, br#"{"timezone":"UTC"}"#))).unwrap();
            assert!(!result.ok && !result.unknown);
            let value = parse_json(std::str::from_utf8(&result.bytes).unwrap()).unwrap();
            assert_eq!(
                value.get("code"),
                Some(&JsValue::from("tool_execution_failed"))
            );
            assert_eq!(value.get("message"), Some(&JsValue::from(expected)));
            assert_eq!(ports.host.load(Ordering::SeqCst), 0);
            assert_eq!(ports.zone.load(Ordering::SeqCst), 0);
        }
    }
}
