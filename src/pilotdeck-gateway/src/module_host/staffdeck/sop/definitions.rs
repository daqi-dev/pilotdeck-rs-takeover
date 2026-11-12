use super::*;
use crate::metadata::object;
use std::{collections::HashSet, path::Path};

pub(super) fn load(path: &Path) -> Result<JsValue, SopError> {
    if !path.exists() {
        return Err(invalid(format!(
            "StaffDeck SOP definitions file does not exist: {}",
            path.display()
        )));
    }
    let source = std::fs::read(path).map_err(|e| {
        invalid(format!(
            "Failed to read StaffDeck SOP definitions at {}: {e}",
            path.display()
        ))
    })?;
    let document = pilotdeck_yaml::parse_document(
        &JsString::from(String::from_utf8_lossy(&source).into_owned()),
        pilotdeck_yaml::Options {
            pretty_errors: false,
            ..Default::default()
        },
    );
    if !document.errors.is_empty() {
        return Err(invalid(format!(
            "Invalid StaffDeck SOP YAML at {}: {}",
            path.display(),
            document
                .errors
                .iter()
                .map(|v| String::from_utf16_lossy(v.message.units()))
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    let value = document
        .to_json()
        .and_then(|graph| graph.to_js_value())
        .map_err(|e| invalid(String::from_utf16_lossy(e.message.units())))?;
    normalize(value, path)
}
fn normalize(mut value: JsValue, path: &Path) -> Result<JsValue, SopError> {
    if matches!(value, JsValue::Array(_)) {
        value = object([("sops", value)]);
    } else if !matches!(value.get("sops"), Some(JsValue::Array(_))) {
        if let (Some(id), Some(version), Some(content)) = (
            nonempty(&value, "skill_id").or_else(|| nonempty(&value, "id")),
            nonempty(&value, "version"),
            value.get("content").filter(|v| v.as_object().is_some()),
        ) {
            let mut published = object([
                ("id", id.as_str().into()),
                ("skill_id", id.as_str().into()),
                ("version", version.as_str().into()),
                ("content", content.clone()),
            ]);
            for key in ["name", "business_domain", "description"] {
                if let Some(text) = nonempty(&value, key) {
                    set(&mut published, key, text.as_str().into());
                }
            }
            value = object([("sops", JsValue::Array(vec![published]))]);
        }
    }
    let Some(JsValue::Array(rows)) = value
        .get("sops")
        .filter(|v| matches!(v, JsValue::Array(rows) if !rows.is_empty()))
    else {
        return Err(invalid(format!(
            "StaffDeck SOP definitions at {} must contain a non-empty sops list.",
            path.display()
        )));
    };
    let mut ids = HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        if row.as_object().is_none() {
            return Err(invalid(format!(
                "StaffDeck SOP definition {index} must be an object."
            )));
        }
        let id = nonempty(row, "id")
            .or_else(|| nonempty(row, "skill_id"))
            .ok_or_else(|| invalid(format!("StaffDeck SOP definition {index} must have an id.")))?;
        if !ids.insert(id.clone()) {
            return Err(invalid(format!(
                "StaffDeck SOP definitions contain duplicate id '{id}'."
            )));
        }
        for nodes in [
            row.get("nodes"),
            row.get("content").and_then(|v| v.get("nodes")),
        ] {
            let Some(JsValue::Array(nodes)) = nodes else {
                continue;
            };
            for (index, node) in nodes.iter().enumerate() {
                if !optional(node, "contextMode", |v| {
                    v == &JsValue::from("new_session") || v == &JsValue::from("inherit")
                }) {
                    return Err(invalid(format!(
                        "StaffDeck SOP definition '{id}' node {index} has invalid contextMode '{}'.",
                        node.get("contextMode")
                            .map(|v| crate::js_value::to_string_units(v)
                                .map(|u| String::from_utf16_lossy(&u))
                                .unwrap_or_default())
                            .unwrap_or_default()
                    )));
                }
                if !optional(node, "model", |v| model(v).is_some()) {
                    return Err(invalid(format!(
                        "StaffDeck SOP definition '{id}' node {index} has invalid model; expected 'provider/model'."
                    )));
                }
            }
        }
    }
    Ok(object([("sops", JsValue::Array(rows.clone()))]))
}
fn nonempty(value: &JsValue, key: &str) -> Option<String> {
    text(value, key)
        .map(|s| crate::metadata::js_trim(&s).to_owned())
        .filter(|s| !s.is_empty())
}
fn invalid(message: impl Into<String>) -> SopError {
    SopError::new("SOP_DEFINITION_INVALID", message)
}
pub(super) fn model(value: &JsValue) -> Option<(String, String)> {
    let text = value.as_string()?.to_utf8().ok()?;
    if text.chars().any(char::is_whitespace) {
        return None;
    }
    let (provider, name) = text.split_once('/')?;
    (!provider.is_empty() && !name.is_empty() && !name.starts_with('/'))
        .then(|| (provider.into(), name.into()))
}
pub(super) fn node<'a>(bundle: &'a JsValue, sop_id: &str, node_id: &str) -> Option<&'a JsValue> {
    let JsValue::Array(rows) = bundle.get("sops")? else {
        return None;
    };
    let definition = rows
        .iter()
        .find(|v| text(v, "skill_id").or_else(|| text(v, "id")).as_deref() == Some(sop_id))?;
    let content = definition
        .get("content")
        .filter(|v| v.as_object().is_some())
        .unwrap_or(definition);
    let JsValue::Array(nodes) = content.get("nodes")? else {
        return None;
    };
    nodes
        .iter()
        .find(|v| is(v, "node_id", node_id) || is(v, "nodeId", node_id))
}
pub(super) fn context_mode(
    step: &JsValue,
    profile: &JsValue,
    bundle: &JsValue,
    selected: &str,
) -> String {
    text(step, "contextMode")
        .or_else(|| {
            node(bundle, selected, &text(step, "nodeId").unwrap_or_default())
                .and_then(|v| text(v, "contextMode"))
        })
        .or_else(|| text(profile, "contextMode"))
        .unwrap_or_else(|| "new_session".into())
}
pub(super) fn required_tools(
    bundle: &JsValue,
    selected: &str,
    available: &[JsString],
) -> Result<(), SopError> {
    let Some(JsValue::Array(rows)) = bundle.get("sops") else {
        return Ok(());
    };
    let Some(definition) = rows
        .iter()
        .find(|v| text(v, "skill_id").or_else(|| text(v, "id")).as_deref() == Some(selected))
    else {
        return Ok(());
    };
    let Some(JsValue::Array(nodes)) = definition.get("content").and_then(|v| v.get("nodes")) else {
        return Ok(());
    };
    let mut required = Vec::new();
    for node in nodes {
        let Some(JsValue::Array(actions)) = node.get("allowed_actions") else {
            continue;
        };
        for action in actions {
            let Some(action) = action.as_string().and_then(|v| v.to_utf8().ok()) else {
                continue;
            };
            let Some(name) = action
                .strip_prefix("call_tool:")
                .map(crate::metadata::js_trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            if !required.contains(&name.to_owned()) && !available.iter().any(|v| v.equals_str(name))
            {
                required.push(name.to_owned());
            }
        }
    }
    if required.is_empty() {
        return Ok(());
    }
    Err(SopError {
        code: "SOP_REQUIRED_TOOL_UNAVAILABLE".into(),
        message: format!(
            "StaffDeck SOP '{selected}' requires unavailable PilotDeck tools: {}.",
            required.join(", ")
        ),
        details: Some(object([(
            "missingToolNames",
            JsValue::Array(required.iter().map(|v| v.as_str().into()).collect()),
        )])),
        retryability: None,
    })
}
