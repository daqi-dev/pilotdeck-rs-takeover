//! PilotDeck's admitted AgentLoop exposed through the public ifACE Scheduler SDK.
//!
//! This crate does not instantiate a Gateway or own a second execution runtime.
//! The composition root supplies pure admission/preparation and resource codecs;
//! every model, tool and context host effect uses the invocation-bound Kernel
//! client. Capability declarations remain conservative until their full product
//! adapters are installed and validated.
#![forbid(unsafe_code)]

pub mod original;
pub mod resources;

use agentcpm_iface::{
    BoxFuture,
    r#loop::{LoopContract, LoopDescriptor, StepOutcome},
    types::*,
};
use agentcpm_scheduler_protocol::{self as scheduler, Client, LoopFactory};
use pilotdeck_agent::{
    ProjectedMessage,
    ports::{CallControl, CancelCause, Cancellation, HostPort, PortError, PortResult, ToolHints},
    turn::{
        self, TurnClock, TurnEventPort, TurnPreparationPort, TurnResultKind, TurnRuntimeConfig,
    },
};
use resources::{KernelHostPort, ResourceCodec};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

/// Admitted, immutable product values and turn-local pure collaborators.
/// There is deliberately no `HostPort` field: the Native adapter always creates
/// its own Scheduler-bound resource port, preventing an old Gateway bypass.
pub struct AdmittedTurn {
    pub config: TurnRuntimeConfig,
    pub input: turn::TurnInput,
    pub initial: Vec<ProjectedMessage>,
    pub preparation: Box<pilotdeck_turn_preparation::OriginalTurnPreparation>,
    pub file_state: pilotdeck_agent::session_state::AgentFileState,
    pub events: Box<dyn TurnEventPort + Send>,
    pub clock: Box<dyn TurnClock + Send>,
    pub tool_hints: ToolHints,
}

/// Product admission retains its rich boundary data in `ctx.invocation.payload`.
/// The minimal ifACE text messages must not silently replace those boundaries.
/// Initialization uses the already-bound Kernel port. Implementations may not
/// install a second history/permission/execution owner or supply another host.
pub trait AdmittedTurnFactory: Send + Sync {
    fn resource_codec(
        &self,
        ctx: &scheduler::CallContext,
        input: &TurnInput,
    ) -> PortResult<Box<dyn ResourceCodec>>;
    fn prepare(
        &self,
        ctx: &scheduler::CallContext,
        input: &TurnInput,
        host: &mut dyn HostPort,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<AdmittedTurn>;
}

pub struct NativeFactory {
    admission: Arc<dyn AdmittedTurnFactory>,
    calls: Mutex<BTreeMap<String, Weak<Shared>>>,
}

impl NativeFactory {
    pub fn new(admission: Arc<dyn AdmittedTurnFactory>) -> Self {
        Self {
            admission,
            calls: Mutex::new(BTreeMap::new()),
        }
    }
}

/// Run the official bidirectional Native provider protocol on stdin/stdout.
/// A product binary supplies the admitted-turn factory; it must not put logs on
/// stdout or replace the SDK with a private Kernel transport.
pub async fn serve(admission: Arc<dyn AdmittedTurnFactory>) -> scheduler::Result<()> {
    scheduler::provider::serve(scheduler::provider::Export::Loop(Arc::new(
        NativeFactory::new(admission),
    )))
    .await
}

#[derive(Clone)]
struct Completion {
    outcome: StepOutcome,
    status: RequestState,
    result: Option<String>,
}

struct Shared {
    input: Mutex<Option<(TurnId, TurnInput)>>,
    cancellation: Cancellation,
    completion: watch::Sender<Option<Completion>>,
}

struct NativeLoop {
    context: scheduler::CallContext,
    client: Arc<dyn Client>,
    admission: Arc<dyn AdmittedTurnFactory>,
    shared: Arc<Shared>,
}

impl Drop for NativeLoop {
    fn drop(&mut self) {
        // Drop cannot claim that accepted work was undone. The hard cancellation
        // wakes cooperating ports; the Kernel retains durable reconciliation.
        self.shared.cancellation.cancel(CancelCause::OwnerRetired);
    }
}

fn protocol(message: &str) -> Error {
    Error::new(ErrorCategory::Protocol, -32602, message)
}

impl LoopFactory for NativeFactory {
    fn create(
        &self,
        ctx: scheduler::CallContext,
        client: Arc<dyn Client>,
    ) -> BoxFuture<'_, scheduler::Result<Arc<dyn LoopContract>>> {
        Box::pin(async move {
            if ctx.call_ref.is_empty()
                || ctx.session_ref.is_empty()
                || ctx.invocation.context_ref.is_empty()
            {
                return Err(scheduler::Failure::external(
                    "CONTEXT_BINDING",
                    "explicit call/session/context identity required",
                ));
            }
            let mut calls = self
                .calls
                .lock()
                .map_err(|_| scheduler::Failure::external("LOOP_LOCK", "call registry poisoned"))?;
            calls.retain(|_, weak| weak.strong_count() > 0);
            if calls.contains_key(&ctx.call_ref) {
                return Err(scheduler::Failure::external(
                    "DUPLICATE_CALL",
                    "call already has a Native loop",
                ));
            }
            let (completion, _) = watch::channel(None);
            let shared = Arc::new(Shared {
                input: Mutex::new(None),
                cancellation: Cancellation::default(),
                completion,
            });
            calls.insert(ctx.call_ref.clone(), Arc::downgrade(&shared));
            Ok(Arc::new(NativeLoop {
                context: ctx,
                client,
                admission: self.admission.clone(),
                shared,
            }) as Arc<dyn LoopContract>)
        })
    }

