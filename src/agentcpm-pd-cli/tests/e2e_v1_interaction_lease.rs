//! Interaction-lease end-to-end suite (t011, `interaction.claim` /
//! `interaction.release`, capability a016).
//!
//! Every scenario drives the real `agentcpm-pd` binary over the real `/v1/ws`
//! transport (embedded gateway auto-start). Because each `await_cli`
//! invocation is its own process and its own connection, the whole suite is
//! also the cross-process/cross-connection lease-persistence proof: leases
//! acquired by one CLI process conflict and release correctly from the next
//! (007 §7.4 — interactions bind to the principal and run, not to a
//! connection).
//!
//! Determinism rules follow the WP5 suite: bounded polls for positive
//! assertions; the single real-time wait sleeps strictly past a 1 s lease ttl
//! (waiting FOR expiry, never racing it).

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use common_v1::*;

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

/// Bounded poll until the pending question interaction of `run_id` shows up
/// in `interaction.list`, returning (interactionId, revision).
async fn poll_pending_question(home: &Path, run_id: &str) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let out = await_cli(
            home,
            &[
                "interaction",
                "list",
                "--output",
                "json",
                "--kind",
                "question",
                "--state",
                "pending",
            ],
        );
        let listed = expect_ok("interaction.list", &out);
        for item in listed["items"].as_array().cloned().unwrap_or_default() {
            if item["runId"].as_str() == Some(run_id) {
                return (
                    item["interactionId"].as_str().unwrap().to_owned(),
                    item["revision"].as_str().unwrap().to_owned(),
                );
            }
        }
        assert!(
            Instant::now() < deadline,
            "no pending question interaction for run `{run_id}`"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn interaction_get(home: &Path, interaction_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "interaction",
            "get",
            "--output",
            "json",
            "--interaction-id",
            interaction_id,
        ],
    );
    expect_ok("interaction.get", &out)["interaction"].clone()
}

