//! `skills-migrate` entry (`cli.compat.skills-migrate`, adapted):
//! `pilotdeck skills migrate [options]`.
//!
//! `skills.scan`/`skills.plan_import`/`skills.apply_import` are delivered
//! by S3; the three-stage orchestration calls them through the executor
//! and surfaces their failures verbatim. Registered accepted differences
//! (mapping data): the old
//! default home-layout scan (`--from` implying the claude-code/openclaw/
//! hermes directories) is NOT inherited — only explicit `--source`
//! directories are scanned, `--from` labels the origin/legacyFormat of
//! those sources; a dry run (no `--execute`) stops after planning and
//! writes nothing; partial failures exit non-zero with per-item results
//! (the old `--json` partial-success exit 0 is not kept).

use std::time::Duration;

use serde_json::{Value, json};

use crate::entry::{EntryCtx, call_op, call_op_keyed, parse_cli, usage_errors};
use crate::state;
use crate::{EXIT_FAILED, EXIT_OK, SHIM_NAME};

const APPLY_MAX_POLLS: u32 = 600;
const APPLY_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Parsed `skills migrate` arguments.
#[derive(Clone, Debug, Default)]
pub struct SkillsMigrateArgs {
    pub execute: bool,
    pub from: Option<String>,
    /// Repeatable `--source DIR`.
    pub sources: Vec<String>,
    pub overwrite: bool,
    pub rename: bool,
    pub project: Option<String>,
    pub pilot_home: Option<String>,
    pub json: bool,
}

/// Normalizes the old `--from` aliases (cc/claude/claude-code →
/// claude-code); `None` for unknown sources.
fn normalize_from(raw: &str) -> Option<String> {
    match raw {
        "cc" | "claude" | "claude-code" => Some("claude-code".to_owned()),
        "openclaw" => Some("openclaw".to_owned()),
        "hermes" => Some("hermes".to_owned()),
        "all" => Some("all".to_owned()),
        _ => None,
    }
}

/// Maps a normalized legacy source label onto the contract's `legacyFormat`
/// enum (`cc|openclaw|hermes|custom`; `state/cli-v1-operations.json`,
/// skills.plan_import input schema). The old `claude-code` label maps onto
/// `cc`; `all` maps onto `custom` (the gateway rejects `origin="legacy"`
/// without `legacyFormat`).
fn contract_legacy_format(label: &str) -> &'static str {
    match label {
        "claude-code" => "cc",
        "openclaw" => "openclaw",
        "hermes" => "hermes",
        _ => "custom",
    }
}

struct FromMapping {
    origin: &'static str,
    legacy_format: Option<&'static str>,
    report_kind: String,
}

/// Resolves `--from` (omitted, single source, `all`, or comma-separated list)
/// onto the contract's `(origin, legacyFormat)` pair without ever emitting
/// `origin="legacy"` without `legacyFormat` or silently degrading `--from`
/// lists to `origin="directory"`.
fn resolve_from_mapping(raw: Option<&str>) -> FromMapping {
    let Some(raw) = raw else {
        return FromMapping {
            origin: "directory",
            legacy_format: None,
            report_kind: "directory".to_owned(),
        };
    };
    let normalized: Vec<String> = raw.split(',').filter_map(normalize_from).collect();
    let mut formats: Vec<&'static str> = normalized
        .iter()
        .map(|label| contract_legacy_format(label))
        .collect();
    formats.sort_unstable();
    formats.dedup();
    let legacy_format = match formats.as_slice() {
        [single] => Some(*single),
        _ => Some("custom"),
    };
    let report_kind = if normalized.iter().any(|l| l == "all") {
        "all".to_owned()
    } else {
        let mut deduped = Vec::new();
        for item in normalized {
            if !deduped.contains(&item) {
                deduped.push(item);
            }
        }
        deduped.join(",")
    };
    FromMapping {
        origin: "legacy",
        legacy_format,
        report_kind,
    }
}

