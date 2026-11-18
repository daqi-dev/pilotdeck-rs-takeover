//! End-to-end suite for the collections domain (`cli.task.collections`, S3):
//! `operations.{get,list,events,cancel,snapshot}` over the offline driver's
//! maintenance long operation, `resources snapshot`, `server resources`,
//! and `server diagnostics` — all driven through the real `agentcpm-pd`
//! argv against an isolated `AGENTCPM_PD_HOME` and the real `/v1/ws`
//! transport.
//!
//! Acceptance anchors covered here (the unit suite in the gateway covers
//! the same anchors at the store level):
//! * a runId never substitutes for an operationId;
//! * repeated cancels record one intent; `requested` ≠ `cancelled`;
//! * cross-page changes are never silently completed (SNAPSHOT_CHANGED);
//! * expired/invalid cursors answer explicit gap frames;
//! * conditional resource reads recover the true state after changes;
//! * browser resources answer CAPABILITY_UNAVAILABLE without a manager;
//! * diagnostics never leak prompt bodies.
//!
//! `resources.watch` is `execution=stream` and rides the generalized
//! subscription pump (t015b); this suite covers the unary operations.
//!
//! Local execution note: this machine is under a build freeze (disk
//! incident, 2026-11-18), so the suite was written but not executed here;
//! it runs in the integrator's cb8 verification pass.

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::time::Duration;

use serde_json::{Value, json};

use common_v1::*;

/// Parses a failed argv invocation: exactly one envelope line,
/// `outcome: "failed"`, non-zero exit. Returns the `error` object.
fn expect_error(method: &str, out: &CliOutcome) -> Value {
    assert_ne!(
        out.code, 0,
        "`{method}` must exit non-zero; stdout:\n{}\nstderr:\n{}",
        out.stdout, out.stderr
    );
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "`{method}` must print exactly one envelope line; stdout:\n{}",
        out.stdout
    );
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(value["version"], OP_VERSION, "{value}");
    assert_eq!(value["outcome"], "failed", "{value}");
    value["error"].clone()
}

async fn operations_list(home: &std::path::Path, page: Option<&str>) -> Value {
    let mut args = vec![
        "operations",
        "list",
        "--output",
        "json",
        "--scope",
        r#"{"kind":"instance"}"#,
    ];
    if let Some(cursor) = page {
        args.extend(["--page", cursor]);
    }
    let out = await_cli(home, &args);
    expect_ok("operations.list", &out)
}

async fn operations_get(home: &std::path::Path, operation_id: &str) -> Value {
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
    expect_ok("operations.get", &out)
}

async fn operations_cancel(home: &std::path::Path, operation_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "operations",
            "cancel",
            "--output",
            "json",
            "--operation-id",
            operation_id,
        ],
    );
    expect_ok("operations.cancel", &out)
}

async fn operations_events(
    home: &std::path::Path,
    operation_id: &str,
    after_cursor: Option<&str>,
) -> Value {
    let mut args = vec![
        "operations",
        "events",
        "--output",
        "json",
        "--operation-id",
        operation_id,
    ];
    if let Some(cursor) = after_cursor {
        args.extend(["--after-cursor", cursor]);
    }
    let out = await_cli(home, &args);
    expect_ok("operations.events", &out)
}

async fn find_maintenance_operation(home: &std::path::Path) -> Option<Value> {
    let page = operations_list(home, None).await;
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|op| op["operationType"] == "instance.maintenance")
        .cloned()
}

