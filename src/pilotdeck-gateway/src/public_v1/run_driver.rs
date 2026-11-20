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
/// failures with queue pause, approval/question interaction waits, event
/// bursts, and file writes through the files domain's production path (A5,
/// snap-wire §5 — run-owned file side effects for the snapshot chain).
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
                DriverDirective::Maintenance(ms) => {
                    use crate::public_v1::domains::operations_ops::LongOperationOutcome;
                    let Ok((operation_id, _rev)) = ctx.store.begin_long_operation(
                        "instance.maintenance",
                        &json!({"kind": "instance"}),
                        true,
                    ) else {
                        // The seam write failed: the run itself still fails
                        // loudly instead of pretending the work happened.
                        fail_run_now(
                            &ctx.store,
                            &run_id,
                            "OPERATION_CREATE_FAILED",
                            &json!({"runId": run_id, "errorCode": "OPERATION_CREATE_FAILED"}),
                        );
                        return;
                    };
                    let _ = ctx.store.mark_operation_running(&operation_id);
                    let started = Instant::now();
                    let deadline = started + Duration::from_millis(ms);
                    let step = Duration::from_millis(20);
                    let mut next_progress = started;
                    while Instant::now() < deadline {
                        if *ctx.cancel_rx.borrow() {
                            // Run cancelled: the driven operation is cancelled
                            // with it (the driver owns its lifecycle window).
                            let _ = ctx.store.finish_long_operation(
                                &operation_id,
                                LongOperationOutcome::Cancelled,
                            );
                            cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                            return;
                        }
                        if ctx.store.operation_cancel_requested(&operation_id) == Ok(true) {
                            // `operations.cancel` fired for the operation:
                            // the operation terminalizes as cancelled, the
                            // run itself completes normally (they are
                            // independent lifecycles).
                            let _ = ctx.store.finish_long_operation(
                                &operation_id,
                                LongOperationOutcome::Cancelled,
                            );
                            succeed_run_now(
                                &ctx.store,
                                &run_id,
                                &json!({
                                    "runId": run_id,
                                    "maintenanceOperationId": operation_id,
                                    "maintenanceCancelled": true,
                                }),
                            );
                            return;
                        }
                        if Instant::now() >= next_progress {
                            let _ = ctx.store.record_operation_progress(
                                &operation_id,
                                "elapsedMs",
                                &json!(
                                    Instant::now()
                                        .saturating_duration_since(started)
                                        .as_millis() as u64
                                ),
                            );
                            next_progress = Instant::now() + Duration::from_millis(100);
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(step) => {}
                            _ = ctx.cancel_rx.changed() => {}
                        }
                    }
                    let _ = ctx.store.finish_long_operation(
                        &operation_id,
                        LongOperationOutcome::Succeeded {
                            result: json!({
                                "maintenance": "instance",
                                "durationMs": ms,
                            }),
                            result_schema_id: Some("agentcpm-pd.maintenance.result/1".to_owned()),
                        },
                    );
                    succeed_run_now(
                        &ctx.store,
                        &run_id,
                        &json!({"runId": run_id, "maintenanceOperationId": operation_id}),
                    );
                }
                DriverDirective::AwaitSteer => {
                    // run_wait 同款「订阅-再查-再等」循环：先登记 notified 再
                    // 查表，steer.submit 提交后的 state_notifier().notify_waiters()
                    // 不会丢唤醒。
                    let notifier = ctx.store.state_notifier();
                    loop {
                        let notified = notifier.notified();
                        let taken = match ctx.store.take_pending_steer(&run_id) {
                            Ok(steer) => steer,
                            Err(_) => {
                                // 消费原语写失败必须响亮收场（approval 指令的
                                // INTERACTION_CREATE_FAILED 同款），不得静默
                                // 回退成 worker 的成功兜底。
                                fail_run_now(
                                    &ctx.store,
                                    &run_id,
                                    "STEER_CONSUME_FAILED",
                                    &json!({"runId": run_id, "errorCode": "STEER_CONSUME_FAILED"}),
                                );
                                return;
                            }
                        };
                        if let Some(steer) = taken {
                            let text = steer.text();
                            let _ = ctx.store.append_run_progress_event(
                                &run_id,
                                &ctx.run.session_id,
                                "run.output.delta",
                                &json!({"runId": run_id, "steerId": steer.steer_id, "text": text}),
                            );
                            succeed_run_now(
                                &ctx.store,
                                &run_id,
                                &json!({
                                    "runId": run_id,
                                    "steerId": steer.steer_id,
                                    "consumedSteer": true,
                                    "text": text,
                                }),
                            );
                            return;
                        }
                        if *ctx.cancel_rx.borrow() {
                            cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                            return;
                        }
                        tokio::select! {
                            _ = notified => {}
                            _ = ctx.cancel_rx.changed() => {}
                        }
                    }
                }
                DriverDirective::FireHook { hook_id } => {
                    // cli.task.hooks 的确定性触发桥：经 hooks 域的 fire 缝
                    // （fire_hook_for_run——生产 run 流水线的触发接线是
                    // runtime 任务的缝合点）创建绑定本 run 的 invocation，
                    // run 转 waiting，等回执提交或过期。
                    let fired = match crate::public_v1::domains::hooks::fire_hook_for_run(
                        &ctx.runtime,
                        &ctx.run,
                        &hook_id,
                    ) {
                        Ok(Some(fired)) => fired,
                        Ok(None) => {
                            fail_run_now(
                                &ctx.store,
                                &run_id,
                                "HOOK_BINDING_NOT_FOUND",
                                &json!({
                                    "runId": run_id,
                                    "errorCode": "HOOK_BINDING_NOT_FOUND",
                                    "hookId": hook_id,
                                }),
                            );
                            return;
                        }
                        Err(_error) => {
                            fail_run_now(
                                &ctx.store,
                                &run_id,
                                "HOOK_FIRE_FAILED",
                                &json!({
                                    "runId": run_id,
                                    "errorCode": "HOOK_FIRE_FAILED",
                                    "hookId": hook_id,
                                }),
                            );
                            return;
                        }
                    };
                    let _ = ctx.store.transition_run_state(
                        &run_id,
                        crate::public_v1::store::RunTransition::new(
                            agentcpm_pd_protocol::contract::RunState::Waiting,
                            "run.waiting",
                            &json!({
                                "runId": run_id,
                                "invocationId": fired.invocation_id,
                                "hookId": fired.hook_id,
                                "deadlineMs": fired.deadline_ms,
                            }),
                        ),
                    );
                    // The local wait bound; the authoritative expiry is the
                    // invocation row's expires_at (applied lazily on reads).
                    let wait_bound = tokio::time::Instant::now()
                        + Duration::from_millis(
                            u64::try_from(fired.deadline_ms).unwrap_or(60_000).max(1),
                        );
                    let notifier = ctx.store.state_notifier();
                    loop {
                        // 先登记 notified 再查表（AwaitSteer 同款防丢唤醒）。
                        let notified = notifier.notified();
                        if *ctx.cancel_rx.borrow() {
                            let _ = crate::public_v1::domains::hooks::cancel_invocation(
                                &ctx.runtime,
                                &fired.invocation_id,
                            );
                            cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                            return;
                        }
                        match crate::public_v1::domains::hooks::invocation_state(
                            &ctx.runtime,
                            &fired.invocation_id,
                        ) {
                            Ok(Some(state)) if state.state == "delivered" => {
                                // 回执送达：run 复活成功终态。合同面 `run.result`
                                // 只能是 Artifact|null（cli-v1 合同 Run 定义），
                                // hook 结果不经由此字段外发；它的可观察面是
                                // `hooks.submit_result` 落在 run 事件流里的
                                // `run.progress` 投影（payload.hookResult）。
                                let _ = ctx.store.transition_run_state(
                                    &run_id,
                                    crate::public_v1::store::RunTransition::new(
                                        agentcpm_pd_protocol::contract::RunState::Succeeded,
                                        "run.completed",
                                        &json!({
                                            "runId": run_id,
                                            "invocationId": fired.invocation_id,
                                            "hookId": fired.hook_id,
                                        }),
                                    )
                                    .outcome_code("SUCCEEDED"),
                                );
                                return;
                            }
                            Ok(Some(state)) if state.state == "expired" => {
                                // 过期回执不能复活 run。
                                fail_run_now(
                                    &ctx.store,
                                    &run_id,
                                    "HOOK_INVOCATION_EXPIRED",
                                    &json!({
                                        "runId": run_id,
                                        "errorCode": "HOOK_INVOCATION_EXPIRED",
                                        "invocationId": fired.invocation_id,
                                    }),
                                );
                                return;
                            }
                            Ok(Some(state)) if state.state == "cancelled" => {
                                fail_run_now(
                                    &ctx.store,
                                    &run_id,
                                    "HOOK_INVOCATION_CANCELLED",
                                    &json!({
                                        "runId": run_id,
                                        "errorCode": "HOOK_INVOCATION_CANCELLED",
                                        "invocationId": fired.invocation_id,
                                    }),
                                );
                                return;
                            }
                            Ok(Some(_)) => {}
                            Ok(None) | Err(_) => {
                                fail_run_now(
                                    &ctx.store,
                                    &run_id,
                                    "HOOK_INVOCATION_VANISHED",
                                    &json!({
                                        "runId": run_id,
                                        "errorCode": "HOOK_INVOCATION_VANISHED",
                                        "invocationId": fired.invocation_id,
                                    }),
                                );
                                return;
                            }
                        }
                        if tokio::time::Instant::now() >= wait_bound {
                            // 到期未回执：invocation 转过期（谁先到谁赢——读
                            // 侧/提交侧的惰性过期是同一原子迁移）。
                            let _ = crate::public_v1::domains::hooks::expire_invocation(
                                &ctx.runtime,
                                &fired.invocation_id,
                            );
                            fail_run_now(
                                &ctx.store,
                                &run_id,
                                "HOOK_INVOCATION_EXPIRED",
                                &json!({
                                    "runId": run_id,
                                    "errorCode": "HOOK_INVOCATION_EXPIRED",
                                    "invocationId": fired.invocation_id,
                                }),
                            );
                            return;
                        }
                        let step = wait_bound
                            .saturating_duration_since(tokio::time::Instant::now())
                            .min(Duration::from_millis(50));
                        let step = if step.is_zero() {
                            Duration::from_millis(5)
                        } else {
                            step
                        };
                        tokio::select! {
                            _ = notified => {}
                            _ = ctx.cancel_rx.changed() => {}
                            _ = tokio::time::sleep(step) => {}
                        }
                    }
                }
                DriverDirective::Writes(writes) => {
                    // A5 (snap-wire §5): each instruction goes through the
                    // files domain's production write path. The worker already
                    // attributes file writes in this project to the run (it
                    // set the active write attribution before `execute`), so
                    // the capture-ledger rows carry this run's id and the
                    // boundary aggregation turns them into one run-owned
                    // snapshot once the run reaches a terminal state.
                    let project_id = ctx.run.project_id.clone();
                    let write_count = writes.len();
                    for write in writes {
                        let write = match write {
                            Ok(write) => write,
                            Err(raw) => {
                                // A malformed directive must not silently
                                // degrade into success: the prompt's intent
                                // is durable side effects (the P2 review
                                // discipline of the approval/steer arms).
                                fail_run_now(
                                    &ctx.store,
                                    &run_id,
                                    "FILE_WRITE_FAILED",
                                    &json!({
                                        "runId": run_id,
                                        "errorCode": "FILE_WRITE_FAILED",
                                        "message": "malformed [pd-driver:write=<relpath>@<content>] directive",
                                        "directive": raw,
                                    }),
                                );
                                return;
                            }
                        };
                        if *ctx.cancel_rx.borrow() {
                            cancel_run_now(&ctx.store, &run_id, &json!({"runId": run_id}));
                            return;
                        }
                        if let Err(error) = crate::public_v1::domains::files::driver_write_file(
                            &ctx.store,
                            &ctx.runtime,
                            &project_id,
                            write.path.as_bytes(),
                            write.content.as_bytes(),
                        ) {
                            fail_run_now(
                                &ctx.store,
                                &run_id,
                                "FILE_WRITE_FAILED",
                                &json!({
                                    "runId": run_id,
                                    "errorCode": "FILE_WRITE_FAILED",
                                    "path": write.path,
                                    "cause": error.code,
                                    "message": error.message,
                                }),
                            );
                            return;
                        }
                    }
                    let _ = ctx.store.append_run_progress_event(
                        &run_id,
                        &ctx.run.session_id,
                        "run.output.delta",
                        &json!({
                            "runId": run_id,
                            "text": format!("wrote {write_count} file(s)"),
                        }),
                    );
                    succeed_run_now(
                        &ctx.store,
                        &run_id,
                        &json!({"runId": run_id, "wroteFiles": write_count}),
                    );
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
    /// Drives one instance-scoped maintenance operation through the
    /// collections seam (`operations` table) for the duration, then
    /// completes the run. The operation is cancellable through
    /// `operations.cancel` independently of the run's own cancellation.
    Maintenance(u64),
    /// t009 steer seam: the run suspends until a `steer.submit` input
    /// arrives for it (`steers` table), consumes it through
    /// `V1Store::take_pending_steer`, projects the steer text as a
    /// progress event, and completes. Cancellable like every other
    /// directive.
    AwaitSteer,
    /// cli.task.hooks trigger bridge: fires the session's bound hook through
    /// the hooks domain's `fire_hook_for_run` seam, moves the run to
    /// `waiting`, and completes with the submitted hook result (or fails on
    /// expiry / cancellation). A hook whose binding is missing or disabled
    /// fails the run (`HOOK_BINDING_NOT_FOUND`).
    FireHook {
        hook_id: String,
    },
    /// A5 (snap-wire §5): ordered file writes, one per
    /// `[pd-driver:write=<relpath>@<literal>]` instruction in the prompt.
    /// Each goes through the files domain's production write path (the same
    /// `write_file` + capture-ledger registration a `files.write` request
    /// performs), so a run can leave real file side effects that the worker
    /// aggregates into one run-owned snapshot at the run boundary. `Err`
    /// carries a malformed directive body verbatim (the run fails on it
    /// instead of silently skipping durable side effects).
    Writes(Vec<Result<DriverFileWrite, String>>),
}

