//! Default LSP and model-backed title owners for the native composition root.
use crate::{
    metadata::object,
    runtime::{
        model::router::{AbortFlag, RouterModelInvocation},
        project::{
            DisposeFuture, Family, FamilyResource, Resource, ResourceFamilySource, RuntimeError,
            RuntimeResult, StageContext, StageSink, model::ModelResources,
        },
        turn::runner::tail::{SuggestionGenerator, TitleProvider},
    },
};
use pilotdeck_protocol::{JsString, JsValue, parse_json};
use std::{
    any::Any,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// The TS default has no registered language providers. Keeping a real owner
/// preserves disposed/query validation, including the explicit unavailable error.
#[derive(Default)]
pub struct DefaultLsp {
    disposed: AtomicBool,
}
impl DefaultLsp {
    pub fn bind(&self) -> Result<Self, RuntimeError> {
        if self.disposed.load(Ordering::Acquire) {
            return self.query("", "", 0.0, 0.0).map(|_| Self::default());
        }
        Ok(Self::default())
    }
    pub fn query(
        &self,
        workspace: &str,
        path: &str,
        line: f64,
        character: f64,
    ) -> Result<JsValue, RuntimeError> {
        let (code, message) = if self.disposed.load(Ordering::Acquire) {
            ("LSP_DISPOSED", "LSP service is disposed".to_owned())
        } else if crate::metadata::js_trim(workspace).is_empty() {
            (
                "LSP_WORKSPACE_REQUIRED",
                "LSP query requires a workspace root".to_owned(),
            )
        } else if crate::metadata::js_trim(path).is_empty() {
            (
                "LSP_MALFORMED_RESPONSE",
                "LSP query requires a file path".to_owned(),
            )
        } else if [line, character]
            .iter()
            .any(|n| !n.is_finite() || n.fract() != 0.0 || *n < 0.0)
        {
            (
                "LSP_MALFORMED_RESPONSE",
                "LSP position must contain non-negative integer coordinates".to_owned(),
            )
        } else {
            let root = crate::metadata::resolve_path(
                &std::env::current_dir().map_err(|e| RuntimeError::new(e.to_string()))?,
                Path::new(workspace),
            );
            let file = crate::metadata::resolve(&root, path);
            if !file.starts_with(&root)
                || file
                    .strip_prefix(&root)
                    .ok()
                    .and_then(|p| p.to_str())
                    .is_some_and(|p| p.starts_with(".."))
            {
                (
                    "LSP_MALFORMED_RESPONSE",
                    format!("LSP source {path} resolves outside the workspace"),
                )
            } else {
                let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                let extension = name
                    .rfind('.')
                    .filter(|i| *i > 0 && name != "..")
                    .map(|i| &name[i..]);
                (
                    "LSP_UNAVAILABLE",
                    format!(
                        "No LSP provider is registered for {}",
                        extension.unwrap_or("the requested file")
                    ),
                )
            }
        };
        Err(RuntimeError::new(message.clone()).with_js_error(
            crate::runtime::project::RuntimeJsError {
                name: "LspError".into(),
                code: Some(code.into()),
                message: message.into(),
                requires_host: false,
            },
        ))
    }
}
impl FamilyResource for DefaultLsp {
    fn dispose(&self) -> DisposeFuture<'_> {
        self.disposed.store(true, Ordering::Release);
        Box::pin(async { Ok(()) })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
pub struct LspFamily;
impl ResourceFamilySource for LspFamily {
    fn family(&self) -> Family {
        Family::LspService
    }
    fn stage(&self, _: &StageContext, sink: &mut StageSink<'_>) -> RuntimeResult {
        sink.register_owner(self.family(), Arc::new(DefaultLsp::default()));
        Ok(())
    }
}

pub struct TitleResource(pub TitleProvider);
impl FamilyResource for TitleResource {
    fn dispose(&self) -> DisposeFuture<'_> {
        // Native TS title provider has no dispose method. SessionTail owns
        // cancellation of pending work; the model registry owns its providers.
        Box::pin(async { Ok(()) })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
pub struct TitleFamily;
impl ResourceFamilySource for TitleFamily {
    fn family(&self) -> Family {
        Family::SessionTitleProvider
    }
    fn stage(&self, context: &StageContext, sink: &mut StageSink<'_>) -> RuntimeResult {
        let resource = context
            .resource(Resource::Model)
            .ok_or_else(|| RuntimeError::new("Title model resource is missing."))?;
        let model = resource
            .as_any()
            .downcast_ref::<ModelResources>()
            .ok_or_else(|| RuntimeError::new("Title model resource is not native."))?;
        let selection = context
            .snapshot()
            .get("config")
            .and_then(|v| v.get("agent"))
            .and_then(|v| v.get("model"))
            .cloned()
            .unwrap_or(JsValue::Undefined);
        let language = language(&context.env());
        sink.register_owner(
            self.family(),
            Arc::new(TitleResource(title_provider(
                model.providers.clone(),
                selection,
                language,
                Duration::from_secs(30),
            ))),
        );
        Ok(())
    }
}

fn language(env: &crate::config::Env) -> String {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(raw) = env.get(key) {
            let raw = raw.split(['.', '@']).next().unwrap_or("").replace('_', "-");
            if !["c", "posix", "und", ""].contains(&raw.to_lowercase().as_str())
                && let Ok(locale) = raw.parse::<icu_locale_core::Locale>()
            {
                return locale.to_string();
            }
        }
    }
    "en-US".into()
}

const TITLE_PROMPT: &str = "Generate a concise title (3-7 words) that captures the main topic or goal of this coding session. The title should be clear enough that the user recognizes the session in a list. For Latin-script languages, use sentence case: capitalize only the first word and proper nouns.\n\nLanguage requirement (highest priority): write the title in the same natural language as the user's input. Do not translate the user's input into English. If the input contains multiple languages, use the language of the user's main request (or the most recent natural-language request), while leaving product names and code identifiers unchanged. If the user's language cannot be determined, write the title in the system language specified below.\n\nReturn JSON with a single \"title\" field.\n\nExamples (the title language must still follow the user's input):\n{\"title\": \"Fix login button on mobile\"}\n{\"title\": \"Add OAuth authentication\"}\n{\"title\": \"修复移动端登录按钮\"}\n{\"title\": \"添加 OAuth 认证\"}\n\nBad (too vague): {\"title\": \"Code changes\"}\nBad (too long): {\"title\": \"Investigate and fix the issue where the login button does not respond on mobile devices\"}\nBad (wrong case): {\"title\": \"Fix Login Button On Mobile\"}\n\nDo not output Markdown, code fences, explanations, analysis, thinking text, <think> tags, or extra fields.";

pub fn title_provider(
    model: Arc<dyn RouterModelInvocation>,
    selection: JsValue,
    language: String,
    timeout: Duration,
) -> TitleProvider {
    let provenance = object([
        ("provider", field(&selection, "provider")),
        ("model", field(&selection, "model")),
    ]);
    TitleProvider {
        provider_id: Some("native-llm".into()),
        model_provenance: Some(provenance),
        generate: Arc::new(move |input| {
            let model = model.clone();
            let selection = input.model.clone().unwrap_or_else(|| selection.clone());
            let system_prompt = format!("{TITLE_PROMPT}\n\nSystem language: {language}");
            Box::pin(async move {
                let mut text = normalized(&input.text);
                if text.is_empty() {
                    return Ok(None);
                }
                if text.len() > 1200 {
                    let mut short = text[..597].to_vec();
                    short.extend(" ... ".encode_utf16());
                    short.extend_from_slice(&text[text.len() - 598..]);
                    text = short;
                }
                let request = object([
                    ("provider", field(&selection, "provider")),
                    ("model", field(&selection, "model")),
                    ("systemPrompt", JsValue::String(system_prompt.into())),
                    (
                        "messages",
                        JsValue::Array(vec![object([
                            ("role", "user".into()),
                            (
                                "content",
                                JsValue::Array(vec![object([
                                    ("type", "text".into()),
                                    ("text", JsValue::String(JsString::from_utf16(text))),
                                ])]),
                            ),
                        ])]),
                    ),
                    ("maxOutputTokens", JsValue::Number(4096.0)),
                    (
                        "metadata",
                        object([
                            ("purpose", "session_title_generation".into()),
                            ("sessionId", JsValue::String(input.session_id)),
                            ("turnId", JsValue::String(input.turn_id)),
                        ]),
                    ),
                ]);
                let abort = AbortFlag::new();
                if input.signal.is_cancelled() {
                    abort.abort();
                }
                let response = model.complete(request, Some(abort.clone()));
                tokio::pin!(response);
                let result = tokio::select! {
                    result = &mut response => result,
                    _ = input.signal.cancelled() => { abort.abort(); response.await },
                    _ = tokio::time::sleep(timeout) => { abort.abort(); response.await },
                };
                Ok(match result {
                    Ok(response) => match parse_title(&response) {
                        Ok(title) => Some(title),
                        Err((reason, message)) => {
                            log_title_failure(reason, message.as_deref());
                            None
                        }
                    },
                    Err(error) => {
                        let message = String::from_utf16_lossy(error.message.units());
                        log_title_failure("provider_error", Some(&message));
                        None
                    }
                })
            })
        }),
    }
}
fn field(value: &JsValue, name: &str) -> JsValue {
    value.get(name).cloned().unwrap_or(JsValue::Undefined)
}
fn normalized(text: &JsString) -> Vec<u16> {
    let mut output = Vec::new();
    let mut space = false;
    for &unit in text.units() {
        let white = crate::runtime::memory::js_is_space_unit(unit);
        if white {
            space = !output.is_empty();
        } else {
            if space {
                output.push(32);
            }
            space = false;
            output.push(unit);
        }
    }
    output
}
/// `logSessionTitleFailure` (SessionTitleGenerator.ts:196-200): a skipped
/// title is a `console.debug` line on stdout; the error message is cut to
/// 200 UTF-16 units.
fn log_title_failure(reason: &str, message: Option<&str>) {
    let suffix = match message.filter(|message| !message.is_empty()) {
        Some(message) => {
            let units: Vec<u16> = message.encode_utf16().take(200).collect();
            format!(": {}", String::from_utf16_lossy(&units))
        }
        None => String::new(),
    };
    println!("[session-title] generation skipped ({reason}){suffix}");
}
type TitleSkip = (&'static str, Option<String>);
/// `parseGeneratedTitle` / `sanitizeGeneratedTitle`
/// (SessionTitleGenerator.ts:150-194); the error is the logged skip reason.
fn parse_title(response: &JsValue) -> Result<JsString, TitleSkip> {
    let mut units = Vec::new();
    if let Some(JsValue::Array(blocks)) = response.get("content") {
        for block in blocks {
            if block.get("type") == Some(&"text".into())
                && let Some(text) = block.get("text").and_then(JsValue::as_string)
            {
                units.extend_from_slice(text.units());
            }
        }
    }
    // Preserve literal unpaired UTF-16 units in JSON strings by escaping them
    // before the UTF-8 parser boundary, as JSON.parse does on JS strings.
    let mut text = String::new();
    for decoded in char::decode_utf16(units) {
        match decoded {
            Ok(ch) => text.push(ch),
            Err(error) => text.push_str(&format!("\\u{:04x}", error.unpaired_surrogate())),
        }
    }
    let text = crate::metadata::js_trim(&text);
    if text.is_empty() {
        return Err(("empty_content", None));
    }
    let body = crate::metadata::js_trim(strip_json_fence(text));
    let parsed = parse_json(body).map_err(|error| {
        let message =
            pilotdeck_protocol::json_syntax::parse_error(body).unwrap_or_else(|| error.to_string());
        ("invalid_json", Some(message))
    })?;
    let title = parsed
        .as_object()
        .and_then(|object| object.get("title"))
        .and_then(JsValue::as_string)
        .ok_or(("missing_title", None))?;
    let mut title = normalized(title);
    title.truncate(80);
    if title.is_empty() {
        return Err(("missing_title", None));
    }
    Ok(JsString::from_utf16(title))
}

/// `stripJsonFence`'s match (SessionTitleGenerator.ts:180-183):
/// `/^```(?:json)?\s*([\s\S]*?)\s*```$/i` on the trimmed text. The caller
/// trims the capture, so the two `\s*` only move whitespace that trim drops
/// anyway: the capture is what lies between the opening fence (followed by
/// `json` in any ASCII case, when the rest still ends in a fence) and the
/// closing fence. JS's non-unicode `i` folds to ASCII only, so `\u{17f}` is
/// no `s` here. Matched by hand: building the equivalent `regex` costs tens of
/// milliseconds in a debug build, and every server's first title paid it
/// between the title response and the title record.
fn strip_json_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let closed = |inner: &str| inner.len() >= 3 && inner.ends_with("```");
    if let Some(after) = rest
        .get(..4)
        .filter(|tag| tag.eq_ignore_ascii_case("json"))
        .map(|_| &rest[4..])
        && closed(after)
    {
        return &after[..after.len() - 3];
    }
    if closed(rest) {
        return &rest[..rest.len() - 3];
    }
    text
}

const PROMPT_SUGGESTION_SYSTEM_PROMPT: &str = "Suggest the single most useful next message the human user could send after this completed agent turn.\n\nWrite the suggestion in the user's voice and in the same natural language as the original request. It must be a short, concrete follow-up request, not advice to the user and not a summary of the completed work. Do not claim an action already happened. Return only the suggested prompt, without quotes, labels, Markdown, code fences, analysis, or explanation.";
const PROMPT_SUGGESTION_MAX_INPUT_UNITS: usize = 6_000;
const PROMPT_SUGGESTION_MAX_OUTPUT_UNITS: usize = 320;

/// `createPromptSuggestionGenerator` (session/prompt/PromptSuggestionGenerator.ts):
/// the opt-in SDK `promptSuggestions` request, isolated from the transcript.
/// Every failure (provider error, timeout, empty content) is a skipped
/// suggestion, never an error.
pub fn prompt_suggestion_generator(
    model: Arc<dyn RouterModelInvocation>,
    selection: JsValue,
    timeout: Duration,
) -> SuggestionGenerator {
    Arc::new(move |input| {
        let model = model.clone();
        let selection = selection.clone();
        Box::pin(async move {
            let user = suggestion_truncate(&input.user_prompt);
            let assistant = suggestion_truncate(&input.assistant_response);
            if user.is_empty() || assistant.is_empty() {
                return Ok(None);
            }
            let prompt =
                format!("Original user request:\n{user}\n\nCompleted agent response:\n{assistant}");
            let request = object([
                ("provider", field(&selection, "provider")),
                ("model", field(&selection, "model")),
                (
                    "systemPrompt",
                    JsValue::from(PROMPT_SUGGESTION_SYSTEM_PROMPT),
                ),
                (
                    "messages",
                    JsValue::Array(vec![object([
                        ("role", "user".into()),
                        (
                            "content",
                            JsValue::Array(vec![object([
                                ("type", "text".into()),
                                ("text", JsValue::from(prompt.as_str())),
                            ])]),
                        ),
                    ])]),
                ),
                ("maxOutputTokens", JsValue::Number(160.0)),
                ("temperature", JsValue::Number(0.2)),
                (
                    "metadata",
                    object([
                        ("purpose", "prompt_suggestion_generation".into()),
                        ("sessionId", JsValue::String(input.session_id)),
                        ("turnId", JsValue::String(input.turn_id)),
                    ]),
                ),
            ]);
            let abort = AbortFlag::new();
            if input.signal.is_cancelled() {
                abort.abort();
            }
            let response = model.complete(request, Some(abort.clone()));
            tokio::pin!(response);
            let result = tokio::select! {
                result = &mut response => result.ok(),
                _ = input.signal.cancelled() => { abort.abort(); response.await.ok() },
                _ = tokio::time::sleep(timeout) => { abort.abort(); response.await.ok() },
            };
            Ok(result.as_ref().and_then(sanitize_prompt_suggestion))
        })
    })
}

/// `value.replace(/\s+/g, " ").trim()`.
fn collapse_whitespace(value: &str) -> String {
    let mut output = String::new();
    let mut space = false;
    for ch in value.chars() {
        if ch.is_whitespace() || ch == '\u{feff}' {
            space = true;
        } else {
            if space && !output.is_empty() {
                output.push(' ');
            }
            space = false;
            output.push(ch);
        }
    }
    output
}

/// The generator's `truncate`: collapsed whitespace, then the middle cut to
/// 6000 UTF-16 units around ` ... `.
fn suggestion_truncate(value: &JsString) -> String {
    let normalized = collapse_whitespace(&String::from_utf16_lossy(value.units()));
    let units: Vec<u16> = normalized.encode_utf16().collect();
    if units.len() <= PROMPT_SUGGESTION_MAX_INPUT_UNITS {
        return normalized;
    }
    let marker = " ... ";
    let available = PROMPT_SUGGESTION_MAX_INPUT_UNITS - marker.len();
    let head = available / 2;
    let tail = available - head;
    let mut out = String::from_utf16_lossy(&units[..head]);
    out.push_str(marker);
    out.push_str(&String::from_utf16_lossy(&units[units.len() - tail..]));
    out
}

/// `sanitizePromptSuggestion`.
fn sanitize_prompt_suggestion(response: &JsValue) -> Option<JsString> {
    let JsValue::Array(blocks) = response.get("content")? else {
        return None;
    };
    let text = blocks
        .iter()
        .filter(|block| block.get("type") == Some(&"text".into()))
        .map(|block| {
            block
                .get("text")
                .and_then(JsValue::as_string)
                .map(|text| String::from_utf16_lossy(text.units()))
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let replace = |text: &str, pattern: &str| {
        regex::Regex::new(pattern)
            .map(|regex| regex.replace(text, "").into_owned())
            .unwrap_or_else(|_| text.to_owned())
    };
    let text = replace(&text, r"(?i)^```(?:text|markdown)?\s*");
    let text = replace(&text, r"(?i)\s*```$");
    let text = replace(&text, r"(?i)^(?:suggestion|prompt)\s*:\s*");
    let text = collapse_whitespace(&text);
    let text = regex::Regex::new(r#"^["']|["']$"#)
        .map(|regex| regex.replace_all(&text, "").into_owned())
        .unwrap_or(text);
    if text.is_empty() {
        return None;
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() > PROMPT_SUGGESTION_MAX_OUTPUT_UNITS {
        let cut = String::from_utf16_lossy(&units[..PROMPT_SUGGESTION_MAX_OUTPUT_UNITS]);
        return Some(JsString::from(cut.trim()));
    }
    Some(JsString::from(text))
}

#[cfg(test)]
mod fence_tests {
    use super::strip_json_fence;

    /// Expected values are node 22's
    /// `(/^```(?:json)?\s*([\s\S]*?)\s*```$/i.exec(t)?.[1] ?? t).trim()`.
    #[test]
    fn strips_like_the_baseline_pattern() {
        for (text, expected) in [
            ("```json\n{\"title\":\"a\"}\n```", "{\"title\":\"a\"}"),
            ("```JSON {} ```", "{}"),
            ("```Json```", ""),
            ("```json``", "```json``"),
            ("``````", ""),
            ("`````", "`````"),
            ("```j\u{17f}on x```", "j\u{17f}on x"),
            ("```js x```", "js x"),
            ("``` a ``` b ```", "a ``` b"),
            ("plain", "plain"),
            ("```json\u{a0}x\u{2028}```", "x"),
            ("x```y```", "x```y```"),
            ("```jsonx```", "x"),
        ] {
            assert_eq!(
                crate::metadata::js_trim(strip_json_fence(text)),
                expected,
                "{text:?}"
            );
        }
    }
}
