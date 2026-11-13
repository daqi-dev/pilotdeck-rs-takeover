//! Independent transport controls for the fixed TS runtime domain wire and
//! provider-facing tools. These mocks never read the process environment.
use super::super::http::ExternalHttpBinding;
use super::*;
use crate::{
    host::modules::tool_context::{ToolFileState, ToolRuntimeContext},
    skills::{
        ModuleEnvironment, ModuleHttpFuture, ModuleHttpRequest, ModuleHttpResponse,
        ModuleHttpTransport, ModuleTransportError,
    },
};
use std::{collections::VecDeque, sync::Mutex};

struct Environment;
impl ModuleEnvironment for Environment {
    fn var(&self, _key: &str) -> Option<String> {
        panic!("runtime domain tests must not access credentials")
    }
}

enum Reply {
    Value(JsValue),
    Failure,
    Disconnect,
    Pending,
}
struct MockHttp {
    binding: ExternalHttpBinding,
    replies: Mutex<VecDeque<Reply>>,
    seen: Mutex<Vec<JsValue>>,
    manifests: std::sync::atomic::AtomicUsize,
}
impl ModuleHttpTransport for MockHttp {
    fn send<'a>(&'a self, request: ModuleHttpRequest) -> ModuleHttpFuture<'a> {
        Box::pin(async move {
            if request.method == "GET" {
                self.manifests.fetch_add(1, Ordering::Relaxed);
                return Ok(json_response(
                    object([
                        ("protocolVersion", "2.0".into()),
                        (
                            "implementationId",
                            JsValue::String(self.binding.implementation_id.clone()),
                        ),
                        (
                            "contract",
                            JsValue::String(self.binding.contract.clone().into()),
                        ),
                        ("transport", "module-http-v2".into()),
                        (
                            "methods",
                            JsValue::Array(
                                self.binding
                                    .methods
                                    .iter()
                                    .map(|v| JsValue::String(v.clone()))
                                    .collect(),
                            ),
                        ),
                    ]),
                    200,
                ));
            }
            let call = pilotdeck_protocol::parse_json(std::str::from_utf8(&request.body).unwrap())
                .unwrap();
            self.seen.lock().unwrap().push(call.clone());
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra POST");
            let mut response = fields([
                ("kind", "response".into()),
                ("messageId", "mock-reply".into()),
                ("inReplyTo", call.get("messageId").unwrap().clone()),
                ("requestId", call.get("requestId").unwrap().clone()),
                ("ok", JsValue::Bool(true)),
            ]);
            match reply {
                Reply::Value(result) => {
                    response.insert("payload".into(), object([("result", result)]));
                }
                Reply::Failure => {
                    response.insert("ok".into(), JsValue::Bool(false));
                    response.insert("code".into(), "DOMAIN_DENIED".into());
                    response.insert("outcome".into(), "failed".into());
                    response.insert(
                        "error".into(),
                        object([
                            ("code", "DOMAIN_DENIED".into()),
                            ("message", "domain refused".into()),
                            ("retryability", "unsafe".into()),
                            ("details", object([("reason", "synthetic".into())])),
                        ]),
                    );
                    return Ok(json_response(JsValue::Object(response), 403));
                }
                Reply::Disconnect => return Err(ModuleTransportError::Unavailable),
                Reply::Pending => return std::future::pending().await,
            }
            Ok(json_response(JsValue::Object(response), 200))
        })
    }
}
fn json_response(value: JsValue, status: u16) -> ModuleHttpResponse {
    ModuleHttpResponse {
        status,
        body: pilotdeck_protocol::stringify_json(&value)
            .unwrap()
            .unwrap()
            .into_bytes(),
    }
}
fn setup(
    module: &str,
    methods: &[&str],
    replies: Vec<Reply>,
) -> (Arc<HttpModuleClient>, Arc<MockHttp>) {
    let binding = ExternalHttpBinding {
        implementation_id: format!("example.{module}").into(),
        contract: format!("{module}/v1"),
        transport: "module-http-v2".into(),
        endpoint: "http://127.0.0.1:12345".into(),
        manifest_path: "/module-manifest".into(),
        call_path: "/v2/module/call".into(),
        methods: methods.iter().map(|method| (*method).into()).collect(),
        timeout_ms: 10.0,
        credential_env: None,
    };
    let transport = Arc::new(MockHttp {
        binding: binding.clone(),
        replies: Mutex::new(replies.into()),
        seen: Mutex::new(Vec::new()),
        manifests: std::sync::atomic::AtomicUsize::new(0),
    });
    (
        Arc::new(HttpModuleClient::new(
            binding,
            transport.clone(),
            Arc::new(Environment),
        )),
        transport,
    )
}
fn context() -> ToolRunContext {
    ToolRunContext::new(Arc::new(ToolRuntimeContext {
        fields: JsObject::new(),
        files: ToolFileState {
            read_file_state: Arc::default(),
            write_snapshots: Arc::default(),
            allowed_read_files: vec![],
        },
        abort_signal: None,
        model: None,
        elicitation: None,
        user_dialog: None,
        audit_recorder: None,
        file_history: None,
        file_update_notifier: None,
        now: None,
        progress: None,
        subagent: None,
        plan_todo: None,
        goal: None,
        plan_directory: None,
    }))
}
fn input() -> JsValue {
    object([("projectKey", "/synthetic/project".into())])
}
fn catalog() -> JsValue {
    object([(
        "items",
        JsValue::Array(vec![object([
            ("slug", "remote<&>".into()),
            ("name", "display title".into()),
            ("skillFile", "/remote/a&b/SKILL.md".into()),
            ("description", "remote description".into()),
            ("content", "catalog content is not the read result".into()),
        ])]),
    )])
}