/// One `[pd-driver:write=<relpath>@<content>]` instruction: `relpath` is a
/// project-relative path (trimmed), `content` the literal bytes up to the
/// closing `]` (kept verbatim — no trimming).
struct DriverFileWrite {
    path: String,
    content: String,
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
    // A5 (snap-wire §5): collect every write instruction first — a prompt
    // carrying any `[pd-driver:write=...]` is a write prompt; the other
    // directives stay standalone markers.
    let writes = parse_driver_writes(text);
    if !writes.is_empty() {
        return DriverDirective::Writes(writes);
    }
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
    if let Some(rest) = text.split("[pd-driver:maintenance=").nth(1)
        && let Some(num) = rest.split(']').next()
        && let Ok(ms) = num.trim().parse::<u64>()
    {
        // Bounded like `sleep` so a typo cannot pin the run forever.
        return DriverDirective::Maintenance(ms.clamp(1, 600_000));
    }
    if text.contains("[pd-driver:await_steer]") {
        // t009: deterministic steer consumption — the run holds until a
        // `steer.submit` arrives (or run.cancel fires), then completes.
        return DriverDirective::AwaitSteer;
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
    if let Some(rest) = text.split("[pd-driver:fire-hook=").nth(1)
        && let Some(hook_id) = rest.split(']').next()
        && !hook_id.trim().is_empty()
    {
        // cli.task.hooks: deterministic hook firing through the hooks
        // domain's fire seam.
        return DriverDirective::FireHook {
            hook_id: hook_id.trim().to_owned(),
        };
    }
    DriverDirective::Sleep(25)
}

/// Collects every `[pd-driver:write=<relpath>@<content>]` instruction in
/// prompt order (A5, snap-wire §5). The body runs to the first `]` (content
/// therefore cannot contain `]`); `relpath` may not contain `@` and is
/// trimmed, `content` is kept verbatim. A body without an `@` separator or
/// with an empty path is returned as `Err(raw-body)` so the run fails loudly
/// instead of silently dropping a durable side effect.
fn parse_driver_writes(text: &str) -> Vec<Result<DriverFileWrite, String>> {
    const MARKER: &str = "[pd-driver:write=";
    let mut writes = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(MARKER) {
        let after = &rest[start + MARKER.len()..];
        let (body, next) = match after.find(']') {
            Some(end) => (&after[..end], &after[end + 1..]),
            // Unterminated directive: treat the remainder as the body (the
            // lenient `split(']').next()` convention of the other parsers).
            None => (after, ""),
        };
        writes.push(match body.split_once('@') {
            Some((path, content)) if !path.trim().is_empty() => Ok(DriverFileWrite {
                path: path.trim().to_owned(),
                content: content.to_owned(),
            }),
            _ => Err(body.to_owned()),
        });
        rest = next;
    }
    writes
}
