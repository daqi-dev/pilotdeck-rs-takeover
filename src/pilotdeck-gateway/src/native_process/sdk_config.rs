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
    "outputFormat",
    "forwardSubagentText",
    "agentProgressSummaries",
    "outputStyle",
    "permissionMode",
    "additionalWorkingDirectories",
    "managedPermissions",
    "hooks",
    "includeHookEvents",
    "fallbackModel",
    "managedModels",
    "agents",
    "skills",
    "managedTools",
    "deferredTools",
    "persistSession",
    "promptSuggestions",
    "settings",
    "settingSources",
];

/// The `agents.<id>` keys whose effect the native subagent composes. A
/// definition naming any other key (`mcpServers`, `skills`, `memory`,
/// `initialPrompt`, `observer`, `observerMessage`) keeps the
/// whole config unavailable.
const WIRED_AGENT_FIELDS: &[&str] = &[
    "description",
    "prompt",
    "tools",
    "disallowedTools",
    "model",
    "maxTurns",
    "permissionMode",
    "background",
    // Carried on the definition; the baseline's subagent runtime reads it
    // nowhere.
    "effort",
    "criticalSystemReminder_EXPERIMENTAL",
];

fn agents_wired(config: &JsObject) -> bool {
    let Some(JsValue::Object(agents)) = config.get("agents") else {
        return true;
    };
    agents.own_entries().iter().all(|(_, agent)| {
        agent.as_object().is_some_and(|agent| {
            agent
                .own_entries()
                .iter()
                .filter(|(_, value)| !value.is_undefined())
                .all(|(key, _)| WIRED_AGENT_FIELDS.iter().any(|name| key.equals_str(name)))
        })
    })
}

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
    // SessionAgentConfigBundle.ts:140: an output format only makes the loop
    // stop once a structured output exists (AgentLoop.ts:2192-2207).
    if present(config, "outputFormat").is_some_and(truthy) {
        agent.insert("stopOnStructuredOutput".into(), JsValue::Bool(true));
    }
    // SessionAgentConfigBundle.ts:131: only a literal `true` turns on the
    // tool progress events (AgentLoop.ts:2770-2784).
    if config.get("agentProgressSummaries") == Some(&JsValue::Bool(true)) {
        agent.insert("includeToolProgress".into(), JsValue::Bool(true));
    }
}

/// `extensionView.getAllSkills()` of the project the config is set for, after
/// `runtime.pluginRuntime.refresh()`.
async fn project_skill_names(
    registry: &crate::runtime::project::ProjectRuntimeRegistry,
    root: &std::path::Path,
) -> Result<Vec<String>, SubmitError> {
    let lease = registry
        .acquire(Some(root))
        .map_err(|error| err("gateway_request_failed", error.to_string()))?;
    let resources = lease
        .runtime()
        .resources()
        .get::<crate::runtime::project::ProjectResources>()
        .ok_or_else(|| {
            err(
                "gateway_request_failed",
                "Native project resources are missing.",
            )
        })?;
    let Some(resource) = resources
        .context()
        .resource(crate::runtime::project::Resource::PluginRuntime)
    else {
        return Ok(Vec::new());
    };
    let Some(plugins) = resource
        .as_any()
        .downcast_ref::<crate::runtime::project::plugins::PluginResources>()
    else {
        return Ok(Vec::new());
    };
    plugins
        .runtime
        .refresh()
        .await
        .map_err(|error| err("gateway_request_failed", error.to_string()))?;
    Ok(plugins
        .runtime
        .all_skills()
        .await
        .into_iter()
        .map(|skill| skill.name)
        .collect())
}

