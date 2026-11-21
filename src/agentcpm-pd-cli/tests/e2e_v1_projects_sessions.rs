//! t012 e2e: project directory management, session lifecycle, history and
//! archives (`cli.case.c004`, `cli.case.c005`).
//!
//! Every scenario drives the real `agentcpm-pd` binary over the real
//! `/v1/ws` transport against an isolated `AGENTCPM_PD_HOME`, using the
//! shared `common_v1` harness. Scenario -> acceptance mapping:
//!
//! - `projects_and_directories_lifecycle` (c004): project.create/update/
//!   remove round trips with directories.create/list behind it; escapes,
//!   existing-directory conflicts and the remove-only deregistration are
//!   asserted on disk as well as through the envelopes.
//! - `sessions_close_reopen_delete_lifecycle` (c005): session.update CAS,
//!   close/refuse-while-running, SESSION_CLOSED after close with history
//!   still readable, reopen, delete tombstone with SESSION_DELETED.
//! - `history_export_restore_roundtrip` (c005): the async export receipt,
//!   archive.get/entries, restore into a new session, fork's invalid
//!   boundary and read anchors. The archive is discovered through the
//!   on-disk `<home>/v1/archives/` layout because the in-band operation
//!   result channel (`operations.get`) belongs to a later S3 task; the
//!   content-rich history paths (entry copy, attachment parts, corrupt
//!   rows, CAS zero-modification) are covered by the in-process domain
//!   tests in `pilotdeck-gateway` — the v1 turn projection that writes
//!   history entries lands with the D20 wiring task.
//!
//! 本地未跑（2026-11-18 构建冻结令），待集成者 cb8 验证。

// The suite spawns real gateways through the CLI's embedded-gateway feature;
// without it there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common_v1::*;

