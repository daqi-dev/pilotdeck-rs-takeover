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
