//! `run.*` — submission, observation, cancellation, snapshots, and events.

use std::sync::Arc;
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{EventsQueryResult, is_terminal_run_state};

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("run.submit", |rt, input, key, _rev| {
        Box::pin(run_submit(rt, input, key))
    });
    reg.handler("run.get", |rt, input, _key, _rev| {
        Box::pin(run_get(rt, input))
    });
    reg.handler("run.list", |rt, input, _key, _rev| {
        Box::pin(run_list(rt, input))
    });
    reg.handler("run.wait", |rt, input, _key, _rev| {
        Box::pin(run_wait(rt, input))
    });
    reg.handler("run.events", |rt, input, _key, _rev| {
        Box::pin(run_events(rt, input))
    });
    reg.handler("run.snapshot", |rt, input, _key, _rev| {
        Box::pin(run_snapshot(rt, input))
    });
    reg.handler("run.cancel", |rt, input, key, _rev| {
        Box::pin(run_cancel(rt, input, key))
    });
}

async fn run_submit(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let disposition =
        tokio::task::spawn_blocking(move || store.submit_run(&input, idempotency_key.as_deref()))
            .await
            .map_err(|e| DispatchError::Application(internal_err(e)))??;
    match disposition {
        crate::public_v1::store::SubmitDisposition::Deduplicated { receipt } => {
            Ok((ResponseOutcome::Accepted, receipt))
        }
        crate::public_v1::store::SubmitDisposition::NewRun {
            receipt,
            run_id,
            start_immediately,
            ..
        } => {
            if start_immediately {
                rt.start_session_worker(run_id);
            }
            Ok((ResponseOutcome::Accepted, receipt))
        }
    }
}

async fn run_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let run_id = input["runId"].as_str().unwrap_or_default().to_owned();
    let run = tokio::task::spawn_blocking(move || store.get_run(&run_id))
        .await
        .map_err(|e| DispatchError::Application(internal_err(e)))??;
    let snap_cursor = run.final_cursor.clone();
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "run": run.to_contract_json(),
            "snapshotCursor": snap_cursor,
        }),
    ))
}

async fn run_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let states = input.get("states").and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    });
    let store = rt.store().clone();
    let scope = input.get("scope").cloned();
    let page = input.get("page").cloned();
    let res = tokio::task::spawn_blocking(move || {
        store.list_runs(scope.as_ref(), states.as_ref(), page.as_ref())
    })
    .await
    .map_err(|e| DispatchError::Application(internal_err(e)))??;
    Ok((ResponseOutcome::Succeeded, res))
}

async fn run_wait(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let run_id = input["runId"].as_str().unwrap_or_default().to_owned();
    let timeout_ms = input
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(agentcpm_pd_protocol::wire::DEFAULT_UNARY_TIMEOUT_MS);
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(timeout_ms))
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(24 * 60 * 60));
    let notifier = rt.store().state_notifier();
    loop {
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let store = rt.store().clone();
        let rid = run_id.clone();
        let run = tokio::task::spawn_blocking(move || store.get_run(&rid))
            .await
            .map_err(|e| DispatchError::Application(internal_err(e)))??;
        if is_terminal_run_state(run.state) {
            return Ok((
                ResponseOutcome::Succeeded,
                json!({
                    "conditionMet": true,
                    "timedOut": false,
                    "run": run.to_contract_json(),
                }),
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok((
                ResponseOutcome::Succeeded,
                json!({
                    "conditionMet": false,
                    "timedOut": true,
                    "run": run.to_contract_json(),
                }),
            ));
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::select! {
            _ = notified => {}
            _ = tokio::time::sleep(remaining) => {}
        }
    }
}

async fn run_events(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let run_id = input["runId"].as_str().unwrap_or_default().to_owned();
    let after = input
        .get("afterCursor")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let limit = input
        .get("page")
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let store = rt.store().clone();
    let res = tokio::task::spawn_blocking(move || {
        store.query_run_events_page(&run_id, after.as_deref(), limit)
    })
    .await
    .map_err(|e| DispatchError::Application(internal_err(e)))??;
    let val = match res {
        EventsQueryResult::Page(p) => p,
        EventsQueryResult::Gap(g) => g,
    };
    Ok((ResponseOutcome::Succeeded, val))
}

async fn run_snapshot(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let run_id = input["runId"].as_str().unwrap_or_default().to_owned();
    let store = rt.store().clone();
    let snap = tokio::task::spawn_blocking(move || store.build_run_snapshot(&run_id))
        .await
        .map_err(|e| DispatchError::Application(internal_err(e)))??;
    Ok((ResponseOutcome::Succeeded, snap))
}

async fn run_cancel(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let run_id = input["runId"].as_str().unwrap_or_default().to_owned();
    let store = rt.store().clone();
    let rid = run_id.clone();
    let outcome =
        tokio::task::spawn_blocking(move || store.cancel_run(&rid, idempotency_key.as_deref()))
            .await
            .map_err(|e| DispatchError::Application(internal_err(e)))??;
    if outcome.should_notify_worker {
        rt.signal_run_cancel(&run_id);
    }
    Ok((ResponseOutcome::Succeeded, outcome.receipt))
}

fn internal_err(err: tokio::task::JoinError) -> agentcpm_pd_protocol::errors::ContractError {
    agentcpm_pd_protocol::errors::ContractError::new(
        agentcpm_pd_protocol::errors::CODE_UNAVAILABLE,
        format!("blocking run task failed: {err}"),
        agentcpm_pd_protocol::errors::RetrySpec::backoff_read(Some(250)),
        agentcpm_pd_protocol::errors::EffectStatus::Unknown,
    )
}
