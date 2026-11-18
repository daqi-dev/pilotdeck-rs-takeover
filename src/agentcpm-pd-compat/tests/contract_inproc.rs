//! A-layer contract tests: run every fixture scenario in-process through the
//! crate's public API with a scripted `FakeExecutor` and an isolated
//! temporary home (`EntryCtx::with_home`). Asserts exit codes, the exact
//! orchestration call sequence, per-call input subsets, request-key usage,
//! and secret exclusion from every recorded call.
//!
//! stdout/stderr projections are asserted by the B layer
//! (`contract_blackbox.rs`) — libtest captures this process's streams, so
//! printed output is not observable here. Local entry paths (help/version/
//! usage errors/rejections) run through `agentcpm_pd_compat::run`, which is
//! environment-free on those paths; business entries go through their public
//! `entry::*::main` functions so the executor stays a fake.

mod common;

use std::collections::HashSet;

use agentcpm_pd_compat::entry::{self, EntryCtx};
use agentcpm_pd_compat::executor::{ErrorPayload, ExecError, FakeExecutor, Outcome, Response};
use agentcpm_pd_compat::mappings::Mappings;
use agentcpm_pd_compat::run;
use common::{HOME_ENV, TempDir, json_subset, legacy_env_of, scenario_argv, scenario_bin_name};
use serde_json::Value;

/// Converts a fixture `respond` document into a FakeExecutor result.
fn respond_result(respond: &Value) -> Result<Response, ExecError> {
    if respond.get("drop").and_then(Value::as_bool) == Some(true) {
        return Err(ExecError::Exit {
            code: respond
                .get("exit")
                .and_then(Value::as_i64)
                .map(|c| c as i32),
            stderr: respond
                .get("stderr")
                .and_then(Value::as_str)
                .unwrap_or("response dropped")
                .to_owned(),
        });
    }
    let outcome = match respond.get("outcome").and_then(Value::as_str) {
        Some("accepted") => Outcome::Accepted,
        Some("failed") => Outcome::Failed,
        _ => Outcome::Succeeded,
    };
    let error = respond.get("error").map(|e| ErrorPayload {
        code: e
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        message: e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        details: None,
    });
    Ok(Response {
        version: None,
        request_id: None,
        outcome,
        result: respond.get("result").cloned(),
        error,
    })
}

/// Runs one scenario in-process; `Err` carries a failure description.
fn run_scenario(sc: &Value) -> Result<(), String> {
    let argv = scenario_argv(sc);

    // Multi-call binary names route through the pure rejection check.
    if scenario_bin_name(sc) != "pilotdeck" {
        let bin_name = scenario_bin_name(sc);
        let Some(rejection) =
            entry::reject::bin_name_rejection(Some(bin_name), &argv, &legacy_env_of(sc))
        else {
            return Err(format!("bin name `{bin_name}` was not rejected"));
        };
        if let Some(expected) = sc["expect"]["rejectCode"].as_str()
            && rejection.code != expected
        {
            return Err(format!(
                "reject code `{}` != expected `{expected}`",
                rejection.code
            ));
        }
        let code = rejection.emit();
        return check_exit(sc, code);
    }

    let Some(top) = argv.first() else {
        return Err("default-stdin scenarios are blackbox-only".to_owned());
    };
    if matches!(
        top.as_str(),
        "run"
            | "resume"
            | "sessions"
            | "settings"
            | "server"
            | "cron"
            | "gateway"
            | "chat"
            | "skills"
    ) {
        run_business(sc, &argv)
    } else {
        // Local paths (help/version/compat/update/unknown-option/reserved
        // words/prompt fallback) through the public entry point.
        let code = run(argv.iter().cloned());
        check_exit(sc, code)
    }
}

