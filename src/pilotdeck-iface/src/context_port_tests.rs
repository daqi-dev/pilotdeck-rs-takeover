//! Causal tests through the actual synchronous KernelHostPort and ContextCodec.
use super::*;
use crate::resources::KernelHostPort;
use agentcpm_iface::BoxFuture;
use agentcpm_scheduler_protocol::{
    self as p, CallView, Client, ControlKind, ControlOutcome, Destination, Event, Failure,
    Invocation, Result, State, StateChange, VersionedState, Wait,
};
use pilotdeck_agent::ports::{
    CallControl, CancelCause, Cancellation, HostCall, HostPort, HostReply, PortError,
    RecoverModelErrorBoundary,
};
use pilotdeck_protocol::{JsValue, model::BoundarySnapshot};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
enum Scenario {
    CancelAtStatus,
    CancelWhileStatusPending,
    LostCommitAck,
    FailureReleaseWait,
    FailureLostAck,
    PrepareCallerEndedClean,
    PrepareStepAuthorityRevokedClean,
    PrepareWrappedCallerEndedClean,
    PrepareCallerEndedPending,
}
struct StateData {
    methods: Vec<String>,
    prepared: Option<history::PrepareRequest>,
    reservation: Option<history::Reservation>,
    child: Option<p::CallView>,
    committed: Option<ops::Committed>,
    failed: Option<ops::failure::Receipt>,
    settle_payloads: Vec<Value>,
}
struct ScriptClient {
    context: p::CallContext,
    cancellation: Cancellation,
    scenario: Scenario,
    data: Mutex<StateData>,
}
fn unavailable<T>() -> Result<T> {
    Err(Failure::external("TEST_UNAVAILABLE", "unused"))
}
impl Client for ScriptClient {
    fn resource(&self, method: &str, payload: Value) -> BoxFuture<'_, Result<Value>> {
        let method = method.to_owned();
        Box::pin(async move {
            if method == ops::STATUS && matches!(self.scenario, Scenario::CancelWhileStatusPending)
            {
                self.data.lock().unwrap().methods.push(method.clone());
                self.cancellation
                    .cancel(CancelCause::Requested("stop".into()));
                return std::future::pending().await;
            }
            let mut data = self.data.lock().unwrap();
            data.methods.push(method.clone());
            let value = match method.as_str() {
                history::READ => {
                    let pending = if matches!(self.scenario, Scenario::PrepareCallerEndedPending)
                        && data.prepared.is_some()
                    {
                        data.reservation.clone()
                    } else {
                        None
                    };
                    serde_json::to_value(history::HistoryView {
                        revision: 0,
                        history: None,
                        last_receipt: None,
                        pending,
                    })
                    .unwrap()
                }
                history::PREPARE => {
                    let request: history::PrepareRequest = serde_json::from_value(payload).unwrap();
                    let owner = history::Owner {
                        session: self.context.session_ref.clone(),
                        binding: self.context.binding.id.clone(),
                        context: self.context.invocation.context_ref.clone(),
                        call: self.context.call_ref.clone(),
                        origin: self.context.interaction.clone(),
                    };
                    let reservation = history::Reservation {
                        transaction_id: request.transaction_id.clone(),
                        request_digest: history::request_digest(&owner, &request).unwrap(),
                        expected_revision: request.expected_revision,
                        prepare_cursor: 1,
                        owner,
                    };
                    data.prepared = Some(request);
                    data.reservation = Some(reservation.clone());
                    if matches!(
                        self.scenario,
                        Scenario::PrepareCallerEndedClean | Scenario::PrepareCallerEndedPending
                    ) {
                        return Err(Failure::new(
                            "CALLER_ENDED",
                            "history source is no longer the admitted execution",
                        ));
                    }
                    if matches!(self.scenario, Scenario::PrepareStepAuthorityRevokedClean) {
                        return Err(Failure::new(
                            "STEP_AUTHORITY_REVOKED",
                            "original step or Host authority is unavailable",
                        ));
                    }
                    if matches!(self.scenario, Scenario::PrepareWrappedCallerEndedClean) {
                        return Err(Failure::new(
                            "OUTCOME_UNKNOWN",
                            "original step or Host authority ended after resource dispatch",
                        )
                        .with(
                            "cause",
                            serde_json::json!({
                                "code": "CALLER_ENDED",
                                "message_key": "scheduler.step.source_changed",
                                "params": {}
                            }),
                        ));
                    }
                    serde_json::to_value(history::Prepared {
                        reservation,
                        may_dispatch: true,
                    })
                    .unwrap()
                }
                ops::DISPATCH => {
                    let request: ops::DispatchRequest = serde_json::from_value(payload).unwrap();
                    serde_json::to_value(ops::Dispatched {
                        call_ref: "child".into(),
                        transaction_id: request.transaction_id,
                        request_digest: request.request_digest,
                    })
                    .unwrap()
                }
                ops::STATUS => {
                    if let Some(receipt) = data.failed.clone() {
                        serde_json::to_value(ops::OperationStatus::Failed {
                            receipt: Box::new(receipt),
                            child: data.child.clone().unwrap(),
                        })
                        .unwrap()
                    } else if let Some(committed) = data.committed.clone() {
                        serde_json::to_value(ops::OperationStatus::Committed {
                            result: Box::new(committed),
                            child: data.child.clone().unwrap(),
                        })
                        .unwrap()
                    } else {
                        let request = data.prepared.clone().unwrap();
                        let reservation = data.reservation.clone().unwrap();
                        let result = ops::OperationResult {
                            operation: ops::Operation::RecoverFromModelError,
                            output: history::OpaquePayload {
                                profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
                                bytes: pilotdeck_context_runtime::boundary_codec::encode(
                                    &pilotdeck_protocol::parse_json(
                                        r#"{"type":"give_up","reason":"test"}"#,
                                    )
                                    .unwrap(),
                                )
                                .unwrap(),
                            },
                            history: ops::HistoryMutation {
                                replacement: None,
                                consume_input: false,
                                facts: vec![],
                                compaction_boundary: None,
                            },
                            preparation_notice: None,
                        };
                        let mut binding = self.context.binding.clone();
                        binding.id = "recovery".into();
                        binding.revision = "v1".into();
                        let mut invocation = self.context.invocation.clone();
                        invocation.parent_execution_ref = Some(self.context.call_ref.clone());
                        invocation.payload = serde_json::to_value(ops::InvocationRequest {
                            operation: ops::Operation::RecoverFromModelError,
                            input: request.request,
                            reservation,
                        })
                        .unwrap();
                        let failed = matches!(
                            self.scenario,
                            Scenario::FailureReleaseWait | Scenario::FailureLostAck
                        );
                        let completed = matches!(self.scenario, Scenario::LostCommitAck);
                        let child = p::CallView {
                            id: "child".into(),
                            session_ref: self.context.session_ref.clone(),
                            source_binding: Some(self.context.binding.id.clone()),
                            parent_session_ref: Some(self.context.session_ref.clone()),
                            cause_execution_ref: Some(self.context.call_ref.clone()),
                            binding,
                            invocation,
                            state: if completed {
                                State::Completed
                            } else if failed {
                                State::Failed
                            } else {
                                State::Running
                            },
                            result: if completed {
                                Some(serde_json::to_value(result).unwrap())
                            } else {
                                None
                            },
                            presentation: None,
                            settlement_data: None,
                            settlement_error: None,
                            error: failed.then(|| {
                                Failure::external("CONTEXT_PRODUCT_ERROR", "original")
                                    .with("message_utf16", serde_json::json!([65, 55296, 66]))
                            }),
                            waiting: None,
                            cursor: 0,
                            origin: self.context.interaction.clone(),
                            interaction: self.context.interaction.clone(),
                            interactions: vec![],
                            cancel_requested: false,
                            stepped: false,
                            security: self.context.security.clone(),
                        };
                        data.child = Some(child.clone());
                        if !completed && !failed {
                            self.cancellation
                                .cancel(CancelCause::Requested("stop".into()));
                        }
                        serde_json::to_value(ops::OperationStatus::Dispatched { child }).unwrap()
                    }
                }
                ops::failure::SETTLE => {
                    if let Some(first) = data.settle_payloads.first() {
                        assert_eq!(first, &payload);
                    }
                    data.settle_payloads.push(payload.clone());
                    if matches!(self.scenario, Scenario::FailureReleaseWait)
                        && data.settle_payloads.len() == 1
                    {
                        return Err(Failure::new(
                            "CONTEXT_FAILURE_UNCONFIRMED",
                            "release.pending",
                        ));
                    }
                    let request: ops::failure::SettleRequest =
                        serde_json::from_value(payload).unwrap();
                    let child = data.child.as_ref().unwrap();
                    let reservation = data.reservation.as_ref().unwrap();
                    let proof = ops::failure::Proof {
                        call_ref: child.id.clone(),
                        binding_ref: child.binding.id.clone(),
                        binding_revision: child.binding.revision.clone(),
                        operation: ops::Operation::RecoverFromModelError,
                        origin: child.origin.clone(),
                        disposition: ops::failure::Disposition::Failed,
                        outcome_digest: ops::failure::outcome_digest(child).unwrap(),
                        release_scope: ops::failure::ReleaseScope::KernelOwnedNativeResources,
                        trusted_boundary: ops::failure::TrustedBoundary {
                            contract_ref: ops::failure::TRUST_CONTRACT.into(),
                            artifact_sha256: "a".repeat(64),
                            export_name: "recover_from_model_error".into(),
                            args_digest: "b".repeat(64),
                            configuration_digest: "c".repeat(64),
                        },
                        managed_inventory_digest: "d".repeat(64),
                    };
                    let receipt = ops::failure::Receipt {
                        transaction_id: request.transaction_id,
                        request_digest: request.request_digest,
                        settlement_digest: ops::failure::settlement_digest(reservation, &proof)
                            .unwrap(),
                        revision: 1,
                        event_cursor: 2,
                        proof,
                    };
                    data.failed = Some(receipt.clone());
                    if matches!(self.scenario, Scenario::FailureLostAck) {
                        return Err(Failure::external(
                            "LOST_ACK",
                            "settlement acknowledgement lost",
                        ));
                    }
                    serde_json::to_value(receipt).unwrap()
                }
                ops::COMMIT => {
                    let request: ops::CommitRequest = serde_json::from_value(payload).unwrap();
                    let child = data.child.as_ref().unwrap();
                    let committed = ops::Committed {
                        receipt: history::Receipt {
                            transaction_id: request.transaction_id,
                            request_digest: request.request_digest,
                            commit_digest: "commit".into(),
                            revision: 1,
                            event_cursor: 2,
                            consumed_inputs: vec![],
                            replacement_digest: None,
                            compaction_digest: None,
                        },
                        proof: ops::CompletionProof {
                            call_ref: child.id.clone(),
                            binding_ref: child.binding.id.clone(),
                            binding_revision: child.binding.revision.clone(),
                            operation: ops::Operation::RecoverFromModelError,
                            result_digest: ops::result_digest(child.result.as_ref().unwrap())
                                .unwrap(),
                            origin: self.context.interaction.clone(),
                        },
                    };
                    data.committed = Some(committed);
                    return Err(Failure::external("LOST_ACK", "commit acknowledgement lost"));
                }
                _ => panic!("unexpected resource {method}"),
            };
            Ok(value)
        })
    }
    fn publish(
        &self,
        _: &str,
        _: &str,
        _: Value,
        _: State,
        _: Option<Failure>,
        _: Option<Wait>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { unavailable() })
    }
    fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { unavailable() })
    }
    fn invoke(&self, _: Invocation) -> BoxFuture<'_, Result<String>> {
        Box::pin(async { unavailable() })
    }
    fn find(&self, _: &str, _: &str) -> BoxFuture<'_, Result<Option<CallView>>> {
        Box::pin(async { unavailable() })
    }
    fn children(&self, _: &str) -> BoxFuture<'_, Result<Vec<CallView>>> {
        Box::pin(async { unavailable() })
    }
    fn get(&self, _: &str) -> BoxFuture<'_, Result<CallView>> {
        Box::pin(async { unavailable() })
    }
    fn wait(&self, _: &str) -> BoxFuture<'_, Result<CallView>> {
        Box::pin(async { unavailable() })
    }
    fn control(&self, _: &str, _: ControlKind, _: String) -> BoxFuture<'_, Result<ControlOutcome>> {
        Box::pin(async { unavailable() })
    }
    fn respond(
        &self,
        _: &str,
        _: &str,
        _: Vec<agentcpm_scheduler_protocol::Message>,
        _: &str,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { unavailable() })
    }
    fn read_state(&self, _: &str) -> BoxFuture<'_, Result<VersionedState>> {
        Box::pin(async { unavailable() })
    }
    fn write_state(&self, _: StateChange) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { unavailable() })
    }
    fn events(&self, _: u64) -> BoxFuture<'_, Result<Vec<Event>>> {
        Box::pin(async { unavailable() })
    }
    fn wait_events(&self, _: u64) -> BoxFuture<'_, Result<Vec<Event>>> {
        Box::pin(async { unavailable() })
    }
}
fn call() -> HostCall {
    HostCall::RecoverModelErrorBoundary(RecoverModelErrorBoundary {
        session_id: "session".into(),
        turn_id: "turn".into(),
        error: BoundarySnapshot::capture(&JsValue::Null),
        messages: vec![],
        has_attempted_compact: false,
    })
}
async fn run(
    scenario: Scenario,
) -> (
    std::result::Result<HostReply, PortError>,
    Vec<String>,
    Arc<ContextProofs>,
) {
    let mut context = crate::tests::context();
    // A later interaction has a different origin from the original call.
    context.origin.input_ref = "earlier-input".into();
    let cancellation = Cancellation::default();
    let client = Arc::new(ScriptClient {
        context: context.clone(),
        cancellation: cancellation.clone(),
        scenario,
        data: Mutex::new(StateData {
            methods: vec![],
            prepared: None,
            reservation: None,
            child: None,
            committed: None,
            failed: None,
            settle_payloads: vec![],
        }),
    });
    let proofs = Arc::new(ContextProofs::default());
    let codec = ContextCodec::new(
        context.clone(),
        vec![InstalledContextOperation {
            operation: ops::Operation::RecoverFromModelError,
            binding: "recovery".into(),
            revision: "v1".into(),
        }],
        proofs.clone(),
    )
    .unwrap();
    let handle = tokio::runtime::Handle::current();
    let observed = client.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut host = KernelHostPort::new(client, context, handle, Box::new(codec));
        host.call(
            &CallControl {
                cancellation,
                deadline: None,
            },
            call(),
        )
    })
    .await
    .unwrap();
    let methods = observed.data.lock().unwrap().methods.clone();
    (result, methods, proofs)
}
#[tokio::test(flavor = "multi_thread")]
async fn cancellation_during_delay_keeps_transaction_without_redispatch() {
    let (result, methods, proofs) = run(Scenario::CancelAtStatus).await;
    assert!(matches!(result, Err(PortError::AcceptedUnknown { .. })));
    assert_eq!(
        methods,
        [history::READ, history::PREPARE, ops::DISPATCH, ops::STATUS]
    );
    assert_eq!(proofs.unresolved().len(), 1);
    assert!(proofs.unresolved()[0].request_digest.is_some());
}
#[tokio::test(flavor = "multi_thread")]
async fn cancellation_while_status_pending_retains_transaction() {
    let (result, methods, proofs) = run(Scenario::CancelWhileStatusPending).await;
    assert!(matches!(result, Err(PortError::AcceptedUnknown { .. })));
    assert_eq!(
        methods,
        [history::READ, history::PREPARE, ops::DISPATCH, ops::STATUS]
    );
    assert_eq!(proofs.unresolved().len(), 1);
    assert!(proofs.unresolved()[0].request_digest.is_some());
}
#[tokio::test(flavor = "multi_thread")]
async fn lost_commit_ack_reads_committed_child_once_and_returns_original() {
    let (result, methods, proofs) = run(Scenario::LostCommitAck).await;
    let value = pilotdeck_protocol::parse_json(r#"{"type":"give_up","reason":"test"}"#).unwrap();
    let boundary = match result.unwrap() {
        HostReply::Recovery(v) => v.boundary,
        HostReply::RecoveryBoundaryOnly(b) => b,
        _ => panic!("wrong reply"),
    };
    assert_eq!(boundary.value(), &value);
    assert_eq!(
        methods,
        [
            history::READ,
            history::PREPARE,
            ops::DISPATCH,
            ops::STATUS,
            ops::COMMIT,
            ops::STATUS
        ]
    );
    assert!(proofs.unresolved().is_empty());
    assert_eq!(proofs.observed().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn failure_waits_for_release_then_returns_original_utf16() {
    let (result, methods, proofs) = run(Scenario::FailureReleaseWait).await;
    let Err(PortError::HostFailure { message, .. }) = result else {
        panic!("expected original failure")
    };
    assert_eq!(message.units(), &[65, 55296, 66]);
    assert_eq!(
        methods,
        [
            history::READ,
            history::PREPARE,
            ops::DISPATCH,
            ops::STATUS,
            ops::failure::SETTLE,
            ops::failure::SETTLE,
            ops::STATUS
        ]
    );
    assert!(proofs.unresolved().is_empty());
}
#[tokio::test(flavor = "multi_thread")]
async fn failure_lost_ack_reads_status_without_repeating_settlement() {
    let (result, methods, proofs) = run(Scenario::FailureLostAck).await;
    let Err(PortError::HostFailure { message, .. }) = result else {
        panic!("expected original failure")
    };
    assert_eq!(message.units(), &[65, 55296, 66]);
    assert_eq!(
        methods,
        [
            history::READ,
            history::PREPARE,
            ops::DISPATCH,
            ops::STATUS,
            ops::failure::SETTLE,
            ops::STATUS
        ]
    );
    assert!(proofs.unresolved().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_ended_on_prepare_reconciles_to_host_failure_only_when_view_is_clean() {
    let (clean_res, clean_methods, clean_proofs) = run(Scenario::PrepareCallerEndedClean).await;
    assert!(matches!(
        clean_res,
        Err(PortError::HostFailure { ref code, .. }) if code.equals_str("CALLER_ENDED")
    ));
    assert_eq!(
        clean_methods,
        [history::READ, history::PREPARE, history::READ]
    );
    assert!(clean_proofs.unresolved().is_empty());

    let (pending_res, pending_methods, pending_proofs) =
        run(Scenario::PrepareCallerEndedPending).await;
    assert!(matches!(
        pending_res,
        Err(PortError::AcceptedUnknown { .. })
    ));
    assert_eq!(
        pending_methods,
        [history::READ, history::PREPARE, history::READ]
    );
    assert_eq!(pending_proofs.unresolved().len(), 1);

    let (revoked_res, revoked_methods, revoked_proofs) =
        run(Scenario::PrepareStepAuthorityRevokedClean).await;
    assert!(matches!(
        revoked_res,
        Err(PortError::HostFailure { ref code, .. }) if code.equals_str("STEP_AUTHORITY_REVOKED")
    ));
    assert_eq!(
        revoked_methods,
        [history::READ, history::PREPARE, history::READ]
    );
    assert!(revoked_proofs.unresolved().is_empty());

    let (wrapped_res, wrapped_methods, wrapped_proofs) =
        run(Scenario::PrepareWrappedCallerEndedClean).await;
    assert!(matches!(
        wrapped_res,
        Err(PortError::HostFailure { ref code, .. }) if code.equals_str("OUTCOME_UNKNOWN")
    ));
    assert_eq!(
        wrapped_methods,
        [history::READ, history::PREPARE, history::READ]
    );
    assert!(wrapped_proofs.unresolved().is_empty());
}
