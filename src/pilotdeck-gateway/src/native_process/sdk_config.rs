//! `submit_turn.sdkSessionConfig` in the native host: the gateway's own
//! shape check (`validateSdkSessionConfig`, InProcessGateway.ts:3210-3496)
//! and the session-config store behind `setSdkSessionConfig`
//! (ProjectRuntimeRegistry.ts:956-1023).
//!
//! Only the fields whose effect this host composes are accepted
//! ([`WIRED_FIELDS`]). A config that passes the baseline's shape check but
//! names any other field is still answered with the unavailable shape the
//! host gave before, instead of being stored and silently ignored.
use super::session_overrides::SessionOverrides;
use crate::runtime::{
    session::TurnCompletionHandle,
    turn::{
        admission::SubmitSdkSessionConfig,
        submit::{SubmitError, SubmitFuture},
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, stringify_json};
use std::sync::Arc;

/// The `sdkSessionConfig` fields whose effect the native session composes.
pub(super) const WIRED_FIELDS: &[&str] = &[
    "userDialogKinds",
    "systemPrompt",
    "appendSystemPrompt",
    "planModeInstructions",
    "toolAliases",
    "outputStyle",
    "permissionMode",
    "additionalWorkingDirectories",
    "managedPermissions",
];

const INVALID: &str = "INVALID_SDK_SESSION_CONFIG";

fn err(code: &str, message: impl Into<String>) -> SubmitError {
    SubmitError::new(code, message)
}

fn invalid(message: impl Into<String>) -> SubmitError {
    err(INVALID, message)
}

fn text(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}

fn trim_empty(value: &JsString) -> bool {
    crate::metadata::js_trim(&text(value)).is_empty()
}

/// `value !== undefined` for an own property of the config.
fn present<'a>(object: &'a JsObject, key: &str) -> Option<&'a JsValue> {
    object.get(key).filter(|value| !value.is_undefined())
}

/// `value && typeof value === "object" && !Array.isArray(value)`.
fn record(value: &JsValue) -> Option<&JsObject> {
    value.as_object()
}

/// JS truthiness.
fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
        JsValue::String(value) => !value.units().is_empty(),
        JsValue::Array(_) | JsValue::Object(_) => true,
    }
}

fn is_string(value: &JsValue) -> bool {
    matches!(value, JsValue::String(_))
}

fn is_safe_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0
}

fn is_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0
}

fn number(value: Option<&JsValue>) -> Option<f64> {
    value.and_then(JsValue::as_number)
}

/// `Object.keys(value).some((key) => !allowed.includes(key))`.
fn has_other_keys(object: &JsObject, allowed: &[&str]) -> bool {
    object
        .own_entries()
        .iter()
        .any(|(key, _)| !allowed.iter().any(|name| key.equals_str(name)))
}

/// `new Set(items).size !== items.length` over strings.
fn has_duplicates(items: &[JsValue]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(index, item)| items[..index].contains(item))
}

fn string_in(value: &JsValue, allowed: &[&str]) -> bool {
    value
        .as_string()
        .is_some_and(|value| allowed.iter().any(|name| value.equals_str(name)))
}

/// `typeof value === "string" && isAbsolute(value)` (POSIX `path.isAbsolute`).
fn absolute_path(value: &JsValue) -> bool {
    value
        .as_string()
        .is_some_and(|value| value.units().first() == Some(&u16::from(b'/')))
}

/// `isSdkJsonSchema` (InProcessGateway.ts:3498-3514).
fn is_sdk_json_schema(value: &JsValue) -> bool {
    let Some(schema) = record(value) else {
        return false;
    };
    if let Some(kind) = present(schema, "type")
        && !is_string(kind)
        && !matches!(kind, JsValue::Array(items) if items.iter().all(is_string))
    {
        return false;
    }
    if let Some(required) = present(schema, "required")
        && !matches!(required, JsValue::Array(items) if items.iter().all(is_string))
    {
        return false;
    }
    if present(schema, "additionalProperties").is_some_and(|value| value.as_bool().is_none()) {
        return false;
    }
    if present(schema, "enum").is_some_and(|value| !matches!(value, JsValue::Array(_))) {
        return false;
    }
    if let Some(properties) = present(schema, "properties") {
        let Some(properties) = record(properties) else {
            return false;
        };
        if !properties
            .own_entries()
            .iter()
            .all(|(_, value)| is_sdk_json_schema(value))
        {
            return false;
        }
    }
    if present(schema, "items").is_some_and(|items| !is_sdk_json_schema(items)) {
        return false;
    }
    true
}