/// Business entry: public `entry::*::main` with a FakeExecutor and an
/// isolated seeded home.
fn run_business(sc: &Value, argv: &[String]) -> Result<(), String> {
    let home = TempDir::new("inproc-home");
    common::seed_home(sc, home.path());

    let mut exec = FakeExecutor::new();
    if let Some(script) = sc.get("fakeScript").and_then(Value::as_array) {
        for entry in script {
            let respond = entry.get("respond").cloned().unwrap_or(Value::Null);
            exec = exec.push(respond_result(&respond));
        }
    }

    let rest: Vec<String> = argv[1..].to_vec();
    let mut ctx = EntryCtx::with_home(&mut exec, home.path());
    let code = match argv[0].as_str() {
        "run" => entry::run::main(&mut ctx, &rest),
        "resume" => entry::resume::main(&mut ctx, &rest),
        "sessions" => entry::sessions::main(&mut ctx, &rest),
        "settings" => entry::settings::main(&mut ctx, &rest),
        "server" => entry::server::main(&mut ctx, &rest),
        "cron" => entry::cron::main(&mut ctx, &rest),
        "gateway" => entry::setup::main(&mut ctx, &rest),
        "chat" => entry::chat_search::main(&mut ctx, &rest),
        "skills" => entry::skills::main(&mut ctx, &rest),
        other => return Err(format!("unexpected business top `{other}`")),
    };

    let mut errors = Vec::new();
    if let Err(e) = check_exit(sc, code) {
        errors.push(e);
    }
    check_invoke_expectations(sc, &exec)
        .err()
        .into_iter()
        .for_each(|e| errors.push(e));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn check_exit(sc: &Value, code: i32) -> Result<(), String> {
    match sc["expect"]["exitCode"].as_i64() {
        Some(expected) if code as i64 == expected => Ok(()),
        Some(expected) => Err(format!("exit {code} != expected {expected}")),
        None => Err("scenario without expect.exitCode".to_owned()),
    }
}

/// invokeMethods / calls / executorTarget / requestKeyReuse /
/// sameFieldAcrossCalls / callsInputExcludes over the recorded calls.
fn check_invoke_expectations(sc: &Value, exec: &FakeExecutor) -> Result<(), String> {
    let expect = &sc["expect"];

    if let Some(expected) = expect["invokeMethods"].as_array() {
        let expected: Vec<&str> = expected.iter().filter_map(Value::as_str).collect();
        let actual = exec.methods();
        if actual != expected {
            return Err(format!(
                "invoke methods {actual:?} != expected {expected:?}"
            ));
        }
    }

    if let Some(calls) = expect["calls"].as_array() {
        // Selected-call assertions: each entry matches the first call of its
        // method at or after the running cursor (other calls may interleave;
        // the exact sequence is `invokeMethods`' job).
        let mut cursor = 0usize;
        for expected in calls {
            let method = expected["method"].as_str().unwrap_or_default();
            let Some(offset) = exec.calls[cursor..].iter().position(|c| c.method == method) else {
                return Err(format!(
                    "no `{method}` call after recorded index {cursor}: {:?}",
                    exec.methods()
                ));
            };
            let index = cursor + offset;
            cursor = index + 1;
            let call = &exec.calls[index];
            if let Some(input) = expected.get("input")
                && !json_subset(input, &call.input)
            {
                return Err(format!(
                    "call {index} (`{}`) input {} is not a superset of {input}",
                    call.method, call.input
                ));
            }
            match expected["requestKey"].as_bool() {
                Some(true) if call.request_key.is_none() => {
                    return Err(format!(
                        "call {index} ({}) carries no request key",
                        call.method
                    ));
                }
                Some(false) if call.request_key.is_some() => {
                    return Err(format!(
                        "call {index} ({}) unexpectedly carries request key {:?}",
                        call.method, call.request_key
                    ));
                }
                _ => {}
            }
            if let Some(banned) = expected["inputNotContains"].as_array() {
                let raw = serde_json::to_string(&call.input).unwrap_or_default();
                for needle in banned.iter().filter_map(Value::as_str) {
                    if raw.contains(needle) {
                        return Err(format!(
                            "call {index} ({}) input contains `{needle}`",
                            call.method
                        ));
                    }
                }
            }
        }
    }

    if let Some(target) = expect["executorTarget"].as_str()
        && exec.target.as_deref() != Some(target)
    {
        return Err(format!(
            "executor target {:?} != expected `{target}`",
            exec.target
        ));
    }

    if let Some(reuse) = expect["requestKeyReuse"].as_object() {
        let method = reuse["method"].as_str().unwrap_or_default();
        let count = reuse["count"].as_u64().unwrap_or(0) as usize;
        let keys: Vec<Option<&str>> = exec
            .calls
            .iter()
            .filter(|c| c.method == method)
            .map(|c| c.request_key.as_deref())
            .collect();
        if keys.len() != count {
            return Err(format!(
                "`{method}` invoked {} times, expected {count}",
                keys.len()
            ));
        }
        let distinct: HashSet<&str> = keys.iter().filter_map(|k| *k).collect();
        if distinct.len() != 1 {
            return Err(format!(
                "`{method}` calls do not share one request key: {keys:?}"
            ));
        }
    }

    if let Some(same) = expect["sameFieldAcrossCalls"].as_object() {
        let field = same["field"].as_str().unwrap_or_default();
        let indices: Vec<usize> = same["indices"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_u64)
                    .map(|v| v as usize)
                    .collect()
            })
            .unwrap_or_default();
        let mut values: HashSet<&str> = HashSet::new();
        for &index in &indices {
            let Some(call) = exec.calls.get(index) else {
                return Err(format!("sameFieldAcrossCalls index {index} out of range"));
            };
            match call.input.get(field).and_then(Value::as_str) {
                Some(value) => {
                    values.insert(value);
                }
                None => return Err(format!("call {index} has no string field `{field}`")),
            }
        }
        if values.len() != 1 {
            return Err(format!("field `{field}` differs across calls {indices:?}"));
        }
    }

    if let Some(banned) = expect["callsInputExcludes"].as_array() {
        for call in &exec.calls {
            let raw = serde_json::to_string(&call.input).unwrap_or_default();
            for needle in banned.iter().filter_map(Value::as_str) {
                if raw.contains(needle) {
                    return Err(format!(
                        "recorded call `{}` input contains banned `{needle}`",
                        call.method
                    ));
                }
            }
        }
    }

    Ok(())
}

