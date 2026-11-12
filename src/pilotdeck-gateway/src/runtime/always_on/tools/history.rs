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
    #[tokio::test]
    async fn registered_discovery_tool_reads_real_transcript_alias_and_rejects_other_phases() {
        let root = std::env::temp_dir().join(format!("pilotdeck-history-{}", uuid::Uuid::new_v4()));
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _scratch = Scratch(root.clone());
        let project = root.join("project");
        let home = root.join("home/.pilotdeck");
        std::fs::create_dir_all(&project).unwrap();
        let paths = AlwaysOnPaths {
            project_key: project.to_string_lossy().into_owned(),
            pilot_home: home.clone(),
            ..Default::default()
        };
        let phase = Arc::new(PhaseContext::new(
            PhaseKind::Discovery,
            paths.clone(),
            Arc::new(DiscoveryPlanStore::new(paths.clone())),
        ));
        phase
            .state()
            .chat_aliases
            .push(("C1".into(), "web:gateway-parity-text".into()));
        let chats = crate::session_chat_dir(&home, &project, &paths.project_key);
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            chats.join("web:gateway-parity-text.jsonl"),
            include_str!("../../../../tests/fixtures/metadata/transcript.jsonl"),
        )
        .unwrap();
        let contexts = Arc::new(AlwaysOnRunContextRegistry::new());
        contexts
            .register(super::super::super::manager::AlwaysOnRunContext {
                session_key: "discovery".into(),
                run_id: "history-run".into(),
                project_key: paths.project_key.clone(),
                phase: Some(phase),
            })
            .unwrap();
        let tools = always_on_tools(contexts, Arc::new(|| 0.0), Arc::new(|| "unused".into()));
        let tool = &tools[3];
        assert_eq!(
            tool.fields().get("aliases"),
            Some(&JsValue::Array(vec!["AlwaysOnReadChatHistory".into()]))
        );
        let mut context = ToolRunContext::new(Arc::new(
            crate::host::modules::tool_context::ToolRuntimeContext {
                fields: JsObject::new(),
                files: crate::host::modules::tool_context::HostToolCheckpoint::default()
                    .tool_context_state(),
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
            },
        ));
        context
            .fields
            .insert("sessionId".into(), "discovery".into());
        let input =
            |session: &str| JsValue::Object(fields(vec![("sessionId".into(), session.into())]));
        let result = tool.execute(input("C1"), context.clone()).await.unwrap();
        let direct = tool
            .execute(input("web:gateway-parity-text"), context.clone())
            .await
            .unwrap();
        let expected = parse_json(r#"{"sessionId":"web:gateway-parity-text","title":"GP_TEXT","messageCount":2,"conversation":[{"role":"user","text":"GP_TEXT","createdAt":""},{"role":"assistant","text":"GP_MOCK_OK","createdAt":""}]}"#).unwrap();
        assert_eq!(result.data, Some(expected));
        assert_eq!(result.data, direct.data);
        assert_eq!(
            result.content[0].get("text"),
            Some(&"Session \"GP_TEXT\" — 2 messages.".into())
        );
        assert_eq!(
            result.metadata.unwrap().get("runId"),
            Some(&"history-run".into())
        );
        let missing = tool
            .execute(input("missing"), context.clone())
            .await
            .unwrap_err();
        assert_eq!(
            missing.message,
            format!(
                "Could not read session missing: Transcript {} does not exist.",
                chats.join("missing.jsonl").display()
            )
            .into()
        );
        context
            .fields
            .insert("sessionId".into(), "execution".into());
        let wrong = tool.execute(input("C1"), context).await.unwrap_err();
        assert_eq!(
            wrong.message,
            "always_on_read_chat_history is only available during Always-On discovery (Phase 1)."
                .into()
        );
    }

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
