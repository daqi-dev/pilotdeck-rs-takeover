//! The six agent-facing Cron tools (`src/cron/tool/*.ts`,
//! `createCronToolDefinitions`): each one forwards to the process's
//! `CronManager` with the calling session's `cwd` as the project key. They
//! join every session's registry while the manager is enabled
//! (`CronManager.getTools`, composed into the gateway's `extraTools` by
//! `ProjectAutomationBundle.getTools`).
use std::sync::Arc;

use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};

use super::{CronError, CronManager};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::tools::{ExecutableTool, ToolError, ToolFuture, ToolOutput, ToolRunContext},
};

#[derive(Clone, Copy)]
enum Kind {
    Create,
    List,
    Update,
    Delete,
    Stop,
    RunNow,
}

/// `CRON_SCHEDULE_SCHEMA` (CronSchemas.ts), the delay arm included.
const SCHEDULE_ONCE: &str = r#"{"type":"object","required":["type","runAt"],"additionalProperties":false,"properties":{"type":{"type":"string","const":"once"},"runAt":{"type":"string"}}}"#;
const SCHEDULE_CRON: &str = r#"{"type":"object","required":["type","expression"],"additionalProperties":false,"properties":{"type":{"type":"string","const":"cron"},"expression":{"type":"string","description":"Five numeric fields: minute hour day-of-month month day-of-week (0 or 7 = Sunday). If neither day field starts with '*', they match with OR; if either starts with '*' (including */n), they match with AND. Explicit full ranges are not wildcards. '0 9 1 * 1' means the 1st of each month OR every Monday, not their intersection. Do not use '0 9 1-7 * 1' for the first Monday; schedule explicit one-time dates instead."},"timezone":{"type":"string"}}}"#;
const SCHEDULE_DELAY: &str = r#"{"type":"object","required":["type","amount","unit"],"additionalProperties":false,"properties":{"type":{"type":"string","const":"delay"},"amount":{"type":"number","exclusiveMinimum":0},"unit":{"type":"string","enum":["second","minute","hour","day"]}}}"#;

impl Kind {
    const ALL: [Kind; 6] = [
        Kind::Create,
        Kind::List,
        Kind::Update,
        Kind::Delete,
        Kind::Stop,
        Kind::RunNow,
    ];

    fn name(self) -> &'static str {
        match self {
            Kind::Create => "cron_create",
            Kind::List => "cron_list",
            Kind::Update => "cron_update",
            Kind::Delete => "cron_delete",
            Kind::Stop => "cron_stop",
            Kind::RunNow => "cron_run_now",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Kind::Create => "Create Cron Task",
            Kind::List => "List Cron Tasks",
            Kind::Update => "Update Cron Task",
            Kind::Delete => "Delete Cron Task",
            Kind::Stop => "Stop Cron Run",
            Kind::RunNow => "Run Cron Task Now",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Kind::Create => {
                "Create a one-time or recurring background Cron task that submits future work back into a session. For relative reminders like 'in 10 minutes', use schedule.type='delay'. For absolute natural-language times like tonight, tomorrow morning, or next Monday, call get_current_time first to resolve the current local time/timezone, then pass a future schedule.type='once' runAt. Recurring expressions use Unix cron day matching: if neither day field starts with '*', day-of-month and day-of-week match with OR, not AND; see the expression schema for examples."
            }
            Kind::List => "List scheduled Cron tasks and optionally recent Cron run history.",
            Kind::Update => {
                "Update the message or schedule of a scheduled Cron task. Use cron_list first to obtain the task revision. Updates to running tasks are rejected."
            }
            Kind::Delete => "Delete a scheduled Cron task and cancel its future triggers.",
            Kind::Stop => {
                "Stop a currently running Cron run. One-time tasks are removed after being stopped."
            }
            Kind::RunNow => {
                "Start one immediate run of a scheduled Cron task without changing its future schedule."
            }
        }
    }

    fn input_schema(self) -> String {
        match self {
            Kind::Create => format!(
                r#"{{"type":"object","required":["message","schedule"],"additionalProperties":false,"properties":{{"message":{{"type":"string"}},"schedule":{{"anyOf":[{SCHEDULE_ONCE},{SCHEDULE_CRON},{SCHEDULE_DELAY}]}},"sessionKey":{{"type":"string"}},"channelKey":{{"type":"string"}},"mode":{{"type":"string"}},"timezone":{{"type":"string"}}}}}}"#
            ),
            Kind::List => r#"{"type":"object","additionalProperties":false,"properties":{"includeHistory":{"type":"boolean"},"limit":{"type":"number"}}}"#.into(),
            // `CRON_TASK_SCHEDULE_SCHEMA`: no delay arm on update.
            Kind::Update => format!(
                r#"{{"type":"object","required":["taskId","expectedRevision","message","schedule"],"additionalProperties":false,"properties":{{"taskId":{{"type":"string"}},"expectedRevision":{{"type":"number","minimum":0}},"message":{{"type":"string"}},"schedule":{{"anyOf":[{SCHEDULE_ONCE},{SCHEDULE_CRON}]}},"timezone":{{"type":"string"}}}}}}"#
            ),
            Kind::Delete => r#"{"type":"object","required":["taskId"],"additionalProperties":false,"properties":{"taskId":{"type":"string"},"stopRunning":{"type":"boolean"}}}"#.into(),
            Kind::Stop => r#"{"type":"object","additionalProperties":false,"properties":{"taskId":{"type":"string"},"runId":{"type":"string"}}}"#.into(),
            Kind::RunNow => r#"{"type":"object","required":["taskId"],"additionalProperties":false,"properties":{"taskId":{"type":"string"}}}"#.into(),
        }
    }
}

