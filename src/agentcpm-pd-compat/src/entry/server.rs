//! `server` entry (`cli.compat.server`, adapted): `pilotdeck server
//! [--port N] [--host H]`.
//!
//! Orchestration (mapping sequence): `instance.initialize` (identity check
//! with the explicit `AGENTCPM_PD_HOME` data root — the legacy
//! `~/.pilotdeck` is never reused implicitly) → `service.start` on the same
//! local instance. Registered accepted differences:
//!
//! - the service starts in `background` mode (the mapping's primary
//!   option): the old foreground-block-until-SIGINT process model is not
//!   reproduced, and the ready output is the contract-shaped projection of
//!   the `service.start` result (no byte promise);
//! - `--port`/`--host` are accepted and validated (invalid values exit 2
//!   per the mapping data) but the new `service.start` input carries no
//!   binding fields, so they cannot be forwarded — the shim notes this and
//!   the service binds per the new service contract instead of the old
//!   18789/`PILOTDECK_GATEWAY_*` defaults;
//! - the old SIGINT-then-exit-0 foreground semantics cannot be honored by
//!   a daemon-managed service; stopping uses the new `service.stop`
//!   operation.

use serde_json::json;

use crate::SHIM_NAME;
use crate::entry::{EntryCtx, call_op, parse_cli, usage_errors};

/// Parsed `server` arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerArgs {
    pub port: Option<u16>,
    pub host: Option<String>,
}

pub fn parse(rest: &[String]) -> Result<ServerArgs, String> {
    let spec = crate::entry::OptSpec {
        values: &["--port", "--host"],
        flags: &[],
    };
    let parsed = parse_cli(rest, &spec)?;
    if !parsed.positionals.is_empty() {
        return Err("server takes no positional arguments".to_owned());
    }
    let port = match parsed.value("--port") {
        None => None,
        Some(raw) => Some(
            raw.parse::<u16>()
                .map_err(|_| format!("--port expects a number, got `{raw}`"))?,
        ),
    };
    Ok(ServerArgs {
        port,
        host: parsed.value("--host").map(str::to_owned),
    })
}

