//! Regression tests for the two pre-commit fixes flagged by the WP4 audit
//! (`state/cli-v1-audit-20261117.md` §2.3): the `V1Store::resume_queue`
//! self-deadlock and the `-32004 CAPABILITY_UNAVAILABLE` dispatch gate
//! ordering. Each test reproduces the original failure mode, so reintroducing
//! either bug fails (or, for the deadlock, hangs and times out) here.

use std::time::Duration;

use serde_json::json;

use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{InstanceMeta, V1Store};
use crate::public_v1::test_support::TempDir;

fn open_runtime(root_dir: &std::path::Path) -> (std::sync::Arc<PublicV1Runtime>, String) {
    let (file, _token) = load_local_instance_and_token(root_dir, None).unwrap();
    let meta = InstanceMeta {
        instance_id: file.instance_id.clone(),
        owner_id: file.owner_id.clone(),
        product: file.product.clone(),
        protocol_version: file.protocol_version.clone(),
        node_id: file.node_id.clone(),
        generation: "gen-test-1".to_owned(),
        server_revision: "srv-test-1".to_owned(),
        initialization_revision: file.initialization_revision.clone(),
    };
    let store = V1Store::open_and_recover(root_dir, meta).unwrap();
    let rt = PublicV1Runtime::new(store);
    (rt, file.instance_id.clone())
}