/// Parses a failed invocation: exactly one envelope line with
/// `outcome=failed`, a non-zero exit (1 for failures, 2 for usage-class
/// errors like INVALID_ARGUMENT), and returns the contract error code.
fn expect_fail(method: &str, out: &CliOutcome) -> String {
    assert!(
        out.code == 1 || out.code == 2,
        "`{method}` must exit non-zero on a contract failure (got {}); stderr: {}",
        out.code,
        out.stderr
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
    assert_eq!(value["outcome"], "failed", "`{method}` outcome: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn server_path(root_id: &str, relative: &str) -> String {
    serde_json::to_string(&json!({
        "authorizedRootId": root_id,
        "relativePath": {"encoding": "utf8", "value": relative},
    }))
    .unwrap()
}

/// Waits until one archive appears under `<home>/v1/archives/` and becomes
/// readable through the public `archive get` (state `succeeded`); returns
/// the archive id. The id is discovered through the durable on-disk layout
/// because the in-band operation-result channel (`operations.get`) belongs
/// to a later S3 task; the read itself always goes through the public op.
async fn wait_for_archive_ready(home: &std::path::Path) -> String {
    let dir = home.join("v1").join("archives");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let found: Vec<String> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        if let Some(archive_id) = found.first() {
            let out = await_cli(
                home,
                &[
                    "archive",
                    "get",
                    "--output",
                    "json",
                    "--archive-id",
                    archive_id,
                ],
            );
            if out.code == 0 {
                expect_ok("archive.get", &out);
                return archive_id.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "no readable archive appeared under {} within 30s",
        dir.display()
    );
}

async fn session_revision(home: &std::path::Path, session_id: &str) -> String {
    let out = await_cli(
        home,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            session_id,
        ],
    );
    expect_ok("session.get", &out)["session"]["revision"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Waits until `sessions get <id>` succeeds (the restore operation creates
/// the session in the background) or panics.
async fn wait_for_session(home: &std::path::Path, session_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = await_cli(
            home,
            &[
                "sessions",
                "get",
                "--output",
                "json",
                "--session-id",
                session_id,
            ],
        );
        if out.code == 0 {
            return expect_ok("session.get", &out);
        }
        assert!(
            Instant::now() < deadline,
            "session `{session_id}` was not restored in time; last stderr: {}",
            out.stderr
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ===========================================================================
// c004: projects and their directories
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projects_and_directories_lifecycle() {
    let home = TestHome::new("t012-projects");
    let root = home.path();
    init_home(root);
    let instance_id = init_home_instance(root);

    // Create a parent directory inside the default authorized root.
    let made = await_cli(
        root,
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--request-key",
            "t012-dir-1",
            "--path",
            &server_path("default", "team"),
        ],
    );
    let made = expect_ok("directories.create", &made);
    assert_eq!(made["created"], true);

    // The same request replays idempotently; a different input under the
    // same key conflicts; a different key collides with the directory.
    let replay = await_cli(
        root,
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--request-key",
            "t012-dir-1",
            "--path",
            &server_path("default", "team"),
        ],
    );
    let replay = expect_ok("directories.create", &replay);
    assert_eq!(replay["created"], true);
    let conflict = await_cli(
        root,
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--request-key",
            "t012-dir-1",
            "--path",
            &server_path("default", "other"),
        ],
    );
    assert_eq!(
        expect_fail("directories.create", &conflict),
        "IDEMPOTENCY_CONFLICT"
    );
    let exists = await_cli(
        root,
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--request-key",
            "t012-dir-2",
            "--path",
            &server_path("default", "team"),
        ],
    );
    assert_eq!(expect_fail("directories.create", &exists), "ALREADY_EXISTS");

    // Escapes never leave the authorized root.
    for bad in ["../escape", "/etc"] {
        let escape = await_cli(
            root,
            &[
                "directories",
                "create",
                "--output",
                "json",
                "--path",
                &server_path("default", bad),
            ],
        );
        assert_eq!(expect_fail("directories.create", &escape), "PATH_ESCAPE");
    }

    // project.create makes the directory and registers the project.
    let parent_json = serde_json::to_string(&json!({
        "instanceId": instance_id,
        "rootId": "default",
        "relativePath": "team",
    }))
    .unwrap();
    let created = await_cli(
        root,
        &[
            "projects",
            "create",
            "--output",
            "json",
            "--project-id",
            "p-alpha",
            "--parent",
            &parent_json,
            "--directory-name",
            "alpha",
            "--name",
            "Alpha",
        ],
    );
    let created = expect_ok("project.create", &created);
    assert_eq!(created["project"]["registrationState"], "registered");
    assert_eq!(created["project"]["root"]["relativePath"], "team/alpha");
    assert!(
        root.join("workspaces").join("team").join("alpha").is_dir(),
        "the project directory exists inside the authorized root"
    );

    // The new directory shows up in directories.list.
    let listed = await_cli(
        root,
        &[
            "directories",
            "list",
            "--output",
            "json",
            "--path",
            &server_path("default", "team"),
        ],
    );
    let listed = expect_ok("directories.list", &listed);
    let names: Vec<&str> = listed["directories"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d["displayName"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(names.contains(&"alpha"), "listing shows the project dir");

    // A duplicate projectId fails before any filesystem effect.
    let duplicate = await_cli(
        root,
        &[
            "projects",
            "create",
            "--output",
            "json",
            "--project-id",
            "p-alpha",
            "--parent",
            &parent_json,
            "--directory-name",
            "beta",
            "--name",
            "Beta",
        ],
    );
    assert_eq!(expect_fail("project.create", &duplicate), "CREATE_FAILED");
    assert!(!root.join("workspaces").join("team").join("beta").exists());

    // project.update is a CAS on the revision.
    let revision = created["project"]["revision"].as_str().unwrap().to_owned();
    let stale = await_cli(
        root,
        &[
            "projects",
            "update",
            "--output",
            "json",
            "--project-id",
            "p-alpha",
            "--expected-revision",
            &revision,
            "--name",
            "Stale",
        ],
    );
    let updated = expect_ok("project.update", &stale);
    assert_eq!(updated["project"]["name"], "Stale");
    let raced = await_cli(
        root,
        &[
            "projects",
            "update",
            "--output",
            "json",
            "--project-id",
            "p-alpha",
            "--expected-revision",
            &revision,
            "--name",
            "Loser",
        ],
    );
    assert_eq!(expect_fail("project.update", &raced), "REVISION_CONFLICT");

    // project.remove only deregisters: the disk directory survives.
    let fresh_revision = updated["project"]["revision"].as_str().unwrap().to_owned();
    let removed = await_cli(
        root,
        &[
            "projects",
            "remove",
            "--output",
            "json",
            "--project-id",
            "p-alpha",
            "--expected-revision",
            &fresh_revision,
        ],
    );
    let removed = expect_ok("project.remove", &removed);
    assert_eq!(removed["removedProjectId"], "p-alpha");
    let gone = await_cli(
        root,
        &[
            "projects",
            "get",
            "--output",
            "json",
            "--project-id",
            "p-alpha",
        ],
    );
    assert_eq!(expect_fail("project.get", &gone), "NOT_FOUND");
    assert!(
        root.join("workspaces").join("team").join("alpha").is_dir(),
        "removing the registration never deletes the directory"
    );
}

// ===========================================================================
// c005: sessions and their lifecycle
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_close_reopen_delete_lifecycle() {
    let home = TestHome::new("t012-sessions");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "t012s").await;
    let revision = session_revision(root, &session_id).await;

    // session.update renames and retags under CAS.
    let updated = await_cli(
        root,
        &[
            "sessions",
            "update",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &revision,
            "--name",
            "Renamed",
            "--tags",
            "[\"t012\"]",
        ],
    );
    let updated = expect_ok("session.update", &updated);
    assert_eq!(updated["session"]["name"], "Renamed");
    assert_eq!(updated["session"]["tags"], json!(["t012"]));
    let raced = await_cli(
        root,
        &[
            "sessions",
            "update",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &revision,
            "--name",
            "Loser",
        ],
    );
    assert_eq!(expect_fail("session.update", &raced), "REVISION_CONFLICT");

    // A running run refuses the close (active/queued存在拒绝).
    let run = submit_run(root, &session_id, "[pd-driver:sleep=2500]", "t012-close-1").await;
    let run_id = run["runId"].as_str().unwrap().to_owned();
    let fresh_revision = session_revision(root, &session_id).await;
    let busy = await_cli(
        root,
        &[
            "sessions",
            "close",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &fresh_revision,
        ],
    );
    assert_eq!(expect_fail("session.close", &busy), "ACTIVE_RESOURCES");

    let waited = wait_run(root, &run_id);
    let waited = expect_envelope("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");

    // With the run terminal the close succeeds; submissions then answer
    // SESSION_CLOSED while history stays readable.
    let closed = await_cli(
        root,
        &[
            "sessions",
            "close",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &fresh_revision,
        ],
    );
    let closed = expect_ok("session.close", &closed);
    assert_eq!(closed["session"]["sessionState"], "closed");
    let refused = await_cli(
        root,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--request-key",
            "t012-closed-submit",
            "--session-id",
            &session_id,
            "--text",
            "after close",
        ],
    );
    assert_eq!(expect_fail("run.submit", &refused), "SESSION_CLOSED");
    let readable = await_cli(
        root,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            &session_id,
        ],
    );
    let readable = expect_ok("history.read", &readable);
    assert_eq!(readable["items"].as_array().map(Vec::len), Some(0));

    // Reopen restores admission with the fresh revision.
    let closed_revision = closed["session"]["revision"].as_str().unwrap().to_owned();
    let reopened = await_cli(
        root,
        &[
            "sessions",
            "reopen",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &closed_revision,
        ],
    );
    let reopened = expect_ok("session.reopen", &reopened);
    assert_eq!(reopened["session"]["sessionState"], "open");
    let resubmitted = submit_run(root, &session_id, "after reopen", "t012-reopen-1").await;
    let resubmit_id = resubmitted["runId"].as_str().unwrap().to_owned();
    let done = wait_run(root, &resubmit_id);
    let done = expect_envelope("run.wait", &done);
    assert_eq!(done["run"]["state"], "succeeded");

    // Delete: tombstone state, run tombstones kept, reopen impossible.
    let final_revision = session_revision(root, &session_id).await;
    let deleted = await_cli(
        root,
        &[
            "sessions",
            "delete",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &final_revision,
        ],
    );
    let deleted = expect_ok("session.delete", &deleted);
    assert_eq!(deleted["deletedSessionId"], session_id);
    let tombstone = await_cli(
        root,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            &session_id,
        ],
    );
    let tombstone = expect_ok("session.get", &tombstone);
    assert_eq!(tombstone["session"]["sessionState"], "deleted");
    let tombstone_revision = tombstone["session"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let resurrect = await_cli(
        root,
        &[
            "sessions",
            "reopen",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &tombstone_revision,
        ],
    );
    assert_eq!(expect_fail("session.reopen", &resurrect), "SESSION_DELETED");
    let history_gone = await_cli(
        root,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            &session_id,
        ],
    );
    assert_eq!(expect_fail("history.read", &history_gone), "NOT_FOUND");

    // A fresh session still validates the project scope.
    let orphan = await_cli(
        root,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            "s-orphan",
            "--project-id",
            &format!("{project_id}-ghost"),
        ],
    );
    assert_eq!(expect_fail("session.create", &orphan), "NOT_FOUND");
}