pub fn parse(rest: &[String]) -> Result<SkillsMigrateArgs, String> {
    if rest.first().map(String::as_str) != Some("migrate") {
        return Err("expected `skills migrate [options]`".to_owned());
    }
    let spec = crate::entry::OptSpec {
        values: &["--from", "--source", "--project", "--pilot-home"],
        flags: &["--execute", "--overwrite", "--rename", "--json"],
    };
    let parsed = parse_cli(&rest[1..], &spec)?;
    if !parsed.positionals.is_empty() {
        return Err("skills migrate takes no positional arguments".to_owned());
    }
    if parsed.flag("--overwrite") && parsed.flag("--rename") {
        return Err("--overwrite and --rename are mutually exclusive".to_owned());
    }
    let from = match parsed.value("--from") {
        None => None,
        Some(raw) => {
            // Comma-separated list; every element must be a known source.
            for item in raw.split(',') {
                if normalize_from(item).is_none() {
                    return Err(format!(
                        "Unknown skills source \"{item}\". Use cc, openclaw, hermes, or all."
                    ));
                }
            }
            Some(raw.to_owned())
        }
    };
    Ok(SkillsMigrateArgs {
        execute: parsed.flag("--execute"),
        from,
        sources: parsed
            .values_of("--source")
            .into_iter()
            .map(str::to_owned)
            .collect(),
        overwrite: parsed.flag("--overwrite"),
        rename: parsed.flag("--rename"),
        project: parsed.value("--project").map(str::to_owned),
        pilot_home: parsed.value("--pilot-home").map(str::to_owned),
        json: parsed.flag("--json"),
    })
}

/// Entry point.
pub fn main(ctx: &mut EntryCtx<'_>, rest: &[String]) -> i32 {
    main_with_poll(ctx, rest, APPLY_POLL_INTERVAL)
}

