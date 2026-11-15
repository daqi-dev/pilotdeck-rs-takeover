use agentcpm_iface::model_session::{EventKind, ModelEventType, StructuredModelEvent};
use pilotdeck_protocol::{JsObject, JsValue, stringify_json};

fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    static UNDEFINED: JsValue = JsValue::Undefined;
    value.get(key).unwrap_or(&UNDEFINED)
}
fn text(value: &JsValue) -> Result<String, String> {
    let value = value
        .as_string()
        .ok_or_else(|| "required event field is not a string".to_owned())?;
    String::from_utf16(value.units())
        .map_err(|_| "event field cannot be represented in UTF-8".to_owned())
}
fn usage_projection(value: &JsValue) -> Result<String, String> {
    if value.as_object().is_none() {
        return Err("usage is not an object".into());
    }
    let mut projected = JsObject::new();
    for (source, target) in [
        ("inputTokens", "input_tokens"),
        ("outputTokens", "output_tokens"),
        ("totalTokens", "total_tokens"),
    ] {
        if let Some(n) = field(value, source).as_number()
            && n.is_finite()
            && n >= 0.0
            && n.fract() == 0.0
            && n < 9_223_372_036_854_775_808.0
        {
            projected.insert(target.into(), JsValue::Number(n));
        }
    }
    json(&JsValue::Object(projected))
}
fn json(value: &JsValue) -> Result<String, String> {
    stringify_json(value)
        .map_err(|e| format!("{e:?}"))?
        .ok_or_else(|| "event is not JSON".to_owned())
}

/// The structured event is the sole authority; its projection is not emitted separately.
pub(crate) fn structured(value: &JsValue) -> Result<EventKind, String> {
    let tag = text(field(value, "type"))?;
    let event_type =
        ModelEventType::parse(&tag).ok_or_else(|| format!("unknown model event type: {tag}"))?;
    let projection = match event_type {
        ModelEventType::TextDelta => {
            let raw = field(value, "text")
                .as_string()
                .ok_or_else(|| "required text is not a string".to_owned())?;
            String::from_utf16(raw.units())
                .ok()
                .map(|value| EventKind::ContentDelta {
                    bytes: value.into_bytes(),
                    content_type: "text/plain".into(),
                })
        }
        ModelEventType::ToolCallStart => Some(EventKind::ToolCallStarted {
            tool_call_id: text(field(value, "id"))?,
            name: text(field(value, "name"))?,
        }),
        ModelEventType::ToolCallDelta => {
            let tool_call_id = text(field(value, "id"))?;
            let delta = field(value, "delta")
                .as_string()
                .ok_or_else(|| "tool delta is not a string".to_owned())?;
            String::from_utf16(delta.units())
                .ok()
                .map(|arguments_delta| EventKind::ToolCallDelta {
                    tool_call_id,
                    arguments_delta,
                })
        }
        ModelEventType::ToolCallEnd => {
            let call = field(value, "toolCall");
            Some(EventKind::ToolCallCompleted {
                tool_call_id: text(field(call, "id"))?,
                name: text(field(call, "name"))?,
                arguments_json: json(field(call, "input"))?,
            })
        }
        ModelEventType::MessageEnd => {
            let reason = text(field(value, "finishReason"))?;
            if !matches!(
                reason.as_str(),
                "stop" | "length" | "tool_call" | "content_filter" | "error" | "unknown"
            ) {
                return Err("invalid finish reason".into());
            }
            Some(EventKind::ResponseMetadata {
                finish_reason: Some(reason),
            })
        }
        ModelEventType::Usage => Some(EventKind::Usage {
            usage_json: usage_projection(field(value, "usage"))?,
            estimated: false,
        }),
        _ => None,
    };
    let event = StructuredModelEvent::new(event_type, json(value)?, projection)
        .map_err(|e| e.to_string())?;
    Ok(EventKind::Structured {
        event: Box::new(event),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::parse_json;

    #[test]
    fn usage_projects_only_representable_counters_and_retains_raw() {
        let kind = structured(&parse_json(r#"{"type":"usage","usage":{"inputTokens":0,"outputTokens":5,"totalTokens":9223372036854775808,"nativeCost":0.1}}"#).unwrap()).unwrap();
        let EventKind::Structured { event } = kind else {
            panic!("structured event expected")
        };
        assert!(event.event_json().contains("nativeCost"));
        assert!(
            matches!(event.projection(), Some(EventKind::Usage { usage_json, .. }) if usage_json == r#"{"input_tokens":0,"output_tokens":5}"#)
        );
    }
    #[test]
    fn unpaired_text_stays_in_raw_and_tool_identifiers_are_never_lossy() {
        let kind =
            structured(&parse_json(r#"{"type":"text_delta","text":"\ud800"}"#).unwrap()).unwrap();
        let EventKind::Structured { event } = kind else {
            panic!("structured event expected")
        };
        assert!(event.projection().is_none());
        assert!(event.event_json().contains(r#"\ud800"#));
        assert!(
            structured(
                &parse_json(r#"{"type":"tool_call_start","id":"\ud800","name":"x"}"#).unwrap()
            )
            .is_err()
        );
        assert!(
            structured(&parse_json(r#"{"type":"tool_call_start","id":"x"}"#).unwrap()).is_err()
        );
    }

    #[test]
    fn message_end_does_not_claim_stream_completion() {
        let event = structured(
            &parse_json(r#"{"type":"message_end","finishReason":"stop","raw":{"x":"\ud800"}}"#)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(event.end_kind(), None);
        let EventKind::Structured { event } = event else {
            panic!("structured event expected")
        };
        assert!(event.event_json().contains(r#"\ud800"#));
        assert!(matches!(
            event.projection(),
            Some(EventKind::ResponseMetadata { .. })
        ));
    }
}