/// `resolveSdkSessionSkills` (ProjectRuntimeRegistry.ts:1850-1869): each
/// requested name is an exact skill name or the unique `:<name>` suffix of
/// one; duplicates collapse.
fn resolve_session_skills(
    requested: &[JsValue],
    available: &[String],
) -> Result<Vec<String>, SubmitError> {
    let mut resolved: Vec<String> = Vec::new();
    for requested in requested {
        let requested = requested.as_string().map(text).unwrap_or_default();
        let exact: Vec<&String> = available
            .iter()
            .filter(|name| **name == requested)
            .collect();
        let suffix = format!(":{requested}");
        let candidates = if exact.is_empty() {
            available
                .iter()
                .filter(|name| name.ends_with(&suffix))
                .collect()
        } else {
            exact
        };
        match candidates.as_slice() {
            [] => {
                return Err(err(
                    "SDK_SKILL_NOT_FOUND",
                    format!("SDK session skill is not available in this project: {requested}"),
                ));
            }
            [name] => {
                if !resolved.contains(name) {
                    resolved.push((*name).clone());
                }
            }
            _ => {
                return Err(err(
                    "SDK_SKILL_AMBIGUOUS",
                    format!(
                        "SDK session skill is ambiguous; use its fully-qualified name: {requested}"
                    ),
                ));
            }
        }
    }
    Ok(resolved)
}

/// `sdkSessionConfig.skills` as the stored (resolved) name list.
pub(super) fn session_skills(config: Option<&JsValue>) -> Option<Vec<String>> {
    match config.and_then(|config| config.get("skills")) {
        Some(JsValue::Array(names)) => Some(
            names
                .iter()
                .filter_map(|name| name.as_string().map(text))
                .collect(),
        ),
        _ => None,
    }
}

/// `options.setSdkSessionConfig` for the native host.
pub(super) struct NativeSdkSessionConfig {
    pub options: super::Options,
    pub overrides: Arc<SessionOverrides>,
    /// The project runtimes whose model catalog the server-side checks
    /// resolve against (`this.resolve(projectKey).snapshot`).
    pub project: Arc<crate::runtime::project::ProjectRuntimeRegistry>,
}

/// `runtime.snapshot.config.model` of the project the config is set for.
pub(super) fn project_model_config(
    registry: &crate::runtime::project::ProjectRuntimeRegistry,
    root: &std::path::Path,
) -> Result<JsValue, SubmitError> {
    let lease = registry
        .acquire(Some(root))
        .map_err(|error| err("gateway_request_failed", error.to_string()))?;
    let resources = lease
        .runtime()
        .resources()
        .get::<crate::runtime::project::ProjectResources>()
        .ok_or_else(|| {
            err(
                "gateway_request_failed",
                "Native project resources are missing.",
            )
        })?;
    Ok(resources
        .context()
        .snapshot()
        .get("config")
        .and_then(|config| config.get("model"))
        .cloned()
        .unwrap_or(JsValue::Undefined))
}

/// `resolveSdkConfiguredModel` (ProjectRuntimeRegistry.ts:1891-1924): the one
/// catalog entry whose id, `provider/model` or bare model key equals the
/// request.
pub(super) fn resolve_configured_model(
    model_config: &JsValue,
    requested: &JsString,
    code: &str,
    label: &str,
) -> Result<(JsString, JsString), SubmitError> {
    let mut candidates = Vec::new();
    if let Some(providers) = model_config.get("providers").and_then(JsValue::as_object) {
        for (provider, config) in providers.own_entries() {
            let Some(models) = config.get("models").and_then(JsValue::as_object) else {
                continue;
            };
            for (model, definition) in models.own_entries() {
                let mut qualified = provider.units().to_vec();
                qualified.push(u16::from(b'/'));
                qualified.extend_from_slice(model.units());
                if definition.get("id").and_then(JsValue::as_string) == Some(requested)
                    || requested.units() == qualified.as_slice()
                    || requested == model
                {
                    candidates.push((provider.clone(), model.clone()));
                }
            }
        }
    }
    if candidates.len() != 1 {
        return Err(err(
            code,
            format!(
                "{label} is not uniquely resolvable from the Gateway catalog: {}",
                text(requested)
            ),
        ));
    }
    Ok(candidates.remove(0))
}

#[derive(Clone, Copy, PartialEq)]
enum SelectorKind {
    Tools,
    Models,
}