#[tokio::test]
async fn runtime_skills_project_management_catalog_and_preserve_wire_identity() {
    let (client, mock) = setup(
        "skills",
        &["list", "read"],
        vec![
            Reply::Value(catalog()),
            Reply::Value(object([("content", "remote body".into())])),
        ],
    );
    let port = ExternalSkillModule::new(client);
    let skills = port.list(input(), None).await.unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "remote<&>".into());
    assert_eq!(skills[0].path, "/remote/a&b/SKILL.md".into());
    let name = JsString::from_utf16(vec![0xd800, 0x78]);
    let read_input = object([
        ("name", JsValue::String(name)),
        ("projectKey", JsValue::Null),
    ]);
    assert_eq!(
        port.read(read_input.clone(), None).await.unwrap(),
        Some("remote body".into())
    );
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for (index, operation) in ["list", "read"].iter().enumerate() {
        assert_eq!(seen[index].get("runId"), Some(&"skills".into()));
        assert_eq!(seen[index].get("operationId"), Some(&"skills".into()));
        assert_eq!(seen[index].get("module"), Some(&"skills".into()));
        assert_eq!(
            seen[index].get("requestId"),
            Some(&JsValue::String(
                format!("skills-{operation}-{}", index + 1).into()
            ))
        );
        assert_eq!(
            seen[index].get("payload").unwrap().get("operation"),
            Some(&(*operation).into())
        );
    }
    assert_eq!(seen[0].get("payload").unwrap().get("input"), Some(&input()));
    assert_eq!(
        seen[1].get("payload").unwrap().get("input"),
        Some(&read_input)
    );
}

