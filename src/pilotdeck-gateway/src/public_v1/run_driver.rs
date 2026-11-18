//! The offline deterministic run driver (`007 §7.2`, no-model-key path) and
//! the run-state helpers it shares with the runtime's worker loop.
//!
//! Moved verbatim from `runtime.rs` in the S3 substrate refactor; the
//! runtime re-exports the driver types so existing imports keep working.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use agentcpm_pd_protocol::contract::RunState;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::public_v1::runtime::PublicV1Runtime;
use crate::public_v1::store::{RunTransition, StoredRun, V1Store};

pub type DriverFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

pub trait RunDriver: Send + Sync {
    fn execute(&self, ctx: RunDriverContext) -> DriverFuture;
}

#[derive(Clone)]
pub struct RunDriverContext {
    pub store: Arc<V1Store>,
    pub run: StoredRun,
    pub runtime: Arc<PublicV1Runtime>,
    pub cancel_rx: watch::Receiver<bool>,
}

/// Offline deterministic driver supporting real run progression, cancellation,
/// failures with queue pause, approval/question interaction waits, and event bursts.
pub struct OfflineRunDriver;

pub(crate) fn cancel_run_now(store: &V1Store, run_id: &str, payload: &Value) {
    let _ = store.transition_run_state(
        run_id,
        RunTransition::new(RunState::Cancelled, "run.cancelled", payload).outcome_code("CANCELLED"),
    );
}

pub(crate) fn succeed_run_now(store: &V1Store, run_id: &str, payload: &Value) {
    let _ = store.transition_run_state(
        run_id,
        RunTransition::new(RunState::Succeeded, "run.completed", payload).outcome_code("SUCCEEDED"),
    );
}

pub(crate) fn fail_run_now(store: &V1Store, run_id: &str, code: &str, payload: &Value) {
    let _ = store.transition_run_state(
        run_id,
        RunTransition::new(RunState::Failed, "run.failed", payload)
            .outcome_code(code)
            .pause_session_reason("run_failed"),
    );
}

