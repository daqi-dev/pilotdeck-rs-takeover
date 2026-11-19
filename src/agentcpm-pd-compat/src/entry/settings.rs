//! `settings` entries (`cli.compat.settings-get` / `settings-set`, both
//! adapted): `pilotdeck settings [get]` and `pilotdeck settings set <json> |
//! settings set --json <payload>`.
//!
//! Both target operations are S3-delivered (`configuration.resolve` /
//! `configuration.read_layer` / `configuration.validate` /
//! `configuration.update`); the full orchestration below calls them
//! through the executor and passes any failure through verbatim — never
//! swallowed, never fake success.
//!
//! - `get`: `configuration.resolve` on the instance scope, projected onto
//!   the old `PilotDeckResolvedSettings` shape. The old full YAML snapshot
//!   plus `loadedAt`/`contentHash`/`sources`/`diagnostics` structures have
//!   no equivalent and are listed as not projectable (stderr note).
//! - `set`: the old JSON payload (a `PilotDeckLocalSettingsUpdate` object)
//!   flattens into `changes` entries (dotted key paths, `null` leaves
//!   become `unset`), then the CAS chain runs:
//!   `configuration.read_layer` (revision baseline) →
//!   `configuration.validate` (invalid input is a usage error, exit 2) →
//!   `configuration.update` with the shim's request key and
//!   `expectedRevision`. Success prints the old single-line
//!   `{"updated":true}`; reload failures are reported honestly (issues on
//!   stderr, exit 1) instead of fake success.

use serde_json::{Value, json};

use crate::entry::run::{legacy_flags, resolve_target_or_fail};
use crate::entry::{EntryCtx, call_op, call_op_keyed, note_dropped, parse_cli, usage_errors};
use crate::state;
use crate::{EXIT_FAILED, EXIT_OK, SHIM_NAME};

/// Parsed `settings` subcommand.
#[derive(Clone, Debug)]
pub enum SettingsArgs {
    /// `settings [get]`
    Get { flags: crate::entry::Parsed },
    /// `settings set <json>` or `settings set --json <payload>`
    Set {
        payload: String,
        flags: crate::entry::Parsed,
    },
}

pub fn parse(rest: &[String]) -> Result<SettingsArgs, String> {
    let Some(first) = rest.first().map(String::as_str) else {
        return Ok(SettingsArgs::Get {
            flags: crate::entry::Parsed::default(),
        });
    };
    let spec = crate::entry::OptSpec {
        values: &["--gateway-url", "--auth-token"],
        flags: &[],
    };
    match first {
        "get" => {
            let parsed = parse_cli(&rest[1..], &spec)?;
            if !parsed.positionals.is_empty() {
                return Err("settings get takes no positional arguments".to_owned());
            }
            Ok(SettingsArgs::Get { flags: parsed })
        }
        "set" => {
            let set_spec = crate::entry::OptSpec {
                values: &["--json", "--gateway-url", "--auth-token"],
                flags: &[],
            };
            let parsed = parse_cli(&rest[1..], &set_spec)?;
            if let Some(payload) = parsed.value("--json") {
                return Ok(SettingsArgs::Set {
                    payload: payload.to_owned(),
                    flags: parsed,
                });
            }
            match parsed.positionals.as_slice() {
                [payload] => Ok(SettingsArgs::Set {
                    payload: payload.clone(),
                    flags: parsed,
                }),
                _ => Err("settings set requires exactly one <json> payload".to_owned()),
            }
        }
        other => Err(format!("unknown settings subcommand `{other}`")),
    }
}

/// Entry point.
pub fn main(ctx: &mut EntryCtx<'_>, rest: &[String]) -> i32 {
    let args = match parse(rest) {
        Ok(args) => args,
        Err(message) => return usage_errors::usage(&message),
    };
    let home = match ctx.require_home() {
        Ok(home) => home.to_path_buf(),
        Err(code) => return code,
    };
    let config = match ctx.load_config(&home) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let parsed_flags = match &args {
        SettingsArgs::Get { flags } | SettingsArgs::Set { flags, .. } => flags,
    };
    let flags = legacy_flags(parsed_flags);
    let target = match resolve_target_or_fail(&config, &flags) {
        Ok(target) => target,
        Err(code) => return code,
    };
    ctx.executor().set_target(target);

    match args {
        SettingsArgs::Get { .. } => get(ctx),
        SettingsArgs::Set { payload, .. } => set(ctx, &payload),
    }
}

