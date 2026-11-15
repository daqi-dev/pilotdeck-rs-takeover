//! Concrete model-error recovery binding using the extracted original policy.
//! NOT INSTALLED / NOT TESTED: requires Kernel protected Context dispatch.
use crate::context_capability::{
    AdmissionAttempt, AdmittedContext, ContextBinding, ContextSettlement,
};
use agentcpm_iface::BoxFuture;
use agentcpm_scheduler_protocol::{
    CallContext, Client, Failure, Result, State, context_history as history,
    context_operations as ops,
};
use pilotdeck_context_runtime::{
    boundary_codec,
    compaction::{
        ports::CompactionRecoveryPort,
        recovery::{self, ContextOverflowRecovery},
    },
    host::{HostError, ModuleError},
    operations::{ContextFuture, ContextInput, ContextRuntime, Operation},
};
use pilotdeck_protocol::{JsObject, JsValue};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Selected by the trusted product composition from its actual Context config.
/// No default policy is installed by this binding.
#[derive(Clone)]
pub enum RecoveryPolicy {
    Configured(ContextOverflowRecovery),
    LegacyFallback { first_keep_ratio: f64 },
}
pub struct RecoveryBinding {
    policy: RecoveryPolicy,
}
impl RecoveryBinding {
    pub fn new(policy: RecoveryPolicy) -> Self {
        Self { policy }
    }
}
/// Construct the real recovery capability for a trusted, fixed host binding.
/// The host must register this exact binding in Kernel protected dispatch; this
/// function by itself does not install or advertise it.
pub fn recovery_capability(
    binding_ref: String,
    policy: RecoveryPolicy,
) -> Result<crate::context_capability::NativeContextCapability> {
    crate::context_capability::NativeContextCapability::new(
        Operation::RecoverFromModelError,
        binding_ref,
        Arc::new(RecoveryBinding::new(policy)),
    )
}

