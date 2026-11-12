//! Named observers are real background forks with a read-only override.
use super::*;
fn truncate(value: &JsString, maximum: usize) -> JsString {
    let units = crate::runtime::memory::js_trim_units(value.units());
    if units.len() <= maximum {
        return JsString::from_utf16(units.to_vec());
    }
    let mut units = units[..maximum.saturating_sub(72)].to_vec();
    units.extend("\n\n[observer digest truncated]".encode_utf16());
    JsString::from_utf16(units)
}
fn join(parts: &[JsString], separator: &str) -> JsString {
    let mut units = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            units.extend(separator.encode_utf16());
        }
        units.extend_from_slice(part.units());
    }
    JsString::from_utf16(units)
}
fn string(event: &JsValue, key: &str) -> JsString {
    event
        .get(key)
        .and_then(JsValue::as_string)
        .cloned()
        .unwrap_or_else(|| "undefined".into())
}
fn activity_line(event: &JsValue) -> Option<JsString> {
    let kind = string(event, "type");
    if kind.equals_str("model_request_started") {
        return Some(join(
            &[
                "model request: ".into(),
                string(event, "provider"),
                "/".into(),
                string(event, "model"),
            ],
            "",
        ));
    }
    if kind.equals_str("tool_calls_detected") {
        let names = match event.get("calls") {
            Some(JsValue::Array(calls)) => calls
                .iter()
                .map(|call| string(call, "name"))
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let names = join(&names, ", ");
        return Some(join(
            &[
                "tool calls: ".into(),
                if names.is_empty() {
                    "none".into()
                } else {
                    names
                },
            ],
            "",
        ));
    }
    if kind.equals_str("tool_result") {
        let result = event.get("result").cloned().unwrap_or(JsValue::Undefined);
        let outcome = if string(&result, "type").equals_str("success") {
            "success"
        } else {
            "error"
        };
        return Some(join(
            &[
                "tool result: ".into(),
                string(&result, "toolName"),
                format!(" ({outcome})").into(),
            ],
            "",
        ));
    }
    if kind.equals_str("assistant_message") {
        return Some("assistant response emitted".into());
    }
    if kind.equals_str("warning") {
        return Some(join(
            &["warning: ".into(), truncate(&string(event, "code"), 120)],
            "",
        ));
    }
    if kind.equals_str("agent_status") {
        return Some(join(
            &[
                "agent status: ".into(),
                truncate(&string(event, "event"), 120),
            ],
            "",
        ));
    }
    None
}
pub(super) fn digest(
    observed: &SubagentDefinition,
    directive: &JsString,
    activity: &[JsValue],
    outcome: Result<&str, &JsString>,
) -> JsString {
    let lines = activity
        .iter()
        .filter_map(activity_line)
        .map(|line| join(&["- ".into(), line], ""))
        .collect::<Vec<_>>();
    let mut lines_out:Vec<JsString>=vec!["You are an observer. Do not execute the observed task or attempt to change its outcome.".into(),"Review this read-only activity digest and produce a concise report of risks, failures, or notable findings.".into(),"".into(),format!("Observed agent: {}",observed.id).into(),join(&["Observed directive: ".into(),truncate(directive,2000)],""),"".into(),"Activity:".into()];
    lines_out.extend(if lines.is_empty() {
        vec!["- No observable model or tool activity was recorded.".into()]
    } else {
        lines
    });
    lines_out.push("".into());
    lines_out.push(
        format!(
            "Outcome: {}",
            if outcome.is_ok() {
                "completed"
            } else {
                "failed"
            }
        )
        .into(),
    );
    match outcome {
        Ok(markdown) if !markdown.is_empty() => {
            lines_out.push("Observed final report:".into());
            lines_out.push(truncate(&JsString::from(markdown), 4000));
        }
        Err(error) if !error.is_empty() => lines_out.push(join(
            &["Observed error: ".into(), truncate(error, 1000)],
            "",
        )),
        _ => {}
    }
    if let Some(postamble) = observed
        .observer_message
        .as_ref()
        .map(|value| truncate(value, usize::MAX))
        .filter(|value| !value.is_empty())
    {
        lines_out.push("".into());
        lines_out.push(postamble);
    }
    truncate(&join(&lines_out, "\n"), 12000)
}
pub(super) fn read_only_override(observer: &SubagentDefinition) -> SubagentDefinition {
    let mut observer = observer.clone();
    observer.allowed_tools.clear();
    observer.disallowed_tools.clear();
    observer.is_read_only = true;
    observer.permission_mode = Some("plan".into());
    observer.mcp_servers = None;
    observer.memory_disabled = true;
    observer.initial_prompt = None;
    observer.background = false;
    observer.observer = None;
    observer.observer_message = None;
    observer.observer_run = true;
    observer
}
pub(super) fn launch(
    fork: &Fork,
    request: &ForkRequest,
    context: &ToolRunContext,
    observed: &SubagentDefinition,
    observer: Arc<SubagentDefinition>,
    activity: Vec<JsValue>,
    outcome: Result<&str, &JsString>,
) -> Result<(), JsString> {
    let background = fork.runtime.background.clone().ok_or_else(|| {
        JsString::from(format!(
            "Observer definition {} requires a host observer-subagent launcher.",
            observer.id
        ))
    })?;
    let observer = read_only_override(&observer);
    let directive = digest(observed, &request.directive, &activity, outcome);
    let observed_id = request.subagent_id.clone();
    let observed_type = observed.id.clone();
    let observer_id: JsString = uuid::Uuid::new_v4().to_string().into();
    let observer_type = observer.id.clone();
    let mut child = fork.clone();
    child.definition_override = Some(Arc::new(observer));
    child.suppress_auto_observer = true;
    let project = child
        .parent
        .config
        .get("cwd")
        .and_then(JsValue::as_string)
        .map(text)
        .unwrap_or_default();
    let generation = child
        .runtime
        .acquire_generation(std::path::Path::new(&project))
        .map_err(|error| JsString::from(error.to_string()))?;
    child.runtime = child.runtime.with_lease_supplier(generation.supplier());
    let observer_request = ForkRequest {
        definition_id: observer_type.as_str().into(),
        directive,
        subagent_id: observer_id.clone(),
        tool_call_id: None,
        timeout_ms: request.timeout_ms,
    };
    let context = context.clone();
    background.start(crate::runtime::background::StartBackgroundSubagentInput {
        kind: Some(crate::runtime::background::SubagentKind::Observer),
        session_id: child.session.clone(),
        parent_turn_id: child.turn.clone(),
        subagent_id: observer_id.clone(),
        subagent_type: observer_type.as_str().into(),
        observed_subagent_id: Some(observed_id.clone()),
        run: Box::new(move |signal| {
            Box::pin(async move {
                let started = Instant::now();
                let context = detached_context(&context, signal);
                let report_emitter = child.clone();
                let future: ForkFuture = Box::pin(child.run(observer_request, context));
                let outcome = future.await;
                let mut event = object([
                    ("type", "observer_report".into()),
                    ("sessionId", JsValue::String(report_emitter.session.clone())),
                    ("turnId", JsValue::String(report_emitter.turn.clone())),
                    ("observedSubagentId", JsValue::String(observed_id)),
                    ("observedSubagentType", observed_type.as_str().into()),
                    ("observerSubagentId", JsValue::String(observer_id)),
                    ("observerSubagentType", observer_type.as_str().into()),
                    ("success", JsValue::Bool(outcome.is_ok())),
                    (
                        "durationMs",
                        JsValue::Number(started.elapsed().as_millis() as f64),
                    ),
                ]);
                match &outcome {
                    Ok(report) => {
                        event.insert("report".into(), JsValue::String(report.markdown.clone()));
                    }
                    Err(error) => {
                        event.insert("error".into(), JsValue::String(error.clone()));
                    }
                }
                report_emitter.emit(event);
                let result = outcome.map(|_| ());
                let released = generation
                    .close()
                    .await
                    .map_err(|error| JsString::from(error.to_string()));
                result.and(released)
            })
        }),
    });
    Ok(())
}