/// `settings [get]`: `configuration.resolve` on the instance scope.
// S3-delivered: configuration.resolve (readOnly, sync; scope
// {"kind":"instance"} is the configuration domain's Scope oneOf first
// branch, instanceId is target-derived so its omission is legal).
fn get(ctx: &mut EntryCtx<'_>) -> i32 {
    let result = match call_op(
        ctx,
        "configuration.resolve",
        &json!({
            "scope": {"kind": "instance"},
        }),
    ) {
        Ok(result) => result,
        Err(code) => return code,
    };
    note_dropped(
        "settings get",
        &["loadedAt", "contentHash", "sources", "diagnostics"],
    );
    // Old PilotDeckResolvedSettings projection: schemaVersion, version
    // (← storedRevision), config (← values). The old full YAML snapshot
    // and provenance structures are not projectable (noted above).
    let projected = json!({
        "schemaVersion": 1,
        "version": result.get("storedRevision").cloned().unwrap_or(Value::Null),
        "config": result.get("values").cloned().unwrap_or(json!({})),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&projected).unwrap_or_else(|_| "{}".to_owned())
    );
    EXIT_OK
}

/// `settings set <json>`: read_layer → validate → update (CAS chain).
// S3-delivered: configuration.read_layer/validate (readOnly, sync) and
// configuration.update (conditionalWrite, keyed — expectedRevision from
// read_layer, changes items match `$defs.Change` {key, action: set|unset,
// value?}).
fn set(ctx: &mut EntryCtx<'_>, payload: &str) -> i32 {
    // Parse and flatten the old PilotDeckLocalSettingsUpdate payload into
    // contract `changes`; a non-object or non-scalar tree is a usage
    // error (exit 2) before any business call.
    let document: Value = match serde_json::from_str(payload) {
        Ok(document) => document,
        Err(e) => {
            return usage_errors::usage(&format!("Invalid settings JSON: {e}"));
        }
    };
    let Some(changes) = flatten_changes("", &document) else {
        return usage_errors::usage(
            "settings set expects a JSON object of settings (nested objects, scalar leaves)",
        );
    };
    if changes.is_empty() {
        return usage_errors::usage("settings set payload contains no settings");
    }

    // CAS baseline.
    let layer = match call_op(
        ctx,
        "configuration.read_layer",
        &json!({
            "scope": {"kind": "instance"},
        }),
    ) {
        Ok(layer) => layer,
        Err(code) => return code,
    };
    let Some(revision) = layer.get("revision").and_then(Value::as_str) else {
        eprintln!("{SHIM_NAME}: configuration.read_layer returned no revision");
        return EXIT_FAILED;
    };

    // Validate before writing.
    let validate_input = json!({
        "scope": {"kind": "instance"},
        "expectedRevision": revision,
        "changes": changes,
    });
    let validation = match call_op(ctx, "configuration.validate", &validate_input) {
        Ok(validation) => validation,
        Err(code) => return code,
    };
    if validation.get("valid").and_then(Value::as_bool) != Some(true) {
        // Invalid input is a usage error (exit 2), issues surfaced.
        if let Some(issues) = validation.get("issues") {
            eprintln!("{SHIM_NAME}: invalid settings: {issues}");
        } else {
            eprintln!("{SHIM_NAME}: invalid settings");
        }
        return usage_errors::usage("the settings payload did not validate");
    }

    // Conditional update with the shim's request key.
    let request_key = state::generate_request_key();
    let result = match call_op_keyed(ctx, "configuration.update", &validate_input, &request_key) {
        Ok(result) => result,
        Err(code) => return code,
    };

    // Old output was the single line {"updated":true}. Reload failures are
    // reported honestly instead of fake success.
    let issues_empty = result
        .get("issues")
        .and_then(Value::as_array)
        .is_none_or(|issues| issues.is_empty());
    if !issues_empty {
        eprintln!(
            "{SHIM_NAME}: settings stored but reload reported issues: {:?}",
            result.get("issues")
        );
        println!("{{\"updated\":true}}");
        return EXIT_FAILED;
    }
    println!("{{\"updated\":true}}");
    EXIT_OK
}