/// Entry point: identity check, then service start.
pub fn main(ctx: &mut EntryCtx<'_>, rest: &[String]) -> i32 {
    let args = match parse(rest) {
        Ok(args) => args,
        Err(message) => return usage_errors::usage(&message),
    };
    let home = match ctx.require_home() {
        Ok(home) => home.to_path_buf(),
        Err(code) => return code,
    };

    // Binding options cannot map onto service.start's input schema.
    if args.port.is_some() || args.host.is_some() {
        eprintln!(
            "{SHIM_NAME}: note: --port/--host are not forwarded; the agentcpm-pd \
             service binds per its own service contract (legacy 18789 defaults are \
             not inherited)"
        );
    }

    // Identity check against the explicit data root. The legacy
    // ~/.pilotdeck is never read; AGENTCPM_PD_HOME is required above.
    let data_root = home.display().to_string();
    let identity = match call_op(ctx, "instance.initialize", &json!({"dataRoot": data_root})) {
        Ok(result) => result,
        Err(code) => return code,
    };
    let Some(instance_id) = identity
        .pointer("/identity/instanceId")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
    else {
        eprintln!("{SHIM_NAME}: instance.initialize returned no instance identity");
        return crate::EXIT_FAILED;
    };

    // Start (or attach to) the service on the same instance. Background
    // mode returns the ready result immediately (registered difference:
    // the old entry blocked in the foreground).
    let result = match call_op(
        ctx,
        "service.start",
        &json!({
            "dataRoot": data_root,
            "expectedInstanceId": instance_id,
            "mode": "background",
        }),
    ) {
        Ok(result) => result,
        Err(code) => return code,
    };

    // Old ready output: three lines (listening url / WebSocket / token).
    // New projection from the service.start result (contract-shaped, no
    // byte promise): endpoint, pid, generation.
    println!(
        "agentcpm-pd service started: {}",
        result
            .get("endpoint")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    );
    println!(
        "identity: {}",
        result
            .pointer("/identity/instanceId")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    );
    println!(
        "pid: {}",
        result
            .get("pid")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    );
    println!(
        "generation: {}",
        result
            .get("generation")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    );
    if result.get("ready").and_then(|v| v.as_bool()) != Some(true) {
        eprintln!("{SHIM_NAME}: service.start reported ready=false");
        return crate::EXIT_FAILED;
    }
    crate::EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryCtx;
    use crate::executor::{FakeExecutor, Outcome, Response};
    use crate::testutil::TempDir;
    use serde_json::json;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn ok(result: serde_json::Value) -> Result<Response, crate::executor::ExecError> {
        Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Succeeded,
            result: Some(result),
            error: None,
        })
    }

    fn ctx<'a>(exec: &'a mut FakeExecutor, home: &TempDir) -> EntryCtx<'a> {
        EntryCtx::with_home(exec, home.path())
    }

    #[test]
    fn parse_port_and_host() {
        let args = parse(&argv(&["--port", "8080", "--host", "127.0.0.1"])).unwrap();
        assert_eq!(args.port, Some(8080));
        assert_eq!(args.host.as_deref(), Some("127.0.0.1"));
        assert!(parse(&argv(&[])).unwrap() == ServerArgs::default());
        assert!(parse(&argv(&["--port", "not-a-number"])).is_err());
        assert!(parse(&argv(&["--port"])).is_err());
        assert!(parse(&argv(&["stray"])).is_err());
    }

    #[test]
    fn orchestration_initializes_then_starts_the_service() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({
                "identity": {"instanceId": "inst-1", "product": "agentcpm-pd"},
                "dataRoot": "/h",
                "initializationRevision": "r",
                "configured": true,
            })))
            .push(ok(json!({
                "identity": {"instanceId": "inst-1"},
                "pid": 4242,
                "endpoint": "http://127.0.0.1:7000",
                "generation": "g1",
                "ready": true,
            })));
        let home = TempDir::new("server-flow");
        let code = main(&mut ctx(&mut exec, &home), &argv(&[]));
        assert_eq!(code, crate::EXIT_OK);
        assert_eq!(exec.methods(), ["instance.initialize", "service.start"]);
        // Identity assertion: service.start carries the initialized id.
        assert_eq!(exec.calls[1].input["expectedInstanceId"], "inst-1");
        assert_eq!(exec.calls[1].input["mode"], "background");
        assert_eq!(
            exec.calls[0].input["dataRoot"],
            home.path().display().to_string()
        );
    }

    #[test]
    fn identity_mismatch_from_the_target_is_surfaced() {
        let mut exec = FakeExecutor::new().push(Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Failed,
            result: None,
            error: Some(crate::executor::ErrorPayload {
                code: "DATA_ROOT_OCCUPIED".to_owned(),
                message: "occupied".to_owned(),
                details: None,
            }),
        }));
        let home = TempDir::new("server-occupied");
        let code = main(&mut ctx(&mut exec, &home), &argv(&[]));
        assert_eq!(code, crate::EXIT_FAILED);
        assert_eq!(exec.methods(), ["instance.initialize"]);
    }

    #[test]
    fn not_ready_service_fails_honestly() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({
                "identity": {"instanceId": "inst-1"},
                "dataRoot": "/h",
                "initializationRevision": "r",
                "configured": true,
            })))
            .push(ok(json!({
                "identity": {"instanceId": "inst-1"},
                "pid": 1,
                "endpoint": "http://127.0.0.1:1",
                "generation": "g",
                "ready": false,
            })));
        let home = TempDir::new("server-notready");
        let code = main(&mut ctx(&mut exec, &home), &argv(&[]));
        assert_eq!(code, crate::EXIT_FAILED);
    }

    #[test]
    fn invalid_binding_options_are_usage_errors() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("server-usage");
        assert_eq!(main(&mut ctx(&mut exec, &home), &argv(&["--port", "x"])), 2);
        assert!(exec.calls.is_empty(), "usage errors precede business calls");
    }
}