// ===========================================================================
// c005: history export / archive reads / restore roundtrip
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_export_restore_roundtrip() {
    let home = TestHome::new("t012-history");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "t012h").await;
    let revision = session_revision(root, &session_id).await;

    // Empty-history anchors: a foreign fromEntryId is rejected outright.
    let foreign = await_cli(
        root,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--from-entry-id",
            "foreign-entry",
        ],
    );
    assert_eq!(expect_fail("history.read", &foreign), "INVALID_ARGUMENT");

    // history.fork refuses a boundary that is not an entry of the session.
    let bad_fork = await_cli(
        root,
        &[
            "history",
            "fork",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--through-entry-id",
            "no-such-entry",
            "--expected-revision",
            &revision,
            "--new-session-id",
            "s-fork-bad",
        ],
    );
    assert_eq!(
        expect_fail("history.fork", &bad_fork),
        "INVALID_HISTORY_BOUNDARY"
    );

    // The async export answers with an operation receipt.
    let exported = await_cli(
        root,
        &[
            "history",
            "export",
            "--output",
            "json",
            "--request-key",
            "t012-export-1",
            "--session-id",
            &session_id,
            "--expected-revision",
            &revision,
        ],
    );
    let exported = expect_ok("history.export", &exported);
    assert_eq!(exported["kind"], "operation");
    assert_eq!(exported["deduplicated"], false);

    // The archive is discovered through the durable on-disk layout and then
    // read through the public operation.
    let archive_id = wait_for_archive_ready(root).await;

    // archive.get describes the manifest artifact; archive.entries pages
    // the (empty) transcript and the payload artifacts.
    let manifest = await_cli(
        root,
        &[
            "archive",
            "get",
            "--output",
            "json",
            "--archive-id",
            &archive_id,
        ],
    );
    let manifest = expect_ok("archive.get", &manifest);
    assert_eq!(manifest["entryCount"], 0);
    assert_eq!(manifest["sourceRevision"], revision);
    assert_eq!(
        manifest["manifest"]["artifactId"].as_str().unwrap(),
        format!("{archive_id}.manifest")
    );

    let entries = await_cli(
        root,
        &[
            "archive",
            "entries",
            "--output",
            "json",
            "--archive-id",
            &archive_id,
        ],
    );
    let entries = expect_ok("archive.entries", &entries);
    assert_eq!(entries["items"].as_array().map(Vec::len), Some(0));
    let artifacts = entries["artifacts"].as_array().cloned().unwrap_or_default();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["sha256"].as_str().map(str::len), Some(64));
    assert_eq!(entries["pageInfo"]["complete"], true);

    // The same key replays the original receipt (no second archive).
    let replay = await_cli(
        root,
        &[
            "history",
            "export",
            "--output",
            "json",
            "--request-key",
            "t012-export-1",
            "--session-id",
            &session_id,
            "--expected-revision",
            &revision,
        ],
    );
    let replay = expect_ok("history.export", &replay);
    assert_eq!(replay["deduplicated"], true);
    assert_eq!(
        replay["operationId"], exported["operationId"],
        "the same key must return the original operation"
    );

    // Restore creates the target session from the archive.
    let restored = await_cli(
        root,
        &[
            "history",
            "restore",
            "--output",
            "json",
            "--request-key",
            "t012-restore-1",
            "--archive-id",
            &archive_id,
            "--project-id",
            &project_id,
            "--new-session-id",
            "s-restored",
        ],
    );
    let restored_receipt = expect_ok("history.restore", &restored);
    assert_eq!(restored_receipt["kind"], "operation");
    let restored_session = wait_for_session(root, "s-restored").await;
    assert_eq!(restored_session["session"]["sessionState"], "open");
    let restored_history = await_cli(
        root,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            "s-restored",
        ],
    );
    let restored_history = expect_ok("history.read", &restored_history);
    assert_eq!(restored_history["items"].as_array().map(Vec::len), Some(0));

    // A missing blob is explicitly incomplete: corrupt the payload on disk
    // and both archive reads must refuse.
    let entries_blob = root
        .join("v1")
        .join("archives")
        .join(&archive_id)
        .join("entries.json");
    std::fs::remove_file(&entries_blob).expect("remove entries.json blob");
    let incomplete = await_cli(
        root,
        &[
            "archive",
            "get",
            "--output",
            "json",
            "--archive-id",
            &archive_id,
        ],
    );
    assert_eq!(
        expect_fail("archive.get", &incomplete),
        "ARCHIVE_INCOMPLETE"
    );
    let incomplete_entries = await_cli(
        root,
        &[
            "archive",
            "entries",
            "--output",
            "json",
            "--archive-id",
            &archive_id,
        ],
    );
    assert_eq!(
        expect_fail("archive.entries", &incomplete_entries),
        "ARCHIVE_INCOMPLETE"
    );

    // Unknown archives are NOT_FOUND.
    let ghost = await_cli(
        root,
        &[
            "archive",
            "get",
            "--output",
            "json",
            "--archive-id",
            "ghost-archive",
        ],
    );
    assert_eq!(expect_fail("archive.get", &ghost), "NOT_FOUND");
}