fn main_with_poll(ctx: &mut EntryCtx<'_>, rest: &[String], poll_interval: Duration) -> i32 {
    let args = match parse(rest) {
        Ok(args) => args,
        Err(message) => return usage_errors::usage(&message),
    };
    // The old default home-layout scan is not inherited: explicit source
    // directories are required.
    if args.sources.is_empty() {
        return usage_errors::usage(
            "skills migrate requires --source <dir> (implicit home scanning is not supported)",
        );
    }
    if args.pilot_home.is_some() {
        eprintln!(
            "{SHIM_NAME}: --pilot-home cannot be projected: the legacy data root is not \
             scanned by the new data source (error: unsupported_projection)"
        );
        return EXIT_FAILED;
    }
    let home = match ctx.require_home() {
        Ok(home) => home.to_path_buf(),
        Err(code) => return code,
    };
    let config = match ctx.load_config(&home) {
        Ok(config) => config,
        Err(code) => return code,
    };
    ctx.executor().set_target(config.default_target.clone());

    // Destination scope. The skills contract scope is
    // `{kind: global|project|session, id?}` with `id` required for
    // project/session and null for global (`state/cli-v1-operations.json`,
    // skills.* input schemas); `instance` is not a legal kind. `--project`
    // maps onto the project scope (the target project must exist, an
    // unknown id is rejected with INVALID_ARGUMENT and passed through);
    // without it the old instance-wide destination maps onto the global
    // scope.
    let scope = match &args.project {
        Some(project) => json!({"kind": "project", "id": project}),
        None => json!({"kind": "global"}),
    };
    let from_mapping = resolve_from_mapping(args.from.as_deref());
    let conflict_policy = if args.overwrite {
        "overwrite"
    } else if args.rename {
        "rename"
    } else {
        "skip"
    };

    // Stage 1+2 per source: scan, then plan (preview is zero-write).
    let mut all_changes: Vec<Value> = Vec::new();
    let mut plans: Vec<(String, String, String)> = Vec::new(); // (planId, planHash, expectedRevision)
    for source in &args.sources {
        let scan = match call_op(
            ctx,
            "skills.scan",
            &json!({
                "scope": scope,
                "sourceRoot": source,
                "recursive": true,
            }),
        ) {
            Ok(scan) => scan,
            Err(code) => return code,
        };
        let _ = scan;
        let mut plan_input = json!({
            "scope": scope,
            "origin": from_mapping.origin,
            "sourceRef": source,
            "conflictPolicy": conflict_policy,
        });
        if let Some(value) = from_mapping.legacy_format {
            plan_input["legacyFormat"] = json!(value);
        }
        let plan = match call_op(ctx, "skills.plan_import", &plan_input) {
            Ok(plan) => plan,
            Err(code) => return code,
        };
        if let Some(changes) = plan.get("changes").and_then(Value::as_array) {
            all_changes.extend(changes.iter().cloned());
        }
        let Some(plan_id) = plan.get("planId").and_then(Value::as_str) else {
            eprintln!("{SHIM_NAME}: plan carries no planId for source {source}");
            return EXIT_FAILED;
        };
        let plan_hash = plan
            .get("planHash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let expected_revision = plan
            .get("expectedRevision")
            .and_then(Value::as_str)
            .unwrap_or("absent")
            .to_owned();
        plans.push((plan_id.to_owned(), plan_hash, expected_revision));
    }

    // Stage 3: apply the same plans (only with --execute) and wait for each
    // async operation to reach its terminal state via `operations.get`.
    let mut applied_changes: Vec<Value> = Vec::new();
    if args.execute {
        for (plan_id, plan_hash, expected_revision) in &plans {
            let request_key = state::generate_request_key();
            let receipt = match call_op_keyed(
                ctx,
                "skills.apply_import",
                &json!({
                    "scope": scope,
                    "planId": plan_id,
                    "planHash": plan_hash,
                    "expectedRevision": expected_revision,
                }),
                &request_key,
            ) {
                Ok(receipt) => receipt,
                Err(code) => return code,
            };
            let Some(operation_id) = receipt.get("operationId").and_then(Value::as_str) else {
                eprintln!("{SHIM_NAME}: skills.apply_import receipt carries no operationId");
                return EXIT_FAILED;
            };
            if let Err(code) = crate::entry::wait_operation_terminal(
                ctx,
                "skills.apply_import",
                operation_id,
                receipt.get("state").and_then(Value::as_str),
                APPLY_MAX_POLLS,
                poll_interval,
            ) {
                return code;
            }
        }
        applied_changes = all_changes.clone();
    }

    // Old SkillMigrationReport projection.
    let summary = summarize(&all_changes, args.execute);
    let report = json!({
        "mode": if args.execute { "execute" } else { "dry-run" },
        "targetRoot": args.project.clone().unwrap_or_else(|| "instance".to_owned()),
        "sources": args.sources.iter().map(|s| json!({
            "kind": from_mapping.report_kind.clone(),
            "label": s,
            "path": s,
        })).collect::<Vec<_>>(),
        "summary": summary,
        "items": if args.execute { applied_changes } else { all_changes },
    });
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        print_text_report(&report);
    }
    // Partial failures exit non-zero (registered difference from the old
    // --json exit 0).
    let summary = report.get("summary").cloned().unwrap_or(json!({}));
    let errors = summary.get("error").and_then(Value::as_u64).unwrap_or(0);
    let conflicts = summary.get("conflict").and_then(Value::as_u64).unwrap_or(0);
    if errors > 0 || conflicts > 0 {
        return EXIT_FAILED;
    }
    EXIT_OK
}

fn classify_change_action(action: &str, executed: bool) -> &'static str {
    match action {
        "import" | "overwrite" | "rename" => {
            if executed {
                "migrated"
            } else {
                "would_migrate"
            }
        }
        "skip" => "skipped",
        "conflict" => "conflict",
        // Includes `"reject"` and any unexpected action so errors never count
        // as successes.
        _ => "error",
    }
}

/// Computes the old summary counts from the gateway contract's `{source,
/// target, action, reason}` change entries.
fn summarize(changes: &[Value], executed: bool) -> Value {
    let mut migrated = 0usize;
    let mut would_migrate = 0usize;
    let mut skipped = 0usize;
    let mut conflict = 0usize;
    let mut error = 0usize;
    for change in changes {
        let action = change.get("action").and_then(Value::as_str).unwrap_or("");
        match classify_change_action(action, executed) {
            "migrated" => migrated += 1,
            "would_migrate" => would_migrate += 1,
            "skipped" => skipped += 1,
            "conflict" => conflict += 1,
            _ => error += 1,
        }
    }
    json!({
        "migrated": migrated,
        "would_migrate": would_migrate,
        "skipped": skipped,
        "conflict": conflict,
        "error": error,
    })
}