#[tokio::test]
async fn domain_method_guards_precede_io_and_sequence_allocation() {
    let (client, mock) = setup(
        "skills",
        &["list"],
        vec![Reply::Value(JsValue::Array(vec![]))],
    );
    let port = ExternalSkillModule::new(client);
    let error = port.read(JsValue::Null, None).await.unwrap_err();
    assert_eq!(
        error.message,
        "Skill operation 'read' is not declared.".into()
    );
    assert!(mock.seen.lock().unwrap().is_empty());
    assert_eq!(mock.manifests.load(Ordering::Relaxed), 0);
    port.list(input(), None).await.unwrap();
    assert_eq!(
        mock.seen.lock().unwrap()[0].get("requestId"),
        Some(&"skills-list-1".into())
    );
    let (client, mock) = setup("knowledge", &[], vec![]);
    let error = ExternalKnowledgeModule::new(client)
        .call("query", JsValue::Null, None)
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "Knowledge operation 'query' is not declared.".into()
    );
    assert!(mock.seen.lock().unwrap().is_empty());
    assert_eq!(mock.manifests.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn skills_validate_only_baseline_result_fields() {
    for (result, expected) in [
        (
            JsValue::Null,
            "Skill list result must be an array or a management list result.",
        ),
        (
            JsValue::Array(vec![object([("name", "x".into())])]),
            "Skill list item requires name and path.",
        ),
        (
            JsValue::Array(vec!["bad".into()]),
            "Skill list item requires name and path.",
        ),
    ] {
        let (client, _) = setup("skills", &["list"], vec![Reply::Value(result)]);
        assert_eq!(
            ExternalSkillModule::new(client)
                .list(input(), None)
                .await
                .unwrap_err()
                .message,
            expected.into()
        );
    }
    for result in [
        JsValue::Null,
        JsValue::Undefined,
        "body".into(),
        object([("content", "body".into())]),
    ] {
        let (client, _) = setup("skills", &["read"], vec![Reply::Value(result.clone())]);
        let actual = ExternalSkillModule::new(client)
            .read(JsValue::Null, None)
            .await
            .unwrap();
        assert_eq!(
            actual,
            if matches!(result, JsValue::Null | JsValue::Undefined) {
                None
            } else {
                Some("body".into())
            }
        );
    }
    let (client, _) = setup(
        "skills",
        &["read"],
        vec![Reply::Value(object([("content", JsValue::Number(3.0))]))],
    );
    assert_eq!(
        ExternalSkillModule::new(client)
            .read(JsValue::Null, None)
            .await
            .unwrap_err()
            .message,
        "Skill read result must contain string content or be null.".into()
    );
}

#[tokio::test]
async fn external_read_skill_loads_remote_body_and_never_catalog_or_local_fallback() {
    let (client, mock) = setup(
        "skills",
        &["list", "read"],
        vec![
            Reply::Value(catalog()),
            Reply::Value("remote <raw>".into()),
            Reply::Value(JsValue::Null),
            Reply::Value("unlisted content".into()),
        ],
    );
    let port = Arc::new(ExternalSkillModule::new(client));
    let skills = port.list(input(), None).await.unwrap();
    let tool = ExternalReadSkillTool::new(port, Some("/synthetic/project".into()), skills);
    let found = tool
        .execute(object([("skillName", "remote<&>".into())]), context())
        .await
        .unwrap();
    assert_eq!(found.content[0].get("text"), Some(&"<skill>\n<name>remote&lt;&amp;&gt;</name>\n<path>/remote/a&amp;b/SKILL.md</path>\nremote <raw>\n</skill>".into()));
    let absent = tool
        .execute(object([("skillName", "remote<&>".into())]), context())
        .await
        .unwrap();
    assert_eq!(
        absent.content[0].get("text"),
        Some(&"Skill 'remote<&>' not found. Available skills: remote<&>".into())
    );
    let unlisted = tool
        .execute(object([("skillName", "unlisted".into())]), context())
        .await
        .unwrap();
    assert_eq!(
        unlisted.content[0].get("text"),
        Some(&"unlisted content".into())
    );
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    assert_eq!(
        seen[1].get("payload").unwrap().get("input"),
        Some(&object([
            ("name", "remote<&>".into()),
            ("projectKey", "/synthetic/project".into())
        ]))
    );
}

#[tokio::test]
async fn generic_knowledge_query_needs_no_credential_and_host_scope_overwrites_input() {
    let evidence = object([
        ("answer", "evidence".into()),
        ("citations", JsValue::Array(vec![])),
    ]);
    let (client, mock) = setup(
        "knowledge",
        &["query"],
        vec![Reply::Value(evidence.clone())],
    );
    let tool = KnowledgeQueryTool::new(
        Arc::new(ExternalKnowledgeModule::new(client)),
        &object([
            ("tenantId", "host-tenant".into()),
            ("actorUserId", "host-user".into()),
            ("agentId", "host-agent".into()),
        ]),
    );
    let result = tool
        .execute(
            object([
                ("query", "find evidence".into()),
                ("tenantId", "model-tenant".into()),
                ("knowledge_base_ids", JsValue::Array(vec!["a".into()])),
            ]),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(result.data, Some(evidence.clone()));
    assert_eq!(
        result.content,
        vec![object([("type", "json".into()), ("value", evidence)])]
    );
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen[0].get("runId"), Some(&"knowledge".into()));
    assert_eq!(seen[0].get("operationId"), Some(&"knowledge".into()));
    assert_eq!(seen[0].get("requestId"), Some(&"knowledge-query-1".into()));
    assert_eq!(
        seen[0].get("payload"),
        Some(&object([
            ("operation", "query".into()),
            (
                "input",
                object([
                    ("query", "find evidence".into()),
                    ("tenantId", "host-tenant".into()),
                    ("knowledge_base_ids", JsValue::Array(vec!["a".into()])),
                    ("actorUserId", "host-user".into()),
                    ("agentId", "host-agent".into()),
                ])
            ),
        ]))
    );
}

#[tokio::test]
async fn knowledge_credential_guard_rejects_model_identity_before_dispatch() {
    let (client, mock) = setup("knowledge", &["query"], vec![Reply::Value(JsValue::Null)]);
    let tool = KnowledgeQueryTool::new(
        Arc::new(ExternalKnowledgeModule::new(client)),
        &object([
            ("credentialEnv", "TEST_TOKEN".into()),
            ("tenantId", "ignored-scope".into()),
        ]),
    );
    for field in [
        "tenantId",
        "tenant_id",
        "actorUserId",
        "actor_user_id",
        "agentId",
        "agent_id",
        "modelConfigId",
        "model_config_id",
    ] {
        let error = tool
            .execute(
                object([("query", "q".into()), (field, JsValue::Null)]),
                context(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "tool_execution_failed");
        assert_eq!(
            error.message,
            "Knowledge query cannot supply identity or an SD model selection.".into()
        );
    }
    assert!(mock.seen.lock().unwrap().is_empty());
    tool.execute(object([("query", "q".into())]), context())
        .await
        .unwrap();
    assert_eq!(
        mock.seen.lock().unwrap()[0]
            .get("payload")
            .unwrap()
            .get("input"),
        Some(&object([("query", "q".into())]))
    );
}

#[tokio::test]
async fn domain_errors_preserve_structured_port_failure_and_normalize_tool_failure() {
    let (client, mock) = setup(
        "knowledge",
        &["query"],
        vec![Reply::Failure, Reply::Failure],
    );
    let port = Arc::new(ExternalKnowledgeModule::new(client));
    let error = port
        .call("query", object([("query", "q".into())]), None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "DOMAIN_DENIED".into());
    assert_eq!(error.message, "domain refused".into());
    assert_eq!(error.retryability, Some("unsafe".into()));
    assert_eq!(
        error.details.as_deref(),
        Some(&object([("reason", "synthetic".into())]))
    );
    let error = KnowledgeQueryTool::new(port, &JsValue::Null)
        .execute(object([("query", "q".into())]), context())
        .await
        .unwrap_err();
    assert_eq!(error.code, "tool_execution_failed");
    assert_eq!(error.message, "domain refused".into());
    assert!(error.details.is_none());
    assert_eq!(mock.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn domain_timeout_disconnect_and_cancellation_do_not_replay_unknown_calls() {
    for (reply, code) in [
        (Reply::Pending, "MODULE_TIMEOUT"),
        (Reply::Disconnect, "MODULE_TRANSPORT_UNAVAILABLE"),
    ] {
        let (client, mock) = setup("knowledge", &["query"], vec![reply]);
        let error = ExternalKnowledgeModule::new(client)
            .call("query", object([("query", "q".into())]), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, code.into());
        assert_eq!(error.outcome, Some("result_unknown".into()));
        assert_eq!(mock.seen.lock().unwrap().len(), 1);
    }
    let (client, mock) = setup("skills", &["read"], vec![Reply::Pending]);
    let port = ExternalSkillModule::new(client);
    let abort = Cancellation::new();
    let call = port.read(object([("name", "remote".into())]), Some(abort.clone()));
    tokio::pin!(call);
    tokio::select! { _ = &mut call => panic!("call completed before cancellation"), _ = tokio::task::yield_now() => {} }
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
    abort.abort();
    let error = call.await.unwrap_err();
    assert_eq!(error.code, "MODULE_ABORTED".into());
    assert_eq!(error.outcome, Some("result_unknown".into()));
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn contribution_array_is_catalog_only_and_remote_read_preserves_utf16() {
    let body = JsString::from_utf16(vec![0xd800, 0x26, 0x3c]);
    let (client, _) = setup(
        "skills",
        &["list", "read"],
        vec![
            Reply::Value(JsValue::Array(vec![object([
                ("name", "raw".into()),
                ("path", "/opaque/SKILL.md".into()),
            ])])),
            Reply::Value(JsValue::String(body.clone())),
        ],
    );
    let port = Arc::new(ExternalSkillModule::new(client));
    let skills = port.list(input(), None).await.unwrap();
    assert!(skills[0].content.is_none());
    assert!(skills[0].namespace.is_none());
    let tool = ExternalReadSkillTool::new(port, None, skills);
    let result = tool
        .execute(object([("skillName", "unlisted".into())]), context())
        .await
        .unwrap();
    assert_eq!(result.content[0].get("text"), Some(&JsValue::String(body)));
}

#[tokio::test]
async fn remote_catalog_distinguishes_surrogates_in_names_paths_and_optional_fields() {
    let catalog = pilotdeck_protocol::parse_json(r#"[
        {"name":"\ud800","path":"/first/\udc00<&>/SKILL.md","description":"description-\ud810","content":"catalog-\ud820","namespace":"ns-\ud830"},
        {"name":"\ud801","path":"/second/\udc01<&>/SKILL.md","description":"description-\ud811"}
    ]"#).unwrap();
    // The mock serializes these values to HTTP response bytes and the shared
    // client parses them, so the raw catalog crosses the real JSON codec seam.
    let (client, mock) = setup(
        "skills",
        &["list", "read"],
        vec![
            Reply::Value(catalog.clone()),
            Reply::Value("second-body".into()),
            Reply::Value("unlisted-body".into()),
            Reply::Value(JsValue::Null),
        ],
    );
    let port = Arc::new(ExternalSkillModule::new(client));
    let skills = port.list(input(), None).await.unwrap();
    let JsValue::Array(items) = &catalog else {
        panic!("catalog array")
    };
    assert_eq!(skills.len(), 2);
    for (skill, wire) in skills.iter().zip(items) {
        assert_eq!(
            Some(&skill.name),
            wire.get("name").and_then(JsValue::as_string)
        );
        assert_eq!(
            Some(&skill.path),
            wire.get("path").and_then(JsValue::as_string)
        );
        assert_eq!(
            skill.description.as_ref(),
            wire.get("description").and_then(JsValue::as_string)
        );
        assert_eq!(
            skill.content.as_ref(),
            wire.get("content").and_then(JsValue::as_string)
        );
        assert_eq!(
            skill.namespace.as_ref(),
            wire.get("namespace").and_then(JsValue::as_string)
        );
    }
    assert_ne!(skills[0].name, skills[1].name);
    let second = skills[1].name.clone();
    let tool = ExternalReadSkillTool::new(port, None, skills);
    let result = tool
        .execute(
            object([("skillName", JsValue::String(second.clone()))]),
            context(),
        )
        .await
        .unwrap();
    let expected = pilotdeck_protocol::parse_json(r#""<skill>\n<name>\ud801</name>\n<path>/second/\udc01&lt;&amp;&gt;/SKILL.md</path>\nsecond-body\n</skill>""#).unwrap();
    assert_eq!(result.content[0].get("text"), Some(&expected));
    // U+FFFD is not either catalog entry. A lossy conversion would turn this
    // lookup into a false match and wrap it with the first entry's path.
    let unlisted = tool
        .execute(object([("skillName", "\u{fffd}".into())]), context())
        .await
        .unwrap();
    assert_eq!(
        unlisted.content[0].get("text"),
        Some(&"unlisted-body".into())
    );
    let missing = tool
        .execute(object([("skillName", "missing".into())]), context())
        .await
        .unwrap();
    let expected = pilotdeck_protocol::parse_json(
        r#""Skill 'missing' not found. Available skills: \ud800, \ud801""#,
    )
    .unwrap();
    assert_eq!(missing.content[0].get("text"), Some(&expected));
    let seen = mock.seen.lock().unwrap();
    assert_eq!(
        seen[1].get("payload").unwrap().get("input"),
        Some(&object([("name", JsValue::String(second))]))
    );
}
