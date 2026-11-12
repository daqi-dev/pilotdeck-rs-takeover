use super::{TelemetryClient, TelemetryErrorValue, defaulted, field, object, string, truthy};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct TurnTelemetryContext {
    pub owner_module: JsString,
    pub execution_kind: JsString,
    pub phase: Option<JsString>,
}
pub fn resolve_turn_context(channel_key: &JsString, telemetry: &JsValue) -> TurnTelemetryContext {
    let owner = field(telemetry, "ownerModule");
    let execution = field(telemetry, "executionKind");
    let phase = field(telemetry, "phase").as_string().cloned();
    if truthy(owner) && truthy(execution) {
        return TurnTelemetryContext {
            owner_module: owner.as_string().cloned().unwrap_or_default(),
            execution_kind: execution.as_string().cloned().unwrap_or_default(),
            phase,
        };
    }
    let prefix: Vec<_> = "always-on/".encode_utf16().collect();
    if let Some(suffix) = channel_key.units().strip_prefix(prefix.as_slice()) {
        return TurnTelemetryContext {
            owner_module: "always_on".into(),
            execution_kind: "always_on".into(),
            phase: if suffix.is_empty() {
                phase
            } else {
                Some(JsString::from_utf16(suffix.to_vec()))
            },
        };
    }
    TurnTelemetryContext {
        owner_module: defaulted(owner, "session")
            .as_string()
            .cloned()
            .unwrap_or_default(),
        execution_kind: defaulted(execution, "user_session")
            .as_string()
            .cloned()
            .unwrap_or_default(),
        phase,
    }
}
#[derive(Clone, Debug)]
pub struct GatewayTelemetryContext {
    pub session_id: JsString,
    pub run_id: JsString,
    pub channel_key: JsString,
    pub permission_mode: JsString,
    pub owner_module: JsString,
    pub execution_kind: JsString,
    pub phase: Option<JsString>,
}
pub struct GatewayAgentEventTelemetryObserver {
    telemetry: Option<Arc<dyn TelemetryClient>>,
}
impl GatewayAgentEventTelemetryObserver {
    pub fn new(telemetry: Option<Arc<dyn TelemetryClient>>) -> Self {
        Self { telemetry }
    }
    pub fn observe(&self, event: &JsValue, context: &GatewayTelemetryContext) {
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        let kind = field(event, "type")
            .as_string()
            .and_then(|s| s.to_utf8().ok())
            .unwrap_or_default();
        let subagent = kind.starts_with("subagent_");
        let execution = if subagent {
            "subagent".into()
        } else {
            context.execution_kind.clone()
        };
        let metadata = || {
            let mut out = JsObject::new();
            out.insert("runId".into(), JsValue::String(context.run_id.clone()));
            if subagent {
                out.insert("subagentId".into(), field(event, "subagentId").clone());
                out.insert("subagentType".into(), field(event, "subagentType").clone());
            }
            out
        };
        let stage = |stage, outcome, error_category: Option<&str>, metadata: JsObject| {
            let mut input = base(context, &execution);
            input.insert("loopStage".into(), string(stage));
            input.insert("outcome".into(), string(outcome));
            if let Some(category) = error_category {
                input.insert("errorCategory".into(), string(category));
            }
            input.insert(
                "sessionId".into(),
                JsValue::String(context.session_id.clone()),
            );
            input.insert("metadata".into(), JsValue::Object(metadata));
            telemetry.track_feature_loop_stage(&JsValue::Object(input));
        };
        let error = |error: JsValue,
                     stage,
                     category,
                     code: JsValue,
                     tool: Option<JsValue>,
                     metadata: JsObject| {
            let mut input = base(context, &execution);
            input.insert("loopStage".into(), string(stage));
            input.insert("errorCategory".into(), string(category));
            input.insert(
                "sessionId".into(),
                JsValue::String(context.session_id.clone()),
            );
            input.insert("code".into(), code);
            if let Some(tool) = tool {
                input.insert("toolName".into(), tool);
            }
            input.insert("metadata".into(), JsValue::Object(metadata));
            telemetry.track_error(&TelemetryErrorValue::Value(error), &JsValue::Object(input));
        };
        match kind.as_str() {
            "model_event" | "subagent_model_event" => {
                let inner = field(event, "event");
                let event_type = field(inner, "type");
                if event_type
                    .as_string()
                    .is_some_and(|s| s.equals_str("request_started"))
                {
                    let mut m = JsObject::new();
                    m.insert("runId".into(), JsValue::String(context.run_id.clone()));
                    m.insert("provider".into(), field(inner, "provider").clone());
                    m.insert("model".into(), field(inner, "model").clone());
                    if truthy(field(inner, "providerBaseUrl")) {
                        m.insert(
                            "providerBaseUrl".into(),
                            field(inner, "providerBaseUrl").clone(),
                        );
                    }
                    if subagent {
                        m.insert("subagentId".into(), field(event, "subagentId").clone());
                        m.insert("subagentType".into(), field(event, "subagentType").clone());
                    } else {
                        m.insert(
                            "permissionMode".into(),
                            JsValue::String(context.permission_mode.clone()),
                        );
                        m.insert(
                            "channelKey".into(),
                            JsValue::String(context.channel_key.clone()),
                        );
                    }
                    stage("model_request", "success", None, m);
                } else if event_type
                    .as_string()
                    .is_some_and(|s| s.equals_str("message_end"))
                {
                    stage("model_response", "success", None, metadata());
                } else if event_type
                    .as_string()
                    .is_some_and(|s| s.equals_str("error"))
                {
                    let e = field(inner, "error");
                    let mut m = JsObject::new();
                    m.insert("runId".into(), JsValue::String(context.run_id.clone()));
                    m.insert("provider".into(), field(e, "provider").clone());
                    if subagent {
                        m.insert("subagentId".into(), field(event, "subagentId").clone());
                        m.insert("subagentType".into(), field(event, "subagentType").clone());
                    }
                    error(
                        e.clone(),
                        "model_request",
                        "model_request_error",
                        field(e, "code").clone(),
                        None,
                        m,
                    );
                }
            }
            "tool_calls_detected" | "subagent_tool_calls_detected" => {
                let mut m = metadata();
                let calls = match field(event, "calls") {
                    JsValue::Array(a) => a.as_slice(),
                    _ => &[],
                };
                m.insert("toolCount".into(), JsValue::Number(calls.len() as f64));
                m.insert(
                    "toolNames".into(),
                    JsValue::Array(calls.iter().map(|v| field(v, "name").clone()).collect()),
                );
                stage("tool_prepare", "success", None, m);
            }
            "pre_tool_execute" | "post_tool_execute" | "permission_requested" => {
                let mut m = metadata();
                m.insert("toolName".into(), field(event, "toolName").clone());
                m.insert("toolCallId".into(), field(event, "toolCallId").clone());
                let post = kind == "post_tool_execute";
                let success = !post || truthy(field(event, "success"));
                if post {
                    m.insert("success".into(), field(event, "success").clone());
                }
                stage(
                    if kind == "permission_requested" {
                        "permission_check"
                    } else {
                        "tool_call"
                    },
                    if success { "success" } else { "failed" },
                    if success {
                        None
                    } else {
                        Some("tool_runtime_error")
                    },
                    m,
                );
            }
            "tool_result" | "subagent_tool_result" => {
                let result = field(event, "result");
                let failed = field(result, "type")
                    .as_string()
                    .is_some_and(|s| s.equals_str("error"));
                if failed || subagent {
                    let mut m = metadata();
                    m.insert("toolName".into(), field(result, "toolName").clone());
                    m.insert("toolCallId".into(), field(result, "toolCallId").clone());
                    if failed {
                        let e = field(result, "error");
                        error(
                            field(e, "message").clone(),
                            "tool_call",
                            infer_tool_category(field(e, "code")),
                            field(e, "code").clone(),
                            Some(field(result, "toolName").clone()),
                            m,
                        );
                    } else {
                        stage("tool_call", "success", None, m);
                    }
                }
            }
            "permission_denied" => {
                let mut m = metadata();
                m.insert("toolName".into(), field(event, "toolName").clone());
                error(
                    field(event, "reason").clone(),
                    "permission_check",
                    "permission_error",
                    string("permission_denied"),
                    Some(field(event, "toolName").clone()),
                    m,
                );
            }
            "turn_completed" => {
                let mut m = metadata();
                m.insert(
                    "stopReason".into(),
                    field(field(event, "result"), "stopReason").clone(),
                );
                m.insert(
                    "turns".into(),
                    field(field(event, "result"), "turns").clone(),
                );
                stage("loop_end", "success", None, m);
            }
            "turn_failed" => {
                let e = field(event, "error");
                error(
                    e.clone(),
                    "loop_end",
                    "loop_error",
                    field(e, "code").clone(),
                    None,
                    metadata(),
                );
            }
            "session_aborted" => {
                let mut m = metadata();
                m.insert("reason".into(), field(event, "reason").clone());
                stage("loop_end", "aborted", None, m);
            }
            _ => {}
        }
    }
}
fn base(context: &GatewayTelemetryContext, execution: &JsString) -> JsObject {
    let mut value = JsObject::new();
    for (key, item) in [
        ("module", string("session")),
        ("ownerModule", JsValue::String(context.owner_module.clone())),
        ("executionKind", JsValue::String(execution.clone())),
        (
            "phase",
            context
                .phase
                .as_ref()
                .map(|s| JsValue::String(s.clone()))
                .unwrap_or(JsValue::Undefined),
        ),
    ] {
        value.insert(key.into(), item);
    }
    value
}
fn infer_tool_category(code: &JsValue) -> &'static str {
    let text = code
        .as_string()
        .map(|s| String::from_utf16_lossy(s.units()).to_ascii_lowercase())
        .unwrap_or_default();
    if ["invalid", "argument", "param", "schema"]
        .iter()
        .any(|s| text.contains(s))
    {
        "tool_param_error"
    } else if ["parse", "json", "decode", "format"]
        .iter()
        .any(|s| text.contains(s))
    {
        "tool_result_parse_error"
    } else {
        "tool_runtime_error"
    }
}