/// Flattens a nested settings object into contract changes
/// (`{key: "a.b", action: "set"|"unset", value}`); `null` leaves become
/// `unset` (the old "null clears the leaf" semantics). Returns `None` for
/// non-object roots or invalid shapes.
fn flatten_changes(prefix: &str, value: &Value) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    let map = value.as_object()?;
    for (key, leaf) in map {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match leaf {
            Value::Object(_) => out.extend(flatten_changes(&path, leaf)?),
            Value::Null => out.push(json!({"key": path, "action": "unset"})),
            scalar => out.push(json!({"key": path, "action": "set", "value": scalar})),
        }
    }
    out.sort_by(|a, b| {
        a.get("key")
            .and_then(Value::as_str)
            .cmp(&b.get("key").and_then(Value::as_str))
    });
    Some(out)
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
    fn parse_get_set_and_errors() {
        assert!(matches!(
            parse(&argv(&[])).unwrap(),
            SettingsArgs::Get { .. }
        ));
        assert!(matches!(
            parse(&argv(&["get"])).unwrap(),
            SettingsArgs::Get { .. }
        ));
        let SettingsArgs::Set { payload, .. } = parse(&argv(&["set", r#"{"a":1}"#])).unwrap()
        else {
            panic!("expected set");
        };
        assert_eq!(payload, r#"{"a":1}"#);
        let SettingsArgs::Set { payload, .. } =
            parse(&argv(&["set", "--json", r#"{"b":2}"#])).unwrap()
        else {
            panic!("expected set");
        };
        assert_eq!(payload, r#"{"b":2}"#);
        assert!(parse(&argv(&["frob"])).is_err());
        assert!(parse(&argv(&["set"])).is_err());
        assert!(parse(&argv(&["get", "stray"])).is_err());
    }

    #[test]
    fn flatten_changes_maps_null_leaves_to_unset_and_sorts() {
        let doc = json!({
            "agent": {"thinking": true, "maxContextTokens": null},
            "tools": {"webSearch": {"enabled": false}},
        });
        let changes = flatten_changes("", &doc).unwrap();
        assert_eq!(
            changes,
            vec![
                json!({"key": "agent.maxContextTokens", "action": "unset"}),
                json!({"key": "agent.thinking", "action": "set", "value": true}),
                json!({"key": "tools.webSearch.enabled", "action": "set", "value": false}),
            ]
        );
        // Non-object roots are rejected.
        assert!(flatten_changes("", &json!([1, 2])).is_none());
        assert!(flatten_changes("", &json!("x")).is_none());
        // Empty objects produce zero changes.
        assert!(flatten_changes("", &json!({})).unwrap().is_empty());
    }

    #[test]
    fn get_calls_configuration_resolve_and_projects() {
        let mut exec = FakeExecutor::new().push(ok(json!({
            "values": {"agent": {"thinking": true}},
            "storedRevision": "rev-1",
            "effectiveGeneration": "g",
            "reloadState": "applied",
        })));
        let home = TempDir::new("settings-get");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["get"]));
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.methods(), ["configuration.resolve"]);
        assert_eq!(exec.calls[0].input["scope"]["kind"], "instance");
    }

    #[test]
    fn get_passes_capability_unavailable_through() {
        let mut exec = FakeExecutor::new().push(failed("CAPABILITY_UNAVAILABLE"));
        let home = TempDir::new("settings-get-cap");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["get"]));
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(exec.methods(), ["configuration.resolve"]);
    }

    #[test]
    fn set_runs_the_full_cas_chain_with_a_request_key() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"values": {}, "revision": "rev-9"})))
            .push(ok(
                json!({"valid": true, "issues": [], "candidateDigest": "d"}),
            ))
            .push(ok(json!({
                "storedRevision": "rev-10",
                "effectiveGeneration": "g2",
                "reloadState": "applied",
                "issues": [],
            })));
        let home = TempDir::new("settings-set");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["set", r#"{"agent":{"thinking":true}}"#]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "configuration.read_layer",
                "configuration.validate",
                "configuration.update"
            ]
        );
        let update = &exec.calls[2];
        assert!(update.request_key.is_some(), "update is keyed");
        assert_eq!(update.input["expectedRevision"], "rev-9");
        assert_eq!(
            update.input["changes"],
            json!([{"key": "agent.thinking", "action": "set", "value": true}])
        );
    }

    #[test]
    fn set_rejects_invalid_json_before_any_business_call() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("settings-set-bad");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["set", "not json"]));
        assert_eq!(code, crate::EXIT_USAGE);
        let code = main(&mut ctx(&mut exec, &home), &argv(&["set", "[1]"]));
        assert_eq!(code, crate::EXIT_USAGE);
        let code = main(&mut ctx(&mut exec, &home), &argv(&["set", "{}"]));
        assert_eq!(code, crate::EXIT_USAGE);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn set_reports_validation_failure_as_usage_error() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"values": {}, "revision": "rev-1"})))
            .push(ok(
                json!({"valid": false, "issues": [{"key": "agent.thinking"}]}),
            ));
        let home = TempDir::new("settings-set-invalid");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["set", r#"{"agent":{"thinking":"yes"}}"#]),
        );
        assert_eq!(code, crate::EXIT_USAGE);
        assert_eq!(
            exec.methods(),
            ["configuration.read_layer", "configuration.validate"]
        );
    }

    #[test]
    fn set_reports_reload_issues_honestly() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"values": {}, "revision": "rev-1"})))
            .push(ok(json!({"valid": true, "issues": []})))
            .push(ok(json!({
                "storedRevision": "rev-2",
                "reloadState": "failed",
                "issues": [{"code": "RELOAD_FAILED"}],
            })));
        let home = TempDir::new("settings-set-reload");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["set", r#"{"tools":{"webSearch":{"enabled":true}}}"#]),
        );
        assert_eq!(code, EXIT_FAILED, "reload failure is not fake success");
        assert_eq!(exec.methods().len(), 3);
    }

    #[test]
    fn set_passes_revision_conflict_through() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"values": {}, "revision": "rev-1"})))
            .push(ok(json!({"valid": true, "issues": []})))
            .push(failed("REVISION_CONFLICT"));
        let home = TempDir::new("settings-set-conflict");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["set", r#"{"agent":{"thinking":false}}"#]),
        );
        assert_eq!(code, EXIT_FAILED);
    }
}
