//! `updatePilotLocalSettings` (src/pilot/config/updateLocalSettings.ts): the
//! allowlisted `localSettings` subset the SDK may persist into
//! `pilotdeck.yaml`. Validation, the YAML document edit (yaml@2.9.0
//! `setIn` / `deleteIn` / `toString`, ported in `pilotdeck_yaml::stringify`),
//! validation of the candidate against a scratch PILOT_HOME, and the atomic
//! replace of the config file.
use super::{LocalSettingsUpdate, LocalSettingsWriter, SettingsError};
use crate::config::{
    Env,
    store::{ConfigStore, StoreOptions},
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use pilotdeck_yaml::stringify::{EditableDocument, StringifyError};
use std::path::{Path, PathBuf};

/// The production writer: `getPilotConfigFilePath(resolvePilotHome(env))`
/// plus the `projectRoot` the candidate is validated with.
pub struct PilotLocalSettingsWriter {
    pub pilot_home: PathBuf,
    pub project_root: PathBuf,
    pub env: Env,
}

const READ_FAILED: &str = "LOCAL_SETTINGS_READ_FAILED";
const YAML_INVALID: &str = "LOCAL_SETTINGS_YAML_INVALID";
const INVALID: &str = "INVALID_LOCAL_SETTINGS";
const UNSUPPORTED: &str = "UNSUPPORTED_LOCAL_SETTING";
const CANDIDATE_INVALID: &str = "LOCAL_SETTINGS_INVALID";
const WRITE_FAILED: &str = "LOCAL_SETTINGS_WRITE_FAILED";

fn invalid(path: &str, suffix: &str) -> SettingsError {
    SettingsError::local_settings(INVALID, format!("{path} {suffix}"))
}

fn unsupported(path: &str) -> SettingsError {
    SettingsError::local_settings(
        UNSUPPORTED,
        format!("{path} is not supported by PilotDeck localSettings."),
    )
}

fn record(value: Option<&JsValue>) -> Option<&JsObject> {
    match value {
        Some(JsValue::Object(object)) => Some(object),
        _ => None,
    }
}

/// `value.x !== undefined` over a JSON-shaped wire object.
fn present<'a>(object: &'a JsObject, key: &str) -> Option<&'a JsValue> {
    object.get(key).filter(|value| !value.is_undefined())
}

fn key_text(key: &JsString) -> String {
    String::from_utf16_lossy(key.units())
}

fn check_keys(object: &JsObject, allowed: &[&str], prefix: &str) -> Result<(), SettingsError> {
    for (key, _) in object.entries() {
        let text = key_text(key);
        if !allowed.contains(&text.as_str()) {
            return Err(unsupported(&format!("{prefix}.{text}")));
        }
    }
    Ok(())
}

fn is_integer(value: &JsValue) -> Option<f64> {
    value
        .as_number()
        .filter(|n| n.is_finite() && n.fract() == 0.0)
}