    fn result(
        &self,
        ctx: &scheduler::CallContext,
        _runtime: &dyn LoopContract,
        _outcome: &StepOutcome,
    ) -> scheduler::Result<Value> {
        let shared = self
            .calls
            .lock()
            .map_err(|_| scheduler::Failure::external("LOOP_LOCK", "call registry poisoned"))?
            .get(&ctx.call_ref)
            .and_then(Weak::upgrade)
            .ok_or_else(|| {
                scheduler::Failure::external("RESULT_BINDING", "call result is unavailable")
            })?;
        let completion = shared.completion.borrow().clone().ok_or_else(|| {
            scheduler::Failure::external("RESULT_PENDING", "turn has not finished")
        })?;
        // Preserve the exact PilotDeck JSON text, including escaped lone UTF-16
        // surrogates. serde_json::Value cannot represent those strings losslessly.
        Ok(completion.result.map_or(Value::Null, Value::String))
    }
}

impl NativeLoop {
    fn check_context(&self, context: &str) -> Result<(), Error> {
        if context != self.context.invocation.context_ref {
            return Err(protocol("context_ref differs from the prepared invocation"));
        }
        Ok(())
    }
}

impl LoopContract for NativeLoop {
    fn describe(&self) -> BoxFuture<'_, LoopDescriptor> {
        Box::pin(async {
            LoopDescriptor {
                id: "pilotdeck-admitted-turn".into(),
                language: "rust".into(),
                step_semantics: StepSemantics::TurnToCompletion,
                capabilities: LoopCapabilities {
                    interruptible: true,
                    ..LoopCapabilities::NONE
                },
            }
        })
    }

    fn step(
        &self,
        context: &SessionId,
        turn: TurnId,
        input: TurnInput,
    ) -> BoxFuture<'_, Result<StepOutcome, Error>> {
        let checked = self.check_context(context);
        Box::pin(async move {
            checked?;
            let runtime = tokio::runtime::Handle::try_current()
                .map_err(|_| protocol("Native loop requires a Tokio runtime"))?;
            if input.execution.run_id != self.context.call_ref
                || turn != self.context.interaction.turn.max(1)
            {
                return Err(protocol(
                    "step run/turn differs from the Scheduler invocation",
                ));
            }
            let start = {
                let mut original = self
                    .shared
                    .input
                    .lock()
                    .map_err(|_| protocol("input lock poisoned"))?;
                if let Some((original_turn, original_input)) = original.as_ref() {
                    if *original_turn != turn || original_input != &input {
                        return Err(protocol(
                            "repeated step changed the admitted input or execution identity",
                        ));
                    }
                    false
                } else {
                    *original = Some((turn, input.clone()));
                    true
                }
            };
            if start {
                let admission = self.admission.clone();
                let context = self.context.clone();
                let client = self.client.clone();
                let shared = self.shared.clone();
                // Own the worker independently of a dropped step future. The
                // completion is always retained and never causes a second run.
                tokio::spawn(async move {
                    let worker_shared = shared.clone();
                    let completed = tokio::task::spawn_blocking(move || {
                        execute(
                            admission.as_ref(),
                            context,
                            input,
                            client,
                            runtime,
                            worker_shared.cancellation.clone(),
                        )
                    })
                    .await
                    .unwrap_or_else(|_| unknown());
                    shared.completion.send_replace(Some(completed));
                });
            }
            let mut completion = self.shared.completion.subscribe();
            let value = completion
                .wait_for(Option::is_some)
                .await
                .map_err(|_| protocol("Native turn completion channel closed"))?;
            Ok(value
                .as_ref()
                .expect("wait_for checked completion")
                .outcome
                .clone())
        })
    }

    fn interrupt(
        &self,
        context: &SessionId,
        kind: InterruptKind,
        detail: &str,
    ) -> BoxFuture<'_, Result<InterruptOutcome, Error>> {
        let checked = self.check_context(context);
        let detail = detail.to_owned();
        Box::pin(async move {
            checked?;
            if let Some(done) = self.shared.completion.borrow().as_ref() {
                return Ok(if done.status.is_terminal() {
                    InterruptOutcome::AlreadyTerminal
                } else {
                    InterruptOutcome::TooLate
                });
            }
            let cause = match kind {
                InterruptKind::Cancel => CancelCause::Requested(detail.into()),
                InterruptKind::Expire => CancelCause::Deadline,
                _ => return Ok(InterruptOutcome::Unsupported),
            };
            self.shared.cancellation.cancel(cause);
            Ok(InterruptOutcome::Accepted)
        })
    }

    fn status(&self, context: &SessionId) -> BoxFuture<'_, Result<LoopStatus, Error>> {
        let checked = self.check_context(context);
        Box::pin(async move {
            checked?;
            let completed = self.shared.completion.borrow().clone();
            let started = self
                .shared
                .input
                .lock()
                .map_err(|_| protocol("input lock poisoned"))?
                .is_some();
            Ok(LoopStatus {
                state: completed.as_ref().map_or(
                    if started {
                        RequestState::Running
                    } else {
                        RequestState::Accepted
                    },
                    |c| c.status,
                ),
                turn: Some(self.context.interaction.turn.max(1)),
                terminal: completed.as_ref().and_then(|c| match &c.outcome {
                    StepOutcome::Finished { outcome } => Some(TurnTerminal {
                        code: c.status.as_str().into(),
                        stop_reason: outcome.stop_reason.clone(),
                    }),
                    StepOutcome::Failed {
                        code, stop_reason, ..
                    } => Some(TurnTerminal {
                        code: code.clone(),
                        stop_reason: stop_reason.clone().unwrap_or_default(),
                    }),
                    _ => None,
                }),
                last_seq: 0,
                blocked_on: vec![],
                budget_left: None,
            })
        })
    }
}

