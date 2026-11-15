//! Restricted source-bound native read_file batch adapter (WIP).
use super::{
    grouped_tool_gate::{HostFinalExecutor, grouped_gate},
    managed_tool_action::ActionControl,
    original_tool_batch::OriginalToolBatch,
    project_model_owner::{
        ProjectModelLease, ProjectModelOwner, ProjectOwnerToken, ProjectTurnBorrow,
    },
    tool_batch_envelope::{self, DecodedToolEnvelope},
};
use crate::{
    host::{
        HostError, HostErrorKind, HostFuture,
        modules::capability::{ToolCall, parse_tool_call},
    },
    runtime::tools::scheduler::{FinalToolContinuation, FinalToolPermit, FinalToolRequest},
};
use agentcpm_host::{
    TrustedToolBatchBackend, TrustedToolBatchExecution, TrustedToolBatchInstallation,
    TrustedToolBatchSetup, TrustedToolFinalCandidate, TrustedToolFinalExecution,
    TrustedToolFinalPermit, TrustedToolFinalResolution, TrustedToolLifecycle,
    TrustedToolPreparation, TrustedToolProjectReservation,
};
use agentcpm_iface::{
    tool::{ToolEffect, ToolSpec},
    types::{Error, ErrorCategory},
};
use agentcpm_scheduler_protocol::{context_history::OpaquePayload, tool_batch::BusinessResult};
use futures_util::FutureExt;
use pilotdeck_protocol::{JsObject, JsValue, boundary_codec};
use std::{
    any::Any,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::task::JoinHandle;

fn denied() -> Error {
    Error::new(
        ErrorCategory::Policy,
        -32000,
        "Managed tool request unavailable.",
    )
}
fn host_denied() -> HostError {
    HostError::new(HostErrorKind::Protocol, "Managed tool request unavailable.")
}
fn encoded(value: &JsValue) -> Result<Vec<u8>, Error> {
    boundary_codec::encode(value).map_err(|_| denied())
}
fn batch_result(single: bool, object: JsObject) -> Result<BusinessResult, Error> {
    let failed = single
        && object
            .get("type")
            .and_then(JsValue::as_string)
            .is_some_and(|value| value.equals_str("error"));
    let payload = OpaquePayload {
        profile: boundary_codec::PROFILE.into(),
        bytes: encoded(&JsValue::Object(object))?,
    };
    Ok(if failed {
        BusinessResult::Failed { payload }
    } else {
        BusinessResult::Succeeded { payload }
    })
}
fn original_call(envelope: &JsValue, ordinal: usize) -> Result<ToolCall, Error> {
    let payload = envelope.get("payload").ok_or_else(denied)?;
    let operation = payload
        .get("operation")
        .and_then(JsValue::as_string)
        .ok_or_else(denied)?;
    let value = if operation.equals_str("execute") && ordinal == 0 {
        payload
    } else if operation.equals_str("execute_batch") {
        match payload.get("calls") {
            Some(JsValue::Array(calls)) => calls.get(ordinal).ok_or_else(denied)?,
            _ => return Err(denied()),
        }
    } else {
        return Err(denied());
    };
    parse_tool_call(value).map_err(|_| denied())
}
fn same_call(a: &ToolCall, b: &ToolCall) -> Result<bool, Error> {
    Ok(a.id == b.id && a.name == b.name && encoded(&a.input)? == encoded(&b.input)?)
}
fn json_lossless(value: &JsValue) -> Result<Vec<u8>, Error> {
    let text = pilotdeck_protocol::stringify_json(value)
        .map_err(|_| denied())?
        .ok_or_else(denied)?;
    let reconstructed = pilotdeck_protocol::parse_json(&text).map_err(|_| denied())?;
    if encoded(value)? != encoded(&reconstructed)? {
        return Err(denied());
    }
    Ok(text.into_bytes())
}
struct Candidate {
    request: FinalToolRequest,
    implementation: Arc<dyn Any + Send + Sync>,
    decision: Arc<dyn Any + Send + Sync>,
}
struct Permit(TrustedToolFinalPermit);
impl FinalToolPermit for Permit {
    fn revalidate(&self) -> Result<(), HostError> {
        self.0.revalidate().map_err(|_| host_denied())
    }
}
struct Executor {
    prep: TrustedToolPreparation,
}
impl HostFinalExecutor for Executor {
    fn execute(
        &self,
        request: FinalToolRequest,
        continuation: FinalToolContinuation,
    ) -> HostFuture<JsObject> {
        let prep = self.prep.clone();
        Box::pin(async move {
            let implementation: Arc<dyn Any + Send + Sync> =
                Arc::new(request.implementation.clone());
            let decision: Arc<dyn Any + Send + Sync> =
                Arc::new(request.permission_decision.clone());
            let ordinal = u64::try_from(request.ordinal).map_err(|_| host_denied())?;
            let candidate = prep
                .candidate(
                    ordinal,
                    Candidate {
                        request,
                        implementation,
                        decision,
                    },
                )
                .map_err(|_| host_denied())?;
            let result = prep
                .execute_final(
                    candidate,
                    Box::new(move |permit| {
                        let control = ActionControl::new();
                        let future = continuation(Arc::new(Permit(permit)));
                        let result = Box::pin(async move {
                            let result = future.await.map_err(|_| denied())?;
                            let failed = result
                                .get("type")
                                .and_then(JsValue::as_string)
                                .is_some_and(|value| value.equals_str("error"));
                            let payload = OpaquePayload {
                                profile: boundary_codec::PROFILE.into(),
                                bytes: encoded(&JsValue::Object(result))?,
                            };
                            Ok(if failed {
                                BusinessResult::Failed { payload }
                            } else {
                                BusinessResult::Succeeded { payload }
                            })
                        });
                        Ok(TrustedToolFinalExecution {
                            result: control.wrap(result),
                            control,
                        })
                    }),
                )
                .await
                .map_err(|_| host_denied())?;
            let payload = match result {
                BusinessResult::Succeeded { payload } | BusinessResult::Failed { payload } => {
                    payload
                }
            };
            if payload.profile != boundary_codec::PROFILE {
                return Err(host_denied());
            }
            match boundary_codec::decode(&payload.bytes).map_err(|_| host_denied())? {
                JsValue::Object(ref value) => Ok(value.clone()),
                _ => Err(host_denied()),
            }
        })
    }
}
struct Owner {
    turn: Option<ProjectTurnBorrow>,
    lease: Option<ProjectModelLease>,
    token: ProjectOwnerToken,
    _reservation: TrustedToolProjectReservation,
}
struct Scope {
    group: super::detached_tasks::ProducerGroup<Owner>,
    cleaned: Arc<AtomicBool>,
    task: Mutex<Option<JoinHandle<()>>>,
    _cleanup_owner: Arc<tokio::sync::Mutex<Option<Owner>>>,
    cancel: crate::host::Cancellation,
    joined: AtomicBool,
}
fn monitor_joined(
    cleaned: &AtomicBool,
    joined: &AtomicBool,
    task: &Mutex<Option<JoinHandle<()>>>,
) -> bool {
    if !cleaned.load(Ordering::Acquire) {
        return false;
    }
    if joined.load(Ordering::Acquire) {
        return true;
    }
    let Ok(mut slot) = task.try_lock() else {
        return false;
    };
    let Some(task) = slot.as_mut() else {
        return joined.load(Ordering::Acquire);
    };
    if !task.is_finished() {
        return false;
    }
    let Some(result) = task.now_or_never() else {
        return false;
    };
    drop(slot.take());
    if result.is_ok() {
        joined.store(true, Ordering::Release);
        true
    } else {
        false
    }
}
impl TrustedToolLifecycle for Scope {
    fn stop(&self) {
        self.cancel.cancel(None);
        self.group.close_root();
    }
    fn is_released(&self) -> bool {
        monitor_joined(&self.cleaned, &self.joined, &self.task)
    }
}
struct CloseOnDrop(super::detached_tasks::ProducerGroup<Owner>);
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close_root();
    }
}
struct Installation;
impl TrustedToolLifecycle for Installation {
    fn stop(&self) {}
    fn is_released(&self) -> bool {
        true
    }
}
pub struct NativeToolBatchSetup;
impl TrustedToolBatchSetup for NativeToolBatchSetup {
    fn build(&self) -> Result<TrustedToolBatchInstallation, Error> {
        Ok(TrustedToolBatchInstallation {
            backend: Arc::new(Backend),
            lifecycle: Arc::new(Installation),
        })
    }
}
struct Backend;
impl TrustedToolBatchBackend for Backend {
    fn validate_envelope(
        &self,
        envelope: &OpaquePayload,
    ) -> Result<Arc<dyn Any + Send + Sync>, Error> {
        Ok(Arc::new(tool_batch_envelope::validate_envelope(envelope)?))
    }
    fn start(&self, prep: TrustedToolPreparation) -> Result<TrustedToolBatchExecution, Error> {
        let reservation = prep.reserve_project()?;
        let owner = reservation
            .owner::<ProjectModelOwner>()
            .ok_or_else(denied)?;
        let turn = owner.borrow_tool_turn(&reservation).map_err(|_| denied())?;
        let envelope = prep
            .decoded::<DecodedToolEnvelope>()
            .ok_or_else(denied)?
            .value()
            .clone();
        let single = envelope
            .get("payload")
            .and_then(|value| value.get("operation"))
            .and_then(JsValue::as_string)
            .is_some_and(|value| value.equals_str("execute"));
        let handle = tokio::runtime::Handle::try_current().map_err(|_| denied())?;
        let token = owner.token().map_err(|_| denied())?;
        let authority_valid = reservation.revalidate().is_ok();
        let cancel = crate::host::Cancellation::default();
        let (group, gate) = grouped_gate(
            Owner {
                turn: Some(turn.clone()),
                lease: None,
                token,
                _reservation: reservation,
            },
            handle.clone(),
            Arc::new(Executor { prep }),
            cancel.clone(),
        );
        let cleaned = Arc::new(AtomicBool::new(false));
        let monitor_group = group.clone();
        let completed = cleaned.clone();
        let cleanup_owner = Arc::new(tokio::sync::Mutex::new(None::<Owner>));
        let retained = cleanup_owner.clone();
        let monitor = handle.spawn(async move {
            loop {
                let mut slot = retained.lock().await;
                if let Some(owner) = monitor_group.take_drained_owner() {
                    *slot = Some(owner);
                    let owner = slot.as_mut().expect("cleanup owner installed");
                    drop(owner.turn.take());
                    let released = match owner.lease.as_mut() {
                        Some(lease) => lease.release().await.is_ok(),
                        None => true,
                    };
                    if released {
                        let owner = slot.take();
                        drop(slot);
                        drop(owner);
                        completed.store(true, Ordering::Release);
                    }
                    break;
                }
                drop(slot);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        let scope = Arc::new(Scope {
            group: group.clone(),
            cleaned,
            task: Mutex::new(Some(monitor)),
            _cleanup_owner: cleanup_owner,
            cancel,
            joined: AtomicBool::new(false),
        });
        let future = if authority_valid {
            OriginalToolBatch::bind(turn, gate, envelope)
                .map(|batch| {
                    let (borrow, future) = batch.into_run();
                    drop(borrow);
                    future
                })
                .map_err(|_| denied())
        } else {
            drop(gate);
            drop(turn);
            Err(denied())
        };
        let result = Box::pin(async move {
            let _close = CloseOnDrop(group);
            let future = future?;
            _close
                .0
                .with_open_owner(|owned| {
                    let owner = owned
                        ._reservation
                        .owner::<ProjectModelOwner>()
                        .ok_or_else(denied)?;
                    let (lease, valid) = owned
                        .token
                        .checkout_for_managed(owner)
                        .map_err(|_| denied())?;
                    owned.lease = Some(lease);
                    if !valid {
                        return Err(denied());
                    }
                    owned._reservation.revalidate()
                })
                .ok_or_else(denied)??;
            let object = future.await.map_err(|_| denied())?;
            batch_result(single, object)
        });
        Ok(TrustedToolBatchExecution {
            result,
            control: scope,
        })
    }
    fn resolve_final(
        &self,
        prep: &TrustedToolPreparation,
        candidate: &TrustedToolFinalCandidate,
    ) -> Result<TrustedToolFinalResolution, Error> {
        prep.revalidate()?;
        let data = candidate.state::<Candidate>().ok_or_else(denied)?;
        data.request
            .hooks
            .as_ref()
            .ok_or_else(denied)?
            .revalidate()
            .map_err(|_| denied())?;
        if candidate.ordinal() != u64::try_from(data.request.ordinal).map_err(|_| denied())? {
            return Err(denied());
        }
        let envelope = prep
            .decoded::<DecodedToolEnvelope>()
            .ok_or_else(denied)?
            .value();
        if !same_call(
            &original_call(envelope, data.request.ordinal)?,
            &data.request.original_call,
        )? {
            return Err(denied());
        }
        let tool = data.request.implementation.as_ref();
        if !(tool as &dyn Any).is::<crate::runtime::tools::builtin::read_file::ReadFileTool>() {
            return Err(denied());
        }
        if !data
            .request
            .registry
            .get(&data.request.canonical_name)
            .is_some_and(|current| Arc::ptr_eq(&current, &data.request.implementation))
        {
            return Err(denied());
        }
        let fields = tool.fields();
        let name = data
            .request
            .canonical_name
            .to_utf8()
            .map_err(|_| denied())?;
        let description = fields
            .get("description")
            .and_then(JsValue::as_string)
            .ok_or_else(denied)?
            .to_utf8()
            .map_err(|_| denied())?;
        let parameters = json_lossless(fields.get("inputSchema").ok_or_else(denied)?)?;
        let policy_args = json_lossless(&data.request.final_input)?;
        prep.revalidate()?;
        Ok(TrustedToolFinalResolution {
            spec: ToolSpec {
                name,
                description,
                parameters,
            },
            impl_id: "pilotdeck.native.read_file".into(),
            version: "1".into(),
            policy_args,
            // The original continuation also invokes the native event sink and
            // updates shared checkpoint state. Until those effects have their
            // own Host authorization boundary they cannot satisfy the iface
            // ReadOnly contract, even though the file operation only reads.
            effect: ToolEffect::SideEffect,
            implementation: data.implementation.clone(),
            decision: data.decision.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::{JsObject, JsString};
    #[test]
    fn ordinal_uses_original_handler_parser_and_preserves_utf16() {
        let mut call = JsObject::new();
        call.insert(
            "toolCallId".into(),
            JsValue::String(JsString::from_utf16(vec![0xd800])),
        );
        call.insert("name".into(), "read_file".into());
        call.insert("arguments".into(), JsValue::Null);
        let mut payload = JsObject::new();
        payload.insert("operation".into(), "execute_batch".into());
        payload.insert("calls".into(), JsValue::Array(vec![JsValue::Object(call)]));
        let mut envelope = JsObject::new();
        envelope.insert("payload".into(), JsValue::Object(payload));
        let call = original_call(&JsValue::Object(envelope), 0).unwrap();
        assert_eq!(call.id.units(), &[0xd800]);
        assert!(matches!(call.input, JsValue::Object(_)));
    }
    #[test]
    fn policy_projection_refuses_undefined_instead_of_silent_coercion() {
        let mut value = JsObject::new();
        value.insert("file_path".into(), "safe.txt".into());
        assert!(json_lossless(&JsValue::Object(value.clone())).is_ok());
        value.insert("unknown".into(), JsValue::Undefined);
        assert!(json_lossless(&JsValue::Object(value)).is_err());
    }

    #[test]
    fn single_failure_preserves_payload_and_batch_keeps_original_envelope() {
        let mut object = JsObject::new();
        object.insert("type".into(), "error".into());
        object.insert("detail".into(), "original".into());
        let original = encoded(&JsValue::Object(object.clone())).unwrap();
        match batch_result(true, object.clone()).unwrap() {
            BusinessResult::Failed { payload } => assert_eq!(payload.bytes, original),
            _ => panic!("single tool failure was relabelled"),
        }
        assert!(matches!(
            batch_result(false, object).unwrap(),
            BusinessResult::Succeeded { .. }
        ));
    }

    #[tokio::test]
    async fn monitor_requires_successful_join_and_never_accepts_panic_or_cancel() {
        let clean = AtomicBool::new(true);
        let joined = AtomicBool::new(false);
        let slot = Mutex::new(Some(tokio::spawn(async {})));
        while !slot.lock().unwrap().as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        assert!(monitor_joined(&clean, &joined, &slot));
        assert!(monitor_joined(&clean, &joined, &slot));

        for cancel in [false, true] {
            let clean = AtomicBool::new(true);
            let joined = AtomicBool::new(false);
            let task = if cancel {
                tokio::spawn(std::future::pending::<()>())
            } else {
                tokio::spawn(async { panic!("monitor failed") })
            };
            if cancel {
                task.abort();
            }
            let slot = Mutex::new(Some(task));
            while !slot.lock().unwrap().as_ref().unwrap().is_finished() {
                tokio::task::yield_now().await;
            }
            assert!(!monitor_joined(&clean, &joined, &slot));
            assert!(!monitor_joined(&clean, &joined, &slot));
        }
    }
}
