use super::*;
use crate::runtime::{manager::file_catalog, session::factory::project_transcript};

fn fields(values: Vec<(JsString, JsValue)>) -> JsObject {
    let mut object = JsObject::new();
    for (key, value) in values {
        object.insert(key, value);
    }
    object
}

pub(super) fn execute(
    input: &JsValue,
    run_id: &str,
    phase: &PhaseContext,
) -> Result<ToolOutput, ToolError> {
    let requested = text_field(input, "sessionId");
    let session = phase
        .state()
        .chat_aliases
        .iter()
        .find(|(alias, _)| alias == &requested)
        .map_or_else(|| requested.clone(), |(_, real)| real.clone());
    let project = std::path::Path::new(&phase.paths.project_key);
    let chat = crate::session_chat_dir(&phase.paths.pilot_home, project, &phase.paths.project_key);
    let path = chat.join(format!(
        "{}.jsonl",
        crate::storage::sanitize_session_id(&session.as_str().into())
    ));
    let read = crate::storage::read_transcript(path)
        .map_err(|error| ToolError::failed(error.to_string()))?;
    if read.entries.is_empty() {
        let reason = read
            .diagnostics
            .first()
            .map_or("No transcript entries found.", |d| d.message.as_str());
        return Err(ToolError::failed(format!(
            "Could not read session {requested}: {reason}"
        )));
    }
    let replay =
        project_transcript(&read.entries).map_err(|error| ToolError::failed(error.message))?;
    let sessions = file_catalog::list_sessions(&chat, project, false).map_err(ToolError::failed)?;
    let title = [replay.metadata.get("title"), replay.metadata.get("aiTitle")]
        .into_iter()
        .flatten()
        .find(|value| !matches!(value, JsValue::Undefined | JsValue::Null))
        .cloned()
        .or_else(|| {
            sessions
                .iter()
                .find(|(_, value)| text_field(value, "sessionId") == session)
                .and_then(|(_, value)| value.get("summary"))
                .cloned()
        })
        .unwrap_or_else(|| JsValue::from(session.as_str()));
    let conversation = conversation(&replay.messages);
    let count = conversation.len();
    let data = fields(vec![
        ("sessionId".into(), JsValue::from(session.as_str())),
        ("title".into(), title.clone()),
        ("messageCount".into(), JsValue::Number(count as f64)),
        ("conversation".into(), JsValue::Array(conversation)),
    ]);
    let title = String::from_utf16_lossy(
        &crate::js_value::to_string_units_checked(&title)
            .map_err(|error| ToolError::failed(error.message()))?,
    );
    Ok(output(
        format!("Session \"{title}\" — {count} messages."),
        data,
        fields(vec![("runId".into(), JsValue::from(run_id))]),
    ))
}

fn conversation(messages: &[JsValue]) -> Vec<JsValue> {
    messages
        .iter()
        .filter_map(|message| {
            let role = text_field(message, "role");
            if !matches!(role.as_str(), "user" | "assistant") {
                return None;
            }
            let JsValue::Array(blocks) = message.get("content")? else {
                return None;
            };
            let parts: Vec<_> = blocks
                .iter()
                .filter(|block| text_field(block, "type") == "text")
                .filter_map(|block| block.get("text").and_then(JsValue::as_string))
                .map(|text| crate::runtime::memory::js_trim_units(text.units()))
                .filter(|text| !text.is_empty())
                .collect();
            if parts.is_empty() {
                return None;
            }
            let mut text = Vec::new();
            for (index, part) in parts.into_iter().enumerate() {
                if index > 0 {
                    text.extend([10, 10]);
                }
                text.extend_from_slice(part);
            }
            if role == "assistant" && text.len() > 300 {
                text.truncate(300);
                text.extend([46, 46, 46]);
            }
            Some(JsValue::Object(fields(vec![
                ("role".into(), JsValue::from(role.as_str())),
                ("text".into(), JsValue::String(JsString::from_utf16(text))),
                ("createdAt".into(), JsValue::from("")),
            ])))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conversation_preserves_user_text_and_clips_assistant_utf16() {
        let user = "中😀".repeat(200);
        let message = |role, text: &str| {
            let mut value =
                parse_json(r#"{"role":"user","content":[{"type":"text","text":""}]}"#).unwrap();
            let JsValue::Object(object) = &mut value else {
                unreachable!()
            };
            object.insert("role".into(), JsValue::from(role));
            object.insert(
                "content".into(),
                JsValue::Array(vec![JsValue::Object(fields(vec![
                    ("type".into(), "text".into()),
                    ("text".into(), JsValue::from(text)),
                ]))]),
            );
            value
        };
        let result = conversation(&[
            message("user", &user),
            message("assistant", &user),
            message("system", "hidden"),
        ]);
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0]
                .get("text")
                .unwrap()
                .as_string()
                .unwrap()
                .len_utf16(),
            600
        );
        assert_eq!(
            result[1]
                .get("text")
                .unwrap()
                .as_string()
                .unwrap()
                .len_utf16(),
            303
        );
        assert_eq!(text_field(&result[0], "createdAt"), "");
    }
}