/// `/^[A-Za-z0-9][A-Za-z0-9_.:-]*\*?$/`: a tool selector.
fn valid_tool_selector(selector: &str) -> bool {
    let body = selector.strip_suffix('*').unwrap_or(selector);
    let mut chars = body.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// `validateSdkSelectorPolicy(next.managedTools | next.managedModels, kind)`
/// (ProjectRuntimeRegistry.ts:1958-1975), the registry's second pass after
/// the gateway's shape check: each list's trimmed selectors are valid and
/// distinct.
fn validate_selector_policy(
    policy: Option<&JsValue>,
    kind: SelectorKind,
) -> Result<(), SubmitError> {
    let Some(policy) = policy.and_then(JsValue::as_object) else {
        return Ok(());
    };
    let field = if kind == SelectorKind::Tools {
        "managedTools"
    } else {
        "managedModels"
    };
    for label in ["allow", "deny"] {
        let invalid = || {
            err(
                "INVALID_SDK_MANAGED_SETTINGS",
                format!("{field}.{label} contains an invalid selector."),
            )
        };
        let Some(JsValue::Array(entries)) = policy.get(label) else {
            return Err(invalid());
        };
        let mut seen: Vec<String> = Vec::new();
        for entry in entries {
            let Some(entry) = entry.as_string() else {
                return Err(invalid());
            };
            let trimmed = crate::metadata::js_trim(&text(entry)).to_owned();
            let valid = trimmed == "*"
                || (kind == SelectorKind::Tools && valid_tool_selector(&trimmed))
                || kind == SelectorKind::Models
                    && trimmed.split_once('/').is_some_and(|(provider, model)| {
                        let plain = |part: &str| {
                            !part.is_empty()
                                && part
                                    .chars()
                                    .all(|c| c != '/' && !crate::runtime::tools::js_space(c))
                        };
                        plain(provider) && (model == "*" || plain(model))
                    });
            if !valid {
                return Err(invalid());
            }
            seen.push(trimmed);
        }
        let mut distinct = seen.clone();
        distinct.sort();
        distinct.dedup();
        if distinct.len() != seen.len() {
            return Err(err(
                "INVALID_SDK_MANAGED_SETTINGS",
                format!("{field}.{label} cannot contain duplicates."),
            ));
        }
    }
    Ok(())
}

/// `assertSdkManagedModelAllowed` (ProjectRuntimeRegistry.ts:1926-1940).
pub(super) fn assert_managed_model_allowed(
    policy: Option<&JsValue>,
    provider: &JsString,
    model: &JsString,
) -> Result<(), SubmitError> {
    let Some(policy) = policy.and_then(JsValue::as_object) else {
        return Ok(());
    };
    let (provider, model) = (text(provider), text(model));
    let matches = |selector: &JsValue| {
        selector.as_string().is_some_and(|selector| {
            let selector = text(selector);
            selector == "*"
                || selector == format!("{provider}/*")
                || selector == format!("{provider}/{model}")
        })
    };
    let list = |label: &str| match policy.get(label) {
        Some(JsValue::Array(entries)) => entries.clone(),
        _ => Vec::new(),
    };
    let (allow, deny) = (list("allow"), list("deny"));
    if !deny.iter().any(matches) && (allow.is_empty() || allow.iter().any(matches)) {
        return Ok(());
    }
    Err(err(
        "SDK_MANAGED_MODEL_DENIED",
        format!("SDK managedSettings.models denies model {provider}/{model}."),
    ))
}

/// `resolveSdkSettings(runtime, config)` (ProjectRuntimeRegistry.ts:1038-1056):
/// the session overlay from the selected Gateway config files
/// (`settingSources`: user = `<pilot home>/pilotdeck.yaml`, project =
/// `<project>/.pilotdeck/pilotdeck.yaml`, local =
/// `<project>/.pilotdeck/pilotdeck.local.yaml`, in that precedence) under the
/// SDK's own `settings` (pilot/config/sdkSessionSettings.ts:81-103). The
/// files are read on every call, like the baseline. No organization policy
/// exists in this host, so no host defaults, managed, or enforced layers.
pub(super) fn resolve_settings(
    home: &std::path::Path,
    os_home: Option<&std::path::Path>,
    project_root: &std::path::Path,
    config: Option<&JsValue>,
) -> Result<Option<JsValue>, crate::runtime::sdk_session::SdkSessionError> {
    let Some(config) = config.and_then(JsValue::as_object) else {
        return Ok(None);
    };
    let env: crate::config::Env = [("PILOT_HOME".to_owned(), home.to_string_lossy().into_owned())]
        .into_iter()
        .collect();
    crate::runtime::sdk_session::resolve_session_settings(
        crate::runtime::sdk_session::ResolveSettingsOptions {
            project_root,
            cwd: project_root,
            env: &env,
            homedir: os_home,
            host_defaults: None,
            host_managed_settings: None,
            host_setting_sources: None,
            settings: present(config, "settings"),
            setting_sources: present(config, "settingSources"),
            host_enforced_settings: None,
        },
    )
    // `mapPilotSdkSettingsError`: a settings error keeps its code as a
    // `DialogGatewayError`.
    .map_err(|error| {
        if error.name == "PilotSdkSessionSettingsError" {
            crate::runtime::sdk_session::SdkSessionError::new(error.code, error.message)
        } else {
            error
        }
    })
}

/// A settings failure inside `setSdkSessionConfig` as the submit preflight
/// reports it: a `DialogGatewayError` keeps its code, anything else is
/// `INVALID_SDK_SESSION_CONFIG` (InProcessGateway.ts:1105-1112).
pub(super) fn settings_submit_error(
    error: crate::runtime::sdk_session::SdkSessionError,
) -> SubmitError {
    if error.name == "DialogGatewayError" {
        err(&error.code, error.message)
    } else {
        invalid(error.message)
    }
}

/// The resolved overlay's `agent` section.
pub(super) fn settings_agent(settings: Option<&JsValue>) -> Option<&JsObject> {
    settings
        .and_then(|settings| settings.get("agent"))
        .and_then(JsValue::as_object)
}

/// Applies the resolved SDK settings overlay to the project's `agent`
/// section before the session config is composed from it
/// (SessionAgentConfigBundle.ts:60-151): the session model
/// (`resolveSdkModel(settings.agent.model, agent.model)`), the context and
/// output limits (`settings ?? agent ?? capabilities`), thinking
/// (`settings.agent.thinking ?? agent.thinking`, under the session's own
/// thinking override) and the subagent section (`default`: `null` drops the
/// configured fork model, a model id resolves against the session model;
/// `timeoutMs` / `maxDepth` replace the configured ones).
pub(super) fn apply_settings_overlay(agent: &mut JsValue, settings: Option<&JsValue>) {
    let Some(overlay) = settings_agent(settings) else {
        return;
    };
    let JsValue::Object(section) = agent else {
        return;
    };
    let current = section.get("model").cloned().unwrap_or(JsValue::Undefined);
    let parent = (
        current
            .get("provider")
            .cloned()
            .unwrap_or(JsValue::Undefined),
        current.get("model").cloned().unwrap_or(JsValue::Undefined),
    );
    let requested = match overlay.get("model").and_then(JsValue::as_string) {
        Some(model) if !model.is_empty() => resolve_sdk_model(model, parent.clone()),
        _ => parent.clone(),
    };
    if requested != parent {
        let mut model = JsObject::new();
        model.insert("provider".into(), requested.0.clone());
        model.insert("model".into(), requested.1.clone());
        section.insert("model".into(), JsValue::Object(model));
    }
    for key in ["maxContextTokens", "maxOutputTokens", "thinking"] {
        if let Some(value) = present(overlay, key) {
            section.insert(key.into(), value.clone());
        }
    }
    let Some(subagents) = overlay.get("subagents").and_then(JsValue::as_object) else {
        return;
    };
    let mut composed = section
        .get("subagents")
        .and_then(JsValue::as_object)
        .cloned()
        .unwrap_or_default();
    match subagents.get("default") {
        None => {}
        Some(JsValue::String(model)) => {
            let (provider, name) = resolve_sdk_model(model, requested);
            let mut selected = JsObject::new();
            selected.insert("provider".into(), provider);
            selected.insert("model".into(), name);
            composed.insert("default".into(), JsValue::Object(selected));
        }
        Some(_) => {
            let mut kept = JsObject::new();
            for (key, value) in composed.entries() {
                if !key.equals_str("default") {
                    kept.insert(key.clone(), value.clone());
                }
            }
            composed = kept;
        }
    }
    for key in ["timeoutMs", "maxDepth"] {
        if let Some(value) = present(subagents, key) {
            composed.insert(key.into(), value.clone());
        }
    }
    section.insert("subagents".into(), JsValue::Object(composed));
}

/// `resolveSdkModel` (SessionAgentConfigBundle.ts:242-250): split at the first
/// slash; an empty side or no slash keeps the parent model.
fn resolve_sdk_model(requested: &JsString, parent: (JsValue, JsValue)) -> (JsValue, JsValue) {
    let units = requested.units();
    match units.iter().position(|unit| *unit == u16::from(b'/')) {
        Some(slash) if slash > 0 && slash != units.len() - 1 => (
            JsValue::String(JsString::from_utf16(units[..slash].to_vec())),
            JsValue::String(JsString::from_utf16(units[slash + 1..].to_vec())),
        ),
        _ => parent,
    }
}

/// `subagentDefinitions: toSdkSubagentDefinitions(sdk.agents)`
/// (SessionAgentConfigBundle.ts:141,206-239): the SDK's agents replace the
/// session's subagent table. No organization `maxTurns` cap exists here.
pub(super) fn apply_agent_fields(agent: &mut JsObject, config: Option<&JsValue>) {
    let Some(JsValue::Object(agents)) = config.and_then(|config| config.get("agents")) else {
        return;
    };
    let mut definitions = JsObject::new();
    for (id, sdk) in agents.own_entries() {
        let field = |key: &str| sdk.get(key).filter(|value| !value.is_undefined());
        let mut definition = JsObject::new();
        definition.insert("id".into(), JsValue::String(id.clone()));
        definition.insert(
            "description".into(),
            field("description").cloned().unwrap_or(JsValue::Undefined),
        );
        definition.insert(
            "systemPromptSuffix".into(),
            field("prompt").cloned().unwrap_or(JsValue::Undefined),
        );
        definition.insert(
            "allowedTools".into(),
            match field("tools") {
                Some(tools) if truthy(tools) => tools.clone(),
                _ => JsValue::Array(vec!["*".into()]),
            },
        );
        if let Some(JsValue::Array(denied)) = field("disallowedTools")
            && !denied.is_empty()
        {
            definition.insert("disallowedTools".into(), JsValue::Array(denied.clone()));
        }
        definition.insert("omitProjectInstructions".into(), JsValue::Bool(false));
        definition.insert("omitGitStatus".into(), JsValue::Bool(false));
        definition.insert(
            "isReadOnly".into(),
            JsValue::Bool(field("permissionMode") == Some(&"plan".into())),
        );
        if let Some(model) = field("model")
            .and_then(JsValue::as_string)
            .filter(|model| !model.is_empty())
            && let resolved = resolve_sdk_model(model, ("".into(), "".into()))
            && let (JsValue::String(provider), JsValue::String(name)) = (&resolved.0, &resolved.1)
            && !provider.is_empty()
            && !name.is_empty()
        {
            let mut model = JsObject::new();
            model.insert("provider".into(), JsValue::String(provider.clone()));
            model.insert("model".into(), JsValue::String(name.clone()));
            definition.insert("modelOverride".into(), JsValue::Object(model));
        }
        for (key, source) in [
            ("maxTurns", "maxTurns"),
            ("effort", "effort"),
            ("permissionMode", "permissionMode"),
            (
                "criticalSystemReminder",
                "criticalSystemReminder_EXPERIMENTAL",
            ),
        ] {
            if let Some(value) = field(source) {
                definition.insert(key.into(), value.clone());
            }
        }
        if field("background") == Some(&JsValue::Bool(true)) {
            definition.insert("background".into(), JsValue::Bool(true));
        }
        definitions.insert(id.clone(), JsValue::Object(definition));
    }
    agent.insert("subagentDefinitions".into(), JsValue::Object(definitions));
}

/// The model-routing fields of `SessionAgentConfigBundle.compose`
/// (SessionAgentConfigBundle.ts:127-131): `fallbackModel` becomes the one
/// session fallback the router tries after a pre-content retryable failure,
/// and `managedModels` the session's restrictive model policy.
pub(super) fn apply_model_fields(
    agent: &mut JsObject,
    config: Option<&JsValue>,
    settings: Option<&JsValue>,
) {
    let Some(config) = config.and_then(JsValue::as_object) else {
        return;
    };
    if let Some(policy) = present(config, "managedModels").filter(|policy| truthy(policy)) {
        agent.insert("managedModelPolicy".into(), policy.clone());
    }
    // `sdk?.fallbackModel ?? sdk?.settings?.agent?.fallbackModel`, where
    // `settings` is the resolved overlay (sources under the SDK's own).
    let nullish = |value: &&JsValue| !matches!(value, JsValue::Null);
    let requested = match present(config, "fallbackModel")
        .filter(nullish)
        .or_else(|| settings_agent(settings).and_then(|agent| present(agent, "fallbackModel")))
    {
        None | Some(JsValue::Null) => return,
        Some(value) => value,
    };
    let parent = (
        agent.get("provider").cloned().unwrap_or(JsValue::Undefined),
        agent.get("model").cloned().unwrap_or(JsValue::Undefined),
    );
    let (provider, model) = match requested.as_string() {
        Some(requested) if !requested.is_empty() => resolve_sdk_model(requested, parent),
        _ => parent,
    };
    let mut fallback = JsObject::new();
    fallback.insert("provider".into(), provider);
    fallback.insert("model".into(), model);
    agent.insert(
        "fallbackModels".into(),
        JsValue::Array(vec![JsValue::Object(fallback)]),
    );
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
        // With a session skill list, `resolveSdkSubagentScopes` gives every
        // agent that list as its own `skills` scope, which the native child
        // context does not compose.
        let scoped_agents = matches!(config.get("skills"), Some(JsValue::Array(_)))
            && config
                .get("agents")
                .is_some_and(|agents| !agents.is_undefined());
        let wired = wired && !scoped_agents && config.as_object().is_none_or(agents_wired);
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
        Box::pin(self.apply(session, config, project))
    }
}

