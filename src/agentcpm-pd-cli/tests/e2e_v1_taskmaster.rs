//! Taskmaster + notifications.events end-to-end suite (t019 T3).
//!
//! Every scenario drives the real `agentcpm-pd` binary over the real
//! `/v1/ws` transport (embedded gateway auto-start). The taskmaster chain
//! runs against a real seeded workspace (t012's `directories create` +
//! `projects register`, the files-suite pattern); the notifications
//! scenario derives its events from real offline-driver runs. The
//! deterministic notification source needs no mock trigger: a
//! `[pd-driver:fail]` run produces the `error` family, a plain run the
//! `stop` family (see the gateway domain's module doc for the derivation).
//!
//! Local verification status: this suite was written under the 2026-11-18
//! build freeze (no local cargo build); it is delivered for the
//! integrator's cb8 run — see the task report.

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common_v1::{
    CliOutcome, ConnectSession, TestHome, await_cli, expect_ok, init_home, init_home_instance,
    wait_run,
};

const INIT_SCHEMA_ID: &str = "agentcpm-pd.taskmaster.init/1";

/// Parses a failed envelope (`outcome: "failed"`) and returns its
/// `error.code`; also asserts the invocation exited non-zero.
fn expect_failed(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must exit non-zero on failure");
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` envelope: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

async fn _cli_json(root: &Path, args: &[&str]) -> Value {
    let out = await_cli(root, args);
        let method = if args.len() > 1 && !args[1].starts_with('-') {
            format!("{}.{}", args[0].replace('-', "_"), args[1].replace('-', "_"))
        } else {
            args[0].replace('-', "_")
        };
    expect_ok(&method, &out)
}

/// Seeds a project whose authorized root is a real workspace directory
/// (the t013 files-suite pattern: `directories create` bootstraps the
/// default authorized root under `<home>/workspaces`).
async fn seed_workspace(tag: &str) -> (TestHome, PathBuf, String, String) {
    let home = TestHome::new(tag);
    init_home(home.path());
    let instance_id = init_home_instance(home.path());

    let workspace_rel = format!("{tag}-ws");
    let out = await_cli(
        home.path(),
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--path",
            &json!({
                "authorizedRootId": "default",
                "relativePath": {"encoding": "utf8", "value": workspace_rel},
            })
            .to_string(),
            "--request-key",
            &format!("dirs-{tag}"),
        ],
    );
    expect_ok("directories.create", &out);
    let workspace = home.path().join("workspaces").join(&workspace_rel);
    assert!(
        workspace.is_dir(),
        "workspace seeded: {}",
        workspace.display()
    );

    let project_id = format!("p-{tag}");
    let out = await_cli(
        home.path(),
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--name",
            "T019 T3 E2E",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"{workspace_rel}\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    (home, workspace, project_id, instance_id)
}

async fn seed_session(home: &Path, project_id: &str, session_id: &str) {
    let out = await_cli(
        home,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--project-id",
            project_id,
        ],
    );
    expect_ok("session.create", &out);
}

/// Polls `operations get` until the operation is terminal and returns the
/// operation object.
async fn wait_operation(home: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = await_cli(
            home,
            &[
                "operations",
                "get",
                "--output",
                "json",
                "--operation-id",
                operation_id,
            ],
        );
        let page = expect_ok("operations.get", &out);
        let state = page["operation"]["state"].as_str().unwrap_or_default();
        if matches!(
            state,
            "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
        ) {
            return page["operation"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "operation `{operation_id}` never became terminal (state `{state}`)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The full taskmaster chain: initialize → write PRD → read → template
/// generation → parse → task list → next → write task (all three modes) →
/// remove document, with the conditional-write gates on the way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn taskmaster_full_chain() {
    let (home, workspace, project_id, _instance_id) = seed_workspace("tm-chain").await;
    let root = home.path();

    // --- initialize: receipt first, completion through operations.get ---
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "initialize",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--schema-id",
            INIT_SCHEMA_ID,
            "--configuration",
            r#"{"rules":["default"]}"#,
            "--request-key",
            "tm-init-1",
        ],
    );
    let receipt = expect_ok("taskmaster.initialize", &out);
    assert_eq!(receipt["kind"], "operation");
    assert_eq!(receipt["durable"], true);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], "succeeded");
    let completion = &operation["result"];
    assert_eq!(completion["projectId"], project_id);
    assert_eq!(completion["initialized"], true);
    assert!(
        completion["createdRelativePaths"]
            .as_array()
            .is_some_and(|paths| paths.contains(&json!(".taskmaster/tasks/tasks.json")))
    );
    assert!(completion["configurationRevision"].as_str().is_some());
    assert!(
        workspace.join(".taskmaster/tasks/tasks.json").is_file(),
        "the task set landed in the project workspace"
    );

    // Re-initialize over the edited structure answers ALREADY_INITIALIZED.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "initialize",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--schema-id",
            INIT_SCHEMA_ID,
            "--configuration",
            "{}",
            "--request-key",
            "tm-init-2",
        ],
    );
    assert_eq!(
        expect_failed("taskmaster.initialize", &out),
        "ALREADY_INITIALIZED"
    );

    // --- documents: create, gate, update, template ---
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "write-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
            "--content",
            "# Spec\n\n## Alpha\n\nDo the alpha step\n\n## Beta\n\nDo the beta step\n",
            "--expected-revision",
            "absent",
        ],
    );
    let created = expect_ok("taskmaster.write_document", &out);
    let doc_revision = created["document"]["revision"].as_str().unwrap().to_owned();

    // absent on an existing document must not overwrite.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "write-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
            "--content",
            "overwritten",
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(
        expect_failed("taskmaster.write_document", &out),
        "REVISION_CONFLICT"
    );

    // Template generation with a variable.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "write-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "api-prd.md",
            "--template-id",
            "api",
            "--template-variables",
            r#"[{"name":"Date","value":"2026-11-18"}]"#,
            "--expected-revision",
            "absent",
        ],
    );
    let generated = expect_ok("taskmaster.write_document", &out);
    assert_eq!(generated["document"]["templateId"], "api");
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "read-documents",
            "--output",
            "json",
            "--kind",
            "prd",
            "--project-id",
            &project_id,
            "--document-id",
            "api-prd.md",
        ],
    );
    let read = expect_ok("taskmaster.read_documents", &out);
    assert!(
        read["items"][0]["content"]
            .as_str()
            .is_some_and(|content| content.contains("2026-11-18")),
        "the generated template carries the substituted date"
    );

    // The listing carries no content; the single read does.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "read-documents",
            "--output",
            "json",
            "--kind",
            "prd",
            "--project-id",
            &project_id,
        ],
    );
    let listed = expect_ok("taskmaster.read_documents", &out);
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert!(items.len() >= 2, "both documents are listed: {listed}");
    assert!(items.iter().all(|item| item["content"].is_null()));
    assert!(listed["snapshotRevision"].as_str().is_some());

    // Templates are the global surface: 4 built-ins, single read with content.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "read-documents",
            "--output",
            "json",
            "--kind",
            "template",
        ],
    );
    let templates = expect_ok("taskmaster.read_documents", &out);
    assert_eq!(templates["items"].as_array().map(Vec::len), Some(4));

    // --- parse: the PRD becomes the task set ---
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "list-tasks",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let empty = expect_ok("taskmaster.list_tasks", &out);
    let set_revision = empty["snapshotRevision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "taskmaster",
            "parse-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
            "--document-revision",
            &doc_revision,
            "--expected-task-revision",
            &set_revision,
            "--request-key",
            "tm-parse-1",
        ],
    );
    let receipt = expect_ok("taskmaster.parse_document", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], "succeeded");
    let completion = &operation["result"];
    assert_eq!(completion["documentRevision"], doc_revision);
    assert_eq!(
        completion["createdTaskIds"].as_array().map(Vec::len),
        Some(2),
        "both sections became tasks: {completion}"
    );
    let first_task_id = completion["createdTaskIds"][0].as_str().unwrap().to_owned();

    // A stale task set revision must reject a second parse.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "parse-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
            "--document-revision",
            &doc_revision,
            "--expected-task-revision",
            &set_revision,
            "--request-key",
            "tm-parse-2",
        ],
    );
    assert_eq!(
        expect_failed("taskmaster.parse_document", &out),
        "REVISION_CONFLICT"
    );

    // --- tasks: list, next, write_task (three modes) ---
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "list-tasks",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let listed = expect_ok("taskmaster.list_tasks", &out);
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["title"], "Alpha");
    assert_eq!(items[0]["status"], "pending");
    let first_task_revision = items[0]["revision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "taskmaster",
            "list-tasks",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--selection",
            "next",
        ],
    );
    let next = expect_ok("taskmaster.list_tasks", &out);
    assert_eq!(next["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(next["items"][0]["title"], "Alpha");

    // createGenerated with a prompt.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "write-task",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--mode",
            "createGenerated",
            "--prompt",
            "Add export\nSupport exporting the report",
            "--expected-revision",
            "absent",
            "--request-key",
            "tm-add-1",
        ],
    );
    let receipt = expect_ok("taskmaster.write_task", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(operation["state"], "succeeded");
    let task = &operation["result"]["task"];
    assert_eq!(task["title"], "Add export");
    assert_eq!(task["status"], "pending");
    let generated_id = task["taskId"].as_str().unwrap().to_owned();

    // Unknown dependency rejects with DEPENDENCY_CONFLICT.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "write-task",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--mode",
            "createGenerated",
            "--prompt",
            "Depends on nothing",
            "--dependency-ids",
            r#"["99"]"#,
            "--expected-revision",
            "absent",
            "--request-key",
            "tm-add-2",
        ],
    );
    assert_eq!(
        expect_failed("taskmaster.write_task", &out),
        "DEPENDENCY_CONFLICT"
    );

    // setStatus moves only the status.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "write-task",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--mode",
            "setStatus",
            "--task-id",
            &generated_id,
            "--status",
            "done",
            "--expected-revision",
            task["revision"].as_str().unwrap(),
            "--request-key",
            "tm-set-1",
        ],
    );
    let receipt = expect_ok("taskmaster.write_task", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    let set_task = &operation["result"]["task"];
    assert_eq!(set_task["status"], "done");
    assert_eq!(set_task["title"], "Add export");

    // updateFields keeps omitted fields.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "write-task",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--mode",
            "updateFields",
            "--task-id",
            &generated_id,
            "--priority",
            "high",
            "--expected-revision",
            set_task["revision"].as_str().unwrap(),
            "--request-key",
            "tm-upd-1",
        ],
    );
    let receipt = expect_ok("taskmaster.write_task", &out);
    let operation = wait_operation(root, receipt["operationId"].as_str().unwrap()).await;
    let updated_task = &operation["result"]["task"];
    assert_eq!(updated_task["priority"], "high");
    assert_eq!(updated_task["status"], "done");

    // A stale task revision rejects the update.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "write-task",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--mode",
            "updateFields",
            "--task-id",
            &generated_id,
            "--priority",
            "low",
            "--expected-revision",
            &first_task_revision,
            "--request-key",
            "tm-upd-2",
        ],
    );
    assert_eq!(
        expect_failed("taskmaster.write_task", &out),
        "REVISION_CONFLICT"
    );

    // The task set re-reads with every change persisted.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "list-tasks",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let listed = expect_ok("taskmaster.list_tasks", &out);
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 3, "two parsed + one generated task: {listed}");
    let generated = items
        .iter()
        .find(|item| item["taskId"] == json!(generated_id))
        .expect("the generated task persisted");
    assert_eq!(generated["priority"], "high");
    assert_eq!(generated["status"], "done");

    // next now skips the done task and names the first pending one.
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "list-tasks",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--selection",
            "next",
        ],
    );
    let next = expect_ok("taskmaster.list_tasks", &out);
    assert_eq!(next["items"][0]["taskId"], first_task_id);

    // --- remove: gated by the current revision ---
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "remove-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
            "--expected-revision",
            "frev-stale",
        ],
    );
    assert_eq!(
        expect_failed("taskmaster.remove_document", &out),
        "REVISION_CONFLICT"
    );
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "remove-document",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
            "--expected-revision",
            &doc_revision,
        ],
    );
    let removed = expect_ok("taskmaster.remove_document", &out);
    assert_eq!(removed["removed"], true);
    let out = await_cli(
        root,
        &[
            "taskmaster",
            "read-documents",
            "--output",
            "json",
            "--kind",
            "prd",
            "--project-id",
            &project_id,
            "--document-id",
            "prd.txt",
        ],
    );
    assert_eq!(
        expect_failed("taskmaster.read_documents", &out),
        "NOT_FOUND"
    );
}

/// The notifications stream: real run lifecycle events derive the
/// notification families, delivered over `sub.open` with the
/// `notification.available` event type, deduped by notificationId across
/// re-subscriptions, and readable through the unary poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notifications_events_stream_and_unary_poll() {
    let home = TestHome::new("ntf-e2e");
    let root = home.path();
    init_home(root);
    let instance_id = init_home_instance(root);

    let project_id = "p-ntf-e2e";
    let out = await_cli(
        root,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            project_id,
            "--name",
            "Ntf E2E",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"ntf-ws\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    seed_session(root, project_id, "s-ntf-e2e").await;

    // A failed run (the error family) and a completed run (the stop family).
    let out = await_cli(
        root,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--session-id",
            "s-ntf-e2e",
            "--request-key",
            "ntf-run-1",
            "--text",
            "[pd-driver:fail]",
        ],
    );
    let submitted = expect_ok("run.submit", &out);
    let failed_run = submitted["runId"].as_str().unwrap().to_owned();
    let _ = wait_run(root, &failed_run);

    // The stream: default start replays the retained log, so the
    // notification is delivered deterministically after the fact.
    let mut session = ConnectSession::start(root).await;
    session.initialize().await;
    let response = session
        .sub_open(
            "notifications.events",
            json!({"scope": {"kind": "global"}, "follow": true}),
            16,
        )
        .await;
    assert!(
        response["result"]["subscriptionId"].as_str().is_some(),
        "sub.open accepted: {response}"
    );
    let sub_id = response["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    let frame = session
        .wait_for(
            |frame| {
                frame["method"] == json!("sub.event")
                    && frame["params"]["subscriptionId"] == json!(sub_id)
            },
            Duration::from_secs(20),
            "the error notification",
        )
        .await;
    let event = &frame["params"]["event"];
    assert_eq!(event["type"], "notification.available");
    assert_eq!(event["instanceId"], instance_id);
    assert_eq!(event["runId"], failed_run);
    let data = &event["data"];
    assert_eq!(data["eventType"], "error");
    assert_eq!(data["subjectId"], failed_run);
    assert_eq!(data["deliveryState"], "available");
    assert!(
        data["notificationId"]
            .as_str()
            .is_some_and(|id| id.starts_with("ntf-error:"))
    );
    assert!(
        data["summary"]
            .as_str()
            .is_some_and(|s| s.contains("failed"))
    );
    let notification_id = data["notificationId"].as_str().unwrap().to_owned();
    let last_cursor = event["cursor"].as_str().unwrap().to_owned();

    // A second subscription over the same retained log delivers the same
    // notification with the same notificationId — the re-derivation
    // dedupes instead of duplicating.
    let response = session
        .sub_open(
            "notifications.events",
            json!({"scope": {"kind": "global"}, "follow": true}),
            16,
        )
        .await;
    let sub_id2 = response["result"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let frame = session
        .wait_for(
            |frame| {
                frame["method"] == json!("sub.event")
                    && frame["params"]["subscriptionId"] == json!(sub_id2)
            },
            Duration::from_secs(20),
            "the replayed notification",
        )
        .await;
    assert_eq!(
        frame["params"]["event"]["data"]["notificationId"], notification_id,
        "re-subscription replays the same notificationId, not a duplicate"
    );

    // The session queue paused after the failed run; resume it so the
    // plain run can complete and derive the stop family.
    let out = await_cli(
        root,
        &[
            "queue",
            "get",
            "--output",
            "json",
            "--session-id",
            "s-ntf-e2e",
        ],
    );
    let queue = expect_ok("queue.get", &out);
    if queue["queue"]["paused"] == true {
        let out = await_cli(
            root,
            &[
                "queue",
                "resume",
                "--output",
                "json",
                "--session-id",
                "s-ntf-e2e",
                "--expected-revision",
                queue["queue"]["revision"].as_str().unwrap(),
                "--acknowledged-pause-reason",
                queue["queue"]["pauseReason"].as_str().unwrap(),
            ],
        );
        expect_ok("queue.resume", &out);
    }

    let out = await_cli(
        root,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--session-id",
            "s-ntf-e2e",
            "--request-key",
            "ntf-run-2",
            "--text",
            "plain run",
        ],
    );
    let submitted = expect_ok("run.submit", &out);
    let done_run = submitted["runId"].as_str().unwrap().to_owned();
    let _ = wait_run(root, &done_run);

    // The follow subscription delivers the stop notification next.
    let frame = session
        .wait_for(
            |frame| {
                frame["method"] == json!("sub.event")
                    && frame["params"]["subscriptionId"] == json!(sub_id)
                    && frame["params"]["event"]["data"]["eventType"] == json!("stop")
            },
            Duration::from_secs(20),
            "the stop notification",
        )
        .await;
    assert_eq!(frame["params"]["event"]["runId"], done_run);

    // The unary poll: `notifications events` without --follow returns the
    // next notification after a cursor — here the stop notification that
    // landed after the error one, schema-validated.
    let out = await_cli(
        root,
        &[
            "notifications",
            "events",
            "--output",
            "json",
            "--scope",
            "{\"kind\":\"global\"}",
            "--after-cursor",
            &last_cursor,
        ],
    );
    let polled = expect_ok("notifications.events", &out);
    assert_eq!(polled["eventType"], "stop");
    assert_eq!(polled["subjectId"], done_run);
    assert!(polled["cursor"].as_str().is_some_and(|c| c != last_cursor));

    // A project-scoped poll starts from the beginning of the retained log
    // and sees the instance's runs.
    let out = await_cli(
        root,
        &[
            "notifications",
            "events",
            "--output",
            "json",
            "--scope",
            &json!({"kind": "project", "id": project_id}).to_string(),
        ],
    );
    let scoped = expect_ok("notifications.events", &out);
    assert_eq!(scoped["notificationId"], notification_id);

    // Scope misuse rejects.
    let out = await_cli(
        root,
        &[
            "notifications",
            "events",
            "--output",
            "json",
            "--scope",
            "{\"kind\":\"project\"}",
        ],
    );
    assert_eq!(
        expect_failed("notifications.events", &out),
        "INVALID_ARGUMENT"
    );
}