// ===========================================================================
// T06 regression coverage: #2 symlink containment, #10 session close/delete
// replay invariance, #16 project.remove closing sessions
// ===========================================================================

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_create_blocks_symlink_escape_before_probing() {
    let home = TestHome::new("t012-symlink");
    let root = home.path();
    init_home(root);
    let instance_id = init_home_instance(root);

    let parent_created = await_cli(
        root,
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--request-key",
            "t012-sym-dir-1",
            "--path",
            &server_path("default", "symparent"),
        ],
    );
    expect_ok("directories.create", &parent_created);

    let outside = TestHome::new("t012-sym-outside");
    std::fs::write(outside.path().join("sentinel.txt"), "keep").unwrap();

    let parent_dir = root.join("workspaces").join("symparent");
    assert!(parent_dir.is_dir());
    std::os::unix::fs::symlink(outside.path(), parent_dir.join("escape-dir")).unwrap();

    // Symlink intermediate parent pointing outside root fails PATH_NOT_ALLOWED
    // without touching or probing the external target.
    let escape_parent = serde_json::to_string(&json!({
        "instanceId": instance_id,
        "rootId": "default",
        "relativePath": "symparent/escape-dir",
    }))
    .unwrap();
    let res = await_cli(
        root,
        &[
            "projects",
            "create",
            "--output",
            "json",
            "--project-id",
            "p-sym-parent",
            "--parent",
            &escape_parent,
            "--directory-name",
            "child",
            "--name",
            "SymParent",
        ],
    );
    assert_eq!(expect_fail("project.create", &res), "PATH_NOT_ALLOWED");
    assert!(!outside.path().join("child").exists());

    // Symlink directly at the target leaf escaping outside root also fails PATH_NOT_ALLOWED.
    std::os::unix::fs::symlink(outside.path(), parent_dir.join("escape-leaf")).unwrap();
    let direct_parent = serde_json::to_string(&json!({
        "instanceId": instance_id,
        "rootId": "default",
        "relativePath": "symparent",
    }))
    .unwrap();
    let leaf_res = await_cli(
        root,
        &[
            "projects",
            "create",
            "--output",
            "json",
            "--project-id",
            "p-sym-leaf",
            "--parent",
            &direct_parent,
            "--directory-name",
            "escape-leaf",
            "--name",
            "SymLeaf",
        ],
    );
    assert_eq!(expect_fail("project.create", &leaf_res), "PATH_NOT_ALLOWED");
    assert_eq!(
        std::fs::read_to_string(outside.path().join("sentinel.txt")).unwrap(),
        "keep"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_remove_closes_sessions_and_blocks_reopen_or_submit() {
    let home = TestHome::new("t012-proj-rm");
    let root = home.path();
    init_home(root);
    let instance_id = init_home_instance(root);

    let dir_out = await_cli(
        root,
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--request-key",
            "t012-prm-dir",
            "--path",
            &server_path("default", "rmteam"),
        ],
    );
    expect_ok("directories.create", &dir_out);

    let parent_json = serde_json::to_string(&json!({
        "instanceId": instance_id,
        "rootId": "default",
        "relativePath": "rmteam",
    }))
    .unwrap();
    let created = await_cli(
        root,
        &[
            "projects",
            "create",
            "--output",
            "json",
            "--project-id",
            "p-rm-close",
            "--parent",
            &parent_json,
            "--directory-name",
            "alpha",
            "--name",
            "RemoveClose",
        ],
    );
    let created = expect_ok("project.create", &created);
    let proj_rev = created["project"]["revision"].as_str().unwrap().to_owned();

    let sess_out = await_cli(
        root,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            "s-rm-close",
            "--project-id",
            "p-rm-close",
        ],
    );
    expect_ok("session.create", &sess_out);

    let run = submit_run(root, "s-rm-close", "before remove", "t012-prm-run1").await;
    let run_id = run["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(root, &run_id);
    let waited = expect_envelope("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");

    let removed = await_cli(
        root,
        &[
            "projects",
            "remove",
            "--output",
            "json",
            "--project-id",
            "p-rm-close",
            "--expected-revision",
            &proj_rev,
        ],
    );
    let removed = expect_ok("project.remove", &removed);
    assert_eq!(removed["removedProjectId"], "p-rm-close");

    let sess_after = await_cli(
        root,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            "s-rm-close",
        ],
    );
    let sess_after = expect_ok("session.get", &sess_after);
    assert_eq!(sess_after["session"]["sessionState"], "closed");
    let closed_rev = sess_after["session"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let refused = await_cli(
        root,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--request-key",
            "t012-prm-run2",
            "--session-id",
            "s-rm-close",
            "--text",
            "after project remove",
        ],
    );
    assert_eq!(expect_fail("run.submit", &refused), "SESSION_CLOSED");

    let reopen_refused = await_cli(
        root,
        &[
            "sessions",
            "reopen",
            "--output",
            "json",
            "--session-id",
            "s-rm-close",
            "--expected-revision",
            &closed_rev,
        ],
    );
    assert_eq!(expect_fail("session.reopen", &reopen_refused), "NOT_FOUND");

    let hist = await_cli(
        root,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            "s-rm-close",
        ],
    );
    expect_ok("history.read", &hist);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_close_and_delete_preserve_idempotent_run_replay() {
    let home = TestHome::new("t012-close-idem");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t012ci").await;

    let first = submit_run(root, &session_id, "repeatable", "t012-ci-k1").await;
    let first_run_id = first["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(root, &first_run_id);
    let waited = expect_envelope("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");

    let rev_open = session_revision(root, &session_id).await;
    let closed = await_cli(
        root,
        &[
            "sessions",
            "close",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &rev_open,
        ],
    );
    let closed = expect_ok("session.close", &closed);
    let rev_closed = closed["session"]["revision"].as_str().unwrap().to_owned();

    // Fresh submit fails SESSION_CLOSED, while idempotent replay succeeds with same runId.
    let fresh_after_close = await_cli(
        root,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--request-key",
            "t012-ci-k2",
            "--session-id",
            &session_id,
            "--text",
            "blocked",
        ],
    );
    assert_eq!(
        expect_fail("run.submit", &fresh_after_close),
        "SESSION_CLOSED"
    );

    let replay_after_close = submit_run(root, &session_id, "repeatable", "t012-ci-k1").await;
    assert_eq!(replay_after_close["runId"], first_run_id);

    // Reopen then delete; fresh submit fails NOT_FOUND, idempotent replay still returns first_run_id.
    let reopened = await_cli(
        root,
        &[
            "sessions",
            "reopen",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &rev_closed,
        ],
    );
    let reopened = expect_ok("session.reopen", &reopened);
    let rev_reopened = reopened["session"]["revision"].as_str().unwrap().to_owned();

    let deleted = await_cli(
        root,
        &[
            "sessions",
            "delete",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &rev_reopened,
        ],
    );
    expect_ok("session.delete", &deleted);

    let fresh_after_delete = await_cli(
        root,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--request-key",
            "t012-ci-k3",
            "--session-id",
            &session_id,
            "--text",
            "blocked-deleted",
        ],
    );
    assert_eq!(expect_fail("run.submit", &fresh_after_delete), "NOT_FOUND");

    let replay_after_delete = submit_run(root, &session_id, "repeatable", "t012-ci-k1").await;
    assert_eq!(replay_after_delete["runId"], first_run_id);
}