/// `validateSdkSessionConfig` (InProcessGateway.ts:3210-3496), in its order.
pub(super) fn validate(config: &JsValue) -> Result<(), SubmitError> {
    // `typeof config !== "object"`: an array passes, and names none of the
    // checked fields.
    let empty = JsObject::new();
    let config = match config {
        JsValue::Object(config) => config,
        JsValue::Array(_) => &empty,
        _ => return Err(invalid("sdkSessionConfig must be an object.")),
    };
    let string_field = |key: &str, message: &str| -> Result<(), SubmitError> {
        match present(config, key) {
            Some(value) if !is_string(value) => Err(invalid(message)),
            _ => Ok(()),
        }
    };
    string_field("systemPrompt", "systemPrompt must be a string.")?;
    if present(config, "persistSession").is_some_and(|value| value != &JsValue::Bool(false)) {
        return Err(invalid("persistSession must be false when specified."));
    }
    string_field("appendSystemPrompt", "appendSystemPrompt must be a string.")?;
    string_field(
        "planModeInstructions",
        "planModeInstructions must be a string.",
    )?;
    if let Some(style) = present(config, "outputStyle")
        && !style.as_string().is_some_and(|style| !trim_empty(style))
    {
        return Err(invalid("outputStyle must be a non-empty string."));
    }
    if present(config, "permissionMode")
        .is_some_and(|mode| !string_in(mode, &["acceptEdits", "dontAsk"]))
    {
        return Err(invalid("permissionMode must be acceptEdits or dontAsk."));
    }
    if let Some(managed) = present(config, "managedPermissions") {
        let entries_ok = |value: Option<&JsValue>| {
            matches!(value, Some(JsValue::Array(items))
                if items.iter().all(|item| item.as_string().is_some_and(|item| !trim_empty(item))))
        };
        let ok = record(managed).is_some_and(|managed| {
            !has_other_keys(managed, &["deny", "ask", "defaultMode", "canPrompt"])
                && entries_ok(managed.get("deny"))
                && entries_ok(managed.get("ask"))
                && present(managed, "defaultMode")
                    .is_none_or(|mode| mode.as_string().is_some_and(|mode| mode.equals_str("plan")))
                && present(managed, "canPrompt").is_none_or(|value| value == &JsValue::Bool(false))
        });
        if !ok {
            return Err(err(
                "UNSUPPORTED_SDK_MANAGED_SETTING",
                "managedPermissions may contain deny/ask entries, defaultMode=plan, and canPrompt=false only.",
            ));
        }
    }
    if let Some(managed) = present(config, "managedModels") {
        let selector = |value: &JsValue| {
            value.as_string().is_some_and(|value| {
                let value = text(value);
                value == "*" || valid_model_selector(&value)
            })
        };
        let list = |value: Option<&JsValue>| matches!(value, Some(JsValue::Array(items)) if items.iter().all(selector));
        if !record(managed)
            .is_some_and(|managed| list(managed.get("allow")) && list(managed.get("deny")))
        {
            return Err(err(
                "INVALID_SDK_MANAGED_SETTINGS",
                "managedModels selectors must be *, provider/*, or provider/model.",
            ));
        }
    }
    for (key, message) in [
        ("includeHookEvents", "includeHookEvents must be a boolean."),
        (
            "agentProgressSummaries",
            "agentProgressSummaries must be a boolean.",
        ),
        (
            "forwardSubagentText",
            "forwardSubagentText must be a boolean.",
        ),
    ] {
        if present(config, key).is_some_and(|value| value.as_bool().is_none()) {
            return Err(invalid(message));
        }
    }
    if let Some(sandbox) = present(config, "sandbox") {
        let ok = record(sandbox).is_some_and(|sandbox| {
            let field = |key: &str| present(sandbox, key);
            !has_other_keys(sandbox, &["type", "filesystem", "network", "process"])
                && field("type").is_none_or(|value| string_in(value, &["tool_policy"]))
                && field("filesystem").is_none_or(|value| string_in(value, &["read_only", "deny"]))
                && field("network").is_none_or(|value| string_in(value, &["deny"]))
                && field("process").is_none_or(|value| string_in(value, &["deny"]))
                && (field("filesystem").is_some()
                    || field("network").is_some()
                    || field("process").is_some())
        });
        if !ok {
            return Err(invalid(
                "sandbox must contain supported tool_policy restrictions.",
            ));
        }
    }
    if let Some(kinds) = present(config, "userDialogKinds") {
        let ok = matches!(kinds, JsValue::Array(items)
            if !items.is_empty()
                && items.iter().all(|kind| string_in(kind, &["input", "select", "confirm", "form"]))
                && !has_duplicates(items));
        if !ok {
            return Err(invalid(
                "userDialogKinds must be a non-empty unique array containing only input, select, confirm, or form.",
            ));
        }
    }
    if let Some(budget) = present(config, "taskBudget") {
        let total = number(budget.as_object().and_then(|budget| budget.get("total")));
        let ok = matches!(budget, JsValue::Object(_) | JsValue::Array(_))
            && total.is_some_and(|total| total.is_finite() && total > 0.0);
        if !ok {
            return Err(invalid(
                "taskBudget.total must be a positive finite USD amount.",
            ));
        }
        let budget = budget.as_object();
        let scope = budget.and_then(|budget| present(budget, "scope"));
        if scope.is_some_and(|scope| !string_in(scope, &["session", "project"])) {
            return Err(invalid(
                "taskBudget.scope must be session or project when provided.",
            ));
        }
        if let Some(retention) = budget.and_then(|budget| present(budget, "projectRetentionMs")) {
            if !retention
                .as_number()
                .is_some_and(|value| is_safe_integer(value) && value > 0.0)
            {
                return Err(invalid(
                    "taskBudget.projectRetentionMs must be a positive safe integer in milliseconds.",
                ));
            }
            if !scope.is_some_and(|scope| string_in(scope, &["project"])) {
                return Err(invalid(
                    "taskBudget.projectRetentionMs requires taskBudget.scope to be project.",
                ));
            }
        }
    }
    if let Some(settings) = present(config, "settings") {
        validate_settings(settings)?;
    }
    if let Some(sources) = present(config, "settingSources") {
        let ok = matches!(sources, JsValue::Array(items)
            if !items.is_empty()
                && items.iter().all(|source| string_in(source, &["managed", "user", "project", "local"]))
                && !has_duplicates(items));
        if !ok {
            return Err(err(
                "INVALID_SDK_SETTING_SOURCES",
                "settingSources must be a non-empty unique array containing only managed, user, project, or local.",
            ));
        }
    }
    if let Some(directories) = present(config, "additionalWorkingDirectories")
        && !matches!(directories, JsValue::Array(items) if items.iter().all(absolute_path))
    {
        return Err(invalid(
            "additionalWorkingDirectories must contain absolute paths.",
        ));
    }
    if let Some(aliases) = present(config, "toolAliases") {
        let ok = match aliases {
            JsValue::Object(aliases) => aliases.own_entries().iter().all(|(from, to)| {
                !trim_empty(from) && to.as_string().is_some_and(|to| !trim_empty(to))
            }),
            JsValue::Array(_) => false,
            // `typeof x !== "object"` is false for null, whose
            // `Object.entries` throws.
            JsValue::Null => {
                return Err(invalid("Cannot convert undefined or null to object"));
            }
            _ => false,
        };
        if !ok {
            return Err(invalid(
                "toolAliases must be a non-empty string-to-string map.",
            ));
        }
    }
    if let Some(format) = present(config, "outputFormat") {
        if matches!(format, JsValue::Null) {
            return Err(invalid("Cannot read properties of null (reading 'type')"));
        }
        let kind_ok = format
            .get("type")
            .is_some_and(|kind| string_in(kind, &["json_schema"]));
        let schema_ok = format.get("schema").is_some_and(is_sdk_json_schema);
        if !kind_ok || !schema_ok {
            return Err(invalid(
                "outputFormat must be a json_schema object using PilotDeck's JSON-schema subset.",
            ));
        }
    }
    if let Some(agents) = present(config, "agents") {
        validate_agents(agents)?;
    }
    if let Some(skills) = present(config, "skills")
        && !string_in(skills, &["all"])
    {
        let ok = matches!(skills, JsValue::Array(items)
            if !items.is_empty()
                && items.iter().all(|skill| skill.as_string().is_some_and(|skill| !trim_empty(skill)))
                && !has_duplicates(items));
        if !ok {
            return Err(invalid(
                "skills must be all or a non-empty array of unique skill names.",
            ));
        }
    }
    if let Some(plugins) = present(config, "plugins") {
        let ok = matches!(plugins, JsValue::Array(items)
        if !items.is_empty()
            && items.iter().all(|plugin| {
                plugin.get("type").is_some_and(|kind| string_in(kind, &["local"]))
                    && plugin
                        .get("path")
                        .and_then(JsValue::as_string)
                        .is_some_and(|path| !trim_empty(path))
                    && plugin.get("path").is_some_and(absolute_path)
            })
            && {
                let paths: Vec<JsValue> = items
                    .iter()
                    .map(|plugin| plugin.get("path").cloned().unwrap_or(JsValue::Undefined))
                    .collect();
                !has_duplicates(&paths)
            });
        if !ok {
            return Err(invalid(
                "plugins must be a non-empty array of distinct absolute Gateway-local plugin paths.",
            ));
        }
    }
    if let Some(hooks) = present(config, "hooks") {
        validate_hooks(hooks)?;
    }
    Ok(())
}

