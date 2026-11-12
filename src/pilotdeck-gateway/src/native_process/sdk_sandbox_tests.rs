use super::*;
use pilotdeck_protocol::parse_json;
#[test]
fn exact_tool_kind_name_policy_matrix_matches_fixed_ts_function_oracle() {
    let oracle = parse_json(include_str!("../../tests/fixtures/sdk-sandbox/oracle.json")).unwrap();
    let JsValue::Array(records) = oracle.get("records").unwrap() else {
        panic!("matrix")
    };
    assert_eq!(records.len(), 2280);
    for row in records {
        let tool = row.get("tool").unwrap();
        let name = tool.get("name").unwrap().as_string().unwrap();
        let kind = tool.get("kind").and_then(JsValue::as_string);
        assert_eq!(
            tool_allowed(row.get("sandbox"), name, kind),
            row.get("allowed").unwrap().as_bool().unwrap(),
            "{row:?}"
        );
    }
}
#[test]
fn restrictions_are_session_local_and_read_only_is_the_exact_baseline_name_set() {
    let readonly = parse_json(r#"{"type":"tool_policy","filesystem":"read_only"}"#).unwrap();
    let denied = parse_json(r#"{"filesystem":"deny"}"#).unwrap();
    let name = JsString::from("session_filesystem_probe");
    let kind = JsString::from("filesystem");
    assert!(
        tool_allowed(Some(&readonly), &name, Some(&kind)),
        "read_only does not invent a blanket filesystem deny"
    );
    assert!(!tool_allowed(Some(&denied), &name, Some(&kind)));
    assert!(
        tool_allowed(None, &name, Some(&kind)),
        "another session is unaffected"
    );
    assert!(!tool_allowed(Some(&readonly), &"write_file".into(), None));
    assert!(
        tool_allowed(Some(&readonly), &"Write".into(), None),
        "aliases are removed by canonical registration, not guessed here"
    );
    for restriction in [
        r#"{"process":"deny"}"#,
        r#"{"filesystem":"read_only"}"#,
        r#"{"filesystem":"deny"}"#,
        r#"{"network":"deny"}"#,
    ] {
        let policy = parse_json(restriction).unwrap();
        for name in ["execute_code", "agent", "task_run", "task_"] {
            assert!(!tool_allowed(Some(&policy), &name.into(), None));
        }
        assert!(tool_allowed(Some(&policy), &"subagent".into(), None));
        assert!(tool_allowed(Some(&policy), &"send_message".into(), None));
    }
}

mod native_calls {
    use super::*;
    use crate::{
        extensions::{NativeExtension, NativeExtensions},
        host::{HostError, modules::capability::ToolDefinition},
        native_process::{
            LocalGateway, LocalOptions, tests::sdk_output_controls_tests::ControlProvider,
        },
        runtime::tools::{ExecutableTool, ToolFuture, ToolOutput, ToolRunContext},
    };
    use std::{
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("sdk-sandbox-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("home")).unwrap();
            std::fs::create_dir_all(root.join("project")).unwrap();
            Self(root.canonicalize().unwrap())
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct Probe {
        fields: pilotdeck_protocol::JsObject,
        executions: Arc<AtomicUsize>,
    }
    impl ToolDefinition for Probe {
        fn fields(&self) -> &pilotdeck_protocol::JsObject {
            &self.fields
        }
        fn is_read_only(&self, _: &JsValue) -> Result<JsValue, HostError> {
            Ok(JsValue::Bool(true))
        }
    }
    impl ExecutableTool for Probe {
        fn execute(&self, _: JsValue, _: ToolRunContext) -> ToolFuture<ToolOutput> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ToolOutput {
                    content: vec![
                        parse_json(r#"{"type":"text","text":"SANDBOX-PROBE-EXECUTED"}"#).unwrap(),
                    ],
                    supplemental_messages: None,
                    data: None,
                    metadata: None,
                })
            })
        }
    }
    fn extensions(
        project: &std::path::Path,
        counter: Arc<AtomicUsize>,
    ) -> (NativeExtensions, crate::extensions::ExtensionRegistration) {
        let extensions = NativeExtensions::default();
        let registration=extensions.register("sdk-sandbox-probe",Some(project),NativeExtension{tools:vec![Arc::new(Probe{fields:parse_json(r#"{"name":"sdk_sandbox_probe","kind":"custom","description":"Local counter capability for session policy verification","inputSchema":{"type":"object"}}"#).unwrap().as_object().unwrap().clone(),executions:counter})],..Default::default()}).unwrap();
        (extensions, registration)
    }
    fn start(
        scratch: &Scratch,
        provider: &ControlProvider,
        extensions: NativeExtensions,
    ) -> LocalGateway {
        let home = scratch.0.join("home");
        let config = home.join("pilotdeck.yaml");
        std::fs::write(&config,format!("schemaVersion: 1\nagent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://{}/v1\n      apiKey: fixture-local-only\n      models: {{mock: {{capabilities: {{supportsToolUse: true}}}}}}\ntelemetry: {{enabled: false}}\n",provider.address)).unwrap();
        let env = [
            ("HOME", home.to_string_lossy().into_owned()),
            ("PILOT_HOME", home.to_string_lossy().into_owned()),
            (
                "PILOTDECK_CONFIG_PATH",
                config.to_string_lossy().into_owned(),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect();
        LocalGateway::start_with_extensions(
            LocalOptions {
                pilot_home: home.clone(),
                project: scratch.0.join("project"),
                os_home: Some(home),
                sidecar: scratch.0.join("absent-sidecar"),
                env,
                builtin_skills_root: None,
                builtin_plugins_root: None,
                telemetry_context: None,
            },
            extensions,
        )
        .unwrap()
    }
    fn run(
        gateway: &LocalGateway,
        session: &str,
        marker: &str,
        policy: Option<serde_json::Value>,
    ) -> Vec<JsValue> {
        let mut config = serde_json::json!({"permissionMode":"bypassPermissions"});
        if let Some(policy) = policy {
            config["sandbox"] = policy;
        }
        let input = serde_json::json!({"sessionKey":session,"channelKey":"test","message":marker,"sdkSessionConfig":config});
        let events = Arc::new(Mutex::new(Vec::new()));
        let capture = events.clone();
        gateway
            .submit_turn(
                parse_json(&input.to_string()).unwrap(),
                Arc::new(move |event| capture.lock().unwrap().push(event)),
            )
            .expect("valid SDK policy is implemented before the config gate is opened");
        let result = events.lock().unwrap().clone();
        assert!(!result.is_empty());
        result
    }
    fn advertised(requests: &[serde_json::Value], marker: &str, expected: bool) {
        let requests: Vec<_> = requests
            .iter()
            .filter(|r| r.get("tools").is_some() && r["messages"].to_string().contains(marker))
            .collect();
        assert!(!requests.is_empty(), "real model request must execute");
        for request in requests {
            let tools = request["tools"].as_array().unwrap();
            assert_eq!(
                tools
                    .iter()
                    .any(|t| t["function"]["name"] == "sdk_sandbox_probe"),
                expected,
                "{request}"
            );
        }
    }
    fn refused(events: &[JsValue]) {
        assert!(
            events.iter().any(|event| event.get("type")
                == Some(&JsValue::from("tool_call_finished"))
                && event.get("ok") == Some(&JsValue::Bool(false))),
            "forced model tool call must be rejected by the real native execution path: {events:?}"
        );
    }
    #[test]
    fn policies_reject_actual_native_calls_without_affecting_other_sessions_or_next_turns() {
        for (index, policy) in [
            serde_json::json!({"process":"deny"}),
            serde_json::json!({"filesystem":"deny"}),
            serde_json::json!({"filesystem":"read_only"}),
            serde_json::json!({"network":"deny"}),
        ]
        .into_iter()
        .enumerate()
        {
            let scratch = Scratch::new();
            let provider = ControlProvider::start("sdk_sandbox_probe");
            let counter = Arc::new(AtomicUsize::new(0));
            let (extensions, _registration) =
                extensions(&scratch.0.join("project"), counter.clone());
            let gateway = start(&scratch, &provider, extensions);
            let marker = format!("SANDBOX-DENIED-{index}");
            let events = run(&gateway, "restricted", &marker, Some(policy.clone()));
            refused(&events);
            assert_eq!(counter.load(Ordering::SeqCst), 0);
            advertised(&provider.requests.lock().unwrap(), &marker, false);
            let allowed = format!("SANDBOX-ALLOWED-{index}");
            run(&gateway, "unrestricted", &allowed, None);
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            advertised(&provider.requests.lock().unwrap(), &allowed, true);
            let again = format!("SANDBOX-RESTRICTED-AGAIN-{index}");
            let events = run(&gateway, "restricted", &again, Some(policy));
            refused(&events);
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            advertised(&provider.requests.lock().unwrap(), &again, false);
            gateway.shutdown(Duration::from_secs(10)).unwrap();
        }
    }
    #[test]
    fn rebuilt_native_gateway_applies_same_policy_and_keeps_unrestricted_session_separate() {
        let scratch = Scratch::new();
        let provider = ControlProvider::start("sdk_sandbox_probe");
        let counter = Arc::new(AtomicUsize::new(0));
        let (extensions, _registration) = extensions(&scratch.0.join("project"), counter.clone());
        let first = start(&scratch, &provider, extensions.clone());
        let policy = serde_json::json!({"type":"tool_policy","process":"deny"});
        refused(&run(
            &first,
            "restricted-restart",
            "SANDBOX-BEFORE-REBUILD",
            Some(policy.clone()),
        ));
        first.shutdown(Duration::from_secs(10)).unwrap();
        drop(first);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        let rebuilt = start(&scratch, &provider, extensions);
        refused(&run(
            &rebuilt,
            "restricted-restart",
            "SANDBOX-AFTER-REBUILD",
            Some(policy),
        ));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        run(
            &rebuilt,
            "fresh-unrestricted",
            "SANDBOX-ALLOWED-AFTER-REBUILD",
            None,
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        advertised(
            &provider.requests.lock().unwrap(),
            "SANDBOX-AFTER-REBUILD",
            false,
        );
        advertised(
            &provider.requests.lock().unwrap(),
            "SANDBOX-ALLOWED-AFTER-REBUILD",
            true,
        );
        rebuilt.shutdown(Duration::from_secs(10)).unwrap();
    }
}
