//! Pure JSON Schema validation for tool inputs (`validateToolInput`).

use std::fmt;

use pilotdeck_protocol::{JsObject, JsString, JsValue, stringify_json};

const MAX_SCHEMA_DEPTH: usize = 128;

/// One structured validation issue produced by [`validate_tool_input`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaValidationIssue {
    pub path: String,
    pub code: String,
    pub message: String,
}

/// Fail-closed validation failure returned by [`validate_tool_input`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaValidationError {
    pub issues: Vec<SchemaValidationIssue>,
}

impl SchemaValidationError {
    pub fn new(issues: Vec<SchemaValidationIssue>) -> Self {
        Self { issues }
    }

    pub fn single(
        path: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            issues: vec![SchemaValidationIssue {
                path: path.into(),
                code: code.into(),
                message: message.into(),
            }],
        }
    }

    pub fn issues(&self) -> &[SchemaValidationIssue] {
        &self.issues
    }

    pub fn message(&self) -> String {
        self.issues
            .iter()
            .map(|issue| issue.message.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl fmt::Display for SchemaValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for SchemaValidationError {}

/// Validate a tool input [`JsValue`] against a JSON Schema [`JsValue`].
pub fn validate_tool_input(schema: &JsValue, input: &JsValue) -> Result<(), SchemaValidationError> {
    let mut issues = Vec::new();
    validate_node(schema, input, "$", &mut issues, 0);
    if issues.is_empty() {
        Ok(())
    } else {
        Err(SchemaValidationError::new(issues))
    }
}

/// Validate that a JSON value represents a structurally well-formed schema subset.
pub fn validate_schema_shape(schema: &JsValue) -> Result<(), SchemaValidationError> {
    let mut issues = Vec::new();
    validate_schema_node(schema, "$", &mut issues, 0);
    if issues.is_empty() {
        Ok(())
    } else {
        Err(SchemaValidationError::new(issues))
    }
}

fn validate_schema_node(
    schema: &JsValue,
    path: &str,
    issues: &mut Vec<SchemaValidationIssue>,
    depth: usize,
) {
    if depth > MAX_SCHEMA_DEPTH {
        push_issue(
            issues,
            path,
            "max_depth_exceeded",
            format!("{path} exceeded maximum schema nesting depth ({MAX_SCHEMA_DEPTH})."),
        );
        return;
    }
    let Some(obj) = schema.as_object() else {
        push_issue(
            issues,
            path,
            "invalid_schema",
            format!("{path} schema must be a JSON object."),
        );
        return;
    };
    if let Some(kind) = obj.get("type")
        && !is_valid_type_decl(kind)
    {
        push_issue(
            issues,
            path,
            "invalid_schema",
            format!("{path}.type must be a valid JSON Schema type string or array of strings."),
        );
    }
    if let Some(enum_val) = obj.get("enum")
        && !matches!(enum_val, JsValue::Array(_))
    {
        push_issue(
            issues,
            path,
            "invalid_schema",
            format!("{path}.enum must be an array."),
        );
    }
    if let Some(required) = obj.get("required") {
        match required {
            JsValue::Array(items) if items.iter().all(|v| v.as_string().is_some()) => {}
            JsValue::Null | JsValue::Undefined => {}
            _ => push_issue(
                issues,
                path,
                "invalid_schema",
                format!("{path}.required must be an array of strings."),
            ),
        }
    }
    if let Some(properties) = obj.get("properties") {
        match properties {
            JsValue::Object(props) => {
                for (key, prop_schema) in props.own_entries() {
                    let child = format!("{path}.properties.{}", lossy(key));
                    validate_schema_node(prop_schema, &child, issues, depth + 1);
                }
            }
            JsValue::Null | JsValue::Undefined => {}
            _ => push_issue(
                issues,
                path,
                "invalid_schema",
                format!("{path}.properties must be an object."),
            ),
        }
    }
    if let Some(additional) = obj.get("additionalProperties") {
        match additional {
            JsValue::Bool(_) | JsValue::Null | JsValue::Undefined => {}
            JsValue::Object(_) => {
                validate_schema_node(
                    additional,
                    &format!("{path}.additionalProperties"),
                    issues,
                    depth + 1,
                );
            }
            _ => push_issue(
                issues,
                path,
                "invalid_schema",
                format!("{path}.additionalProperties must be a boolean or schema object."),
            ),
        }
    }
    if let Some(items) = obj.get("items") {
        match items {
            JsValue::Object(_) => {
                validate_schema_node(items, &format!("{path}.items"), issues, depth + 1);
            }
            JsValue::Bool(_) | JsValue::Null | JsValue::Undefined => {}
            _ => push_issue(
                issues,
                path,
                "invalid_schema",
                format!("{path}.items must be a schema object or boolean."),
            ),
        }
    }
    for key in ["minLength", "maxLength", "minItems", "maxItems"] {
        if let Some(val) = obj.get(key)
            && !matches!(val, JsValue::Null | JsValue::Undefined)
            && !val
                .as_number()
                .is_some_and(|n| n.is_finite() && n >= 0.0 && n.fract() == 0.0)
        {
            push_issue(
                issues,
                path,
                "invalid_schema",
                format!("{path}.{key} must be a non-negative integer."),
            );
        }
    }
    for key in ["minimum", "maximum"] {
        if let Some(val) = obj.get(key)
            && !matches!(val, JsValue::Null | JsValue::Undefined)
            && !val.as_number().is_some_and(f64::is_finite)
        {
            push_issue(
                issues,
                path,
                "invalid_schema",
                format!("{path}.{key} must be a finite number."),
            );
        }
    }
}

fn validate_node(
    schema: &JsValue,
    input: &JsValue,
    path: &str,
    issues: &mut Vec<SchemaValidationIssue>,
    depth: usize,
) {
    if depth > MAX_SCHEMA_DEPTH {
        push_issue(
            issues,
            path,
            "max_depth_exceeded",
            format!("{path} exceeded maximum validation depth ({MAX_SCHEMA_DEPTH})."),
        );
        return;
    }
    let Some(schema_obj) = schema.as_object() else {
        push_issue(
            issues,
            path,
            "invalid_schema",
            format!("{path} schema must be a JSON object."),
        );
        return;
    };
    if input.is_undefined() {
        push_issue(
            issues,
            path,
            "invalid_type",
            format!("{path} must not be undefined."),
        );
        return;
    }

    if let Some(kind) = schema_obj.get("type") {
        if !is_valid_type_decl(kind) {
            push_issue(
                issues,
                path,
                "invalid_schema",
                format!("{path}.type must be a JSON Schema type string or array of type strings."),
            );
            return;
        }
        if !matches_type(input, kind) {
            push_issue(
                issues,
                path,
                "invalid_type",
                format!("{path} must be {}.", describe_type(kind)),
            );
            return;
        }
    }

    if let Some(enum_val) = schema_obj.get("enum") {
        match enum_val {
            JsValue::Array(variants) => {
                if !variants.iter().any(|variant| json_equal(input, variant, 0)) {
                    let allowed = variants
                        .iter()
                        .map(display_value)
                        .collect::<Vec<_>>()
                        .join(", ");
                    push_issue(
                        issues,
                        path,
                        "invalid_enum",
                        format!("{path} must be one of {allowed}."),
                    );
                    return;
                }
            }
            _ => {
                push_issue(
                    issues,
                    path,
                    "invalid_schema",
                    format!("{path}.enum must be an array."),
                );
                return;
            }
        }
    }

    match input {
        JsValue::String(text) => {
            validate_string_bounds(schema_obj, text, path, issues);
        }
        JsValue::Number(num) => {
            validate_number_bounds(schema_obj, *num, path, issues);
        }
        JsValue::Object(obj) if schema_allows_object(schema_obj) => {
            validate_object(schema_obj, obj, path, issues, depth);
        }
        JsValue::Array(items) if schema_allows_array(schema_obj) => {
            validate_array_bounds(schema_obj, items, path, issues);
            validate_array(schema_obj, items, path, issues, depth);
        }
        _ => {}
    }
}

fn non_negative_integer_bound(
    schema_obj: &JsObject,
    key: &str,
    path: &str,
    issues: &mut Vec<SchemaValidationIssue>,
) -> Option<usize> {
    let value = schema_obj.get(key)?;
    if matches!(value, JsValue::Null | JsValue::Undefined) {
        return None;
    }
    let Some(n) = value
        .as_number()
        .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0)
    else {
        push_issue(
            issues,
            path,
            "invalid_schema",
            format!("{path}.{key} must be a non-negative integer."),
        );
        return None;
    };
    Some(n as usize)
}

fn finite_number_bound(
    schema_obj: &JsObject,
    key: &str,
    path: &str,
    issues: &mut Vec<SchemaValidationIssue>,
) -> Option<f64> {
    let value = schema_obj.get(key)?;
    if matches!(value, JsValue::Null | JsValue::Undefined) {
        return None;
    }
    let Some(n) = value.as_number().filter(|n| n.is_finite()) else {
        push_issue(
            issues,
            path,
            "invalid_schema",
            format!("{path}.{key} must be a finite number."),
        );
        return None;
    };
    Some(n)
}

fn validate_string_bounds(
    schema_obj: &JsObject,
    text: &JsString,
    path: &str,
    issues: &mut Vec<SchemaValidationIssue>,
) {
    let len = lossy(text).chars().count();
    if let Some(min_len) = non_negative_integer_bound(schema_obj, "minLength", path, issues)
        && len < min_len
    {
        push_issue(
            issues,
            path,
            "min_length",
            format!("{path} must have at least {min_len} character(s)."),
        );
    }
    if let Some(max_len) = non_negative_integer_bound(schema_obj, "maxLength", path, issues)
        && len > max_len
    {
        push_issue(
            issues,
            path,
            "max_length",
            format!("{path} must have at most {max_len} character(s)."),
        );
    }
}

fn validate_number_bounds(
    schema_obj: &JsObject,
    num: f64,
    path: &str,
    issues: &mut Vec<SchemaValidationIssue>,
) {
    if let Some(min) = finite_number_bound(schema_obj, "minimum", path, issues)
        && num < min
    {
        push_issue(issues, path, "minimum", format!("{path} must be >= {min}."));
    }
    if let Some(max) = finite_number_bound(schema_obj, "maximum", path, issues)
        && num > max
    {
        push_issue(issues, path, "maximum", format!("{path} must be <= {max}."));
    }
}

fn validate_array_bounds(
    schema_obj: &JsObject,
    items: &[JsValue],
    path: &str,
    issues: &mut Vec<SchemaValidationIssue>,
) {
    if let Some(min_items) = non_negative_integer_bound(schema_obj, "minItems", path, issues)
        && items.len() < min_items
    {
        push_issue(
            issues,
            path,
            "min_items",
            format!("{path} must contain at least {min_items} item(s)."),
        );
    }
    if let Some(max_items) = non_negative_integer_bound(schema_obj, "maxItems", path, issues)
        && items.len() > max_items
    {
        push_issue(
            issues,
            path,
            "max_items",
            format!("{path} must contain at most {max_items} item(s)."),
        );
    }
}

fn validate_object(
    schema_obj: &JsObject,
    input_obj: &JsObject,
    path: &str,
    issues: &mut Vec<SchemaValidationIssue>,
    depth: usize,
) {
    let props_obj = match schema_obj.get("properties") {
        None | Some(JsValue::Null | JsValue::Undefined) => None,
        Some(JsValue::Object(props)) => Some(props),
        Some(_) => {
            push_issue(
                issues,
                path,
                "invalid_schema",
                format!("{path}.properties must be an object."),
            );
            return;
        }
    };

    if let Some(required) = schema_obj.get("required") {
        match required {
            JsValue::Array(keys) => {
                for key_val in keys {
                    let Some(key) = key_val.as_string() else {
                        push_issue(
                            issues,
                            path,
                            "invalid_schema",
                            format!("{path}.required entries must be strings."),
                        );
                        return;
                    };
                    let present = input_obj
                        .get_key(key)
                        .is_some_and(|value| !value.is_undefined());
                    if !present {
                        let key_path = format!("{path}.{}", lossy(key));
                        push_issue(
                            issues,
                            &key_path,
                            "required",
                            format!("{key_path} is required."),
                        );
                    }
                }
            }
            JsValue::Null | JsValue::Undefined => {}
            _ => {
                push_issue(
                    issues,
                    path,
                    "invalid_schema",
                    format!("{path}.required must be an array."),
                );
                return;
            }
        }
    }

    match schema_obj.get("additionalProperties") {
        Some(JsValue::Bool(false)) => {
            for (key, value) in input_obj.own_entries() {
                if value.is_undefined() {
                    continue;
                }
                let known = props_obj.is_some_and(|props| props.get_key(key).is_some());
                if !known {
                    let key_path = format!("{path}.{}", lossy(key));
                    push_issue(
                        issues,
                        &key_path,
                        "unknown_property",
                        format!("{key_path} is not allowed."),
                    );
                }
            }
        }
        Some(extra_schema @ JsValue::Object(_)) => {
            for (key, value) in input_obj.own_entries() {
                if value.is_undefined() {
                    continue;
                }
                let known = props_obj.is_some_and(|props| props.get_key(key).is_some());
                if !known {
                    let key_path = format!("{path}.{}", lossy(key));
                    validate_node(extra_schema, value, &key_path, issues, depth + 1);
                }
            }
        }
        None | Some(JsValue::Bool(true) | JsValue::Null | JsValue::Undefined) => {}
        Some(_) => {
            push_issue(
                issues,
                path,
                "invalid_schema",
                format!("{path}.additionalProperties must be a boolean or schema object."),
            );
            return;
        }
    }

    if let Some(props) = props_obj {
        for (key, prop_schema) in props.own_entries() {
            if let Some(prop_val) = input_obj.get_key(key)
                && !prop_val.is_undefined()
            {
                let child_path = format!("{path}.{}", lossy(key));
                validate_node(prop_schema, prop_val, &child_path, issues, depth + 1);
            }
        }
    }
}

fn validate_array(
    schema_obj: &JsObject,
    items_val: &[JsValue],
    path: &str,
    issues: &mut Vec<SchemaValidationIssue>,
    depth: usize,
) {
    match schema_obj.get("items") {
        None | Some(JsValue::Bool(true) | JsValue::Null | JsValue::Undefined) => {}
        Some(JsValue::Bool(false)) => {
            if !items_val.is_empty() {
                push_issue(
                    issues,
                    path,
                    "invalid_items",
                    format!("{path} must be empty because items=false."),
                );
            }
        }
        Some(item_schema @ JsValue::Object(_)) => {
            for (idx, item) in items_val.iter().enumerate() {
                let item_path = format!("{path}[{idx}]");
                validate_node(item_schema, item, &item_path, issues, depth + 1);
            }
        }
        Some(_) => {
            push_issue(
                issues,
                path,
                "invalid_schema",
                format!("{path}.items must be a schema object or boolean."),
            );
        }
    }
}

fn schema_allows_object(schema_obj: &JsObject) -> bool {
    match schema_obj.get("type") {
        None => true,
        Some(kind) => type_contains(kind, "object"),
    }
}

fn schema_allows_array(schema_obj: &JsObject) -> bool {
    match schema_obj.get("type") {
        None => true,
        Some(kind) => type_contains(kind, "array"),
    }
}

fn type_contains(kind: &JsValue, target: &str) -> bool {
    match kind {
        JsValue::String(s) => s.equals_str(target),
        JsValue::Array(arr) => arr.iter().any(|item| type_contains(item, target)),
        _ => false,
    }
}

fn is_valid_type_decl(kind: &JsValue) -> bool {
    match kind {
        JsValue::String(_) => true,
        JsValue::Array(items) => !items.is_empty() && items.iter().all(|v| v.as_string().is_some()),
        _ => false,
    }
}

fn matches_type(value: &JsValue, kind: &JsValue) -> bool {
    match kind {
        JsValue::String(name) => matches_single_type(value, name),
        JsValue::Array(kinds) => kinds.iter().any(|k| matches_type(value, k)),
        _ => false,
    }
}

fn matches_single_type(value: &JsValue, name: &JsString) -> bool {
    match lossy(name).as_str() {
        "null" => matches!(value, JsValue::Null),
        "object" => matches!(value, JsValue::Object(_)),
        "array" => matches!(value, JsValue::Array(_)),
        "string" => value.as_string().is_some(),
        "boolean" => value.as_bool().is_some(),
        "number" => value.as_number().is_some_and(f64::is_finite),
        "integer" => value
            .as_number()
            .is_some_and(|n| n.is_finite() && n.fract() == 0.0),
        _ => false,
    }
}

fn describe_type(kind: &JsValue) -> String {
    match kind {
        JsValue::Array(kinds) => kinds
            .iter()
            .map(display_value)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => display_value(kind),
    }
}

fn display_value(value: &JsValue) -> String {
    match value {
        JsValue::String(s) => lossy(s),
        _ => stringify_json(value)
            .ok()
            .flatten()
            .unwrap_or_else(|| "undefined".to_owned()),
    }
}

fn json_equal(left: &JsValue, right: &JsValue, depth: usize) -> bool {
    if depth > MAX_SCHEMA_DEPTH {
        return false;
    }
    match (left, right) {
        (JsValue::Null, JsValue::Null) => true,
        (JsValue::Bool(a), JsValue::Bool(b)) => a == b,
        (JsValue::Number(a), JsValue::Number(b)) => a.is_finite() && b.is_finite() && a == b,
        (JsValue::String(a), JsValue::String(b)) => a == b,
        (JsValue::Array(a), JsValue::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_equal(x, y, depth + 1))
        }
        (JsValue::Object(a), JsValue::Object(b)) => {
            let a_entries: Vec<_> = a
                .own_entries()
                .into_iter()
                .filter(|(_, v)| !v.is_undefined())
                .collect();
            let b_entries: Vec<_> = b
                .own_entries()
                .into_iter()
                .filter(|(_, v)| !v.is_undefined())
                .collect();
            a_entries.len() == b_entries.len()
                && a_entries.iter().all(|(k, av)| {
                    b.get_key(k)
                        .is_some_and(|bv| !bv.is_undefined() && json_equal(av, bv, depth + 1))
                })
        }
        _ => false,
    }
}