/// The full lease lifecycle over the public CLI: claim, renew, competing
/// owners rejected, overlong ttl rejected, respond while leased (the lease
/// grants no respond right), release after the answer, re-claim — then the
/// expiry/takeover flow with a 1 s lease.
#[tokio::test]
async fn claim_release_and_competition_lifecycle() {
    let home = TestHome::new("ia-lease");
    init_home(home.path());
    let (_project_id, session_id) = seed_project_session(home.path(), "lease").await;

    // --- First flow: one question dialog through claim -> respond -> release.
    let run = submit_run(
        home.path(),
        &session_id,
        "[pd-driver:question]",
        "ia-lease-q1",
    )
    .await;
    let run_id = run["runId"].as_str().unwrap().to_owned();
    let (interaction_id, revision) = poll_pending_question(home.path(), &run_id).await;

    // Claim: contract-shaped result (schema-validated by expect_ok).
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &revision,
            "--lease-id",
            "lease-a",
        ],
    );
    let claim = expect_ok("interaction.claim", &out);
    assert_eq!(claim["leaseId"], "lease-a");
    assert!(claim["expiresAt"].as_str().is_some());
    let rev_after_claim = claim["revision"].as_str().unwrap().to_owned();
    assert_ne!(rev_after_claim, revision);

    // The S2 read path reports an opaque non-secret owner tag instead of
    // leaking the fencing `leaseId` credential.
    let got = interaction_get(home.path(), &interaction_id).await;
    let owner_a = got["leaseOwner"]
        .as_str()
        .expect("leaseOwner present")
        .to_owned();
    assert!(owner_a.starts_with("lease-") && owner_a != "lease-a");
    assert_eq!(got["state"], "pending");

    // A different competing owner is rejected (不同 owner 竞争拒绝), without
    // exposing the active `leaseId` in the error and leaving the lease intact.
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &rev_after_claim,
            "--lease-id",
            "lease-b",
        ],
    );
    assert!(!out.stdout.contains("lease-a"));
    assert_eq!(
        expect_failed("interaction.claim", &out),
        "LEASE_CONFLICT",
        "competing claim must be LEASE_CONFLICT"
    );
    assert_eq!(
        interaction_get(home.path(), &interaction_id).await["leaseOwner"],
        owner_a
    );

    // An overlong ttl is explicitly rejected (超长 ttl 明确拒绝) — the usage
    // exit code 2 for INVALID_ARGUMENT.
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &rev_after_claim,
            "--lease-id",
            "lease-b",
            "--ttl-ms",
            "300001",
        ],
    );
    assert_eq!(
        expect_failed("interaction.claim", &out),
        "INVALID_ARGUMENT",
        "overlong ttl must be INVALID_ARGUMENT"
    );
    assert_eq!(out.code, 2, "INVALID_ARGUMENT maps to the usage exit code");

    // A stale revision is rejected too (conditionalWrite).
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &revision,
            "--lease-id",
            "lease-a",
        ],
    );
    assert_eq!(
        expect_failed("interaction.claim", &out),
        "REVISION_CONFLICT",
        "stale revision must be REVISION_CONFLICT"
    );

    // Renewing with the same lease id keeps the lease (5-minute boundary ttl
    // is accepted) and moves the revision.
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &rev_after_claim,
            "--lease-id",
            "lease-a",
            "--ttl-ms",
            "300000",
        ],
    );
    let renew = expect_ok("interaction.claim", &out);
    let rev_after_renew = renew["revision"].as_str().unwrap().to_owned();
    assert_ne!(rev_after_renew, rev_after_claim);

    // Active lease enforces fencing token on `interaction.respond`: missing or
    // mismatched `leaseId` is rejected with `LEASE_CONFLICT` and zero side
    // effects before the holder answers cleanly with the matching `leaseId`.
    let out_missing = await_cli(
        home.path(),
        &[
            "interaction",
            "respond",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &rev_after_renew,
            "--answer",
            "{\"text\":\"unauthorized answer\"}",
        ],
    );
    assert_eq!(
        expect_failed("interaction.respond", &out_missing),
        "LEASE_CONFLICT",
        "responding without active leaseId must return LEASE_CONFLICT"
    );
    let out_wrong = await_cli(
        home.path(),
        &[
            "interaction",
            "respond",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &rev_after_renew,
            "--lease-id",
            "lease-wrong",
            "--answer",
            "{\"text\":\"wrong lease answer\"}",
        ],
    );
    assert_eq!(
        expect_failed("interaction.respond", &out_wrong),
        "LEASE_CONFLICT",
        "responding with mismatched leaseId must return LEASE_CONFLICT"
    );

    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "respond",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &rev_after_renew,
            "--lease-id",
            "lease-a",
            "--answer",
            "{\"text\":\"leased answer\"}",
        ],
    );
    let responded = expect_ok("interaction.respond", &out);
    assert_eq!(responded["interaction"]["state"], "answered");
    assert_eq!(responded["interaction"]["leaseOwner"], Value::Null);
    let rev_after_answer = responded["interaction"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let waited = wait_run(home.path(), &run_id);
    let waited = expect_envelope("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");

    // Claiming the answered dialog is rejected.
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &rev_after_answer,
            "--lease-id",
            "lease-b",
        ],
    );
    assert_eq!(
        expect_failed("interaction.claim", &out),
        "REVISION_CONFLICT",
        "claim on an answered dialog must be REVISION_CONFLICT"
    );

    // Releasing after an answer that already consumed the lease is an idempotent
    // no-op (`released: false`), leaving the dialog answered and unleased.
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "release",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &rev_after_answer,
            "--lease-id",
            "lease-a",
        ],
    );
    let released = expect_ok("interaction.release", &out);
    assert_eq!(released["released"], false);
    let got = interaction_get(home.path(), &interaction_id).await;
    assert_eq!(got["state"], "answered");
    assert_eq!(got["leaseOwner"], Value::Null);

    // --- Second flow: a 1 s lease expires, another owner takes over, the old
    // lease cannot release the new owner, and the freed dialog is claimable.
    let run2 = submit_run(
        home.path(),
        &session_id,
        "[pd-driver:question]",
        "ia-lease-q2",
    )
    .await;
    let run2_id = run2["runId"].as_str().unwrap().to_owned();
    let (iid2, rev2) = poll_pending_question(home.path(), &run2_id).await;

    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &iid2,
            "--expected-revision",
            &rev2,
            "--lease-id",
            "lease-x",
            "--ttl-ms",
            "1000",
        ],
    );
    let short = expect_ok("interaction.claim", &out);
    let rev2_after_claim = short["revision"].as_str().unwrap().to_owned();

    // Wait strictly past the ttl — waiting FOR expiry, not racing a read.
    tokio::time::sleep(Duration::from_millis(1400)).await;

    // The expired lease no longer blocks another owner.
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &iid2,
            "--expected-revision",
            &rev2_after_claim,
            "--lease-id",
            "lease-y",
        ],
    );
    let takeover = expect_ok("interaction.claim", &out);
    let rev2_after_takeover = takeover["revision"].as_str().unwrap().to_owned();
    let owner_y = interaction_get(home.path(), &iid2).await["leaseOwner"]
        .as_str()
        .expect("leaseOwner after takeover")
        .to_owned();
    assert!(owner_y.starts_with("lease-") && owner_y != "lease-y");

    // The old lease cannot release the new owner (旧 lease 不能释放新 owner).
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "release",
            "--output",
            "json",
            "--interaction-id",
            &iid2,
            "--expected-revision",
            &rev2_after_takeover,
            "--lease-id",
            "lease-x",
        ],
    );
    assert_eq!(
        expect_failed("interaction.release", &out),
        "LEASE_CONFLICT",
        "old lease must not release the new owner"
    );

    // The matching lease releases; the dialog is claimable again.
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "release",
            "--output",
            "json",
            "--interaction-id",
            &iid2,
            "--expected-revision",
            &rev2_after_takeover,
            "--lease-id",
            "lease-y",
        ],
    );
    let released = expect_ok("interaction.release", &out);
    assert_eq!(released["released"], true);
    let rev2_after_release = released["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &iid2,
            "--expected-revision",
            &rev2_after_release,
            "--lease-id",
            "lease-z",
        ],
    );
    let reclaimed = expect_ok("interaction.claim", &out);
    assert_eq!(reclaimed["leaseId"], "lease-z");

    // Unknown interactions answer NOT_FOUND on both operations.
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            "ia-missing",
            "--expected-revision",
            "rev-00000001",
            "--lease-id",
            "lease-a",
        ],
    );
    assert_eq!(
        expect_failed("interaction.claim", &out),
        "NOT_FOUND",
        "missing interaction must be NOT_FOUND"
    );
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "release",
            "--output",
            "json",
            "--interaction-id",
            "ia-missing",
            "--expected-revision",
            "rev-00000001",
            "--lease-id",
            "lease-a",
        ],
    );
    assert_eq!(
        expect_failed("interaction.release", &out),
        "NOT_FOUND",
        "missing interaction must be NOT_FOUND"
    );
}

