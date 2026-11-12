use super::*;
use pilotdeck_protocol::parse_json;
fn obj(raw: &str) -> JsObject {
    parse_json(raw).unwrap().as_object().unwrap().clone()
}
#[test]
fn sdk_mcp_references_snapshot_configs_and_keep_inline_order() {
    let mut agents = obj(
        r#"{"worker":{"mcpServers":["shared",{"local":{"type":"stdio","command":"fixture"}}]}}"#,
    );
    let mut table = vec![(
        "shared".into(),
        parse_json(r#"{"enabled":true,"config":{"type":"http","url":"http://127.0.0.1:1/mcp"}}"#)
            .unwrap(),
    )];
    resolve_sdk_agent_mcp_agents(&mut agents, &table).unwrap();
    table[0].1 = parse_json(r#"{"enabled":false,"config":{}}"#).unwrap();
    let resolved = agents.get("worker").unwrap().get("mcpServers").unwrap();
    let entries = resolved.as_object().unwrap().entries();
    assert_eq!(entries[0].0, JsString::from("shared"));
    assert_eq!(entries[1].0, JsString::from("local"));
    assert_eq!(
        resolved.get("shared").unwrap().get("url"),
        Some(&JsValue::from("http://127.0.0.1:1/mcp"))
    );
}
#[test]
fn sdk_mcp_reference_failure_codes_and_duplicate_rules_match_gateway() {
    let cases = [
        ("[]", "SDK_AGENT_MCP_REFERENCE_NOT_FOUND"),
        (
            r#"[{"enabled":false,"config":{}}]"#,
            "SDK_AGENT_MCP_REFERENCE_DISABLED",
        ),
    ];
    for (raw, code) in cases {
        let table = match parse_json(raw).unwrap() {
            ref value if matches!(value, JsValue::Array(_)) => {
                if let JsValue::Array(items) = value {
                    items
                        .iter()
                        .map(|value| ("shared".into(), value.clone()))
                        .collect::<Vec<_>>()
                } else {
                    unreachable!()
                }
            }
            _ => unreachable!(),
        };
        let mut agents = obj(r#"{"worker":{"mcpServers":["shared"]}}"#);
        assert_eq!(
            resolve_sdk_agent_mcp_agents(&mut agents, &table)
                .unwrap_err()
                .code,
            code
        );
    }
    let mut agents = obj(
        r#"{"worker":{"mcpServers":[{"x":{"type":"stdio","command":"one"}},{"x":{"type":"stdio","command":"two"}}]}}"#,
    );
    assert_eq!(
        resolve_sdk_agent_mcp_agents(&mut agents, &[])
            .unwrap_err()
            .code,
        "INVALID_MCP_SERVER"
    );
}
#[test]
fn fork_initial_prompt_has_two_durable_user_messages_and_keeps_utf16() {
    let initial = JsString::from_utf16(vec![32, 0xd800, 32]);
    let definition = SubagentDefinition {
        initial_prompt: Some(initial),
        ..Default::default()
    };
    let (input, options) = fork_input(
        &definition,
        &"directive".into(),
        &"child".into(),
        &"turn".into(),
    );
    let synthetic = match options.get("syntheticMessages").unwrap() {
        JsValue::Array(items) => items.clone(),
        _ => panic!(),
    };
    let parsed = crate::runtime::turn::runner::TurnInput::from_agent_input(
        &input,
        &options,
        Vec::new(),
        synthetic,
    )
    .unwrap();
    let messages = parsed.loop_messages();
    assert_eq!(messages.len(), 2);
    let oracle = parse_json(include_str!(
        "../../tests/fixtures/agent03-scope/expected.json"
    ))
    .unwrap();
    assert_eq!(
        JsValue::Array(messages.clone()),
        oracle.get("messages").unwrap().clone()
    );
    let content = messages[0].get("content").unwrap();
    let first = if let JsValue::Array(items) = content {
        items[0].get("text").unwrap().as_string().unwrap()
    } else {
        panic!()
    };
    assert_eq!(first.units(), &[0xd800]);
    assert_eq!(
        messages[1],
        parse_json(r#"{"role":"user","content":[{"type":"text","text":"directive"}]}"#).unwrap()
    );
    let (input, options) = fork_input(
        &SubagentDefinition {
            initial_prompt: Some("  ".into()),
            ..Default::default()
        },
        &"directive".into(),
        &"child".into(),
        &"turn".into(),
    );
    assert_eq!(input.get("text"), Some(&JsValue::from("directive")));
    assert!(options.get("syntheticMessages").is_none());
}
#[test]
fn observer_definition_is_read_only_and_does_not_inherit_execution_scopes() {
    let definition = SubagentDefinition {
        id: "review".into(),
        allowed_tools: vec!["*".into()],
        mcp_servers: Some(obj(r#"{"private":{"type":"stdio","command":"fixture"}}"#)),
        initial_prompt: Some("hidden".into()),
        memory_disabled: false,
        background: true,
        observer: Some("again".into()),
        observer_message: Some("postamble".into()),
        ..Default::default()
    };
    let scoped = super::super::observer::read_only_override(&definition);
    assert!(scoped.allowed_tools.is_empty());
    assert!(scoped.mcp_servers.is_none());
    assert!(scoped.memory_disabled);
    assert!(scoped.initial_prompt.is_none());
    assert!(scoped.observer.is_none());
    assert!(scoped.observer_message.is_none());
    assert!(scoped.observer_run);
    assert_eq!(scoped.permission_mode.as_deref(), Some("plan"));
    assert!(!scoped.background);
    let child = build_child_config(
        &obj(r#"{"sdkSkills":["secret"]}"#),
        &scoped,
        &"observer".into(),
        None,
        2.0,
    );
    assert_eq!(
        child.get("subagentMemoryDisabled"),
        Some(&JsValue::Bool(true))
    );
    assert_eq!(child.get("sdkSkills"), Some(&JsValue::Array(Vec::new())));
}
#[test]
fn observer_digest_carries_actual_activity_outcome_and_postamble() {
    let definition = SubagentDefinition {
        id: "worker".into(),
        observer_message: Some("  postamble  ".into()),
        ..Default::default()
    };
    let events = vec![
        parse_json(r#"{"type":"model_request_started","provider":"mock","model":"fixture"}"#)
            .unwrap(),
        parse_json(r#"{"type":"assistant_message","message":{}}"#).unwrap(),
    ];
    let digest =
        super::super::observer::digest(&definition, &" task ".into(), &events, Ok("final"));
    let oracle = parse_json(include_str!(
        "../../tests/fixtures/agent03-scope/expected.json"
    ))
    .unwrap();
    let expected = if let Some(JsValue::Array(results)) = oracle.get("results") {
        if let Some(JsValue::Array(calls)) = results[0].get("calls") {
            calls[1].get("directive").unwrap()
        } else {
            panic!()
        }
    } else {
        panic!()
    };
    assert_eq!(&JsValue::String(digest.clone()), expected);
    let digest = text(&digest);
    assert!(digest.contains("Observed agent: worker\nObserved directive: task"));
    assert!(digest.contains("- model request: mock/fixture\n- assistant response emitted"));
    assert!(digest.ends_with("Observed final report:\nfinal\n\npostamble"));
    assert!(digest.contains("Outcome: completed"));
}

struct GatedMcp {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    calls: std::sync::atomic::AtomicUsize,
    fail: bool,
}
impl crate::runtime::mcp::runtime::SessionMcpRuntime for GatedMcp {
    fn stop(
        &self,
    ) -> crate::runtime::mcp::runtime::McpFuture<
        '_,
        Result<(), crate::runtime::project::RuntimeError>,
    > {
        Box::pin(async move {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.entered.notify_one();
            self.release.notified().await;
            if self.fail {
                Err(crate::runtime::project::RuntimeError::new(
                    "fixture cleanup rejected",
                ))
            } else {
                Ok(())
            }
        })
    }
}
impl crate::runtime::mcp::runtime::McpRuntimePort for GatedMcp {
    fn start(
        &self,
    ) -> crate::runtime::mcp::runtime::McpFuture<
        '_,
        Result<
            Vec<crate::runtime::mcp::runtime::McpClientStatusEntry>,
            crate::runtime::project::RuntimeError,
        >,
    > {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn list_all_tools(
        &self,
    ) -> crate::runtime::mcp::runtime::McpFuture<
        '_,
        Result<
            Vec<crate::runtime::mcp::protocol::McpToolSpec>,
            crate::runtime::project::RuntimeError,
        >,
    > {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn server_info(&self, _: &str) -> Option<crate::runtime::mcp::runtime::McpRuntimeServerInfo> {
        None
    }
    fn call_tool<'a>(
        &'a self,
        _: &'a str,
        _: &'a JsString,
        _: &'a JsValue,
        _: crate::runtime::mcp::client::McpCallOptions,
    ) -> crate::runtime::mcp::runtime::McpFuture<
        'a,
        Result<
            crate::runtime::mcp::client::McpToolCallResult,
            crate::runtime::mcp::client::McpClientError,
        >,
    > {
        Box::pin(async {
            Err(crate::runtime::mcp::client::McpClientError::new(
                "Error",
                None,
                "fixture has no tool calls",
            ))
        })
    }
}
struct LeaseProbe(Arc<std::sync::atomic::AtomicBool>);
impl Drop for LeaseProbe {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}
#[tokio::test]
async fn fork_mcp_stop_waiter_cancellation_keeps_runtime_and_generation_until_real_stop() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runtime = Arc::new(GatedMcp {
        entered: entered.clone(),
        release: release.clone(),
        calls: Default::default(),
        fail: false,
    });
    let owner = ForkMcp::new(runtime.clone(), Arc::new(LeaseProbe(dropped.clone())));
    let first = owner.clone();
    let waiter = tokio::spawn(async move { first.stop().await });
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    waiter.abort();
    let _ = waiter.await;
    assert!(!dropped.load(std::sync::atomic::Ordering::Acquire));
    release.notify_one();
    owner.stop().await.unwrap();
    assert!(dropped.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(runtime.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
}
#[tokio::test]
async fn dropping_fork_mcp_starts_owned_cleanup_and_preserves_failure_receipt() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runtime = Arc::new(GatedMcp {
        entered: entered.clone(),
        release: release.clone(),
        calls: Default::default(),
        fail: true,
    });
    let owner = ForkMcp::new(runtime.clone(), Arc::new(LeaseProbe(dropped.clone())));
    let receipt = owner.start_stop();
    drop(owner);
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    assert!(!dropped.load(std::sync::atomic::Ordering::Acquire));
    release.notify_one();
    let mut receipt = receipt;
    loop {
        let outcome = receipt.borrow().clone();
        if let Some(outcome) = outcome {
            assert!(outcome.is_err());
            break;
        }
        receipt.changed().await.unwrap();
    }
    assert!(dropped.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(runtime.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
}