fn push_issue(issues: &mut Vec<SchemaValidationIssue>, path: &str, code: &str, message: String) {
    issues.push(SchemaValidationIssue {
        path: path.to_owned(),
        code: code.to_owned(),
        message,
    });
}

fn lossy(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::parse_json;

    fn json(raw: &str) -> JsValue {
        parse_json(raw).expect("valid test JSON")
    }

    #[test]
    fn validates_primitive_types_and_integer_vs_number() {
        assert!(validate_tool_input(&json(r#"{"type":"string"}"#), &json(r#""ok""#)).is_ok());
        assert!(validate_tool_input(&json(r#"{"type":"boolean"}"#), &json("true")).is_ok());
        assert!(validate_tool_input(&json(r#"{"type":"number"}"#), &json("3.5")).is_ok());
        assert!(validate_tool_input(&json(r#"{"type":"integer"}"#), &json("42")).is_ok());
        assert!(validate_tool_input(&json(r#"{"type":"integer"}"#), &json("42.0")).is_ok());

        let err = validate_tool_input(&json(r#"{"type":"integer"}"#), &json("42.5")).unwrap_err();
        assert_eq!(err.issues()[0].code, "invalid_type");

        let err = validate_tool_input(&json(r#"{"type":"number"}"#), &json(r#""42""#)).unwrap_err();
        assert_eq!(err.issues()[0].code, "invalid_type");
    }

    #[test]
    fn validates_required_and_additional_properties_false() {
        let schema = json(
            r#"{
                "type": "object",
                "required": ["path"],
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string" },
                    "limit": { "type": "integer" }
                }
            }"#,
        );
        assert!(validate_tool_input(&schema, &json(r#"{"path":"a.txt","limit":10}"#)).is_ok());

        let missing = validate_tool_input(&schema, &json(r#"{"limit":10}"#)).unwrap_err();
        assert_eq!(missing.issues()[0].code, "required");
        assert_eq!(missing.issues()[0].path, "$.path");

        let extra =
            validate_tool_input(&schema, &json(r#"{"path":"a.txt","extra":1}"#)).unwrap_err();
        assert_eq!(extra.issues()[0].code, "unknown_property");
        assert_eq!(extra.issues()[0].path, "$.extra");
    }

    #[test]
    fn validates_nested_objects_arrays_and_enums() {
        let todo_schema = crate::todo::todo_definition()
            .get("inputSchema")
            .cloned()
            .unwrap();
        assert!(
            validate_tool_input(
                &todo_schema,
                &json(r#"{"todos":[{"id":"1","content":"check","status":"in_progress"}],"merge":true}"#)
            )
            .is_ok()
        );

        let bad_status = validate_tool_input(
            &todo_schema,
            &json(r#"{"todos":[{"id":"1","content":"check","status":"done"}]}"#),
        )
        .unwrap_err();
        assert_eq!(bad_status.issues()[0].code, "invalid_enum");
        assert_eq!(bad_status.issues()[0].path, "$.todos[0].status");

        let unknown_nested = validate_tool_input(
            &todo_schema,
            &json(r#"{"todos":[{"id":"1","extra":false}]}"#),
        )
        .unwrap_err();
        assert_eq!(unknown_nested.issues()[0].code, "unknown_property");
        assert_eq!(unknown_nested.issues()[0].path, "$.todos[0].extra");
    }

    #[test]
    fn rejects_non_object_schema_and_excessive_depth_fail_closed() {
        let err = validate_tool_input(&json("null"), &json("{}")).unwrap_err();
        assert_eq!(err.issues()[0].code, "invalid_schema");

        let mut deep_schema = json(r#"{"type":"string"}"#);
        let mut deep_input = json(r#""leaf""#);
        for _ in 0..=MAX_SCHEMA_DEPTH + 2 {
            let mut s = JsObject::new();
            s.insert("type".into(), "array".into());
            s.insert("items".into(), deep_schema);
            deep_schema = JsValue::Object(s);
            deep_input = JsValue::Array(vec![deep_input]);
        }
        let err = validate_tool_input(&deep_schema, &deep_input).unwrap_err();
        assert_eq!(err.issues()[0].code, "max_depth_exceeded");
    }

    #[test]
    fn validates_string_number_and_array_bounds() {
        let schema = json(
            r#"{
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "name": { "type": "string", "minLength": 2, "maxLength": 5 },
                    "offset": { "type": "integer", "minimum": 1, "maximum": 10 },
                    "tags": { "type": "array", "minItems": 1, "maxItems": 2, "items": { "type": "string" } }
                }
            }"#,
        );
        assert!(validate_schema_shape(&schema).is_ok());
        assert!(
            validate_tool_input(&schema, &json(r#"{"name":"ab","offset":1,"tags":["x"]}"#)).is_ok()
        );

        let too_short = validate_tool_input(&schema, &json(r#"{"name":"a"}"#)).unwrap_err();
        assert_eq!(too_short.issues()[0].code, "min_length");

        let too_long = validate_tool_input(&schema, &json(r#"{"name":"abcdef"}"#)).unwrap_err();
        assert_eq!(too_long.issues()[0].code, "max_length");

        let too_small = validate_tool_input(&schema, &json(r#"{"offset":0}"#)).unwrap_err();
        assert_eq!(too_small.issues()[0].code, "minimum");

        let too_big = validate_tool_input(&schema, &json(r#"{"offset":11}"#)).unwrap_err();
        assert_eq!(too_big.issues()[0].code, "maximum");

        let empty_arr = validate_tool_input(&schema, &json(r#"{"tags":[]}"#)).unwrap_err();
        assert_eq!(empty_arr.issues()[0].code, "min_items");

        let full_arr =
            validate_tool_input(&schema, &json(r#"{"tags":["a","b","c"]}"#)).unwrap_err();
        assert_eq!(full_arr.issues()[0].code, "max_items");

        assert!(validate_schema_shape(&json(r#"{"type":"string","minLength":-1}"#)).is_err());
        assert!(validate_schema_shape(&json(r#"{"type":"integer","minimum":"1"}"#)).is_err());
    }
}