#[test]
fn contract_inproc_all_fixtures() {
    // Fixture hygiene: exactly the manifest's 27 slugs on disk.
    let slugs = common::expected_slugs();
    assert_eq!(slugs.len(), 27);

    let mappings = Mappings::load().expect("mappings load");
    let mut failures: Vec<String> = Vec::new();
    let mut ran = 0usize;
    let mut skipped: Vec<String> = Vec::new();

    for slug in &slugs {
        let doc = common::load_fixture(slug);
        for scenario in doc["scenarios"].as_array().expect("scenarios") {
            let name = scenario["name"].as_str().expect("scenario name");
            match scenario.get("inproc").cloned().unwrap_or(Value::Bool(true)) {
                Value::Bool(false) => continue, // blackbox-only scenario
                Value::String(tag) if tag == "guarded" => {
                    // Needs the ambient AGENTCPM_PD_HOME to be unset; the
                    // in-process API cannot remove process env (edition 2024).
                    if std::env::var_os(HOME_ENV).is_some() {
                        skipped.push(format!("{slug}/{name} (ambient {HOME_ENV} set)"));
                        continue;
                    }
                }
                other => {
                    assert!(
                        other.as_bool().is_some(),
                        "{slug}/{name}: bad inproc tag {other:?}"
                    );
                }
            }
            ran += 1;
            if let Err(e) = run_scenario(scenario) {
                failures.push(format!("{slug}/{name}: {e}"));
            }
        }
    }

    let _ = mappings; // loaded for its side of the contract sanity
    eprintln!(
        "inproc: {ran} scenarios ran, {} skipped, {} failures",
        skipped.len(),
        failures.len()
    );
    for skip in &skipped {
        eprintln!("inproc skipped: {skip}");
    }
    assert!(
        failures.is_empty(),
        "{} inproc failure(s):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
    assert!(
        ran > 80,
        "expected most of the fixture scenarios to run in-proc, got {ran}"
    );
}