/// Serve the real recovery policy over the public Native provider protocol.
/// The trusted Kernel composition must separately install the same binding
/// as the fixed recovery role; advertised host features do not grant it.
pub async fn serve_recovery(binding_ref: String, policy: RecoveryPolicy) -> Result<()> {
    let capability = recovery_capability(binding_ref, policy)?;
    agentcpm_scheduler_protocol::provider::serve_with_required_host_features(
        agentcpm_scheduler_protocol::provider::Export::Capability(Arc::new(capability)),
        &[history::FEATURE, ops::FEATURE],
    )
    .await
}
struct Attempt {
    context: CallContext,
    request: ops::InvocationRequest,
    client: Arc<dyn Client>,
    fields: JsObject,
    policy: RecoveryPolicy,
    retired: AtomicBool,
}
impl ContextBinding for RecoveryBinding {
    fn supports(&self, operation: Operation) -> bool {
        operation == Operation::RecoverFromModelError
    }
    fn begin(
        &self,
        context: &CallContext,
        client: Arc<dyn Client>,
        operation: Operation,
        request: &ops::InvocationRequest,
        decoded_input: JsValue,
    ) -> Result<Arc<dyn AdmissionAttempt>> {
        if operation != Operation::RecoverFromModelError
            || request.operation != ops::Operation::RecoverFromModelError
        {
            return Err(Failure::external(
                "CONTEXT_UNSUPPORTED",
                "not a recovery binding",
            ));
        }
        let fields = decoded_input.as_object().cloned().ok_or_else(|| {
            Failure::external("CONTEXT_PROTOCOL", "recovery input is not an object")
        })?;
        Ok(Arc::new(Attempt {
            context: context.clone(),
            request: request.clone(),
            client,
            fields,
            policy: self.policy.clone(),
            retired: AtomicBool::new(false),
        }))
    }
}
impl AdmissionAttempt for Attempt {
    fn admit(&self) -> BoxFuture<'_, Result<AdmittedContext>> {
        Box::pin(async move {
            if self.retired.load(Ordering::Acquire) {
                return Err(Failure::external(
                    "AUTHORITY_REVOKED",
                    "recovery admission retired",
                ));
            }
            // Kernel supplies CallContext; a payload alone is never authority.
            // Protected Context dispatch must itself bind the reservation before
            // this provider is installed. These are defense-in-depth checks.
            let current = self.client.get(&self.context.call_ref).await?;
            let owner = &self.request.reservation.owner;
            let parent = self.client.get(&owner.call).await?;
            if self.retired.load(Ordering::Acquire)
                || current.id != self.context.call_ref
                || current.session_ref != self.context.session_ref
                || current.binding != self.context.binding
                || current.invocation != self.context.invocation
                || current.interaction != self.context.interaction
                || current.origin != self.context.origin
                || current.security != self.context.security
                || current.parent_session_ref.as_deref() != Some(self.context.session_ref.as_str())
                || owner.session != self.context.session_ref
                || current.state != State::Running
                || current.cancel_requested
                || current.source_binding.as_deref() != Some(owner.binding.as_str())
                || current.cause_execution_ref.as_deref() != Some(owner.call.as_str())
                || parent.id != owner.call
                || parent.session_ref != owner.session
                || parent.binding.id != owner.binding
                || parent.invocation.context_ref != owner.context
                || parent.interaction != owner.origin
                || parent.security != current.security
                || parent.state != State::Running
                || parent.cancel_requested
            {
                return Err(Failure::external(
                    "AUTHORITY_REVOKED",
                    "recovery source is not live or does not match reservation",
                ));
            }
            Ok(AdmittedContext {
                runtime: Arc::new(RecoveryRuntime(self.policy.clone())),
                input: ContextInput {
                    fields: self.fields.clone(),
                    abort_signal: None,
                    budget_evaluator: None,
                },
                settlement: Arc::new(RecoverySettlement {
                    retired: AtomicBool::new(false),
                }),
            })
        })
    }
    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }
}
struct RecoveryRuntime(RecoveryPolicy);
impl ContextRuntime for RecoveryRuntime {
    fn supports(&self, op: Operation) -> bool {
        op == Operation::RecoverFromModelError
    }
    fn prepare_for_model(&self, _: ContextInput) -> ContextFuture<JsValue> {
        Box::pin(async {
            Err(
                ModuleError::new("Host context capability does not support prepare_for_model.")
                    .into(),
            )
        })
    }
    fn recover_from_model_error(&self, input: ContextInput) -> ContextFuture<JsValue> {
        let policy = self.0.clone();
        Box::pin(async move {
            let input = JsValue::Object(input.fields);
            match policy {
                RecoveryPolicy::Configured(value) => value.decide(input),
                RecoveryPolicy::LegacyFallback { first_keep_ratio } => {
                    recovery::fallback(&input, first_keep_ratio)
                }
            }
        })
    }
}
struct RecoverySettlement {
    retired: AtomicBool,
}
impl ContextSettlement for RecoverySettlement {
    fn finish<'a>(
        &'a self,
        result: std::result::Result<JsValue, HostError>,
    ) -> BoxFuture<'a, Result<ops::OperationResult>> {
        Box::pin(async move {
            if self.retired.load(Ordering::Acquire) {
                return Err(Failure::external(
                    "AUTHORITY_REVOKED",
                    "recovery operation retired",
                ));
            }
            let value = result.map_err(|error| {
                Failure::external("CONTEXT_PRODUCT_ERROR", "original context recovery failed")
                    .with("message_utf16", json!(error.module_error.0.units()))
            })?;
            let bytes = boundary_codec::encode(&value).map_err(|_| {
                Failure::external(
                    "CONTEXT_PROTOCOL",
                    "recovery output exceeds profile or is unsupported",
                )
            })?;
            Ok(ops::OperationResult {
                operation: ops::Operation::RecoverFromModelError,
                output: history::OpaquePayload {
                    profile: boundary_codec::PROFILE.into(),
                    bytes,
                },
                history: ops::HistoryMutation {
                    replacement: None,
                    consume_input: false,
                    facts: vec![],
                    compaction_boundary: None,
                },
                preparation_notice: None,
            })
        })
    }
    fn validate_history(&self, history: &ops::HistoryMutation) -> Result<()> {
        if history.replacement.is_some()
            || history.consume_input
            || !history.facts.is_empty()
            || history.compaction_boundary.is_some()
        {
            return Err(Failure::external(
                "CONTEXT_PROTOCOL",
                "pure recovery cannot propose a history mutation",
            ));
        }
        Ok(())
    }
    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_recovery_runtime_replays_original_oracle() {
        let oracle = pilotdeck_protocol::parse_json(include_str!(
            "../../pilotdeck-gateway/tests/fixtures/compaction-orchestrator/oracle.json"
        ))
        .unwrap();
        let Some(JsValue::Array(cases)) = oracle.get("recoveryCases") else {
            panic!("missing recoveryCases")
        };
        assert_eq!(cases.len(), 41);
        for case in cases {
            let options = case.get("options").unwrap();
            let policy = ContextOverflowRecovery {
                truncate_first_keep_ratio: options
                    .get("truncateFirstKeepRatio")
                    .and_then(JsValue::as_number)
                    .unwrap_or(0.5),
                truncate_second_keep_ratio: options
                    .get("truncateSecondKeepRatio")
                    .and_then(JsValue::as_number)
                    .unwrap_or(0.25),
            };
            let runtime = RecoveryRuntime(RecoveryPolicy::Configured(policy));
            let fields = case.get("input").unwrap().as_object().unwrap().clone();
            let result = runtime
                .recover_from_model_error(ContextInput {
                    fields,
                    abort_signal: None,
                    budget_evaluator: None,
                })
                .await
                .unwrap();
            assert_eq!(
                pilotdeck_protocol::stringify_json(&result),
                pilotdeck_protocol::stringify_json(case.get("expected").unwrap())
            );
        }
    }
}