/// `/^[^/*\s]+\/(?:\*|[^/*\s]+)$/`.
fn valid_model_selector(value: &str) -> bool {
    let Some((provider, model)) = value.split_once('/') else {
        return false;
    };
    let plain = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c != '/' && c != '*' && !crate::runtime::tools::js_space(c))
    };
    plain(provider) && (model == "*" || plain(model))
}

fn validate_settings(settings: &JsValue) -> Result<(), SubmitError> {
    let Some(settings) = record(settings) else {
        return Err(err("INVALID_SDK_SETTINGS", "settings must be an object."));
    };
    if has_other_keys(settings, &["agent"]) {
        return Err(err(
            "UNSUPPORTED_SDK_SETTING",
            "Only settings.agent is supported by the SDK session overlay.",
        ));
    }
    let Some(agent) = present(settings, "agent") else {
        return Ok(());
    };
    let settings_error = |message: &str| Err(err("INVALID_SDK_SETTINGS", message));
    let Some(agent) = record(agent).filter(|agent| {
        !has_other_keys(
            agent,
            &[
                "model",
                "fallbackModel",
                "maxContextTokens",
                "maxOutputTokens",
                "thinking",
                "subagents",
            ],
        )
    }) else {
        return settings_error("settings.agent contains unsupported fields.");
    };
    let model_id = |value: Option<&JsValue>| match value {
        None | Some(JsValue::Undefined) | Some(JsValue::Null) => true,
        Some(value) => value.as_string().is_some_and(|value| !trim_empty(value)),
    };
    if !model_id(agent.get("model")) {
        return settings_error("settings.agent.model must be a non-empty model id or null.");
    }
    if !model_id(agent.get("fallbackModel")) {
        return settings_error(
            "settings.agent.fallbackModel must be a non-empty model id or null.",
        );
    }
    for key in ["maxContextTokens", "maxOutputTokens"] {
        if let Some(value) = present(agent, key)
            && !value
                .as_number()
                .is_some_and(|value| is_integer(value) && value > 0.0)
        {
            return Err(err(
                "INVALID_SDK_SETTINGS",
                format!("settings.agent.{key} must be a positive integer."),
            ));
        }
    }
    if let Some(thinking) = present(agent, "thinking") {
        let ok = record(thinking).is_some_and(|thinking| {
            thinking.get("enabled").and_then(JsValue::as_bool).is_some()
                && !has_other_keys(thinking, &["enabled", "budgetTokens"])
                && present(thinking, "budgetTokens").is_none_or(|value| {
                    value
                        .as_number()
                        .is_some_and(|value| is_integer(value) && value >= 0.0)
                })
        });
        if !ok {
            return settings_error(
                "settings.agent.thinking must contain enabled and an optional non-negative integer budgetTokens.",
            );
        }
    }
    if let Some(subagents) = present(agent, "subagents") {
        let ok = record(subagents).is_some_and(|subagents| {
            !has_other_keys(subagents, &["default", "timeoutMs", "maxDepth"])
                && model_id(subagents.get("default"))
                && present(subagents, "timeoutMs").is_none_or(|value| {
                    value
                        .as_number()
                        .is_some_and(|value| is_integer(value) && value > 0.0)
                })
                && present(subagents, "maxDepth").is_none_or(|value| {
                    value
                        .as_number()
                        .is_some_and(|value| is_safe_integer(value) && value >= 0.0)
                })
        });
        if !ok {
            return settings_error(
                "settings.agent.subagents supports a non-empty default model id, positive integer timeoutMs, and non-negative safe integer maxDepth.",
            );
        }
    }
    Ok(())
}