/// Text rendering following the old format semantics (title, target,
/// summary line, per-item status marks) over the contract's `{source,
/// target, action, reason}` change items.
fn print_text_report(report: &Value) {
    let mode = report
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("dry-run");
    println!(
        "Skills migration {}",
        if mode == "execute" {
            "(EXECUTED)"
        } else {
            "(DRY RUN)"
        }
    );
    println!(
        "Target: {}",
        report
            .get("targetRoot")
            .and_then(Value::as_str)
            .unwrap_or("")
    );
    let summary = report.get("summary").cloned().unwrap_or(json!({}));
    println!(
        "Summary: migrated {}, would_migrate {}, skipped {}, conflict {}, error {}",
        summary.get("migrated").and_then(Value::as_u64).unwrap_or(0),
        summary
            .get("would_migrate")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        summary.get("skipped").and_then(Value::as_u64).unwrap_or(0),
        summary.get("conflict").and_then(Value::as_u64).unwrap_or(0),
        summary.get("error").and_then(Value::as_u64).unwrap_or(0),
    );
    if let Some(items) = report.get("items").and_then(Value::as_array) {
        for item in items {
            let action = item.get("action").and_then(Value::as_str).unwrap_or("");
            let mark = match action {
                "import" | "overwrite" | "rename" => '+',
                "conflict" => '?',
                "reject" => 'x',
                "skip" => '-',
                _ => '!',
            };
            let target = item.get("target").and_then(Value::as_str).unwrap_or("");
            let source = item.get("source").and_then(Value::as_str).unwrap_or("");
            let reason = item.get("reason").and_then(Value::as_str).unwrap_or("");
            if reason.is_empty() {
                println!("{mark} {target} (from {source})");
            } else {
                println!("{mark} {target} (from {source}: {reason})");
            }
        }
    }
    if mode != "execute" {
        println!("Dry run: no files were written. Re-run with --execute to apply.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryCtx;
    use crate::executor::{ErrorPayload, FakeExecutor, Outcome, Response};
    use crate::testutil::TempDir;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn ok(result: Value) -> Result<Response, crate::executor::ExecError> {
        Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Succeeded,
            result: Some(result),
            error: None,
        })
    }

    fn failed(code: &str) -> Result<Response, crate::executor::ExecError> {
        Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Failed,
            result: None,
            error: Some(ErrorPayload {
                code: code.to_owned(),
                message: "not served".to_owned(),
                details: None,
            }),
        })
    }

    fn ctx<'a>(exec: &'a mut FakeExecutor, home: &TempDir) -> EntryCtx<'a> {
        EntryCtx::with_home(exec, home.path())
    }

    #[test]
    fn parse_options_repeatable_sources_and_alias_normalization() {
        let args = parse(&argv(&[
            "migrate",
            "--execute",
            "--source",
            "/a",
            "--source",
            "/b",
            "--from",
            "cc,hermes",
            "--project",
            "p",
            "--json",
        ]))
        .unwrap();
        assert!(args.execute && args.json);
        assert_eq!(args.sources, ["/a", "/b"]);
        assert_eq!(args.from.as_deref(), Some("cc,hermes"));
        assert_eq!(normalize_from("cc").as_deref(), Some("claude-code"));
        assert_eq!(normalize_from("lark"), None);
    }

    #[test]
    fn parse_rejects_bad_shapes() {
        assert!(parse(&argv(&["list"])).is_err());
        assert!(parse(&argv(&["migrate", "--overwrite", "--rename"])).is_err());
        assert!(parse(&argv(&["migrate", "stray"])).is_err());
        assert!(parse(&argv(&["migrate", "--bogus"])).is_err());
        assert!(parse(&argv(&["migrate", "--from", "bogus"])).is_err());
    }

    #[test]
    fn no_sources_is_a_usage_error_not_an_implicit_home_scan() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("skills-nosources");
        assert_eq!(main(&mut ctx(&mut exec, &home), &argv(&["migrate"])), 2);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn pilot_home_is_rejected_as_unsupported_projection() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("skills-pilothome");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["migrate", "--source", "/a", "--pilot-home", "/old"]),
        );
        assert_eq!(code, EXIT_FAILED);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn dry_run_scans_and_plans_without_applying() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"candidates": []})))
            .push(ok(json!({
                "planId": "plan-1", "planHash": "h", "sourceHash": "sh",
                "expectedRevision": "rev-1",
                "changes": [{"source": "a", "target": "a", "action": "import", "reason": "imports a new skill"}],
                "diagnostics": [],
            })));
        let home = TempDir::new("skills-dryrun");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["migrate", "--source", "/a", "--from", "cc"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.methods(), ["skills.scan", "skills.plan_import"]);
        // Without --project the destination is the contract's global scope.
        assert_eq!(exec.calls[0].input["scope"], json!({"kind": "global"}));
        let plan_input = &exec.calls[1].input;
        assert_eq!(plan_input["scope"], json!({"kind": "global"}));
        assert_eq!(plan_input["origin"], "legacy");
        // The contract legacyFormat enum is cc|openclaw|hermes|custom.
        assert_eq!(plan_input["legacyFormat"], "cc");
        assert_eq!(plan_input["conflictPolicy"], "skip");
        assert_eq!(plan_input["sourceRef"], "/a");
    }

    #[test]
    fn project_scope_uses_the_contract_id_field() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"candidates": []})))
            .push(ok(json!({
                "planId": "plan-2", "planHash": "h", "sourceHash": "sh",
                "expectedRevision": "rev-2",
                "changes": [], "diagnostics": [],
            })));
        let home = TempDir::new("skills-project-scope");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["migrate", "--source", "/a", "--project", "proj-7"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            exec.calls[0].input["scope"],
            json!({"kind": "project", "id": "proj-7"})
        );
        assert_eq!(
            exec.calls[1].input["scope"],
            json!({"kind": "project", "id": "proj-7"})
        );
    }

    #[test]
    fn from_all_and_comma_list_map_to_valid_contract_legacy_format() {
        for (raw_from, expected_format) in [
            ("all", "custom"),
            ("cc,hermes", "custom"),
            ("cc,claude", "cc"),
        ] {
            let mut exec = FakeExecutor::new()
                .push(ok(json!({"candidates": []})))
                .push(ok(json!({
                    "planId": "plan-3", "planHash": "h", "sourceHash": "sh",
                    "expectedRevision": "rev-3",
                    "changes": [], "diagnostics": [],
                })));
            let home = TempDir::new("skills-from-all");
            let code = main(
                &mut ctx(&mut exec, &home),
                &argv(&["migrate", "--source", "/a", "--from", raw_from]),
            );
            assert_eq!(code, EXIT_OK, "--from {raw_from}");
            let plan_input = &exec.calls[1].input;
            assert_eq!(plan_input["origin"], "legacy", "--from {raw_from}");
            assert_eq!(
                plan_input["legacyFormat"], expected_format,
                "--from {raw_from}"
            );
        }
    }

    #[test]
    fn execute_applies_the_same_plan_and_polls_operations_get() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"candidates": []})))
            .push(ok(json!({
                "planId": "plan-9", "planHash": "hash-9", "sourceHash": "sh",
                "expectedRevision": "rev-9",
                "changes": [{"source": "x", "target": "x", "action": "overwrite", "reason": "overwrites"}],
                "diagnostics": [],
            })))
            .push(ok(json!({"kind": "operation", "operationId": "op-apply-9",
                       "durable": true, "acceptedRevision": "rev-9",
                       "deduplicated": false, "state": "queued"})))
            .push(ok(json!({"operation": {"operationId": "op-apply-9", "state": "running"}})))
            .push(ok(json!({"operation": {"operationId": "op-apply-9", "state": "succeeded"}})));
        let home = TempDir::new("skills-execute");
        let code = main_with_poll(
            &mut ctx(&mut exec, &home),
            &argv(&["migrate", "--source", "/a", "--execute", "--overwrite"]),
            Duration::ZERO,
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "skills.scan",
                "skills.plan_import",
                "skills.apply_import",
                "operations.get",
                "operations.get"
            ]
        );
        let apply = &exec.calls[2];
        assert_eq!(apply.input["planId"], "plan-9");
        assert_eq!(apply.input["planHash"], "hash-9");
        assert_eq!(apply.input["expectedRevision"], "rev-9");
        assert_eq!(apply.input["scope"], json!({"kind": "global"}));
        assert!(apply.request_key.is_some());
        assert_eq!(exec.calls[1].input["conflictPolicy"], "overwrite");
        assert_eq!(exec.calls[3].input["operationId"], "op-apply-9");
    }

    #[test]
    fn execute_fails_when_async_apply_operation_fails() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"candidates": []})))
            .push(ok(json!({
                "planId": "plan-9", "planHash": "hash-9", "sourceHash": "sh",
                "expectedRevision": "rev-9",
                "changes": [{"source": "x", "target": "x", "action": "import", "reason": "new"}],
                "diagnostics": [],
            })))
            .push(ok(
                json!({"kind": "operation", "operationId": "op-apply-fail",
                       "durable": true, "acceptedRevision": "rev-9",
                       "deduplicated": false, "state": "queued"}),
            ))
            .push(ok(json!({"operation": {
                "operationId": "op-apply-fail",
                "state": "failed",
                "error": {"code": "INTEGRITY_ERROR", "message": "source changed"}
            }})));
        let home = TempDir::new("skills-apply-fail");
        let code = main_with_poll(
            &mut ctx(&mut exec, &home),
            &argv(&["migrate", "--source", "/a", "--execute"]),
            Duration::ZERO,
        );
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(
            exec.methods(),
            [
                "skills.scan",
                "skills.plan_import",
                "skills.apply_import",
                "operations.get"
            ]
        );
    }

    #[test]
    fn summarize_and_exit_code_handle_contract_change_actions_including_reject() {
        let changes = vec![
            json!({"source": "a", "target": "a", "action": "import", "reason": "new"}),
            json!({"source": "b", "target": "b-imported", "action": "rename", "reason": "renamed"}),
            json!({"source": "c", "target": "c", "action": "overwrite", "reason": "overwritten"}),
            json!({"source": "d", "target": "d", "action": "skip", "reason": "exists"}),
            json!({"source": "bad", "target": "bad", "action": "reject", "reason": "invalid slug"}),
        ];
        assert_eq!(
            summarize(&changes, false),
            json!({
                "migrated": 0,
                "would_migrate": 3,
                "skipped": 1,
                "conflict": 0,
                "error": 1,
            })
        );
        assert_eq!(
            summarize(&changes, true),
            json!({
                "migrated": 3,
                "would_migrate": 0,
                "skipped": 1,
                "conflict": 0,
                "error": 1,
            })
        );

        let mut exec = FakeExecutor::new()
            .push(ok(json!({"candidates": []})))
            .push(ok(json!({
                "planId": "plan-r", "planHash": "h", "sourceHash": "sh",
                "expectedRevision": "rev-r",
                "changes": [
                    {"source": "a", "target": "a", "action": "import", "reason": "new"},
                    {"source": "bad", "target": "bad", "action": "reject", "reason": "invalid"}
                ],
                "diagnostics": [],
            })));
        let home = TempDir::new("skills-reject");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["migrate", "--source", "/a"]),
        );
        assert_eq!(code, EXIT_FAILED, "action=reject must exit non-zero");
    }

    #[test]
    fn main_requires_home_and_applies_default_target() {
        let mut exec = FakeExecutor::new();
        let mut no_home_ctx = EntryCtx::without_home(&mut exec);
        assert_eq!(
            main(&mut no_home_ctx, &argv(&["migrate", "--source", "/a"])),
            EXIT_FAILED
        );
        assert!(exec.calls.is_empty());

        let home = TempDir::new("skills-target");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                default_target: Some("profile-skills".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"candidates": []})))
            .push(ok(json!({
                "planId": "plan-t", "planHash": "h", "sourceHash": "sh",
                "expectedRevision": "rev-t",
                "changes": [], "diagnostics": [],
            })));
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["migrate", "--source", "/a"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.target.as_deref(), Some("profile-skills"));
    }

    #[test]
    fn capability_unavailable_passes_through() {
        let mut exec = FakeExecutor::new().push(failed("CAPABILITY_UNAVAILABLE"));
        let home = TempDir::new("skills-cap");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["migrate", "--source", "/a"]),
        );
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(exec.methods(), ["skills.scan"]);
    }
}
