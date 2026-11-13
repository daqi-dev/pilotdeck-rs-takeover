//! Outbound `pilotdeck.context/v1` runtime. The selected runtime is wrapped by
//! the host's existing durable context handler, rather than recording here.
//!
//! Fixed baseline: `composition/runtimePorts.ts:115-123` and
//! `agent/modules/context/hostContextRuntime.ts:40-124` at cc7c4e6.
use std::sync::Arc;

use pilotdeck_protocol::{JsObject, JsString, JsValue, compaction::project_compaction_budget};

use crate::{
    host::{
        HostError, HostFuture, ModuleError,
        modules::context::{ContextInput, ContextRuntime, Operation},
    },
    runtime::network::Cancellation,
};

use super::http::{HttpModuleClient, HttpModuleError, ModuleCallInput};

/// The client belongs to the session's configuration generation. Cancellation
/// belongs to this turn; no mutable "current turn" is retained in the client.
/// TS intentionally uses the session id for the HTTP run id, independently of
/// the actual turn id carried in the input.
pub(crate) fn context_runtime(
    client: Arc<HttpModuleClient>,
    session_id: JsString,
    abort: Option<Cancellation>,
) -> Result<Arc<dyn ContextRuntime>, HostError> {
    Ok(Arc::new(ExternalContextRuntime::new(
        client, session_id, abort,
    )?))
}

struct ExternalContextRuntime {
    client: Arc<HttpModuleClient>,
    run_id: JsString,
    operation_id: JsString,
    abort: Option<Cancellation>,
}

impl ExternalContextRuntime {
    fn new(
        client: Arc<HttpModuleClient>,
        session_id: JsString,
        abort: Option<Cancellation>,
    ) -> Result<Self, HostError> {
        if !client.binding().supports("prepare_for_model") {
            return Err(
                ModuleError::new("Host context module must support prepare_for_model.").into(),
            );
        }
        let mut operation_id: Vec<u16> = "context-".encode_utf16().collect();
        operation_id.extend_from_slice(session_id.units());
        Ok(Self {
            client,
            run_id: session_id,
            operation_id: JsString::from_utf16(operation_id),
            abort,
        })
    }

    fn invoke(&self, operation: Operation, input: ContextInput) -> HostFuture<JsValue> {
        if !self.supports(operation) {
            return Box::pin(async move {
                Err(ModuleError::new(format!(
                    "Host context capability does not support {}.",
                    operation.name()
                ))
                .into())
            });
        }
        let client = self.client.clone();
        let abort = input
            .abort_signal
            .as_ref()
            .and_then(|signal| signal.downcast_ref::<Cancellation>())
            .cloned()
            .or_else(|| self.abort.clone());
        let call = ModuleCallInput {
            run_id: self.run_id.clone(),
            operation_id: self.operation_id.clone(),
            request_id: format!(
                "context-{}-{}",
                operation.name(),
                uuid::Uuid::new_v4().simple()
            )
            .into(),
            module: "context".into(),
            payload: JsValue::Object(object([
                ("operation", operation.name().into()),
                (
                    "input",
                    JsValue::Object(serialize_input(operation, input.fields)),
                ),
            ])),
            idempotency_key: None,
            abort,
        };
        Box::pin(async move {
            let response = client.call(call).await.map_err(context_error)?;
            if response.get("ok") == Some(&JsValue::Bool(false)) {
                return Err(context_error(HttpModuleError::from_response(&response)));
            }
            // A result may be null, false, zero, or undefined. Existence, rather
            // than truthiness or an object schema, is the consumer contract.
            let result = response
                .get("payload")
                .and_then(|payload| payload.get("result"))
                .ok_or_else(|| {
                    ModuleError::new(format!(
                        "Context module {} returned no result.",
                        operation.name()
                    ))
                })?;
            if operation == Operation::CaptureTurn {
                Ok(JsValue::Undefined)
            } else {
                Ok(result.clone())
            }
        })
    }
}

/// `hostContextRuntime` exposes message and code only. In particular a module's
/// structured error is not a canonical host error, and HTTP result_unknown is
/// not an instruction to replay or a whole-turn unknown-result transition.
fn context_error(error: HttpModuleError) -> HostError {
    let mut failure = ModuleError::new(error.message);
    failure.code = Some(error.code);
    failure.into()
}

impl ContextRuntime for ExternalContextRuntime {
    fn supports(&self, operation: Operation) -> bool {
        self.client.binding().supports(operation.name())
    }
    fn prepare_for_model(&self, input: ContextInput) -> HostFuture<JsValue> {
        self.invoke(Operation::PrepareForModel, input)
    }
    fn apply_tool_results(&self, input: ContextInput) -> HostFuture<JsValue> {
        self.invoke(Operation::ApplyToolResults, input)
    }
    fn recover_from_model_error(&self, input: ContextInput) -> HostFuture<JsValue> {
        self.invoke(Operation::RecoverFromModelError, input)
    }
    fn capture_turn(&self, input: ContextInput) -> HostFuture<JsValue> {
        self.invoke(Operation::CaptureTurn, input)
    }
    fn try_auto_compact(&self, input: ContextInput) -> HostFuture<JsValue> {
        self.invoke(Operation::TryAutoCompact, input)
    }
}

fn serialize_input(operation: Operation, source: JsObject) -> JsObject {
    let mut serializable = without(&source, &["abortSignal", "budgetEvaluator", "budgetStage"]);
    if operation == Operation::TryAutoCompact {
        let projection = project_compaction_budget(&JsValue::Object(source));
        if let JsValue::Object(projection) = projection {
            for (from, to) in [
                ("request", "budgetRequest"),
                ("preparation", "budgetPreparation"),
                ("calibration", "budgetCalibration"),
            ] {
                if let Some(value) = projection.get(from) {
                    serializable.insert(to.into(), value.clone());
                }
            }
            serializable.insert(
                "budgetProjection".into(),
                JsValue::Object(without(
                    &projection,
                    &["request", "preparation", "calibration"],
                )),
            );
        }
    }
    serializable
}

fn without(source: &JsObject, excluded: &[&str]) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in source.entries() {
        if !excluded.iter().any(|name| key.equals_str(name)) {
            result.insert(key.clone(), value.clone());
        }
    }
    result
}

fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in fields {
        result.insert(key.into(), value);
    }
    result
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