/// Waits until the driver's maintenance operation appears in the unified
/// listing and reaches `running` (bounded: the run starts asynchronously
/// after the submit receipt).
async fn await_maintenance_operation(home: &std::path::Path) -> Value {
    for _ in 0..40 {
        if let Some(op) = find_maintenance_operation(home).await
            && op["state"] == "running"
        {
            return op;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("the maintenance operation never reached running in operations.list");
}

/// Waits until any maintenance operation record exists (any state — for
/// scenarios over already-terminal operations).
async fn await_any_maintenance_operation(home: &std::path::Path) -> Value {
    for _ in 0..40 {
        if let Some(op) = find_maintenance_operation(home).await {
            return op;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("the maintenance operation never appeared in operations.list");
}

#[tokio::test]
async fn operations_surface_over_a_driver_maintenance_operation() {
    let home = TestHome::new("collections-maintenance");
    init_home(home.path());
    let (_project_id, session_id) = seed_project_session(home.path(), "maint").await;

    // An 8 s maintenance operation driven by the offline driver: long
    // enough that every observation below lands inside its window, and
    // cancelled (not waited) at the end, so the duration never extends the
    // scenario's runtime.
    let submitted = submit_run(
        home.path(),
        &session_id,
        "[pd-driver:maintenance=8000]",
        "key-collections-maint-1",
    )
    .await;
    let run_id = submitted["runId"].as_str().unwrap().to_owned();

    // The operation surfaces through the unified listing while running.
    let running = await_maintenance_operation(home.path()).await;
    let operation_id = running["operationId"].as_str().unwrap().to_owned();
    assert_eq!(running["state"], "running");
    assert_eq!(running["cancelRequested"], false);

    // A runId never substitutes for an operationId.
    let run_as_op = await_cli(
        home.path(),
        &[
            "operations",
            "get",
            "--output",
            "json",
            "--operation-id",
            &run_id,
        ],
    );
    let error = expect_error("operations.get", &run_as_op);
    assert_eq!(error["code"], "NOT_FOUND", "{error}");

    // Progress accumulates while the operation runs.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let observed = operations_get(home.path(), &operation_id).await;
    assert_eq!(observed["operation"]["state"], "running");
    let progress = observed["operation"]["progress"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        !progress.is_empty(),
        "the maintenance operation must report progress: {observed}"
    );

    // Event pages: only delivered events advance the cursor; EOF (caughtUp
    // without terminal) is not a terminal state.
    let events = operations_events(home.path(), &operation_id, None).await;
    let frame = &events["frame"];
    let types: Vec<&str> = frame["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(types.first(), Some(&"operation.queued"));
    assert!(types.contains(&"operation.started"), "{types:?}");
    assert_eq!(frame["caughtUp"], true);
    assert_eq!(frame["terminal"], false, "EOF is not a terminal state");
    let last_cursor = frame["nextCursor"].as_str().unwrap().to_owned();

    // Cancel: requested (not cancelled), one intent, repeatable.
    let receipt = operations_cancel(home.path(), &operation_id).await;
    assert_eq!(receipt["receipt"]["disposition"], "requested");
    assert_eq!(receipt["receipt"]["state"], "cancelling");
    let cancelled_revision = receipt["receipt"]["revision"].as_str().unwrap().to_owned();

    // A second cancel (fresh CLI invocation, fresh key) never records a
    // second intent. The driver polls the cancel flag every ~20 ms, so the
    // repeat may either still observe `cancelling` (same revision — no new
    // intent) or the already-processed terminal state.
    let again = operations_cancel(home.path(), &operation_id).await;
    match again["receipt"]["disposition"].as_str().unwrap() {
        "requested" => {
            assert_eq!(again["receipt"]["state"], "cancelling");
            assert_eq!(
                again["receipt"]["revision"], cancelled_revision,
                "repeated cancel must not bump the revision (one intent)"
            );
        }
        "alreadyTerminal" => {
            assert_eq!(again["receipt"]["state"], "cancelled");
        }
        other => panic!("unexpected cancel disposition `{other}`: {again}"),
    }

    // The run itself completes: cancelling the operation does not cancel
    // the run (independent lifecycles).
    let waited = wait_run(home.path(), &run_id);
    let waited = expect_ok("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");

    // Terminal state is queryable after completion.
    let terminal = operations_get(home.path(), &operation_id).await;
    assert_eq!(terminal["operation"]["state"], "cancelled");

    // Cancelling a terminal operation: alreadyTerminal.
    let terminal_receipt = operations_cancel(home.path(), &operation_id).await;
    assert_eq!(
        terminal_receipt["receipt"]["disposition"],
        "alreadyTerminal"
    );
    assert_eq!(terminal_receipt["receipt"]["state"], "cancelled");

    // Events after the terminal: the cancel_requested and cancelled
    // events are all visible, caught up, and terminal.
    let mut all_types = Vec::new();
    let mut cursor = last_cursor;
    for _ in 0..8 {
        let page = operations_events(home.path(), &operation_id, Some(&cursor)).await;
        let frame = &page["frame"];
        for event in frame["events"].as_array().unwrap() {
            all_types.push(event["type"].as_str().unwrap().to_owned());
        }
        if frame["terminal"] == true {
            break;
        }
        cursor = frame["nextCursor"].as_str().unwrap().to_owned();
    }
    assert!(
        all_types.contains(&"operation.cancel_requested".to_owned()),
        "{all_types:?}"
    );
    assert_eq!(all_types.last().unwrap(), "operation.cancelled");

    // Snapshot: checkpoint + exactly one artifact-referenced state section.
    let snap = await_cli(
        home.path(),
        &[
            "operations",
            "snapshot",
            "--output",
            "json",
            "--operation-id",
            &operation_id,
        ],
    );
    let snap = expect_ok("operations.snapshot", &snap);
    assert_eq!(snap["operation"]["state"], "cancelled");
    assert_eq!(snap["snapshot"]["checkpoint"]["resourceKind"], "operation");
    assert_eq!(snap["snapshot"]["checkpoint"]["resourceId"], operation_id);
    let sections = snap["snapshot"]["sections"].as_array().unwrap();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0]["name"], "state");
    assert!(
        sections[0]["artifactId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(sections[0]["sha256"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn operations_list_pagination_never_silently_completes_across_changes() {
    let home = TestHome::new("collections-pagination");
    init_home(home.path());
    let (_project_id, session_id) = seed_project_session(home.path(), "page").await;

    // Five quick maintenance operations, all terminal before paging.
    for i in 0..5 {
        let submitted = submit_run(
            home.path(),
            &session_id,
            "[pd-driver:maintenance=1]",
            &format!("key-collections-page-{i}"),
        )
        .await;
        let run_id = submitted["runId"].as_str().unwrap().to_owned();
        let waited = wait_run(home.path(), &run_id);
        expect_ok("run.wait", &waited);
    }
    let mut seen = find_all_maintenance_operations(home.path()).await;
    assert_eq!(seen.len(), 5, "five operations before paging: {seen:?}");

    // Page 1 with limit 2: incomplete, nextCursor present.
    let page1 = operations_list(home.path(), Some(r#"{"limit":2}"#)).await;
    assert_eq!(page1["items"].as_array().unwrap().len(), 2);
    assert_eq!(page1["pageInfo"]["complete"], false);
    let next = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();

    // A new operation lands between the pages.
    let submitted = submit_run(
        home.path(),
        &session_id,
        "[pd-driver:maintenance=1]",
        "key-collections-page-new",
    )
    .await;
    let run_id = submitted["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(home.path(), &run_id);
    expect_ok("run.wait", &waited);

    // Page 2 with the stale cursor: SNAPSHOT_CHANGED, not a silent page.
    let drifted = await_cli(
        home.path(),
        &[
            "operations",
            "list",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"instance"}"#,
            "--page",
            &format!(r#"{{"limit":2,"cursor":"{next}"}}"#),
        ],
    );
    let error = expect_error("operations.list", &drifted);
    assert_eq!(error["code"], "SNAPSHOT_CHANGED", "{error}");

    // Restarting the query observes the new operation: no silent loss.
    seen = find_all_maintenance_operations(home.path()).await;
    assert_eq!(seen.len(), 6, "the new operation must be visible: {seen:?}");

    // Full pagination converges with complete=true.
    let mut cursor: Option<String> = None;
    let mut paged = 0usize;
    for _ in 0..16 {
        let page_arg = match &cursor {
            Some(c) => format!(r#"{{"limit":2,"cursor":"{c}"}}"#),
            None => r#"{"limit":2}"#.to_owned(),
        };
        let page = operations_list(home.path(), Some(&page_arg)).await;
        paged += page["items"].as_array().unwrap().len();
        if page["pageInfo"]["complete"] == true {
            break;
        }
        cursor = page["pageInfo"]["nextCursor"]
            .as_str()
            .map(ToOwned::to_owned);
    }
    assert_eq!(paged, 6, "pagination must deliver every operation");

    // Scope filtering: a project-scoped listing does not leak instance ops.
    let scoped = await_cli(
        home.path(),
        &[
            "operations",
            "list",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"project","projectId":"p-page"}"#,
        ],
    );
    let scoped = expect_ok("operations.list", &scoped);
    assert_eq!(
        scoped["items"].as_array().unwrap().len(),
        0,
        "instance-scoped maintenance operations must not appear in a project listing"
    );
}

async fn find_all_maintenance_operations(home: &std::path::Path) -> Vec<Value> {
    operations_list(home, None).await["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|op| op["operationType"] == "instance.maintenance")
        .cloned()
        .collect()
}

#[tokio::test]
async fn operations_events_reports_gap_frames_for_bad_cursors() {
    let home = TestHome::new("collections-gap");
    init_home(home.path());
    let (_project_id, session_id) = seed_project_session(home.path(), "gap").await;

    let submitted = submit_run(
        home.path(),
        &session_id,
        "[pd-driver:maintenance=1]",
        "key-collections-gap-1",
    )
    .await;
    let run_id = submitted["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(home.path(), &run_id);
    expect_ok("run.wait", &waited);

    let op = await_any_maintenance_operation(home.path()).await;
    let operation_id = op["operationId"].as_str().unwrap().to_owned();

    // An unparseable cursor is an explicit gap frame, never an empty page.
    let gap = operations_events(home.path(), &operation_id, Some("garbage")).await;
    assert_eq!(gap["frame"]["kind"], "gap");
    assert_eq!(gap["frame"]["reason"], "cursor_invalid");
    assert_eq!(gap["frame"]["recoveryMethod"], "operations.snapshot");
    assert_eq!(gap["frame"]["recovery"], "restart_query");

    // An unknown operation is NOT_FOUND, not an empty page.
    let unknown = await_cli(
        home.path(),
        &[
            "operations",
            "events",
            "--output",
            "json",
            "--operation-id",
            "op-does-not-exist",
        ],
    );
    let error = expect_error("operations.events", &unknown);
    assert_eq!(error["code"], "NOT_FOUND", "{error}");
}

#[tokio::test]
async fn resources_snapshot_conditional_reads_recover_the_true_state() {
    let home = TestHome::new("collections-rsrc");
    init_home(home.path());
    let (project_id, session_id) = seed_project_session(home.path(), "rsrc").await;
    let instance_id = init_home_instance(home.path());

    let resources =
        format!(r#"[{{"kind":"projects"}},{{"kind":"session","resourceId":"{session_id}"}}]"#);
    let out = await_cli(
        home.path(),
        &[
            "resources",
            "snapshot",
            "--output",
            "json",
            "--resources",
            &resources,
        ],
    );
    let snap = expect_ok("resources.snapshot", &out);
    assert_eq!(snap["instanceId"], instance_id);
    let sessions_entry = snap["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["resource"]["kind"] == "session")
        .cloned()
        .unwrap();
    assert_eq!(sessions_entry["state"], "present");
    assert_eq!(sessions_entry["snapshotSchemaId"], "resources.session.v1");
    let session_revision = sessions_entry["revision"].as_str().unwrap().to_owned();

    // The same known version folds into unchanged: a lost notification is
    // recovered by the conditional read without re-sending state. `projects`
    // carries no knownVersion here, so it must be re-sent in full — a
    // consumer cannot be told "unchanged" for a resource it never named.
    let known = json!([{"resource": {"kind": "session", "resourceId": session_id}, "revision": session_revision}]).to_string();
    let out = await_cli(
        home.path(),
        &[
            "resources",
            "snapshot",
            "--output",
            "json",
            "--resources",
            &resources,
            "--known-versions",
            &known,
        ],
    );
    let unchanged = expect_ok("resources.snapshot", &out);
    assert_eq!(unchanged["unchanged"].as_array().unwrap().len(), 1);
    let resend = unchanged["snapshots"].as_array().unwrap();
    assert_eq!(
        resend.len(),
        1,
        "only the resource without a knownVersion is re-sent"
    );
    assert_eq!(resend[0]["resource"]["kind"], "projects");

    // A second consumer submitting a run (activity changes) bumps the
    // session revision: the conditional read recovers the true state. The
    // run is long enough that the snapshot below observes it active.
    submit_run(
        home.path(),
        &session_id,
        "[pd-driver:sleep=8000]",
        "key-collections-rsrc-1",
    )
    .await;
    let out = await_cli(
        home.path(),
        &[
            "resources",
            "snapshot",
            "--output",
            "json",
            "--resources",
            &resources,
            "--known-versions",
            &known,
        ],
    );
    let changed = expect_ok("resources.snapshot", &out);
    assert_eq!(changed["unchanged"].as_array().unwrap().len(), 0);
    let new_revision = changed["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["resource"]["kind"] == "session")
        .unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(new_revision, session_revision);

    // Kinds without a revision source report unavailable, not present.
    let out = await_cli(
        home.path(),
        &[
            "resources",
            "snapshot",
            "--output",
            "json",
            "--resources",
            r#"[{"kind":"terminal"}]"#,
        ],
    );
    let unavailable = expect_ok("resources.snapshot", &out);
    assert_eq!(unavailable["snapshots"][0]["state"], "unavailable");

    // A missing project is absent.
    let out = await_cli(
        home.path(),
        &[
            "resources",
            "snapshot",
            "--output",
            "json",
            "--resources",
            r#"[{"kind":"project","projectId":"missing-project"}]"#,
        ],
    );
    let absent = expect_ok("resources.snapshot", &out);
    assert_eq!(absent["snapshots"][0]["state"], "absent");
    let _ = project_id;
}

#[tokio::test]
async fn system_resources_lists_sessions_and_refuses_browsers() {
    let home = TestHome::new("collections-sysres");
    init_home(home.path());
    let (_project_id, _session_id) = seed_project_session(home.path(), "sysres").await;

    // kind defaults to all: managed sessions are listed, complete.
    let out = await_cli(home.path(), &["server", "resources", "--output", "json"]);
    let all = expect_ok("system.resources", &out);
    let items = all["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|item| item["kind"] == "session"));
    assert!(items.iter().all(|item| {
        item["resourceStatus"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    }));
    assert_eq!(all["pageInfo"]["complete"], true);

    // kind=browser: explicit CAPABILITY_UNAVAILABLE — the gateway wires no
    // browser manager and never touches the user's ordinary browser.
    let out = await_cli(
        home.path(),
        &[
            "server",
            "resources",
            "--output",
            "json",
            "--kind",
            "browser",
        ],
    );
    let error = expect_error("system.resources", &out);
    assert_eq!(error["code"], "CAPABILITY_UNAVAILABLE", "{error}");

    // An invalid kind is rejected by the input schema before dispatch.
    let out = await_cli(
        home.path(),
        &[
            "server",
            "resources",
            "--output",
            "json",
            "--kind",
            "printer",
        ],
    );
    let error = expect_error("system.resources", &out);
    assert_eq!(error["code"], "INVALID_ARGUMENT", "{error}");
}

#[tokio::test]
async fn system_diagnostics_redacted_and_honest() {
    let home = TestHome::new("collections-diag");
    init_home(home.path());
    let (_project_id, session_id) = seed_project_session(home.path(), "diag").await;

    // A clean instance has diagnostics that paginate (possibly empty but
    // complete — read failures would answer UNAVAILABLE, not this).
    let out = await_cli(home.path(), &["server", "diagnostics", "--output", "json"]);
    let clean = expect_ok("system.diagnostics", &out);
    assert_eq!(clean["pageInfo"]["complete"], true);

    // A failing run pauses the session queue: the diagnostic appears, and
    // neither the prompt body nor any token material leaks into it. (The
    // run.wait envelope reports the terminal state; its CLI exit code is 1
    // by the command tree's exit policy, so the envelope is parsed without
    // the exit-0 assertion.)
    let submitted = submit_run(
        home.path(),
        &session_id,
        "[pd-driver:fail]",
        "key-collections-diag-1",
    )
    .await;
    let run_id = submitted["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(home.path(), &run_id);
    let waited = expect_envelope("run.wait", &waited);
    assert_eq!(
        waited["conditionMet"], true,
        "run never terminated: {waited}"
    );

    let out = await_cli(home.path(), &["server", "diagnostics", "--output", "json"]);
    let diags = expect_ok("system.diagnostics", &out);
    let items = diags["items"].as_array().unwrap();
    let paused = items
        .iter()
        .find(|item| item["code"] == "SESSION_QUEUE_PAUSED")
        .cloned()
        .unwrap_or_else(|| panic!("SESSION_QUEUE_PAUSED missing: {items:?}"));
    assert_eq!(paused["resourceId"], session_id.as_str());
    let message = paused["message"].as_str().unwrap();
    assert!(!message.contains("[pd-driver"), "body leaked: {message}");

    // `since` validates its input and filters point-in-time diagnostics.
    let out = await_cli(
        home.path(),
        &[
            "server",
            "diagnostics",
            "--output",
            "json",
            "--since",
            "not-a-date",
        ],
    );
    let error = expect_error("system.diagnostics", &out);
    assert_eq!(error["code"], "INVALID_ARGUMENT", "{error}");

    let out = await_cli(
        home.path(),
        &[
            "server",
            "diagnostics",
            "--output",
            "json",
            "--since",
            "2000-01-01T00:00:00+00:00",
        ],
    );
    let filtered = expect_ok("system.diagnostics", &out);
    assert!(
        filtered["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "SESSION_QUEUE_PAUSED"),
        "an old `since` keeps the paused-session diagnostic"
    );

    // Pagination over a stable view: page 1 with limit 1, then page 2 with
    // the pinned cursor; drift answers SNAPSHOT_CHANGED instead of a
    // silently wrong page.
    let page1 = await_cli(
        home.path(),
        &[
            "server",
            "diagnostics",
            "--output",
            "json",
            "--page",
            r#"{"limit":1}"#,
        ],
    );
    let page1 = expect_ok("system.diagnostics", &page1);
    if page1["pageInfo"]["complete"] != true {
        let next = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        let page2 = await_cli(
            home.path(),
            &[
                "server",
                "diagnostics",
                "--output",
                "json",
                "--page",
                &format!(r#"{{"limit":1,"cursor":"{next}"}}"#),
            ],
        );
        let page2 = expect_ok("system.diagnostics", &page2);
        assert_eq!(page2["pageInfo"]["complete"], true);
    }
}
