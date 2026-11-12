//! Port of TS `src/always-on/context/ChatDigestBuilder.ts` over the native
//! JSONL session catalog (`listProjectSessions`) and the head/tail user
//! prompt reader (`ProjectSessionTranscriptReader.readUserPromptDigest`,
//! `extractUserPromptsFromJsonl`).
use std::path::Path;

use pilotdeck_protocol::{JsValue, parse_json};

use super::prompts::DigestSession;
use crate::runtime::manager::file_catalog::{list_sessions, read_session_lite};

const MAX_SESSIONS: usize = 10;
const MAX_PROMPTS: usize = 8;
const MAX_PROMPT_LENGTH: usize = 500;

/// `DIGEST_EXCLUDED_PREFIXES` (ChatDigestBuilder.ts:37).
const EXCLUDED_PREFIX: &str = "always-on-execute:";

fn js_trim(value: &str) -> &str {
    value.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

/// `text.length > max ? text.slice(0, max) + "..." : text` over UTF-16.
fn clip(text: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() > MAX_PROMPT_LENGTH {
        format!(
            "{}...",
            String::from_utf16_lossy(&units[..MAX_PROMPT_LENGTH])
        )
    } else {
        text.to_string()
    }
}

/// `extractUserPromptsFromJsonl` (ProjectSessionTranscriptReader.ts:84-112).
fn prompts_from_jsonl(source: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut prompts = Vec::new();
    for line in source.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !line.contains("\"type\":\"accepted_input\"") {
            continue;
        }
        let Ok(entry) = parse_json(line) else {
            continue;
        };
        let Some(JsValue::Array(messages)) = entry.get("messages") else {
            continue;
        };
        for message in messages {
            let Some(JsValue::Array(blocks)) = message.get("content") else {
                continue;
            };
            for block in blocks {
                let is_text = block
                    .get("type")
                    .and_then(JsValue::as_string)
                    .is_some_and(|kind| kind.equals_str("text"));
                let Some(JsValue::String(text)) = block.get("text") else {
                    continue;
                };
                if !is_text {
                    continue;
                }
                let text = String::from_utf16_lossy(text.units());
                let text = js_trim(&text).to_string();
                if text.is_empty() || seen.contains(&text) {
                    continue;
                }
                prompts.push(clip(&text));
                seen.push(text);
                if prompts.len() >= MAX_PROMPTS {
                    return prompts;
                }
            }
        }
    }
    prompts
}

/// `buildChatDigest` (ChatDigestBuilder.ts:44-93): the newest sessions'
/// user prompts with `chat_<n>` aliases. Returns the digest and the alias
/// map.
pub(crate) fn build_chat_digest(
    chat_dir: &Path,
    project_root: &Path,
) -> (Vec<DigestSession>, Vec<(String, String)>) {
    let sessions = list_sessions(chat_dir, project_root, false).unwrap_or_default();
    let mut digest = Vec::new();
    let mut aliases = Vec::new();
    for (_, info) in sessions
        .into_iter()
        .filter(|(_, info)| {
            !info
                .get("sessionId")
                .and_then(JsValue::as_string)
                .is_some_and(|id| {
                    id.units()
                        .starts_with(&EXCLUDED_PREFIX.encode_utf16().collect::<Vec<_>>())
                })
        })
        .take(MAX_SESSIONS)
    {
        let Some(session_id) = info.get("sessionId").and_then(JsValue::as_string).cloned() else {
            continue;
        };
        let file = chat_dir.join(format!(
            "{}.jsonl",
            crate::storage::sanitize_session_id(&session_id)
        ));
        let prompts = read_session_lite(&file)
            .map(|lite| prompts_from_jsonl(&format!("{}\n{}", lite.head, lite.tail)))
            .unwrap_or_default();
        if prompts.is_empty() {
            continue;
        }
        let alias = format!("chat_{}", aliases.len() + 1);
        let id = String::from_utf16_lossy(session_id.units());
        aliases.push((alias.clone(), id));
        let title = info
            .get("summary")
            .map(|value| {
                String::from_utf16_lossy(
                    &crate::js_value::to_string_units(value).unwrap_or_default(),
                )
            })
            .unwrap_or_else(|| "undefined".into());
        let last_modified = info
            .get("lastModified")
            .and_then(JsValue::as_number)
            .map(crate::runtime::cron::iso_from_epoch_ms)
            .unwrap_or_default();
        digest.push(DigestSession {
            alias,
            title,
            last_modified,
            user_prompts: prompts,
        });
    }
    (digest, aliases)
}