/// One planned document edit: `set(path, value)` with `None` meaning
/// `null` (delete).
type Edit = (Vec<&'static str>, Option<JsValue>);

/// `normalizeSettings` + `applySettings`' ordering: the validated edits in
/// the order the original applies them.
fn plan(value: &JsValue) -> Result<Vec<Edit>, SettingsError> {
    let Some(settings) = record(Some(value)) else {
        return Err(SettingsError::local_settings(
            INVALID,
            "localSettings must be an object.",
        ));
    };
    for (key, _) in settings.entries() {
        let text = key_text(key);
        if !["agent", "extension", "tools"].contains(&text.as_str()) {
            return Err(unsupported(&format!("localSettings.{text}")));
        }
    }
    let mut edits: Vec<Edit> = Vec::new();
    if let Some(agent) = present(settings, "agent") {
        let Some(agent) = record(Some(agent)) else {
            return Err(invalid("localSettings.agent", "must be an object."));
        };
        check_keys(
            agent,
            &[
                "maxContextTokens",
                "maxOutputTokens",
                "thinking",
                "subagents",
            ],
            "localSettings.agent",
        )?;
        for key in ["maxContextTokens", "maxOutputTokens"] {
            let Some(value) = present(agent, key) else {
                continue;
            };
            if !matches!(value, JsValue::Null) && !is_integer(value).is_some_and(|n| n > 0.0) {
                return Err(invalid(
                    &format!("localSettings.agent.{key}"),
                    "must be a positive integer or null.",
                ));
            }
            edits.push((
                vec!["agent", key],
                (!matches!(value, JsValue::Null)).then(|| value.clone()),
            ));
        }
        if let Some(thinking) = present(agent, "thinking") {
            if matches!(thinking, JsValue::Null) {
                edits.push((vec!["agent", "thinking"], None));
            } else {
                let Some(thinking) = record(Some(thinking)) else {
                    return Err(invalid(
                        "localSettings.agent.thinking",
                        "must be an object or null.",
                    ));
                };
                check_keys(
                    thinking,
                    &["enabled", "budgetTokens"],
                    "localSettings.agent.thinking",
                )?;
                let Some(JsValue::Bool(enabled)) = thinking.get("enabled") else {
                    return Err(invalid(
                        "localSettings.agent.thinking.enabled",
                        "must be a boolean.",
                    ));
                };
                let budget = present(thinking, "budgetTokens");
                if let Some(budget) = budget
                    && !is_integer(budget).is_some_and(|n| n >= 0.0)
                {
                    return Err(invalid(
                        "localSettings.agent.thinking.budgetTokens",
                        "must be a non-negative integer.",
                    ));
                }
                let mut out = JsObject::new();
                out.insert("enabled".into(), JsValue::Bool(*enabled));
                if let Some(budget) = budget {
                    out.insert("budgetTokens".into(), budget.clone());
                }
                edits.push((vec!["agent", "thinking"], Some(JsValue::Object(out))));
            }
        }
        if let Some(subagents) = present(agent, "subagents") {
            if matches!(subagents, JsValue::Null) {
                edits.push((vec!["agent", "subagents"], None));
            } else {
                let Some(subagents) = record(Some(subagents)) else {
                    return Err(invalid(
                        "localSettings.agent.subagents",
                        "must be an object or null.",
                    ));
                };
                check_keys(
                    subagents,
                    &["default", "timeoutMs", "maxDepth"],
                    "localSettings.agent.subagents",
                )?;
                if let Some(model) = present(subagents, "default") {
                    let value = match model {
                        JsValue::Null => None,
                        JsValue::String(text)
                            if !crate::metadata::js_trim(&key_text(text)).is_empty() =>
                        {
                            Some(JsValue::String(
                                crate::metadata::js_trim(&key_text(text)).into(),
                            ))
                        }
                        _ => {
                            return Err(invalid(
                                "localSettings.agent.subagents.default",
                                "must be a non-empty \"provider/model\" string, \"inherit\", or null.",
                            ));
                        }
                    };
                    edits.push((vec!["agent", "subagents", "default"], value));
                }
                if let Some(timeout) = present(subagents, "timeoutMs") {
                    if !matches!(timeout, JsValue::Null)
                        && !is_integer(timeout).is_some_and(|n| n > 0.0)
                    {
                        return Err(invalid(
                            "localSettings.agent.subagents.timeoutMs",
                            "must be a positive integer or null.",
                        ));
                    }
                    edits.push((
                        vec!["agent", "subagents", "timeoutMs"],
                        (!matches!(timeout, JsValue::Null)).then(|| timeout.clone()),
                    ));
                }
                if let Some(depth) = present(subagents, "maxDepth") {
                    // Number.isSafeInteger
                    if !matches!(depth, JsValue::Null)
                        && !is_integer(depth)
                            .is_some_and(|n| (0.0..=9_007_199_254_740_991.0).contains(&n))
                    {
                        return Err(invalid(
                            "localSettings.agent.subagents.maxDepth",
                            "must be a non-negative safe integer or null.",
                        ));
                    }
                    edits.push((
                        vec!["agent", "subagents", "maxDepth"],
                        (!matches!(depth, JsValue::Null)).then(|| depth.clone()),
                    ));
                }
            }
        }
    }
    if let Some(extension) = present(settings, "extension") {
        let Some(extension) = record(Some(extension)) else {
            return Err(invalid("localSettings.extension", "must be an object."));
        };
        check_keys(
            extension,
            &["includeHookEvents", "builtinPluginsEnabled"],
            "localSettings.extension",
        )?;
        let mut extension_edits = Vec::new();
        if let Some(include) = present(extension, "includeHookEvents") {
            if !matches!(include, JsValue::Null | JsValue::Bool(_)) {
                return Err(invalid(
                    "localSettings.extension.includeHookEvents",
                    "must be a boolean or null.",
                ));
            }
            extension_edits.push((
                vec!["extension", "includeHookEvents"],
                (!matches!(include, JsValue::Null)).then(|| include.clone()),
            ));
        }
        if let Some(enabled) = present(extension, "builtinPluginsEnabled") {
            if matches!(enabled, JsValue::Null) {
                extension_edits.push((vec!["extension", "builtinPluginsEnabled"], None));
            } else {
                let Some(enabled) = record(Some(enabled)) else {
                    return Err(invalid(
                        "localSettings.extension.builtinPluginsEnabled",
                        "must be an object of booleans or null.",
                    ));
                };
                let mut normalized = JsObject::new();
                for (name, value) in enabled.entries() {
                    if crate::metadata::js_trim(&key_text(name)).is_empty()
                        || !matches!(value, JsValue::Bool(_))
                    {
                        return Err(invalid(
                            "localSettings.extension.builtinPluginsEnabled",
                            "must be an object with non-empty keys and boolean values.",
                        ));
                    }
                    normalized.insert(name.clone(), value.clone());
                }
                extension_edits.push((
                    vec!["extension", "builtinPluginsEnabled"],
                    Some(JsValue::Object(normalized)),
                ));
            }
        }
        edits.extend(extension_edits);
    }
    if let Some(tools) = present(settings, "tools") {
        let Some(tools) = record(Some(tools)) else {
            return Err(invalid("localSettings.tools", "must be an object."));
        };
        for (key, _) in tools.entries() {
            let text = key_text(key);
            if text != "webSearch" {
                return Err(unsupported(&format!("localSettings.tools.{text}")));
            }
        }
        if let Some(web_search) = present(tools, "webSearch") {
            let Some(web_search) = record(Some(web_search)) else {
                return Err(invalid(
                    "localSettings.tools.webSearch",
                    "must be an object.",
                ));
            };
            for (key, _) in web_search.entries() {
                let text = key_text(key);
                if text != "enabled" {
                    return Err(unsupported(&format!(
                        "localSettings.tools.webSearch.{text}"
                    )));
                }
            }
            if let Some(enabled) = present(web_search, "enabled") {
                if !matches!(enabled, JsValue::Null | JsValue::Bool(_)) {
                    return Err(invalid(
                        "localSettings.tools.webSearch.enabled",
                        "must be a boolean or null.",
                    ));
                }
                edits.push((
                    vec!["tools", "webSearch", "enabled"],
                    (!matches!(enabled, JsValue::Null)).then(|| enabled.clone()),
                ));
            }
        }
    }
    Ok(edits)
}

/// Node's `fs` error text for a failed `readFile(path)`.
fn read_error_message(error: &std::io::Error, path: &Path) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => format!(
            "ENOENT: no such file or directory, open '{}'",
            path.display()
        ),
        std::io::ErrorKind::PermissionDenied => {
            format!("EACCES: permission denied, open '{}'", path.display())
        }
        _ => error.to_string(),
    }
}