/// Releasing a dialog nobody ever leased reports `released: false` and the
/// current revision — the idempotent observation path.
#[tokio::test]
async fn release_without_a_live_lease_reports_false() {
    let home = TestHome::new("ia-lease-release-false");
    init_home(home.path());
    let (_project_id, session_id) = seed_project_session(home.path(), "rel-false").await;

    let run = submit_run(
        home.path(),
        &session_id,
        "[pd-driver:question]",
        "ia-lease-rel-1",
    )
    .await;
    let run_id = run["runId"].as_str().unwrap().to_owned();
    let (interaction_id, revision) = poll_pending_question(home.path(), &run_id).await;

    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "release",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &revision,
            "--lease-id",
            "lease-never",
        ],
    );
    let released = expect_ok("interaction.release", &out);
    assert_eq!(released["released"], false);
    assert_eq!(released["revision"].as_str(), Some(revision.as_str()));
    // The dialog is untouched and still claimable.
    let got = interaction_get(home.path(), &interaction_id).await;
    assert_eq!(got["state"], "pending");
    assert_eq!(got["leaseOwner"], Value::Null);
    let out = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &revision,
            "--lease-id",
            "lease-a",
        ],
    );
    expect_ok("interaction.claim", &out);
}

#[tokio::test]
async fn release_on_expired_interaction_after_gateway_restart_cleans_up() {
    let home = TestHome::new("ia-lease-expired-rel");
    init_home(home.path());
    let (_project_id, session_id) = seed_project_session(home.path(), "ia-exprel").await;

    let run = submit_run(
        home.path(),
        &session_id,
        "[pd-driver:question]",
        "ia-exprel-1",
    )
    .await;
    let run_id = run["runId"].as_str().unwrap().to_owned();
    let (interaction_id, revision) = poll_pending_question(home.path(), &run_id).await;

    let claimed = await_cli(
        home.path(),
        &[
            "interaction",
            "claim",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &revision,
            "--lease-id",
            "lease-secret",
        ],
    );
    let _ = expect_ok("interaction.claim", &claimed);

    // Restart the isolated gateway so pending interactions transition to `expired`.
    let pid = home.gateway_pid().expect("gateway pid");
    signal_pid(pid, rustix::process::Signal::KILL);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let after_restart = interaction_get(home.path(), &interaction_id).await;
    assert_eq!(after_restart["state"], "expired");
    let exp_rev = after_restart["revision"].as_str().unwrap().to_owned();

    // Release on an already-expired interaction clears residual lease state
    // and answers `released: false` even under a different leaseId.
    let rel = await_cli(
        home.path(),
        &[
            "interaction",
            "release",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &exp_rev,
            "--lease-id",
            "lease-other",
        ],
    );
    let rel_ok = expect_ok("interaction.release", &rel);
    assert_eq!(rel_ok["released"], false);
    assert_eq!(rel_ok["revision"], exp_rev);
    assert_eq!(
        interaction_get(home.path(), &interaction_id).await["leaseOwner"],
        Value::Null
    );
}