/// `V1Store::resume_queue` used to re-lock `self.conn` (via
/// `build_queue_json(self.conn.lock()...)`) while the guard taken at function
/// entry was still alive, self-deadlocking the store mutex forever. The fix
/// reuses the held guard and drops it before notifying. A regression would
/// hang this test's worker thread until the watchdog below fires.
#[tokio::test]
async fn queue_resume_returns_after_a_failing_run_without_self_deadlock() {
    let tmp = TempDir::new("resume-deadlock");
    let root_dir = tmp.0.join("pd-home");
    initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
    let (rt, iid) = open_runtime(&root_dir);

    // Project and session for the run.
    rt.dispatch(
        "project.register",
        json!({
            "instanceId": iid,
            "projectId": "proj-r",
            "name": "Regression",
            "environmentId": "local",
            "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
        }),
        None,
        None,
    )
    .await
    .unwrap();
    rt.dispatch(
        "session.create",
        json!({"instanceId": iid, "sessionId": "sess-r", "projectId": "proj-r"}),
        None,
        None,
    )
    .await
    .unwrap();

    // A failing run leaves the session queue paused (`run_failed`).
    let (_, submitted) = rt
        .dispatch(
            "run.submit",
            json!({"instanceId": iid, "sessionId": "sess-r", "input": [{"kind": "text", "text": "[pd-driver:fail]"}]}),
            Some("key-resume-deadlock-1"),
            None,
        )
        .await
        .unwrap();
    let run_id = submitted["runId"].as_str().unwrap().to_owned();
    let (_, waited) = rt
        .dispatch(
            "run.wait",
            json!({"instanceId": iid, "runId": run_id, "timeoutMs": 5000}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(waited["run"]["state"], "failed");

    let (_, queue) = rt
        .dispatch(
            "queue.get",
            json!({"instanceId": iid, "sessionId": "sess-r"}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        queue["queue"]["paused"], true,
        "failed run must pause queue"
    );
    let revision = queue["queue"]["revision"].as_str().unwrap().to_owned();
    let reason = queue["queue"]["pauseReason"].as_str().unwrap().to_owned();

    // `queue.resume` runs on its own thread with a watchdog: the old code
    // blocked forever inside `resume_queue` after committing the un-pause.
    let (tx, rx) = std::sync::mpsc::channel();
    let resume_rt = rt.clone();
    let resume_input = json!({
        "instanceId": iid,
        "sessionId": "sess-r",
        "expectedRevision": revision,
        "acknowledgedPauseReason": reason,
    });
    std::thread::Builder::new()
        .name("resume-queue-watchdog".into())
        .spawn(move || {
            let worker = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = worker.block_on(async {
                resume_rt
                    .dispatch("queue.resume", resume_input, None, None)
                    .await
            });
            let _ = tx.send(result);
        })
        .unwrap();
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok((_, resumed))) => {
            assert_eq!(resumed["queue"]["paused"], false);
            assert_eq!(resumed["queue"]["pauseReason"], serde_json::Value::Null);
        }
        Ok(Err(error)) => panic!("queue.resume failed: {error:?}"),
        Err(_) => panic!(
            "V1Store::resume_queue self-deadlocked: no result within 15s \
             (store mutex re-acquired while still held)"
        ),
    }
}

/// The `-32004 CAPABILITY_UNAVAILABLE` gate for future-round operations must
/// fire before the idempotency-key check (`-32086`) and before input schema
/// validation. A regression would surface `-32086` (missing key) or a
/// validation error instead of the capability error for `operations.cancel`
/// (future round, `execution: sync`, `retry: idempotentSubmit`).
#[tokio::test]
async fn capability_unavailable_gate_precedes_idempotency_and_validation() {
    let tmp = TempDir::new("gate-order");
    let root_dir = tmp.0.join("pd-home");
    initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
    let (rt, iid) = open_runtime(&root_dir);

    // Missing idempotency key: the pre-gate must answer -32004, not -32086.
    let missing_key = rt
        .dispatch(
            "operations.cancel",
            json!({"instanceId": iid, "operationId": "op-1"}),
            None,
            None,
        )
        .await
        .unwrap_err();
    match &missing_key {
        DispatchError::Rpc { code: -32004, .. } => {}
        other => panic!("expected -32004 before the -32086 key check, got {other:?}"),
    }

    // Key present but the input fails schema validation (missing required
    // `operationId`): still -32004, not an INVALID_ARGUMENT validation error.
    let invalid_input = rt
        .dispatch(
            "operations.cancel",
            json!({"instanceId": iid}),
            Some("key-gate-order-1"),
            None,
        )
        .await
        .unwrap_err();
    match &invalid_input {
        DispatchError::Rpc { code: -32004, .. } => {}
        other => panic!("expected -32004 before input validation, got {other:?}"),
    }
}

fn seed_project_session(store: &std::sync::Arc<V1Store>, iid: &str, project: &str, session: &str) {
    store
        .register_project(&json!({
            "instanceId": iid,
            "projectId": project,
            "name": "Regression",
            "environmentId": "local",
            "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
        }))
        .unwrap();
    store
        .create_session(&json!({"instanceId": iid, "sessionId": session, "projectId": project}))
        .unwrap();
}

/// P1 (wp4 review): `queue.resume` must pin the resumed head run as the
/// session's active run (inside the resume transaction), and a `run.submit`
/// while it executes must queue instead of starting a second concurrent run.
/// Each run is then driven exactly once (single `run.started`).
#[tokio::test]
async fn queue_resume_pins_active_run_and_serializes_late_submits() {
    let tmp = TempDir::new("resume-serial");
    let root_dir = tmp.0.join("pd-home");
    initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
    let (file, _token) = load_local_instance_and_token(&root_dir, None).unwrap();
    let iid = file.instance_id.clone();
    let meta = InstanceMeta {
        instance_id: file.instance_id.clone(),
        owner_id: file.owner_id.clone(),
        product: file.product.clone(),
        protocol_version: file.protocol_version.clone(),
        node_id: file.node_id.clone(),
        generation: "gen-test-1".to_owned(),
        server_revision: "srv-test-1".to_owned(),
        initialization_revision: file.initialization_revision.clone(),
    };

    // Build the crash-recovery shape with the store directly (no workers):
    // run A running, run B queued behind it, then A fails and pauses.
    {
        let store = V1Store::open_and_recover(&root_dir, meta.clone()).unwrap();
        seed_project_session(&store, &iid, "proj-rs", "sess-rs");
        let a = match store
            .submit_run(
                &json!({"instanceId": iid, "sessionId": "sess-rs", "input": [{"kind": "text", "text": "[pd-driver:sleep=60000]"}]}),
                Some("key-rs-a"),
            )
            .unwrap()
        {
            crate::public_v1::store::SubmitDisposition::NewRun { run_id, .. } => run_id,
            other => panic!("expected a new run, got {other:?}"),
        };
        store
            .transition_run_state(
                &a,
                crate::public_v1::store::RunTransition::new(
                    agentcpm_pd_protocol::contract::RunState::Running,
                    "run.started",
                    &json!({"runId": a}),
                ),
            )
            .unwrap();
        let b = match store
            .submit_run(
                &json!({"instanceId": iid, "sessionId": "sess-rs", "input": [{"kind": "text", "text": "[pd-driver:sleep=500]"}]}),
                Some("key-rs-b"),
            )
            .unwrap()
        {
            crate::public_v1::store::SubmitDisposition::NewRun { run_id, .. } => run_id,
            other => panic!("expected a new run, got {other:?}"),
        };
        store
            .transition_run_state(
                &a,
                crate::public_v1::store::RunTransition::new(
                    agentcpm_pd_protocol::contract::RunState::Failed,
                    "run.failed",
                    &json!({"runId": a}),
                )
                .outcome_code("DRIVER_INJECTED_FAILURE")
                .pause_session_reason("run_failed"),
            )
            .unwrap();
        assert_eq!(
            store.get_run(&b).unwrap().state,
            agentcpm_pd_protocol::contract::RunState::Queued
        );
    }

    // "Restart": a fresh runtime over the recovered store.
    let (rt, iid2) = open_runtime(&root_dir);
    assert_eq!(iid, iid2);
    let (_, queue) = rt
        .dispatch(
            "queue.get",
            json!({"instanceId": iid, "sessionId": "sess-rs"}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(queue["queue"]["paused"], true);
    let b_id = queue["queue"]["items"][0]["runId"]
        .as_str()
        .unwrap()
        .to_owned();

    let (_, resumed) = rt
        .dispatch(
            "queue.resume",
            json!({
                "instanceId": iid,
                "sessionId": "sess-rs",
                "expectedRevision": queue["queue"]["revision"],
                "acknowledgedPauseReason": queue["queue"]["pauseReason"],
            }),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(resumed["queue"]["paused"], false);

    // The resumed run is pinned as the session's active run...
    let (_, session) = rt
        .dispatch(
            "session.get",
            json!({"instanceId": iid, "sessionId": "sess-rs"}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(session["session"]["activeRunId"], b_id.as_str());

    // ...so a submit during its execution stays queued (no concurrent run).
    let (_, submitted) = rt
        .dispatch(
            "run.submit",
            json!({"instanceId": iid, "sessionId": "sess-rs", "input": [{"kind": "text", "text": "[pd-driver:sleep=50]"}]}),
            Some("key-rs-c"),
            None,
        )
        .await
        .unwrap();
    let c_id = submitted["runId"].as_str().unwrap().to_owned();
    let (_, c_get) = rt
        .dispatch(
            "run.get",
            json!({"instanceId": iid, "runId": c_id}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        c_get["run"]["state"], "queued",
        "a submit during the resumed run must not start concurrently"
    );

    // The resumed run then the queued run are each driven exactly once.
    for run_id in [&b_id, &c_id] {
        let (_, waited) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run_id, "timeoutMs": 5000}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(waited["run"]["state"], "succeeded", "run {run_id}");
        let (_, events) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        let started = events["frame"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "run.started")
            .count();
        assert_eq!(started, 1, "run {run_id} must be driven exactly once");
    }
}

/// P1 (wp4 review): a competing worker for a run that is already being driven
/// must lose the atomic claim and skip it — the run's lifecycle events are
/// emitted exactly once.
#[tokio::test]
async fn a_competing_worker_claims_nothing_and_the_run_is_driven_once() {
    let tmp = TempDir::new("claim-once");
    let root_dir = tmp.0.join("pd-home");
    initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
    let (rt, iid) = open_runtime(&root_dir);

    rt.dispatch(
        "project.register",
        json!({
            "instanceId": iid,
            "projectId": "proj-claim",
            "name": "Claim",
            "environmentId": "local",
            "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
        }),
        None,
        None,
    )
    .await
    .unwrap();
    rt.dispatch(
        "session.create",
        json!({"instanceId": iid, "sessionId": "sess-claim", "projectId": "proj-claim"}),
        None,
        None,
    )
    .await
    .unwrap();

    let (_, submitted) = rt
        .dispatch(
            "run.submit",
            json!({"instanceId": iid, "sessionId": "sess-claim", "input": [{"kind": "text", "text": "[pd-driver:sleep=2000]"}]}),
            Some("key-claim-1"),
            None,
        )
        .await
        .unwrap();
    let run_id = submitted["runId"].as_str().unwrap().to_owned();

    // Wait until the first worker owns the run, then start a competing worker.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (_, got) = rt
                .dispatch(
                    "run.get",
                    json!({"instanceId": iid, "runId": run_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if got["run"]["state"] == "running" {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first worker started the run");
    rt.start_session_worker(run_id.clone());

    let (_, waited) = rt
        .dispatch(
            "run.wait",
            json!({"instanceId": iid, "runId": run_id, "timeoutMs": 5000}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(waited["run"]["state"], "succeeded");

    let (_, events) = rt
        .dispatch(
            "run.events",
            json!({"instanceId": iid, "runId": run_id}),
            None,
            None,
        )
        .await
        .unwrap();
    let event_list = events["frame"]["events"].as_array().unwrap().clone();
    let started = event_list
        .iter()
        .filter(|event| event["type"] == "run.started")
        .count();
    let completed = event_list
        .iter()
        .filter(|event| event["type"] == "run.completed")
        .count();
    assert_eq!(started, 1, "run.started emitted exactly once");
    assert_eq!(completed, 1, "run.completed emitted exactly once");
}

/// P2 (wp4 review): cancelling a waiting (interaction-paused) run used to let
/// the offline driver append a second terminal `run.cancelled` event after
/// the durable one. The terminal-state guard rejects that late write, so the
/// run ends with exactly one terminal event.
#[tokio::test]
async fn cancelling_a_waiting_run_emits_a_single_terminal_event() {
    let tmp = TempDir::new("cancel-waiting");
    let root_dir = tmp.0.join("pd-home");
    initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
    let (rt, iid) = open_runtime(&root_dir);

    rt.dispatch(
        "project.register",
        json!({
            "instanceId": iid,
            "projectId": "proj-cw",
            "name": "CancelWaiting",
            "environmentId": "local",
            "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
        }),
        None,
        None,
    )
    .await
    .unwrap();
    rt.dispatch(
        "session.create",
        json!({"instanceId": iid, "sessionId": "sess-cw", "projectId": "proj-cw"}),
        None,
        None,
    )
    .await
    .unwrap();

    let (_, submitted) = rt
        .dispatch(
            "run.submit",
            json!({"instanceId": iid, "sessionId": "sess-cw", "input": [{"kind": "text", "text": "[pd-driver:approval]"}]}),
            Some("key-cw-1"),
            None,
        )
        .await
        .unwrap();
    let run_id = submitted["runId"].as_str().unwrap().to_owned();

    // Wait for the interaction to be pending (the default `state` filter).
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (_, pending) = rt
                .dispatch(
                    "interaction.list",
                    json!({"instanceId": iid, "kind": "approval"}),
                    None,
                    None,
                )
                .await
                .unwrap();
            if pending["items"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("approval interaction became pending");

    let (_, cancelled) = rt
        .dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": run_id}),
            Some("key-cw-cancel-1"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(cancelled["receipt"]["state"], "cancelled");

    let (_, waited) = rt
        .dispatch(
            "run.wait",
            json!({"instanceId": iid, "runId": run_id, "timeoutMs": 5000}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(waited["run"]["state"], "cancelled");

    // Let the parked driver observe the cancel signal: without the terminal
    // guard it wakes and appends a second `run.cancelled` event.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let (_, events) = rt
        .dispatch(
            "run.events",
            json!({"instanceId": iid, "runId": run_id}),
            None,
            None,
        )
        .await
        .unwrap();
    let event_list = events["frame"]["events"].as_array().unwrap().clone();
    let cancelled_events = event_list
        .iter()
        .filter(|event| event["type"] == "run.cancelled")
        .count();
    assert_eq!(
        cancelled_events, 1,
        "the durable cancel is the only terminal event (late driver writes are rejected)"
    );
    assert_eq!(
        event_list.last().unwrap()["type"],
        "run.cancelled",
        "the terminal event is the last event of the stream"
    );
}

/// P1 (wp4 review): every `run.snapshot` section's inline `data` stays within
/// the contract's 256 KiB budget; oversized history tails are truncated with
/// a `nextCursor`, and the state section sheds bulky fields.
#[tokio::test]
async fn run_snapshot_sections_respect_the_256k_budget() {
    let tmp = TempDir::new("snapshot-cap");
    let root_dir = tmp.0.join("pd-home");
    initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
    let (file, _token) = load_local_instance_and_token(&root_dir, None).unwrap();
    let iid = file.instance_id.clone();
    let meta = InstanceMeta {
        instance_id: file.instance_id.clone(),
        owner_id: file.owner_id.clone(),
        product: file.product.clone(),
        protocol_version: file.protocol_version.clone(),
        node_id: file.node_id.clone(),
        generation: "gen-test-1".to_owned(),
        server_revision: "srv-test-1".to_owned(),
        initialization_revision: file.initialization_revision.clone(),
    };

    let big = "x".repeat(600_000);
    let store = V1Store::open_and_recover(&root_dir, meta).unwrap();
    seed_project_session(&store, &iid, "proj-snap", "sess-snap");
    let run_id = match store
        .submit_run(
            &json!({"instanceId": iid, "sessionId": "sess-snap", "input": [{"kind": "text", "text": "snapshot budget"}]}),
            Some("key-snap-1"),
        )
        .unwrap()
    {
        crate::public_v1::store::SubmitDisposition::NewRun { run_id, .. } => run_id,
        other => panic!("expected a new run, got {other:?}"),
    };
    store
        .transition_run_state(
            &run_id,
            crate::public_v1::store::RunTransition::new(
                agentcpm_pd_protocol::contract::RunState::Running,
                "run.started",
                &json!({"runId": run_id}),
            ),
        )
        .unwrap();
    store
        .append_run_progress_event(
            &run_id,
            "sess-snap",
            "run.progress",
            &json!({"runId": run_id, "delta": big}),
        )
        .unwrap();
    let big_result = json!({"runId": run_id, "blob": big});
    store
        .transition_run_state(
            &run_id,
            crate::public_v1::store::RunTransition::new(
                agentcpm_pd_protocol::contract::RunState::Succeeded,
                "run.completed",
                &json!({"runId": run_id}),
            )
            .outcome_code("SUCCEEDED")
            .result_artifact(&big_result),
        )
        .unwrap();
    drop(store);

    let (rt, iid2) = open_runtime(&root_dir);
    assert_eq!(iid, iid2);
    let (_, snapshot) = rt
        .dispatch(
            "run.snapshot",
            json!({"instanceId": iid, "runId": run_id}),
            None,
            None,
        )
        .await
        .unwrap();

    let sections = snapshot["snapshot"]["sections"].as_array().unwrap();
    assert_eq!(sections.len(), 3);
    for section in sections {
        let data_bytes = serde_json::to_vec(&section["data"]).unwrap();
        assert!(
            data_bytes.len() <= 256 * 1024,
            "section {} data is {} bytes (budget 256 KiB)",
            section["name"],
            data_bytes.len()
        );
    }
    let history = &sections
        .iter()
        .find(|section| section["name"] == "history")
        .unwrap()["data"];
    assert_eq!(
        history["truncated"], true,
        "the oversized tail is truncated"
    );
    let history_events = history["events"].as_array().unwrap();
    assert_eq!(
        history_events.len(),
        2,
        "the small prefix (run.queued, run.started) survives; the 600 KiB progress event and everything after it are cut"
    );
    assert!(
        history["nextCursor"].as_str().is_some(),
        "the truncation names the resume cursor"
    );
    let state = &sections
        .iter()
        .find(|section| section["name"] == "state")
        .unwrap()["data"];
    assert_eq!(state["truncated"], true);
    assert_eq!(state["run"]["result"], serde_json::Value::Null);
}

/// P2 (wp4 review): the store's run-state string tables must stay identical
/// to the protocol enum so new variants cannot silently drift into `failed`.
#[test]
fn run_state_tables_match_the_protocol_enum() {
    use agentcpm_pd_protocol::contract::RunState;
    let states = [
        RunState::Queued,
        RunState::Running,
        RunState::Waiting,
        RunState::Cancelling,
        RunState::Succeeded,
        RunState::Failed,
        RunState::Cancelled,
        RunState::Interrupted,
        RunState::ResultUnknown,
    ];
    for state in states {
        let text = crate::public_v1::store::run_state_str(state);
        assert_eq!(text, state.as_str());
        assert_eq!(
            crate::public_v1::store::parse_run_state(text),
            state,
            "round trip through the SQL string table"
        );
        assert_eq!(
            crate::public_v1::store::is_terminal_run_state(state),
            state.is_terminal()
        );
    }
}
