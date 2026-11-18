//! `skills-migrate` entry (`cli.compat.skills-migrate`, adapted):
//! `pilotdeck skills migrate [options]`.
//!
//! `skills.scan`/`skills.plan_import`/`skills.apply_import` are pending S3
//! (`// TODO-S3` markers): the three-stage orchestration is complete and
//! failures (today typically `CAPABILITY_UNAVAILABLE`) pass through
//! verbatim. Registered accepted differences (mapping data): the old
//! default home-layout scan (`--from` implying the claude-code/openclaw/
//! hermes directories) is NOT inherited — only explicit `--source`
//! directories are scanned, `--from` labels the origin/legacyFormat of
//! those sources; a dry run (no `--execute`) stops after planning and
//! writes nothing; partial failures exit non-zero with per-item results
//! (the old `--json` partial-success exit 0 is not kept).

use serde_json::{Value, json};

use crate::entry::{EntryCtx, call_op, call_op_keyed, parse_cli, usage_errors};
use crate::state;
use crate::{EXIT_FAILED, EXIT_OK, SHIM_NAME};

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
    // Destination scope: --project maps onto the project scope, otherwise
    // the instance scope.
    let scope = match &args.project {
        Some(project) => json!({"kind": "project", "projectId": project}),
        None => json!({"kind": "instance"}),
    };
    let legacy_format = args.from.as_deref().and_then(normalize_from);
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
        // TODO-S3: skills.scan is not delivered yet.
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
        // TODO-S3: skills.plan_import is not delivered yet.
        let mut plan_input = json!({
            "scope": scope,
            "origin": if legacy_format.is_some() { "legacy" } else { "directory" },
            "sourceRef": source,
            "conflictPolicy": conflict_policy,
        });
        if let Some(format) = &legacy_format {
            plan_input["legacyFormat"] = json!(format);
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

    // Stage 3: apply the same plans (only with --execute).
    let mut applied_changes: Vec<Value> = Vec::new();
    if args.execute {
        for (plan_id, plan_hash, expected_revision) in &plans {
            let request_key = state::generate_request_key();
            // TODO-S3: skills.apply_import is not delivered yet.
            let result = match call_op_keyed(
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
                Ok(result) => result,
                Err(code) => return code,
            };
            let _ = result;
        }
        applied_changes = all_changes.clone();
    }

    // Old SkillMigrationReport projection.
    let summary = summarize(&all_changes, args.execute);
    let report = json!({
        "mode": if args.execute { "execute" } else { "dry-run" },
        "targetRoot": args.project.clone().unwrap_or_else(|| "instance".to_owned()),
        "sources": args.sources.iter().map(|s| json!({
            "kind": legacy_format.clone().unwrap_or_else(|| "directory".to_owned()),
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

/// Computes the old summary counts from plan/apply changes.
fn summarize(changes: &[Value], executed: bool) -> Value {
    let count = |status: &str| {
        changes
            .iter()
            .filter(|c| {
                c.get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s == status)
            })
            .count()
    };
    let would = if executed { 0 } else { changes.len() };
    let migrated = if executed { changes.len() } else { 0 };
    json!({
        "migrated": migrated,
        "would_migrate": would,
        "skipped": count("skipped"),
        "conflict": count("conflict"),
        "error": count("error"),
    })
}

/// Text rendering following the old format semantics (title, target,
/// summary line, per-item status marks).
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
            let status = item.get("status").and_then(Value::as_str).unwrap_or("");
            let mark = match status {
                "migrated" | "would_migrate" => '+',
                "conflict" => '?',
                "error" => 'x',
                "skipped" => '-',
                _ => '!',
            };
            let slug = item.get("slug").and_then(Value::as_str).unwrap_or("");
            let source = item.get("sourcePath").and_then(Value::as_str).unwrap_or("");
            println!("{mark} {slug} (from {source})");
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
                "changes": [{"slug": "a", "status": "would_migrate", "sourcePath": "/a"}],
                "diagnostics": [],
            })));
        let home = TempDir::new("skills-dryrun");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["migrate", "--source", "/a", "--from", "cc"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.methods(), ["skills.scan", "skills.plan_import"]);
        let plan_input = &exec.calls[1].input;
        assert_eq!(plan_input["origin"], "legacy");
        assert_eq!(plan_input["legacyFormat"], "claude-code");
        assert_eq!(plan_input["conflictPolicy"], "skip");
        assert_eq!(plan_input["sourceRef"], "/a");
    }

    #[test]
    fn execute_applies_the_same_plan() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"candidates": []})))
            .push(ok(json!({
                "planId": "plan-9", "planHash": "hash-9", "sourceHash": "sh",
                "expectedRevision": "rev-9",
                "changes": [{"slug": "x", "status": "migrated"}],
                "diagnostics": [],
            })))
            .push(ok(json!({"receipt": {}})));
        let home = TempDir::new("skills-execute");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["migrate", "--source", "/a", "--execute", "--overwrite"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            exec.methods(),
            ["skills.scan", "skills.plan_import", "skills.apply_import"]
        );
        let apply = &exec.calls[2];
        assert_eq!(apply.input["planId"], "plan-9");
        assert_eq!(apply.input["planHash"], "hash-9");
        assert_eq!(apply.input["expectedRevision"], "rev-9");
        assert!(apply.request_key.is_some());
        assert_eq!(exec.calls[1].input["conflictPolicy"], "overwrite");
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
