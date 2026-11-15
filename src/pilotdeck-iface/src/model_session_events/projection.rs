use super::*;

/// The source boundary must not command a different tool or bill different
/// usage from the Kernel's authoritative projection of the same event.
pub(super) fn validate_projection(raw: &JsValue, event: &Value, declared: &str) -> PortResult<()> {
    let kind = ModelEventType::parse(declared)
        .ok_or_else(|| invalid("unknown structured model event type"))?;
    let projection = event
        .get("projection")
        .ok_or_else(|| invalid("structured model event omitted its projection slot"))?;
    let agrees = |value: &JsValue, source: &str, target: &str| -> PortResult<()> {
        if !raw_text(value, source)?.equals_str(text(projection, target)?) {
            return Err(invalid(
                "source event disagrees with its semantic projection",
            ));
        }
        Ok(())
    };
    let require = |expected: &str| -> PortResult<()> {
        if text(projection, "kind")? != expected {
            return Err(invalid(
                "source event has the wrong semantic projection kind",
            ));
        }
        Ok(())
    };
    match kind {
        ModelEventType::TextDelta => {
            let source = raw_text(raw, "text")?;
            match source.to_utf8() {
                Err(_) if projection.is_null() => {}
                Err(_) => {
                    return Err(invalid(
                        "non-scalar source text has a fabricated Unicode projection",
                    ));
                }
                Ok(source) => {
                    require("content_delta")?;
                    if text(projection, "content_type")? != "text/plain"
                        || text(projection, "content_encoding")? != "utf8"
                        || text(projection, "content")? != source
                        || projection.get("content_bytes").and_then(Value::as_u64)
                            != Some(source.len() as u64)
                    {
                        return Err(invalid("source text and content projection differ"));
                    }
                }
            }
        }
        ModelEventType::ToolCallStart => {
            require("tool_call_started")?;
            agrees(raw, "id", "tool_call_id")?;
            agrees(raw, "name", "name")?;
        }
        ModelEventType::ToolCallDelta => {
            raw_text(raw, "id")?
                .to_utf8()
                .map_err(|_| invalid("tool identity is not a Unicode scalar string"))?;
            match raw_text(raw, "delta")?.to_utf8() {
                Err(_) if projection.is_null() => {}
                Err(_) => {
                    return Err(invalid(
                        "non-scalar argument fragment has a fabricated projection",
                    ));
                }
                Ok(_) => {
                    require("tool_call_delta")?;
                    agrees(raw, "id", "tool_call_id")?;
                    agrees(raw, "delta", "arguments_delta")?;
                }
            }
        }
        ModelEventType::ToolCallEnd => {
            require("tool_call_completed")?;
            let call = raw
                .get("toolCall")
                .ok_or_else(|| invalid("source tool completion lacks its call"))?;
            agrees(call, "id", "tool_call_id")?;
            agrees(call, "name", "name")?;
            let arguments = pilotdeck_protocol::parse_json(text(projection, "arguments_json")?)
                .map_err(|_| invalid("tool projection contains invalid argument JSON"))?;
            if call.get("input") != Some(&arguments) {
                return Err(invalid("source and authorized tool arguments differ"));
            }
        }
        ModelEventType::MessageEnd => {
            require("response_metadata")?;
            let reason = raw_text(raw, "finishReason")?;
            if ![
                "stop",
                "length",
                "tool_call",
                "content_filter",
                "error",
                "unknown",
            ]
            .iter()
            .any(|name| reason.equals_str(name))
            {
                return Err(invalid(
                    "source finish reason is outside the public profile",
                ));
            }
            match raw.get("finishReason") {
                Some(JsValue::String(reason))
                    if reason.equals_str(text(projection, "finish_reason")?) => {}
                _ => return Err(invalid("source and projected finish reason differ")),
            }
        }
        ModelEventType::Usage => {
            require("usage")?;
            if boolean(projection, "estimated")? {
                return Err(invalid("provider usage was relabelled as an estimate"));
            }
            let usage = raw
                .get("usage")
                .and_then(JsValue::as_object)
                .ok_or_else(|| invalid("source usage is not an object"))?;
            let projected = pilotdeck_protocol::parse_json(text(projection, "usage_json")?)
                .map_err(|_| invalid("projected usage is not JSON"))?;
            let projected = projected
                .as_object()
                .ok_or_else(|| invalid("projected usage is not an object"))?;
            let names = [
                ("inputTokens", "input_tokens"),
                ("outputTokens", "output_tokens"),
                ("totalTokens", "total_tokens"),
            ];
            if projected
                .entries()
                .iter()
                .any(|(key, _)| !names.iter().any(|(_, name)| key.equals_str(name)))
            {
                return Err(invalid(
                    "usage projection contains an unrecognized billing field",
                ));
            }
            for (source, target) in names {
                let source = usage
                    .get(source)
                    .and_then(JsValue::as_number)
                    .filter(|value| {
                        value.is_finite()
                            && *value >= 0.0
                            && value.fract() == 0.0
                            && *value < 9_223_372_036_854_775_808.0
                    });
                let actual = projected.get(target).and_then(JsValue::as_number);
                if source != actual {
                    return Err(invalid("source and projected usage differ"));
                }
            }
        }
        ModelEventType::RequestStarted
        | ModelEventType::MessageStart
        | ModelEventType::ThinkingDelta
        | ModelEventType::Error
        | ModelEventType::Other => {
            if !projection.is_null() {
                return Err(invalid(
                    "source-only event carries an unauthorized semantic projection",
                ));
            }
        }
        _ => return Err(invalid("structured model event version is not supported")),
    }
    Ok(())
}

fn raw_text<'a>(value: &'a JsValue, field: &str) -> PortResult<&'a pilotdeck_protocol::JsString> {
    value
        .get(field)
        .and_then(JsValue::as_string)
        .ok_or_else(|| invalid("source event omitted a required string"))
}