impl NativeSdkSessionConfig {
    /// `registry.setSdkSessionConfig(sessionKey, config, projectKey)`
    /// (ProjectRuntimeRegistry.ts:956-1040): shared by `submit_turn` and
    /// `seed_read_state` (InProcessGateway.ts:2431-2436), which has no turn.
    /// `Ok(true)` when the stored config changed.
    pub(super) async fn apply(
        &self,
        session: &JsString,
        config: &JsValue,
        project: Option<&JsString>,
    ) -> Result<bool, SubmitError> {
        {
            // `this.resolve(projectKey)` and `sdkSessionProjects.set`.
            let root = self.options.project_root_for(project);
            validate_selector_policy(config.get("managedTools"), SelectorKind::Tools)?;
            validate_selector_policy(config.get("managedModels"), SelectorKind::Models)?;
            // `resolveSdkSettings(runtime, next)` and the models it names
            // (ProjectRuntimeRegistry.ts:976-986): each must resolve from the
            // catalog and pass the session's model policy.
            let settings = resolve_settings(
                &self.options.home,
                self.options.os_home.as_deref(),
                &root,
                Some(config),
            )
            .map_err(settings_submit_error)?;
            if let Some(agent) = settings_agent(settings.as_ref()) {
                let subagent_default = agent
                    .get("subagents")
                    .and_then(|subagents| subagents.get("default"));
                for (requested, code, label) in [
                    (
                        agent.get("model"),
                        "INVALID_SDK_AGENT_MODEL",
                        "settings.agent.model",
                    ),
                    (
                        agent.get("fallbackModel"),
                        "INVALID_SDK_FALLBACK_MODEL",
                        "fallbackModel",
                    ),
                    (
                        subagent_default,
                        "INVALID_SDK_SUBAGENT_MODEL",
                        "settings.agent.subagents.default",
                    ),
                ] {
                    let Some(requested) = requested.and_then(JsValue::as_string) else {
                        continue;
                    };
                    let model_config = project_model_config(&self.project, &root)?;
                    let resolved = resolve_configured_model(&model_config, requested, code, label)?;
                    assert_managed_model_allowed(
                        config.get("managedModels"),
                        &resolved.0,
                        &resolved.1,
                    )?;
                }
            }
            // `JSON.stringify(previous) === JSON.stringify(next)`.
            let serialize = |value: &JsValue| stringify_json(value).ok().flatten();
            let previous = self.overrides.sdk_config(session);
            if previous.as_ref().map(serialize) == Some(serialize(config)) {
                return Ok(false);
            }
            let mut next = config.clone();
            // `next.skills = resolveSdkSessionSkills(next.skills,
            // extensionView.getAllSkills())` after a plugin refresh.
            if let Some(JsValue::Array(requested)) = config.get("skills") {
                let available = project_skill_names(&self.project, &root).await?;
                let resolved = resolve_session_skills(requested, &available)?;
                if let JsValue::Object(fields) = &mut next {
                    fields.insert(
                        "skills".into(),
                        JsValue::Array(resolved.iter().map(|name| name.as_str().into()).collect()),
                    );
                }
            }
            // `resolveSdkSubagentScopes`: an agent's model resolves against
            // the catalog and the session's model policy, stored qualified.
            if let Some(JsValue::Object(agents)) = config.get("agents") {
                let mut resolved_agents = agents.clone();
                for (id, agent) in agents.own_entries() {
                    let Some(model) = agent
                        .get("model")
                        .and_then(JsValue::as_string)
                        .filter(|model| !model.is_empty())
                    else {
                        continue;
                    };
                    let model_config = project_model_config(&self.project, &root)?;
                    let (provider, name) = resolve_configured_model(
                        &model_config,
                        model,
                        "INVALID_SDK_AGENT_MODEL",
                        "model",
                    )
                    .map_err(|_| {
                        err(
                            "INVALID_SDK_AGENT_MODEL",
                            format!(
                                "Agent {} model is not uniquely resolvable from the Gateway catalog: {}",
                                text(id),
                                text(model)
                            ),
                        )
                    })?;
                    assert_managed_model_allowed(config.get("managedModels"), &provider, &name)?;
                    if let Some(JsValue::Object(agent)) = resolved_agents.get_key(id) {
                        let mut agent = agent.clone();
                        agent.insert(
                            "model".into(),
                            JsValue::String(format!("{}/{}", text(&provider), text(&name)).into()),
                        );
                        resolved_agents.insert(id.clone(), JsValue::Object(agent));
                    }
                }
                if let JsValue::Object(fields) = &mut next {
                    fields.insert("agents".into(), JsValue::Object(resolved_agents));
                }
            }
            let config = &next;
            // `if (next.settings?.agent?.model)` (ProjectRuntimeRegistry.ts:
            // 1006-1010): the SDK's own model, labelled `model`.
            if let Some(requested) = config
                .get("settings")
                .and_then(|settings| settings.get("agent"))
                .and_then(|agent| agent.get("model"))
                .and_then(JsValue::as_string)
                .filter(|requested| !requested.is_empty())
            {
                let model_config = project_model_config(&self.project, &root)?;
                let resolved = resolve_configured_model(
                    &model_config,
                    requested,
                    "INVALID_SDK_MODEL",
                    "model",
                )?;
                assert_managed_model_allowed(
                    config.get("managedModels"),
                    &resolved.0,
                    &resolved.1,
                )?;
            }
            // `if (next.fallbackModel)`: resolvable from this project's
            // catalog and allowed by the session's model policy
            // (ProjectRuntimeRegistry.ts:1014-1018).
            if let Some(requested) = config
                .get("fallbackModel")
                .and_then(JsValue::as_string)
                .filter(|requested| !requested.is_empty())
            {
                let model_config = project_model_config(&self.project, &root)?;
                let resolved = resolve_configured_model(
                    &model_config,
                    requested,
                    "INVALID_SDK_FALLBACK_MODEL",
                    "fallbackModel",
                )?;
                assert_managed_model_allowed(
                    config.get("managedModels"),
                    &resolved.0,
                    &resolved.1,
                )?;
            }
            self.overrides.set_sdk_config(session, config.clone());
            self.overrides.mark_sdk_config(session, root);
            Ok(true)
        }
    }
}