fn edit_error(error: StringifyError) -> SettingsError {
    match error {
        // A plain JS `Error` thrown by the document edit.
        StringifyError::Error(message) => SettingsError::plain("Error", message),
        StringifyError::Unsupported(what) => SettingsError::local_settings(
            WRITE_FAILED,
            format!("Unable to write local settings: {what} is not supported by this host."),
        ),
    }
}

impl PilotLocalSettingsWriter {
    fn config_path(&self) -> PathBuf {
        self.pilot_home.join("pilotdeck.yaml")
    }

    /// `validateCandidateConfig`: load the candidate from a scratch
    /// PILOT_HOME with the same environment and project root.
    fn validate_candidate(&self, candidate: &str) -> Result<(), SettingsError> {
        let failed = |message: String| {
            SettingsError::local_settings(
                CANDIDATE_INVALID,
                format!("localSettings failed PilotDeck validation: {message}"),
            )
        };
        let home = std::env::temp_dir().join(format!(
            "pilotdeck-sdk-settings-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..6]
        ));
        std::fs::create_dir(&home).map_err(|e| failed(e.to_string()))?;
        let result = (|| {
            std::fs::write(home.join("pilotdeck.yaml"), candidate)
                .map_err(|e| failed(e.to_string()))?;
            let mut env = self.env.clone();
            env.insert("PILOT_HOME".into(), home.to_string_lossy().into_owned());
            let mut options = StoreOptions::new(&home, &self.project_root, env);
            options.project = Some(self.project_root.to_string_lossy().into_owned());
            ConfigStore::new(options)
                .map(|_| ())
                .map_err(|error| failed(String::from_utf16_lossy(error.message.units())))
        })();
        let _ = std::fs::remove_dir_all(&home);
        result
    }