fn deadline(absolute_ms: Option<u64>) -> Option<Instant> {
    absolute_ms.map(|absolute| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let monotonic = Instant::now();
        monotonic
            .checked_add(Duration::from_millis(
                absolute.saturating_sub(now.min(u64::MAX as u128) as u64),
            ))
            .unwrap_or(monotonic)
    })
}

fn execute(
    admission: &dyn AdmittedTurnFactory,
    context: scheduler::CallContext,
    input: TurnInput,
    client: Arc<dyn Client>,
    runtime: tokio::runtime::Handle,
    cancellation: Cancellation,
) -> Completion {
    let control = CallControl {
        cancellation: cancellation.clone(),
        deadline: deadline(context.invocation.limits.deadline_ms),
    };
    if let Err(error) = control.check() {
        return port_failure(error);
    }
    let resources = match admission.resource_codec(&context, &input) {
        Ok(value) => value,
        Err(error) => return port_failure(error),
    };
    let mut host = KernelHostPort::new(client.clone(), context.clone(), runtime, resources);
    let mut admitted = match admission.prepare(&context, &input, &mut host, client, &control) {
        Ok(value) => value,
        Err(error) => return port_failure(error),
    };
    let cleanup = CallControl {
        cancellation: cancellation.cleanup_scope(),
        deadline: None,
    };
    let output = turn::run_admitted_turn(
        &mut admitted.config,
        &admitted.input,
        admitted.initial,
        &mut host,
        admitted.preparation.as_mut(),
        admitted.events.as_mut(),
        admitted.clock.as_ref(),
        &control,
        &cleanup,
        &mut admitted.tool_hints,
    );
    // Like the original factory, every owning exit settles advisory notices.
    // An unresolved accepted effect takes precedence over an ordinary failure.
    let settled = admitted.preparation.finish_pending_effects();
    let output = match (output, settled) {
        (Err(error @ PortError::AcceptedUnknown { .. }), _) => Err(error),
        (_, Err(error @ PortError::AcceptedUnknown { .. })) => Err(error),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(output), Ok(())) => Ok(output),
    };
    match output {
        Ok(output) => {
            let result = pilotdeck_agent::turn_wire::result(&admitted.input, &output.result);
            let mut payload = pilotdeck_protocol::JsObject::new();
            payload.insert("result".into(), result.value().clone());
            payload.insert(
                "messages".into(),
                pilotdeck_protocol::JsValue::Array(
                    output
                        .messages
                        .iter()
                        .map(|message| message.boundary.value().clone())
                        .collect(),
                ),
            );
            payload.insert(
                "seedState".into(),
                pilotdeck_agent::session_state::snapshot_file_state(&admitted.file_state)
                    .value()
                    .clone(),
            );
            payload.insert(
                "modelState".into(),
                pilotdeck_agent::session_state::snapshot_model_state(
                    admitted.preparation.model_state(),
                )
                .value()
                .clone(),
            );
            let encoded = match pilotdeck_protocol::stringify_json(
                &pilotdeck_protocol::JsValue::Object(payload),
            ) {
                Ok(Some(value)) => value,
                _ => return unknown(),
            };
            let reason = result
                .value()
                .get("stopReason")
                .and_then(|v| v.as_string())
                .and_then(|v| v.to_utf8().ok())
                .unwrap_or_else(|| "unknown".into());
            let (status, outcome) = match output.result.kind {
                TurnResultKind::Error | TurnResultKind::MaxTurns => (
                    RequestState::Failed,
                    StepOutcome::Failed {
                        code: "PILOTDECK_TURN_FAILED".into(),
                        message: reason.clone(),
                        stop_reason: Some(reason),
                    },
                ),
                kind => {
                    let completed = kind != TurnResultKind::Aborted;
                    (
                        if completed {
                            RequestState::Completed
                        } else {
                            RequestState::Cancelled
                        },
                        StepOutcome::Finished {
                            outcome: TurnOutcome {
                                completed,
                                stop_reason: reason,
                                final_seq: 0,
                            },
                        },
                    )
                }
            };
            Completion {
                outcome,
                status,
                result: Some(encoded),
            }
        }
        Err(error) => port_failure(error),
    }
}

fn unknown() -> Completion {
    Completion {
        outcome: StepOutcome::Unknown { reconcile: true },
        status: RequestState::Unknown,
        result: None,
    }
}

fn port_failure(error: PortError) -> Completion {
    match error {
        PortError::AcceptedUnknown { .. } => unknown(),
        PortError::Cancelled { .. } => Completion {
            outcome: StepOutcome::Finished {
                outcome: TurnOutcome {
                    completed: false,
                    stop_reason: "cancelled".into(),
                    final_seq: 0,
                },
            },
            status: RequestState::Cancelled,
            result: None,
        },
        error => Completion {
            outcome: StepOutcome::Failed {
                code: "PILOTDECK_PORT_FAILURE".into(),
                message: format!("{error:?}"),
                stop_reason: Some("host_port_failure".into()),
            },
            status: RequestState::Failed,
            result: None,
        },
    }
}

#[cfg(test)]
mod original_tests;
#[cfg(test)]
mod tests;