impl RunDriver for OfflineRunDriver {
    fn execute(&self, mut ctx: RunDriverContext) -> DriverFuture {
        Box::pin(async move {
            let run_id = ctx.run.run_id.clone();
            if *ctx.cancel_rx.borrow() {
                cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                return;
            }

            if ctx
                .store
                .transition_run_state(
                    &run_id,
                    RunTransition::new(
                        RunState::Running,
                        "run.started",
                        &json!({"runId": run_id, "sessionId": ctx.run.session_id}),
                    ),
                )
                .is_err()
            {
                return;
            }

            let prompt_text = extract_prompt_text(&ctx.run.input);
            let directive = parse_driver_directive(&prompt_text);

            match directive {
                DriverDirective::BurstEvents(count) => {
                    for i in 0..count {
                        if *ctx.cancel_rx.borrow() {
                            cancel_run_now(
                                &ctx.store,
                                &run_id,
                                &json!({"runId": run_id, "seq": i}),
                            );
                            return;
                        }
                        let _ = ctx.store.append_run_progress_event(
                            &run_id,
                            &ctx.run.session_id,
                            "run.output.delta",
                            &json!({"runId": run_id, "chunkIndex": i, "delta": format!("chunk-{i:05}-{}", "x".repeat(48))}),
                        );
                        if i % 64 == 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                    succeed_run_now(
                        &ctx.store,
                        &run_id,
                        &json!({"runId": run_id, "emittedEvents": count}),
                    );
                }
                DriverDirective::Fail {
                    reason,
                    pause_reason,
                } => {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    let _ = ctx.store.transition_run_state(
                        &run_id,
                        RunTransition::new(
                            RunState::Failed,
                            "run.failed",
                            &json!({"runId": run_id, "errorCode": reason}),
                        )
                        .outcome_code(&reason)
                        .pause_session_reason(&pause_reason),
                    );
                }
                DriverDirective::Approval { prompt } => {
                    let Ok(ia) = ctx.store.create_interaction(
                        &run_id,
                        "approval",
                        &prompt,
                        vec!["allow".to_owned(), "deny".to_owned()],
                        Some("agentcpm-pd.approval.response/1".to_owned()),
                        json!({"type": "object", "properties": {"decision": {"enum": ["allow", "deny"]}}, "required": ["decision"]}),
                    ) else {
                        // P2 (wp4 review): a failed interaction write must not
                        // fall through to the worker's success fallback.
                        let _ = ctx.store.transition_run_state(
                            &run_id,
                            RunTransition::new(
                                RunState::Failed,
                                "run.failed",
                                &json!({"runId": run_id, "errorCode": "INTERACTION_CREATE_FAILED"}),
                            )
                            .outcome_code("INTERACTION_CREATE_FAILED")
                            .pause_session_reason("run_failed"),
                        );
                        return;
                    };
                    let mut rx = ctx.runtime.register_interaction_waiter(&ia.interaction_id);
                    loop {
                        let resolved = rx.borrow_and_update().clone();
                        if let Some(ans) = resolved {
                            let decision = ans
                                .get("decision")
                                .and_then(Value::as_str)
                                .unwrap_or("deny");
                            if decision == "allow" {
                                succeed_run_now(
                                    &ctx.store,
                                    &run_id,
                                    &json!({"runId": run_id, "approved": true}),
                                );
                            } else {
                                let _ = ctx.store.transition_run_state(
                                    &run_id,
                                    RunTransition::new(
                                        RunState::Failed,
                                        "run.failed",
                                        &json!({"runId": run_id, "errorCode": "APPROVAL_DENIED"}),
                                    )
                                    .outcome_code("APPROVAL_DENIED"),
                                );
                            }
                            return;
                        }
                        if *ctx.cancel_rx.borrow() {
                            cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                            return;
                        }
                        tokio::select! {
                            _ = rx.changed() => {}
                            _ = ctx.cancel_rx.changed() => {}
                        }
                    }
                }
                DriverDirective::Question { prompt } => {
                    let Ok(ia) = ctx.store.create_interaction(
                        &run_id,
                        "question",
                        &prompt,
                        vec![],
                        Some("agentcpm-pd.question.response/1".to_owned()),
                        json!({"type": "object"}),
                    ) else {
                        // P2 (wp4 review): a failed interaction write must not
                        // fall through to the worker's success fallback.
                        let _ = ctx.store.transition_run_state(
                            &run_id,
                            RunTransition::new(
                                RunState::Failed,
                                "run.failed",
                                &json!({"runId": run_id, "errorCode": "INTERACTION_CREATE_FAILED"}),
                            )
                            .outcome_code("INTERACTION_CREATE_FAILED")
                            .pause_session_reason("run_failed"),
                        );
                        return;
                    };
                    let mut rx = ctx.runtime.register_interaction_waiter(&ia.interaction_id);
                    loop {
                        let resolved = rx.borrow_and_update().clone();
                        if let Some(ans) = resolved {
                            succeed_run_now(
                                &ctx.store,
                                &run_id,
                                &json!({"runId": run_id, "answer": ans}),
                            );
                            return;
                        }
                        if *ctx.cancel_rx.borrow() {
                            cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                            return;
                        }
                        tokio::select! {
                            _ = rx.changed() => {}
                            _ = ctx.cancel_rx.changed() => {}
                        }
                    }
                }
                DriverDirective::Sleep(ms) => {
                    let deadline = Instant::now() + Duration::from_millis(ms);
                    while Instant::now() < deadline {
                        if *ctx.cancel_rx.borrow() {
                            cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                            return;
                        }
                        let step = (deadline.saturating_duration_since(Instant::now()))
                            .min(Duration::from_millis(20));
                        tokio::select! {
                            _ = tokio::time::sleep(step) => {}
                            _ = ctx.cancel_rx.changed() => {}
                        }
                    }
                    let _ = ctx.store.append_run_progress_event(
                        &run_id,
                        &ctx.run.session_id,
                        "run.output.delta",
                        &json!({"runId": run_id, "text": format!("completed: {prompt_text}")}),
                    );
                    succeed_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                }
            }
        })
    }
}

enum DriverDirective {
    Sleep(u64),
    Fail {
        reason: String,
        pause_reason: String,
    },
    Approval {
        prompt: String,
    },
    Question {
        prompt: String,
    },
    BurstEvents(usize),
}

pub(crate) fn extract_prompt_text(input: &Value) -> String {
    input
        .get("input")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn parse_driver_directive(text: &str) -> DriverDirective {
    if let Some(rest) = text.split("[pd-driver:events=").nth(1)
        && let Some(num) = rest.split(']').next()
        && let Ok(count) = num.trim().parse::<usize>()
    {
        return DriverDirective::BurstEvents(count.clamp(1, 20_000));
    }
    if let Some(rest) = text.split("[pd-driver:sleep=").nth(1)
        && let Some(num) = rest.split(']').next()
        && let Ok(ms) = num.trim().parse::<u64>()
    {
        // P2 (wp4 review): bound the test directive like `events` — an
        // unclamped sleep would pin the run (and its session) forever.
        return DriverDirective::Sleep(ms.clamp(1, 600_000));
    }
    if text.contains("[pd-driver:fail]") {
        return DriverDirective::Fail {
            reason: "DRIVER_INJECTED_FAILURE".to_owned(),
            pause_reason: "run_failed".to_owned(),
        };
    }
    if text.contains("[pd-driver:approval]") {
        return DriverDirective::Approval {
            prompt: text.to_owned(),
        };
    }
    if text.contains("[pd-driver:question]") {
        return DriverDirective::Question {
            prompt: text.to_owned(),
        };
    }
    DriverDirective::Sleep(25)
}