/// One Cron tool bound to the manager (`createCron*Tool(runtime)`).
pub struct CronTool {
    kind: Kind,
    fields: JsObject,
    manager: Arc<CronManager>,
}

/// `createCronToolDefinitions(manager)` in its declaration order.
pub fn cron_tools(manager: &Arc<CronManager>) -> Vec<Arc<dyn ExecutableTool>> {
    Kind::ALL
        .into_iter()
        .map(|kind| {
            let mut fields = JsObject::new();
            fields.insert("name".into(), JsValue::from(kind.name()));
            fields.insert("title".into(), JsValue::from(kind.title()));
            fields.insert("description".into(), JsValue::from(kind.description()));
            fields.insert("kind".into(), JsValue::from("session"));
            fields.insert(
                "inputSchema".into(),
                parse_json(&kind.input_schema()).expect("cron tool schema is valid JSON"),
            );
            Arc::new(CronTool {
                kind,
                fields,
                manager: manager.clone(),
            }) as Arc<dyn ExecutableTool>
        })
        .collect()
}

impl ToolDefinition for CronTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(matches!(self.kind, Kind::List)))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(matches!(self.kind, Kind::List)))
    }
}

/// `{...input, ...extra}`: existing keys keep their place, new ones append.
fn spread(input: &JsValue, extra: Vec<(&str, JsValue)>) -> JsValue {
    let mut out = input.as_object().cloned().unwrap_or_default();
    for (key, value) in extra {
        out.insert(key.into(), value);
    }
    JsValue::Object(out)
}

/// `inferChannelKey(sessionKey)` (CronCreateTool.ts:44-48): the prefix
/// before the first `:`, when it is not empty.
fn infer_channel_key(session: &JsString) -> Option<JsString> {
    let units = session.units();
    let index = units.iter().position(|unit| *unit == u16::from(b':'))?;
    (index > 0).then(|| JsString::from_utf16(units[..index].to_vec()))
}

fn context_field(context: &ToolRunContext, name: &str) -> JsValue {
    context
        .fields
        .get(name)
        .cloned()
        .unwrap_or(JsValue::Undefined)
}

impl ExecutableTool for CronTool {
    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let kind = self.kind;
        let manager = self.manager.clone();
        Box::pin(async move {
            let cwd = context_field(&context, "cwd");
            let session = context_field(&context, "sessionId");
            let result = tokio::task::spawn_blocking(move || -> Result<JsValue, CronError> {
                match kind {
                    Kind::Create => {
                        let session_key = match input.get("sessionKey") {
                            Some(value) if !matches!(value, JsValue::Undefined | JsValue::Null) => {
                                value.clone()
                            }
                            _ => session.clone(),
                        };
                        let channel_key = match input.get("channelKey") {
                            Some(value) if !matches!(value, JsValue::Undefined | JsValue::Null) => {
                                Some(value.clone())
                            }
                            _ => session
                                .as_string()
                                .and_then(infer_channel_key)
                                .map(JsValue::String),
                        };
                        let mut extra = vec![("sessionKey", session_key)];
                        if let Some(channel_key) = channel_key
                            && crate::runtime::cron::truthy(&channel_key)
                        {
                            extra.push(("channelKey", channel_key));
                        }
                        extra.push(("projectKey", cwd));
                        let created = manager.create_task(&spread(&input, extra))?;
                        Ok(created.get("task").cloned().unwrap_or(JsValue::Undefined))
                    }
                    Kind::List => {
                        let base = match input {
                            JsValue::Undefined | JsValue::Null => JsValue::Object(JsObject::new()),
                            other => other,
                        };
                        manager.list_tasks(&spread(&base, vec![("projectKey", cwd)]))
                    }
                    Kind::Update => manager.update_task(&spread(&input, vec![("projectKey", cwd)])),
                    Kind::Delete => manager.delete_task(&spread(&input, vec![("projectKey", cwd)])),
                    Kind::Stop => manager.stop_task(&spread(&input, vec![("projectKey", cwd)])),
                    Kind::RunNow => {
                        manager.run_task_now(&spread(&input, vec![("projectKey", cwd)]))
                    }
                }
            })
            .await
            .map_err(|error| ToolError::failed(error.to_string()))?
            .map_err(|error| ToolError::failed(error.message().clone()))?;
            Ok(output(kind, result))
        })
    }
}

/// `{ content: [{ type: "json", value }], data: result }`. `cron_create`
/// shows the task and keeps the whole `{ task }` result as data.
fn output(kind: Kind, value: JsValue) -> ToolOutput {
    let data = match kind {
        Kind::Create => {
            let mut result = JsObject::new();
            result.insert("task".into(), value.clone());
            JsValue::Object(result)
        }
        _ => value.clone(),
    };
    let mut content = JsObject::new();
    content.insert("type".into(), JsValue::from("json"));
    content.insert("value".into(), value);
    ToolOutput {
        content: vec![JsValue::Object(content)],
        supplemental_messages: None,
        data: Some(data),
        metadata: None,
    }
}