fn validate_agents(agents: &JsValue) -> Result<(), SubmitError> {
    let Some(agents) = record(agents) else {
        return Err(invalid("agents must be an object."));
    };
    let entries = agents.own_entries();
    for (name, agent) in &entries {
        let label = if name.units().is_empty() {
            "<unnamed>".to_owned()
        } else {
            text(name)
        };
        let bad = || Err(invalid(format!("Agent {label} is invalid.")));
        if trim_empty(name) {
            return bad();
        }
        let Some(agent) = record(agent) else {
            return bad();
        };
        let nonempty = |key: &str| {
            agent
                .get(key)
                .and_then(JsValue::as_string)
                .is_some_and(|value| !trim_empty(value))
        };
        let names = |key: &str| match present(agent, key) {
            None => true,
            Some(JsValue::Array(items)) => items
                .iter()
                .all(|item| item.as_string().is_some_and(|item| !trim_empty(item))),
            Some(_) => false,
        };
        let ok = nonempty("description")
            && nonempty("prompt")
            && present(agent, "model")
                .is_none_or(|model| model.as_string().is_some_and(|model| !trim_empty(model)))
            && names("tools")
            && names("disallowedTools")
            && present(agent, "maxTurns").is_none_or(|value| {
                value
                    .as_number()
                    .is_some_and(|value| is_integer(value) && value > 0.0)
            })
            && present(agent, "background").is_none_or(|value| value.as_bool().is_some())
            && present(agent, "observer")
                .is_none_or(|value| value.as_string().is_some_and(|value| !trim_empty(value)))
            && present(agent, "observerMessage").is_none_or(is_string)
            && !(agent
                .get("observerMessage")
                .and_then(JsValue::as_string)
                .is_some_and(|message| !trim_empty(message))
                && !agent.get("observer").is_some_and(truthy))
            && present(agent, "effort")
                .is_none_or(|value| string_in(value, &["low", "medium", "high"]))
            && present(agent, "permissionMode")
                .is_none_or(|value| string_in(value, &["default", "plan", "bypassPermissions"]));
        if !ok {
            return bad();
        }
    }
    for (name, agent) in &entries {
        let Some(observer) = agent.get("observer").filter(|observer| truthy(observer)) else {
            continue;
        };
        let distinct = observer.as_string().is_some_and(|observer| {
            observer != *name && agents.get_key(observer).is_some_and(truthy)
        });
        if !distinct {
            return Err(invalid(format!(
                "Agent {} observer must reference a distinct configured AgentDefinition.",
                text(name)
            )));
        }
    }
    Ok(())
}

