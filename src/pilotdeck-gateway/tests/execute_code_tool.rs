//! `execute_code` paths the black-box scenarios (`scripts/bbx/scenarios/
//! tool-execute-code*.json`) cannot reach: a host without the recursion hook,
//! an aborted run, and the identity of a nested call as the permission layer
//! sees it. Needs `python3` on PATH, like the tool itself.
#![cfg(unix)]
#![allow(clippy::result_large_err)]
use pilotdeck_gateway::{
    host::{
        HostFuture,
        modules::{
            capability::{RuntimeContext, ToolCall, ToolDefinition},
            permission::PermissionDecision,
            tool_context::{HostToolCheckpoint, ToolRuntimeContext},
        },
    },
    runtime::tools::{
        ExecutableTool, SchedulerMode, ToolCancellation, ToolRunContext, ToolRuntime,
        builtin::{
            execute_code::{ExecuteCodeTool, ExecuteCodeToolOptions},
            read_only_tools,
        },
        registry::ToolRegistry,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in entries {
        result.insert(key.into(), value);
    }
    result
}

fn text(value: Option<&JsValue>) -> String {
    value
        .and_then(JsValue::as_string)
        .map(|value| String::from_utf16_lossy(value.units()))
        .unwrap_or_default()
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

struct Workdir(PathBuf);
impl Workdir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("pdrs-execute-code-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn context(cwd: &Workdir, signal: Option<ToolCancellation>) -> ToolRunContext {
    let checkpoint = HostToolCheckpoint::default();
    let cwd = cwd.0.to_string_lossy().into_owned();
    ToolRunContext::new(Arc::new(ToolRuntimeContext {
        fields: object([
            ("cwd", JsValue::from(cwd.as_str())),
            ("sessionId", JsValue::from("s1")),
            ("turnId", JsValue::from("t1")),
            ("permissionMode", JsValue::from("bypassPermissions")),
            (
                "permissionContext",
                JsValue::Object(object([
                    ("mode", JsValue::from("bypassPermissions")),
                    ("cwd", JsValue::from(cwd.as_str())),
                ])),
            ),
        ]),
        files: checkpoint.tool_context_state(),
        abort_signal: signal.map(|signal| Arc::new(signal) as RuntimeContext),
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

/// Allows everything and records `(tool name, tool call id)` per decision.
#[derive(Default)]
struct Recording(Mutex<Vec<(String, String)>>);
impl PermissionDecision for Recording {
    fn decide(
        &self,
        tool: Arc<dyn ToolDefinition>,
        _: JsValue,
        _: RuntimeContext,
        tool_call_id: JsString,
    ) -> HostFuture<JsValue> {
        self.0.lock().unwrap().push((
            text(tool.fields().get("name")),
            String::from_utf16_lossy(tool_call_id.units()),
        ));
        Box::pin(async { Ok(JsValue::Object(object([("type", JsValue::from("allow"))]))) })
    }
}

fn runtime(permission: Arc<Recording>) -> ToolRuntime {
    let registry = ToolRegistry::default();
    registry
        .register(Arc::new(ExecuteCodeTool::new(
            ExecuteCodeToolOptions::default(),
        )))
        .unwrap();
    for tool in read_only_tools() {
        registry.register(tool).unwrap();
    }
    ToolRuntime::new(registry, permission, SchedulerMode::Sequential)
}

fn call(code: &str) -> ToolCall {
    ToolCall {
        id: "outer".into(),
        name: "execute_code".into(),
        input: JsValue::Object(object([("code", JsValue::from(code))])),
    }
}

#[test]
fn a_host_without_the_recursion_hook_is_unsupported() {
    let cwd = Workdir::new();
    let tool = ExecuteCodeTool::new(ExecuteCodeToolOptions::default());
    let output = block_on(tool.execute(call("print(1)").input, context(&cwd, None))).unwrap();
    let data = output.data.unwrap();
    assert_eq!(text(data.get("status")), "unsupported");
    assert_eq!(
        text(data.get("error")),
        "execute_code requires a ToolRuntime recursion hook, but this host did not provide one."
    );
    assert_eq!(text(data.get("output")), "");
}

#[test]
fn an_abort_cancels_the_script_and_kills_it() {
    let cwd = Workdir::new();
    let signal = ToolCancellation::default();
    let runtime = runtime(Arc::default());
    let started = Instant::now();
    let result = block_on(async {
        let run = runtime.execute(
            call("import time\nprint('started', flush=True)\ntime.sleep(60)\n"),
            context(&cwd, Some(signal.clone())),
        );
        let abort = async {
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            signal.cancel();
        };
        tokio::join!(run, abort).0
    })
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_eq!(text(result.get("type")), "success");
    let data = result.get("data").unwrap();
    assert_eq!(text(data.get("status")), "cancelled");
    assert_eq!(text(data.get("error")), "Script execution was cancelled.");
    assert_eq!(text(data.get("output")), "started\n");
}

#[test]
fn a_nested_call_runs_through_the_runtime_under_a_derived_id() {
    let cwd = Workdir::new();
    std::fs::write(cwd.0.join("note.txt"), "nested body\n").unwrap();
    let permission = Arc::new(Recording::default());
    let runtime = runtime(permission.clone());
    let result = block_on(runtime.execute(
        call(
            "from pilotdeck_tools import read_file, glob\nprint(read_file('note.txt')['content'])\nprint(glob('*.txt')['content'])\n",
        ),
        context(&cwd, None),
    ))
    .unwrap();
    let data = result.get("data").unwrap();
    assert_eq!(text(data.get("status")), "success", "{data:?}");
    assert!(text(data.get("output")).contains("nested body"));
    assert_eq!(data.get("tool_calls_made"), Some(&JsValue::Number(2.0)));
    let Some(JsValue::Array(log)) = data.get("tool_call_log") else {
        panic!("tool_call_log is an array");
    };
    assert_eq!(
        log.iter()
            .map(|entry| (text(entry.get("tool")), entry.get("ok").cloned()))
            .collect::<Vec<_>>(),
        vec![
            ("read_file".to_owned(), Some(JsValue::Bool(true))),
            ("glob".to_owned(), Some(JsValue::Bool(true))),
        ]
    );
    assert_eq!(
        *permission.0.lock().unwrap(),
        vec![
            ("execute_code".to_owned(), "outer".to_owned()),
            ("read_file".to_owned(), "outer:code:1".to_owned()),
            ("glob".to_owned(), "outer:code:2".to_owned()),
        ]
    );
}