    /// `atomicWrite(path, content)`: a sibling temp file with the original
    /// mode, renamed over the config.
    fn atomic_write(&self, path: &Path, content: &str) -> Result<(), SettingsError> {
        use std::io::Write;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temporary = path.with_file_name(format!(".{name}.sdk-{}.tmp", uuid::Uuid::new_v4()));
        let mode = std::fs::metadata(path).ok().map(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o7777
        });
        let write = || -> std::io::Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            if let Some(mode) = mode {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(mode);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(content.as_bytes())?;
            drop(file);
            std::fs::rename(&temporary, path)
        };
        write().map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            SettingsError::local_settings(
                WRITE_FAILED,
                format!("Unable to write local settings: {error}"),
            )
        })
    }
}

impl LocalSettingsWriter for PilotLocalSettingsWriter {
    fn update(&self, settings: &JsValue) -> Result<LocalSettingsUpdate, SettingsError> {
        let edits = plan(settings)?;
        let path = self.config_path();
        let original = std::fs::read_to_string(&path).map_err(|error| {
            SettingsError::local_settings(
                READ_FAILED,
                format!(
                    "Unable to read local settings: {}",
                    read_error_message(&error, &path)
                ),
            )
        })?;
        let document = pilotdeck_yaml::parse_document(
            &JsString::from(original.as_str()),
            pilotdeck_yaml::Options {
                pretty_errors: false,
                ..pilotdeck_yaml::Options::default()
            },
        );
        let is_map = document.contents.is_some_and(|id| {
            matches!(
                document.nodes[id].kind,
                pilotdeck_yaml::nodes::NodeKind::Map(_)
            )
        });
        if !document.errors.is_empty() || !is_map {
            let detail = document
                .errors
                .first()
                .map(|e| String::from_utf16_lossy(e.message.units()))
                .unwrap_or_else(|| "Config root must be a YAML mapping.".into());
            return Err(SettingsError::local_settings(
                YAML_INVALID,
                format!("Unable to update local settings: {detail}"),
            ));
        }
        let mut document = EditableDocument::new(document);
        let mut update = LocalSettingsUpdate::default();
        for (path, value) in &edits {
            let label: JsString = path.join(".").into();
            match value {
                None => {
                    document.delete_in(path).map_err(edit_error)?;
                    update.cleared.push(label);
                }
                Some(value) => {
                    document.set_in(path, value).map_err(edit_error)?;
                    update.applied.push(label);
                }
            }
        }
        if update.applied.is_empty() && update.cleared.is_empty() {
            return Ok(update);
        }
        let updated = document.to_yaml_string().map_err(edit_error)?;
        self.validate_candidate(&updated)?;
        self.atomic_write(&path, &updated)?;
        update.changed_paths = update
            .applied
            .iter()
            .chain(update.cleared.iter())
            .cloned()
            .collect();
        Ok(update)
    }
}