fn validate_hooks(hooks: &JsValue) -> Result<(), SubmitError> {
    let Some(hooks) = record(hooks) else {
        return Err(invalid("hooks must be an object."));
    };
    let url = hooks
        .get("url")
        .and_then(JsValue::as_string)
        .and_then(|url| url::Url::parse(&text(url)).ok())
        .ok_or_else(|| invalid("hooks.url must be an absolute HTTP(S) URL."))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(invalid("hooks.url must use http: or https:."));
    }
    if let Some(headers) = present(hooks, "headers") {
        let ok = record(headers).is_some_and(|headers| {
            headers
                .own_entries()
                .iter()
                .all(|(name, value)| !trim_empty(name) && is_string(value))
        });
        if !ok {
            return Err(invalid("hooks.headers must be a string-to-string map."));
        }
    }
    let ok = hooks.get("events").and_then(record).is_some_and(|events| {
        events.own_entries().iter().all(|(event, matchers)| {
            crate::hooks::protocol::is_hook_event(event)
                && matches!(matchers, JsValue::Array(items) if items.iter().all(|matcher| {
                    record(matcher).is_some_and(|matcher| {
                        present(matcher, "matcher").is_none_or(is_string)
                            && present(matcher, "timeout").is_none_or(|timeout| {
                                timeout.as_number().is_some_and(|timeout| timeout.is_finite() && timeout > 0.0)
                            })
                    })
                }))
        })
    });
    if !ok {
        return Err(invalid(
            "hooks.events must map event names to matcher objects with optional string matcher and positive timeout.",
        ));
    }
    Ok(())
}

