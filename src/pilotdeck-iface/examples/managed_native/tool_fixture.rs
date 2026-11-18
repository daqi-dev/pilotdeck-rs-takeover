//! Controlled P2 test loop. It drives the real Host's public Tool batch API;
//! it is not the product NativeCodec or a product end-to-end acceptance test.
use agentcpm_iface::{
    BoxFuture,
    r#loop::{LoopContract, LoopDescriptor, StepOutcome},
    types::*,
};
use agentcpm_scheduler_protocol::{self as p, tool_batch as batch};
use pilotdeck_protocol::{JsValue, boundary_codec, parse_json, stringify_json};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct Factory(Mutex<Value>);
struct Runtime;
fn failed(message: impl Into<String>) -> Error {
    Error::new(ErrorCategory::Protocol, -1, message.into())
}
fn wire(error: p::Failure) -> Error {
    failed(error.wire())
}
impl p::LoopFactory for Factory {
    fn create(
        &self,
        _: p::CallContext,
        _: Arc<dyn p::Client>,
    ) -> BoxFuture<'_, p::Result<Arc<dyn LoopContract>>> {
        Box::pin(async { Ok(Arc::new(Runtime) as Arc<dyn LoopContract>) })
    }
    fn supports_scoped_steps(&self) -> bool {
        true
    }
    fn step_scoped<'a>(
        &'a self,
        context: &'a p::CallContext,
        _: &'a dyn LoopContract,
        _: u64,
        _: TurnInput,
        client: Arc<dyn p::Client>,
    ) -> BoxFuture<'a, Result<StepOutcome, Error>> {
        Box::pin(async move {
            // The trusted assembly supplies only an isolated fixture file path in argv;
            // it is business data, never an owner, session or authority token.
            let path = std::env::args()
                .nth(2)
                .ok_or_else(|| failed("fixture path absent"))?;
            let path_json = serde_json::to_string(&path).map_err(|e| failed(e.to_string()))?;
            let stop_mode = std::env::args().nth(4).as_deref() == Some("stop");
            let input = if stop_mode {
                format!(
                    r#"{{"module":"capability","operation":"execute_batch","payload":{{"operation":"execute_batch","calls":[{{"toolCallId":"first","name":"read_file","arguments":{{"file_path":{path_json}}}}},{{"toolCallId":"second","name":"read_file","arguments":{{"file_path":{path_json}}}}}]}}}}"#
                )
            } else {
                format!(
                    r#"{{"module":"capability","operation":"execute","payload":{{"operation":"execute","toolCallId":"fixture-read","name":"read_file","arguments":{{"file_path":{path_json}}}}}}}"#
                )
            };
            let envelope = parse_json(&input).map_err(|e| failed(format!("{e:?}")))?;
            let request = batch::PrepareRequest {
                idempotency_key: "fixture-read".into(),
                envelope: p::context_history::OpaquePayload {
                    profile: boundary_codec::PROFILE.into(),
                    bytes: boundary_codec::encode(&envelope)
                        .map_err(|e| failed(format!("{e:?}")))?,
                },
            };
            if stop_mode {
                let gate = std::env::args()
                    .nth(3)
                    .ok_or_else(|| failed("stop exchange path absent"))?;
                let receipt = batch::prepare(client.as_ref(), &request)
                    .await
                    .map_err(wire)?;
                let query = batch::Query {
                    batch_ref: receipt.batch_ref,
                };
                let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
                while !std::path::Path::new(&format!("{gate}.go")).exists() {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(failed("parent did not signal stop"));
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let closed = batch::close(client.as_ref(), &query).await.map_err(wire)?;
                if !closed.closed || closed.resources != batch::Resources::Held {
                    return Err(failed("original in-flight batch was not held behind close"));
                }
                std::fs::write(format!("{gate}.closed"), b"closed-held")
                    .map_err(|e| failed(e.to_string()))?;
                loop {
                    let status = batch::status(client.as_ref(), &query).await.map_err(wire)?;
                    if status.resources == batch::Resources::Released {
                        if !status.closed {
                            return Err(failed("released batch lost its close fence"));
                        }
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(failed("stopped batch did not release"));
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            } else {
                // The first session writes only the opaque batch reference into the
                // isolated fixture directory. A second real product session must
                // reject that actual reference under its own scoped source.
                let exchange = std::env::args()
                    .nth(3)
                    .ok_or_else(|| failed("exchange path absent"))?;
                let original_input = context
                    .invocation
                    .payload
                    .as_str()
                    .ok_or_else(|| failed("original request missing"))?;
                let second = original_input.contains("foreign source");
                if second {
                    let reference = std::fs::read_to_string(&exchange).map_err(|e| {
                        failed(format!("first real batch reference unavailable: {e}"))
                    })?;
                    if reference.is_empty() {
                        return Err(failed("empty foreign reference"));
                    }
                    if batch::status(
                        client.as_ref(),
                        &batch::Query {
                            batch_ref: reference,
                        },
                    )
                    .await
                    .is_ok()
                    {
                        return Err(failed("another session's real batch was accepted"));
                    }
                    // Parent requires this marker after the second turn; a read
                    // error can never silently skip the foreign-source assertion.
                    std::fs::write(format!("{exchange}.foreign-rejected"), b"rejected")
                        .map_err(|e| failed(e.to_string()))?;
                } else if std::path::Path::new(&exchange).exists() {
                    return Err(failed("unexpected existing first-batch reference"));
                }
                let mut receipt = batch::prepare(client.as_ref(), &request)
                    .await
                    .map_err(wire)?;
                if !second {
                    std::fs::write(&exchange, &receipt.batch_ref)
                        .map_err(|e| failed(e.to_string()))?;
                }
                let query = batch::Query {
                    batch_ref: receipt.batch_ref.clone(),
                };
                // An arbitrary reference does not confer access to any batch.
                if batch::status(
                    client.as_ref(),
                    &batch::Query {
                        batch_ref: "foreign-fixture-reference".into(),
                    },
                )
                .await
                .is_ok()
                {
                    return Err(failed("foreign batch reference was accepted"));
                }
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                while receipt.result.is_none() {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(failed("batch result timeout"));
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    receipt = batch::status(client.as_ref(), &query).await.map_err(wire)?;
                }
                let payload = match receipt.result.as_ref().unwrap() {
                    batch::BusinessResult::Succeeded { payload } => payload,
                    batch::BusinessResult::Failed { .. } => {
                        return Err(failed("original read_file failed"));
                    }
                };
                if payload.profile != boundary_codec::PROFILE {
                    return Err(failed("wrong result profile"));
                }
                let result: JsValue =
                    boundary_codec::decode(&payload.bytes).map_err(|e| failed(format!("{e:?}")))?;
                let text = stringify_json(&result)
                    .map_err(|e| failed(format!("{e:?}")))?
                    .ok_or_else(|| failed("missing result"))?;
                if !text.contains("PILOTDECK_REAL_TOOL_FIXTURE_MARKER") {
                    return Err(failed("original read_file marker absent"));
                }
                let closed = batch::close(client.as_ref(), &query).await.map_err(wire)?;
                if !closed.closed {
                    return Err(failed("close fence absent"));
                }
                let replay = batch::prepare(client.as_ref(), &request)
                    .await
                    .map_err(wire)?;
                if replay.batch_ref != query.batch_ref || !replay.closed {
                    return Err(failed("closed batch replay created a new batch"));
                }
                let mut settled = batch::status(client.as_ref(), &query).await.map_err(wire)?;
                while settled.resources == batch::Resources::Held {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(failed("batch release timeout"));
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    settled = batch::status(client.as_ref(), &query).await.map_err(wire)?;
                }
                if settled.resources != batch::Resources::Released || !settled.closed {
                    return Err(failed("batch release not confirmed"));
                }
            }
            let raw = context
                .invocation
                .payload
                .as_str()
                .ok_or_else(|| failed("original request missing"))?;
            let original: Value = serde_json::from_str(raw).map_err(|e| failed(e.to_string()))?;
            let session = original["sessionId"]
                .as_str()
                .ok_or_else(|| failed("product session missing"))?;
            let turn = original["turnId"]
                .as_str()
                .ok_or_else(|| failed("product turn missing"))?;
            let result = json!({
                "type": "success",
                "sessionId": session,
                "turnId": turn,
                "stopReason": "stop",
                "turns": 1
            });
            pilotdeck_iface::product::publish_event(
                client.as_ref(),
                context,
                "controller",
                &format!("{}:started", context.call_ref),
                json!({"type":"model_request_started","sessionId":session,"turnId":turn})
                    .to_string(),
            )
            .await
            .map_err(wire)?;
            pilotdeck_iface::product::publish_event(
                client.as_ref(),
                context,
                "controller",
                &format!("{}:completed", context.call_ref),
                json!({"type":"turn_completed","sessionId":session,"turnId":turn,"result":result})
                    .to_string(),
            )
            .await
            .map_err(wire)?;
            // This terminal is a controlled test-loop projection, not proof of
            // the product NativeCodec. Resource assertions already ran above.
            *self.0.lock().unwrap() = json!(json!({"messages":[],"result":result}).to_string());
            Ok(StepOutcome::Finished {
                outcome: TurnOutcome {
                    completed: true,
                    stop_reason: "done".into(),
                    final_seq: 1,
                },
            })
        })
    }
    fn result(
        &self,
        _: &p::CallContext,
        _: &dyn LoopContract,
        _: &StepOutcome,
    ) -> p::Result<Value> {
        Ok(self.0.lock().unwrap().clone())
    }
}
impl LoopContract for Runtime {
    fn describe(&self) -> BoxFuture<'_, LoopDescriptor> {
        Box::pin(async {
            LoopDescriptor {
                id: "p2-tool-fixture".into(),
                language: "rust".into(),
                step_semantics: StepSemantics::TurnToCompletion,
                capabilities: LoopCapabilities::NONE,
            }
        })
    }
    fn step(
        &self,
        _: &SessionId,
        _: u64,
        _: TurnInput,
    ) -> BoxFuture<'_, Result<StepOutcome, Error>> {
        Box::pin(async { Err(failed("scoped step required")) })
    }
    fn status(&self, _: &SessionId) -> BoxFuture<'_, Result<LoopStatus, Error>> {
        Box::pin(async { Err(failed("unused status")) })
    }
    fn interrupt(
        &self,
        _: &SessionId,
        _: InterruptKind,
        _: &str,
    ) -> BoxFuture<'_, Result<InterruptOutcome, Error>> {
        Box::pin(async { Ok(InterruptOutcome::Unsupported) })
    }
}
pub fn serve() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            p::provider::serve(p::provider::Export::Loop(Arc::new(Factory(Mutex::new(
                Value::Null,
            )))))
            .await
        })
        .expect("controlled tool fixture provider");
}
