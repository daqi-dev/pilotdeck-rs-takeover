//! Application-owned deployment selection shared by main and child sessions.
use super::{Deployment, resolve_deployment};
use crate::config::Env;
use pilotdeck_protocol::{JsString, JsValue};
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct LoopSelection {
    pub deployment: Deployment,
    /// The project binding owns identity validation. Environment-selected
    /// modules, like the baseline, may advertise any implementation id.
    pub expected_module_id: Option<JsString>,
}

/// Resolve the normalized project snapshot before the environment fallback.
/// The snapshot loader already validates enabled external bindings; malformed
/// hand-built snapshots fail closed here rather than selecting a native loop.
pub fn select_loop(snapshot: &JsValue, env: &Env, cwd: &Path) -> Result<LoopSelection, String> {
    let binding = snapshot
        .get("config")
        .and_then(|config| config.get("modules"))
        .and_then(|modules| modules.get("agentLoop"));
    if let Some(binding) = binding.filter(|binding| {
        binding.get("enabled") == Some(&JsValue::Bool(true))
            && binding.get("implementationId").is_some()
    }) {
        let text = |key: &str| -> Result<String, String> {
            binding
                .get(key)
                .and_then(JsValue::as_string)
                .and_then(|value| value.to_utf8().ok())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("Invalid agentLoop binding {key}."))
        };
        let expected_module_id = Some(text("implementationId")?.into());
        let deployment = match text("transport")?.as_str() {
            "module-stdio-v2" => {
                let command = text("command")?;
                let args = match binding.get("args") {
                    None => Vec::new(),
                    Some(JsValue::Array(args)) => args
                        .iter()
                        .map(|arg| {
                            arg.as_string()
                                .and_then(|value| value.to_utf8().ok())
                                .ok_or_else(|| "Invalid agentLoop binding args.".to_string())
                        })
                        .collect::<Result<_, _>>()?,
                    _ => return Err("Invalid agentLoop binding args.".into()),
                };
                let mut child_env = env.clone();
                if let Some(configured) = binding.get("env") {
                    let configured = configured
                        .as_object()
                        .ok_or("Invalid agentLoop binding env.")?;
                    for (name, value) in configured.entries() {
                        let name = name
                            .to_utf8()
                            .map_err(|_| "Invalid agentLoop binding env key.")?;
                        let value = value
                            .as_string()
                            .and_then(|value| value.to_utf8().ok())
                            .ok_or("Invalid agentLoop binding env value.")?;
                        child_env.insert(name, value);
                    }
                }
                Deployment::Stdio {
                    command,
                    args,
                    cwd: cwd.to_path_buf(),
                    env: child_env,
                }
            }
            "module-tcp-v2" => {
                let port = binding
                    .get("port")
                    .and_then(JsValue::as_number)
                    .filter(|value| value.fract() == 0.0 && (1.0..=65535.0).contains(value))
                    .ok_or("Invalid agentLoop binding port.")? as u16;
                let connect_timeout_ms = binding
                    .get("connectTimeoutMs")
                    .map(|value| {
                        value
                            .as_number()
                            .filter(|number| {
                                number.is_finite()
                                    && number.fract() == 0.0
                                    && *number > 0.0
                                    && *number <= u64::MAX as f64
                            })
                            .map(|number| number as u64)
                            .ok_or("Invalid agentLoop binding connectTimeoutMs.")
                    })
                    .transpose()?;
                Deployment::Tcp {
                    host: text("host")?,
                    port,
                    connect_timeout_ms,
                }
            }
            _ => return Err("Invalid agentLoop binding transport.".into()),
        };
        return Ok(LoopSelection {
            deployment,
            expected_module_id,
        });
    }
    let deployment = resolve_deployment(env, cwd)?;
    let expected_module_id =
        matches!(deployment, Deployment::Native).then(|| "pilotdeck-agent-loop".into());
    Ok(LoopSelection {
        deployment,
        expected_module_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::parse_json;

    #[test]
    fn project_binding_wins_over_environment_and_preserves_implementation_identity() {
        let env = Env::from([
            ("PILOTDECK_AGENT_LOOP_TRANSPORT".into(), "tcp".into()),
            ("SHARED".into(), "host".into()),
        ]);
        let snapshot = parse_json(r#"{"config":{"modules":{"agentLoop":{"enabled":true,"implementationId":"custom-loop","transport":"module-stdio-v2","command":"my-loop","args":["--serve",""],"env":{"SHARED":"project","EXTRA":"value"}}}}}"#).unwrap();
        let selection = select_loop(&snapshot, &env, Path::new("/project")).unwrap();
        assert_eq!(selection.expected_module_id, Some("custom-loop".into()));
        let Deployment::Stdio {
            command,
            args,
            cwd,
            env,
        } = selection.deployment
        else {
            panic!("stdio binding")
        };
        assert_eq!(command, "my-loop");
        assert_eq!(args, ["--serve", ""]);
        assert_eq!(cwd, Path::new("/project"));
        assert_eq!(env.get("SHARED").map(String::as_str), Some("project"));
        assert_eq!(env.get("EXTRA").map(String::as_str), Some("value"));
    }

    #[test]
    fn default_is_native_and_external_environment_has_no_forced_identity() {
        let snapshot = JsValue::Undefined;
        assert_eq!(
            select_loop(&snapshot, &Env::new(), Path::new("/project")).unwrap(),
            LoopSelection {
                deployment: Deployment::Native,
                expected_module_id: Some("pilotdeck-agent-loop".into())
            }
        );
        let env = Env::from([("PILOTDECK_AGENT_LOOP_TRANSPORT".into(), "stdio".into())]);
        let selected = select_loop(&snapshot, &env, Path::new("/project")).unwrap();
        assert_eq!(selected.expected_module_id, None);
        let Deployment::Stdio { command, args, .. } = selected.deployment else {
            panic!("stdio")
        };
        assert!(command.ends_with("pilotdeck-agent-loop-sidecar"));
        assert!(args.is_empty());
    }

    #[test]
    fn project_tcp_has_its_own_address_and_timeout() {
        let snapshot = parse_json(r#"{"config":{"modules":{"agentLoop":{"enabled":true,"implementationId":"custom-loop","transport":"module-tcp-v2","host":"127.0.0.2","port":1234,"connectTimeoutMs":91}}}}"#).unwrap();
        let selection = select_loop(&snapshot, &Env::new(), Path::new("/project")).unwrap();
        assert_eq!(
            selection.deployment,
            Deployment::Tcp {
                host: "127.0.0.2".into(),
                port: 1234,
                connect_timeout_ms: Some(91)
            }
        );
    }
}