/// The `userDialogKinds` a stored config enables, in the baseline's
/// registration order (ProjectSessionRuntimeBundle.ts:328-333).
pub(super) fn user_dialog_kinds(config: Option<&JsValue>) -> Vec<&'static str> {
    let Some(JsValue::Array(kinds)) = config.and_then(|config| config.get("userDialogKinds"))
    else {
        return Vec::new();
    };
    ["input", "select", "confirm", "form"]
        .into_iter()
        .filter(|kind| kinds.iter().any(|value| string_in(value, &[kind])))
        .collect()
}

/// The prompt-shaping fields of `SessionAgentConfigBundle.compose`
/// (SessionAgentConfigBundle.ts:139-143): `systemPrompt` replaces the
/// assembled prompt, `appendSystemPrompt` is added after it (blank counts as
/// absent; the output style is joined on afterwards), `planModeInstructions`
/// applies while in plan mode and `toolAliases` are single-hop tool names.
pub(super) fn apply_prompt_fields(agent: &mut JsObject, config: Option<&JsValue>) {
    let Some(config) = config.and_then(JsValue::as_object) else {
        return;
    };
    if let Some(prompt) = present(config, "systemPrompt") {
        agent.insert("systemPrompt".into(), prompt.clone());
    }
    if let Some(append) = present(config, "appendSystemPrompt") {
        let blank = append.as_string().is_none_or(trim_empty);
        agent.insert(
            "appendSystemPrompt".into(),
            if blank {
                JsValue::Undefined
            } else {
                append.clone()
            },
        );
    }
    if let Some(instructions) = present(config, "planModeInstructions") {
        agent.insert("planModeInstructions".into(), instructions.clone());
    }
    if let Some(aliases) = present(config, "toolAliases").filter(|aliases| truthy(aliases)) {
        agent.insert("toolAliases".into(), aliases.clone());
    }
}

/// `options.setSdkSessionConfig` for the native host.
pub(super) struct NativeSdkSessionConfig {
    pub options: super::Options,
    pub overrides: Arc<SessionOverrides>,
}

impl SubmitSdkSessionConfig for NativeSdkSessionConfig {
    fn validate(&self, config: &JsValue) -> Result<(), SubmitError> {
        validate(config)?;
        let wired = matches!(config, JsValue::Array(_))
            || config.as_object().is_some_and(|config| {
                config
                    .own_entries()
                    .iter()
                    .filter(|(_, value)| !value.is_undefined())
                    .all(|(key, _)| WIRED_FIELDS.iter().any(|name| key.equals_str(name)))
            });
        if wired {
            Ok(())
        } else {
            Err(err(
                "CAPABILITY_UNAVAILABLE",
                "sdk_session_config is unavailable.",
            ))
        }
    }

    fn set<'a>(
        &'a self,
        session: &'a JsString,
        config: &'a JsValue,
        project: Option<&'a JsString>,
        _completion: &'a TurnCompletionHandle,
    ) -> SubmitFuture<'a, Result<bool, SubmitError>> {
        Box::pin(async move {
            // `this.resolve(projectKey)` and `sdkSessionProjects.set`.
            let root = self.options.project_root_for(project);
            // `JSON.stringify(previous) === JSON.stringify(next)`.
            let serialize = |value: &JsValue| stringify_json(value).ok().flatten();
            let previous = self.overrides.sdk_config(session);
            if previous.as_ref().map(serialize) == Some(serialize(config)) {
                return Ok(false);
            }
            self.overrides.set_sdk_config(session, config.clone());
            self.overrides.mark_sdk_config(session, root);
            Ok(true)
        })
    }
}
