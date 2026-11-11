//! The form dialog schema subset (baseline `tool/dialog/FormDialogSchema.ts`):
//! `validateFormDialogSchema` for `request_user_form` input and
//! `acceptsFormDialogAnswer` for the answer a renderer submits.
//!
//! Only the historical PilotDeck subset is ported. A schema that opts into
//! standards-based validation (`$schema` = Draft 2020-12) is compiled by Ajv in
//! the baseline; this host has no such validator and refuses those schemas.
//! Patterns use the JavaScript-pattern translation of the MCP output-schema
//! validator; a pattern it cannot express never matches an answer.
use super::{object, string, text};
use crate::runtime::{mcp::output_schema::JsRegex, tools::ToolError};
use pilotdeck_protocol::{JsObject, JsString, JsValue, stringify_json};

pub const FORM_DIALOG_DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

const MAX_FORM_SCHEMA_BYTES: usize = 64_000;
const MAX_FORM_SCHEMA_DEPTH: usize = 8;
const MAX_FORM_SCHEMA_NODES: usize = 256;
const MAX_FORM_PROPERTIES: usize = 64;
const MAX_FORM_DEFINITIONS: usize = 64;
const MAX_FORM_ENUM_VALUES: usize = 64;
const MAX_FORM_COMPOSITION_BRANCHES: usize = 32;
const MAX_FORM_DEPENDENCIES: usize = 64;
const MAX_FORM_PATTERN_LENGTH: usize = 4_096;
const SUPPORTED_TYPES: &[&str] = &[
    "object", "array", "string", "number", "integer", "boolean", "null",
];
const SUPPORTED_STRING_FORMATS: &[&str] = &["email", "uri", "uuid", "date", "time", "date-time"];
const SUPPORTED_KEYS: &[&str] = &[
    "type",
    "title",
    "description",
    "format",
    "default",
    "enum",
    "const",
    "$ref",
    "$defs",
    "properties",
    "patternProperties",
    "required",
    "additionalProperties",
    "items",
    "prefixItems",
    "contains",
    "minContains",
    "maxContains",
    "propertyNames",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "dependentRequired",
    "dependentSchemas",
    "minLength",
    "maxLength",
    "pattern",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
];

fn record(value: &JsValue) -> Option<&JsObject> {
    value.as_object()
}

fn present<'a>(object: &'a JsObject, key: &str) -> Option<&'a JsValue> {
    object.get(key).filter(|value| !value.is_undefined())
}

fn trim_empty(value: &JsString) -> bool {
    crate::metadata::js_trim(&text(value)).is_empty()
}

fn is_json_value(value: &JsValue) -> bool {
    match value {
        JsValue::Null | JsValue::String(_) | JsValue::Bool(_) => true,
        JsValue::Number(number) => number.is_finite(),
        JsValue::Array(items) => items.iter().all(is_json_value),
        JsValue::Object(object) => object
            .own_entries()
            .iter()
            .all(|(_, value)| is_json_value(value)),
        JsValue::Undefined => false,
    }
}

fn is_integer(value: &JsValue) -> bool {
    value
        .as_number()
        .is_some_and(|value| value.is_finite() && value.fract() == 0.0)
}

/// `new RegExp(pattern)` succeeds.
fn valid_pattern(pattern: &str) -> bool {
    !matches!(
        crate::runtime::mcp::output_schema::js_regex(pattern),
        JsRegex::Invalid(_)
    )
}

/// `new RegExp(pattern).test(value)`; a pattern this host cannot express
/// matches nothing (the answer is refused rather than accepted unchecked).
fn pattern_matches(pattern: &str, value: &str) -> bool {
    match crate::runtime::mcp::output_schema::js_regex(pattern) {
        JsRegex::Compiled(regex) => regex.is_match(value),
        _ => false,
    }
}

/// `/^[A-Za-z][A-Za-z0-9_-]{0,63}$/u`.
fn is_local_definition_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && value.chars().count() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn local_definition_name(reference: &str) -> Option<&str> {
    reference
        .strip_prefix("#/$defs/")
        .filter(|name| is_local_definition_name(name))
}

/// `validateFormDialogSchema`: `Err(message)` when the schema is refused.
pub fn validate_form_dialog_schema(value: &JsValue) -> Result<(), String> {
    let Some(schema) = record(value) else {
        return Err("schema must be a JSON object.".into());
    };
    if !schema
        .get("type")
        .and_then(JsValue::as_string)
        .is_some_and(|kind| kind.equals_str("object"))
    {
        return Err("schema.type must be object.".into());
    }
    if !is_json_value(value) {
        return Err("schema must contain only finite JSON values.".into());
    }
    let bytes = stringify_json(value)
        .ok()
        .flatten()
        .map_or(0, |json| json.len());
    if bytes > MAX_FORM_SCHEMA_BYTES {
        return Err(format!(
            "schema must not exceed {MAX_FORM_SCHEMA_BYTES} bytes."
        ));
    }
    match present(schema, "$schema") {
        Some(JsValue::String(declared)) if declared.equals_str(FORM_DIALOG_DRAFT_2020_12) => {
            return Err(format!(
                "schema.$schema {FORM_DIALOG_DRAFT_2020_12} is not supported by this gateway."
            ));
        }
        Some(_) => {
            return Err(format!(
                "schema.$schema must be {FORM_DIALOG_DRAFT_2020_12} to enable standards-based form validation."
            ));
        }
        None => {}
    }
    let mut nodes = 0;
    if let Some(issue) = validate_node(value, "$", 0, &mut nodes) {
        return Err(issue);
    }
    match validate_local_definition_references(schema) {
        Some(issue) => Err(issue),
        None => Ok(()),
    }
}

fn validate_node(value: &JsValue, path: &str, depth: usize, nodes: &mut usize) -> Option<String> {
    let Some(node) = record(value) else {
        return Some(format!("{path} must be an object."));
    };
    *nodes += 1;
    if *nodes > MAX_FORM_SCHEMA_NODES {
        return Some(format!(
            "schema may contain at most {MAX_FORM_SCHEMA_NODES} nodes."
        ));
    }
    if depth > MAX_FORM_SCHEMA_DEPTH {
        return Some(format!(
            "schema nesting may not exceed {MAX_FORM_SCHEMA_DEPTH} levels."
        ));
    }
    for (key, _) in node.own_entries() {
        if !SUPPORTED_KEYS.iter().any(|name| key.equals_str(name)) {
            return Some(format!(
                "{path}.{} is not supported by form dialogs.",
                text(key)
            ));
        }
    }
    if let Some(reference) = present(node, "$ref")
        && !reference
            .as_string()
            .is_some_and(|reference| local_definition_name(&text(reference)).is_some())
    {
        return Some(format!(
            "{path}.$ref must be a local #/$defs/<name> reference."
        ));
    }
    if let Some(definitions) = present(node, "$defs") {
        if path != "$" {
            return Some(format!(
                "{path}.$defs is only supported on the root form schema."
            ));
        }
        let Some(definitions) = record(definitions) else {
            return Some(format!("{path}.$defs must be an object."));
        };
        let entries = definitions.own_entries();
        if entries.len() > MAX_FORM_DEFINITIONS {
            return Some(format!(
                "{path}.$defs may contain at most {MAX_FORM_DEFINITIONS} definitions."
            ));
        }
        for (name, definition) in entries {
            let name = text(name);
            if !is_local_definition_name(&name) {
                return Some(format!(
                    "{path}.$defs.{name} must use a simple local definition name."
                ));
            }
            if let Some(issue) = validate_node(
                definition,
                &format!("{path}.$defs.{name}"),
                depth + 1,
                nodes,
            ) {
                return Some(issue);
            }
        }
    }
    if let Some(kind) = present(node, "type")
        && !is_supported_type(kind)
    {
        return Some(format!("{path}.type must name supported JSON value types."));
    }
    for key in ["title", "description", "format", "pattern"] {
        if present(node, key).is_some_and(|value| value.as_string().is_none()) {
            return Some(format!("{path}.{key} must be a string."));
        }
    }
    if let Some(JsValue::String(format)) = present(node, "format")
        && !SUPPORTED_STRING_FORMATS
            .iter()
            .any(|name| format.equals_str(name))
    {
        return Some(format!(
            "{path}.format must be one of {}.",
            SUPPORTED_STRING_FORMATS.join(", ")
        ));
    }
    if let Some(JsValue::String(pattern)) = present(node, "pattern") {
        if pattern.units().len() > MAX_FORM_PATTERN_LENGTH {
            return Some(format!(
                "{path}.pattern must not exceed {MAX_FORM_PATTERN_LENGTH} characters."
            ));
        }
        if !valid_pattern(&text(pattern)) {
            return Some(format!(
                "{path}.pattern must be a valid JavaScript regular expression."
            ));
        }
    }
    for key in ["default", "const"] {
        if present(node, key).is_some_and(|value| !is_json_value(value)) {
            return Some(format!("{path}.{key} must be JSON-serializable."));
        }
    }
    if let Some(issue) = validate_count_bounds(node, path) {
        return Some(issue);
    }
    if let Some(issue) = validate_numeric_bounds(node, path) {
        return Some(issue);
    }
    let required = present(node, "required");
    if let Some(required) = required {
        let ok = matches!(required, JsValue::Array(items)
            if items.iter().all(|key| key.as_string().is_some_and(|key| !trim_empty(key)))
                && !items.iter().enumerate().any(|(index, item)| items[..index].contains(item)));
        if !ok {
            return Some(format!(
                "{path}.required must be an array of unique non-empty property names."
            ));
        }
    }
    let required_keys: Vec<&JsString> = match required {
        Some(JsValue::Array(items)) => items.iter().filter_map(JsValue::as_string).collect(),
        _ => Vec::new(),
    };
    if let Some(properties) = present(node, "properties") {
        let Some(properties) = record(properties) else {
            return Some(format!("{path}.properties must be an object."));
        };
        let entries = properties.own_entries();
        if entries.len() > MAX_FORM_PROPERTIES {
            return Some(format!(
                "{path}.properties may contain at most {MAX_FORM_PROPERTIES} fields."
            ));
        }
        for (key, child) in entries {
            if trim_empty(key) {
                return Some(format!(
                    "{path}.properties cannot contain an empty field name."
                ));
            }
            if let Some(issue) = validate_node(
                child,
                &format!("{path}.properties.{}", text(key)),
                depth + 1,
                nodes,
            ) {
                return Some(issue);
            }
        }
        if required_keys
            .iter()
            .any(|key| properties.get_key(key).is_none())
        {
            return Some(format!(
                "{path}.required must only reference declared properties."
            ));
        }
    } else if !required_keys.is_empty() {
        return Some(format!("{path}.required requires properties."));
    }
    if let Some(patterns) = present(node, "patternProperties") {
        let Some(patterns) = record(patterns) else {
            return Some(format!("{path}.patternProperties must be an object."));
        };
        let entries = patterns.own_entries();
        if entries.len() > MAX_FORM_PROPERTIES {
            return Some(format!(
                "{path}.patternProperties may contain at most {MAX_FORM_PROPERTIES} patterns."
            ));
        }
        for (pattern, child) in entries {
            let pattern = text(pattern);
            let base = format!("{path}.patternProperties");
            if pattern.is_empty() {
                return Some(format!("{base} cannot contain an empty pattern."));
            }
            if pattern.encode_utf16().count() > MAX_FORM_PATTERN_LENGTH {
                return Some(format!(
                    "{base} patterns must not exceed {MAX_FORM_PATTERN_LENGTH} characters."
                ));
            }
            if !valid_pattern(&pattern) {
                return Some(format!(
                    "{base}.{pattern} must be a valid JavaScript regular expression."
                ));
            }
            if let Some(issue) =
                validate_node(child, &format!("{base}.{pattern}"), depth + 1, nodes)
            {
                return Some(issue);
            }
        }
    }
    if let Some(additional) = present(node, "additionalProperties")
        && additional.as_bool().is_none()
        && let Some(issue) = validate_node(
            additional,
            &format!("{path}.additionalProperties"),
            depth + 1,
            nodes,
        )
    {
        return Some(issue);
    }
    for key in ["items"] {
        if let Some(child) = present(node, key)
            && let Some(issue) = validate_node(child, &format!("{path}.{key}"), depth + 1, nodes)
        {
            return Some(issue);
        }
    }
    if let Some(prefix) = present(node, "prefixItems") {
        let JsValue::Array(items) = prefix else {
            return Some(format!(
                "{path}.prefixItems must contain 1-{MAX_FORM_COMPOSITION_BRANCHES} schema entries."
            ));
        };
        if items.is_empty() || items.len() > MAX_FORM_COMPOSITION_BRANCHES {
            return Some(format!(
                "{path}.prefixItems must contain 1-{MAX_FORM_COMPOSITION_BRANCHES} schema entries."
            ));
        }
        for (index, item) in items.iter().enumerate() {
            if let Some(issue) = validate_node(
                item,
                &format!("{path}.prefixItems[{index}]"),
                depth + 1,
                nodes,
            ) {
                return Some(issue);
            }
        }
    }
    if let Some(child) = present(node, "contains")
        && let Some(issue) = validate_node(child, &format!("{path}.contains"), depth + 1, nodes)
    {
        return Some(issue);
    }
    if let Some(issue) = validate_contains_bounds(node, path) {
        return Some(issue);
    }
    if let Some(child) = present(node, "propertyNames")
        && let Some(issue) =
            validate_node(child, &format!("{path}.propertyNames"), depth + 1, nodes)
    {
        return Some(issue);
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        let Some(branches) = present(node, key) else {
            continue;
        };
        let message =
            format!("{path}.{key} must contain 1-{MAX_FORM_COMPOSITION_BRANCHES} schema branches.");
        let JsValue::Array(branches) = branches else {
            return Some(message);
        };
        if branches.is_empty() || branches.len() > MAX_FORM_COMPOSITION_BRANCHES {
            return Some(message);
        }
        for (index, branch) in branches.iter().enumerate() {
            if let Some(issue) =
                validate_node(branch, &format!("{path}.{key}[{index}]"), depth + 1, nodes)
            {
                return Some(issue);
            }
        }
    }
    if let Some(child) = present(node, "not")
        && let Some(issue) = validate_node(child, &format!("{path}.not"), depth + 1, nodes)
    {
        return Some(issue);
    }
    let has_if = present(node, "if").is_some();
    let has_then = present(node, "then").is_some();
    let has_else = present(node, "else").is_some();
    if (has_then || has_else) && !has_if {
        return Some(format!("{path}.then and {path}.else require {path}.if."));
    }
    if has_if && !has_then && !has_else {
        return Some(format!("{path}.if requires {path}.then or {path}.else."));
    }
    for key in ["if", "then", "else"] {
        if let Some(child) = present(node, key)
            && let Some(issue) = validate_node(child, &format!("{path}.{key}"), depth + 1, nodes)
        {
            return Some(issue);
        }
    }
    let declared = node.get("properties").and_then(record);
    if let Some(issue) =
        validate_dependent_required(present(node, "dependentRequired"), declared, path)
    {
        return Some(issue);
    }
    if let Some(dependencies) = present(node, "dependentSchemas") {
        let Some(dependencies) = record(dependencies) else {
            return Some(format!("{path}.dependentSchemas must be an object."));
        };
        let entries = dependencies.own_entries();
        if entries.len() > MAX_FORM_DEPENDENCIES {
            return Some(format!(
                "{path}.dependentSchemas may contain at most {MAX_FORM_DEPENDENCIES} dependencies."
            ));
        }
        for (trigger, schema) in entries {
            if trim_empty(trigger) {
                return Some(format!(
                    "{path}.dependentSchemas cannot contain an empty property name."
                ));
            }
            if !declared.is_some_and(|declared| declared.get_key(trigger).is_some()) {
                return Some(format!(
                    "{path}.dependentSchemas.{} must reference a declared property.",
                    text(trigger)
                ));
            }
            if let Some(issue) = validate_node(
                schema,
                &format!("{path}.dependentSchemas.{}", text(trigger)),
                depth + 1,
                nodes,
            ) {
                return Some(issue);
            }
        }
    }
    if let Some(values) = present(node, "enum") {
        let ok = matches!(values, JsValue::Array(items)
            if !items.is_empty() && items.len() <= MAX_FORM_ENUM_VALUES && items.iter().all(is_json_value));
        if !ok {
            return Some(format!(
                "{path}.enum must contain 1-{MAX_FORM_ENUM_VALUES} JSON values."
            ));
        }
    }
    None
}

fn validate_dependent_required(
    value: Option<&JsValue>,
    properties: Option<&JsObject>,
    path: &str,
) -> Option<String> {
    let value = value?;
    let Some(value) = record(value) else {
        return Some(format!("{path}.dependentRequired must be an object."));
    };
    let entries = value.own_entries();
    if entries.len() > MAX_FORM_DEPENDENCIES {
        return Some(format!(
            "{path}.dependentRequired may contain at most {MAX_FORM_DEPENDENCIES} dependencies."
        ));
    }
    for (trigger, dependencies) in entries {
        let name = text(trigger);
        if trim_empty(trigger) {
            return Some(format!(
                "{path}.dependentRequired cannot contain an empty property name."
            ));
        }
        let Some(properties) =
            properties.filter(|properties| properties.get_key(trigger).is_some())
        else {
            return Some(format!(
                "{path}.dependentRequired.{name} must reference a declared property."
            ));
        };
        let ok = matches!(dependencies, JsValue::Array(items)
            if !items.is_empty()
                && items.len() <= MAX_FORM_PROPERTIES
                && items.iter().all(|item| item.as_string().is_some_and(|item| !trim_empty(item)))
                && !items.iter().enumerate().any(|(index, item)| items[..index].contains(item)));
        if !ok {
            return Some(format!(
                "{path}.dependentRequired.{name} must contain 1-{MAX_FORM_PROPERTIES} unique non-empty declared property names."
            ));
        }
        if let JsValue::Array(items) = dependencies
            && items.iter().any(|item| {
                item.as_string()
                    .is_some_and(|item| properties.get_key(item).is_none())
            })
        {
            return Some(format!(
                "{path}.dependentRequired.{name} must only reference declared properties."
            ));
        }
    }
    None
}

fn validate_count_bounds(node: &JsObject, path: &str) -> Option<String> {
    for key in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
    ] {
        if present(node, key)
            .is_some_and(|value| !is_integer(value) || value.as_number().unwrap_or(0.0) < 0.0)
        {
            return Some(format!("{path}.{key} must be a non-negative integer."));
        }
    }
    for (minimum, maximum) in [
        ("minLength", "maxLength"),
        ("minItems", "maxItems"),
        ("minProperties", "maxProperties"),
    ] {
        if let (Some(low), Some(high)) = (
            node.get(minimum).and_then(JsValue::as_number),
            node.get(maximum).and_then(JsValue::as_number),
        ) && low > high
        {
            return Some(format!(
                "{path}.{minimum} must not exceed {path}.{maximum}."
            ));
        }
    }
    present(node, "uniqueItems")
        .is_some_and(|value| value.as_bool().is_none())
        .then(|| format!("{path}.uniqueItems must be a boolean."))
}

fn validate_numeric_bounds(node: &JsObject, path: &str) -> Option<String> {
    for key in [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ] {
        if present(node, key).is_some_and(|value| !value.as_number().is_some_and(f64::is_finite)) {
            return Some(format!("{path}.{key} must be a finite number."));
        }
    }
    let number = |key: &str| node.get(key).and_then(JsValue::as_number);
    if number("multipleOf").is_some_and(|value| value <= 0.0) {
        return Some(format!("{path}.multipleOf must be greater than zero."));
    }
    if let (Some(low), Some(high)) = (number("minimum"), number("maximum"))
        && low > high
    {
        return Some(format!("{path}.minimum must not exceed {path}.maximum."));
    }
    if let (Some(low), Some(high)) = (number("exclusiveMinimum"), number("exclusiveMaximum"))
        && low >= high
    {
        return Some(format!(
            "{path}.exclusiveMinimum must be less than {path}.exclusiveMaximum."
        ));
    }
    None
}

fn validate_contains_bounds(node: &JsObject, path: &str) -> Option<String> {
    for key in ["minContains", "maxContains"] {
        if present(node, key)
            .is_some_and(|value| !is_integer(value) || value.as_number().unwrap_or(0.0) < 0.0)
        {
            return Some(format!("{path}.{key} must be a non-negative integer."));
        }
    }
    if (present(node, "minContains").is_some() || present(node, "maxContains").is_some())
        && present(node, "contains").is_none()
    {
        return Some(format!(
            "{path}.minContains and {path}.maxContains require {path}.contains."
        ));
    }
    if let (Some(low), Some(high)) = (
        node.get("minContains").and_then(JsValue::as_number),
        node.get("maxContains").and_then(JsValue::as_number),
    ) && low > high
    {
        return Some(format!(
            "{path}.minContains must not exceed {path}.maxContains."
        ));
    }
    None
}

fn validate_local_definition_references(root: &JsObject) -> Option<String> {
    let empty = JsObject::new();
    let definitions = root.get("$defs").and_then(record).unwrap_or(&empty);
    let names: Vec<String> = definitions
        .own_entries()
        .iter()
        .map(|(name, _)| text(name))
        .collect();
    let mut root_references = Vec::new();
    collect_references(&JsValue::Object(root.clone()), "$", &mut root_references);
    for (name, path) in &root_references {
        if !names.contains(name) {
            return Some(format!(
                "{path}.$ref references unknown root definition {name}."
            ));
        }
    }
    let mut graph: Vec<(String, Vec<String>)> = Vec::new();
    for (name, definition) in definitions.own_entries() {
        let name = text(name);
        let mut references = Vec::new();
        collect_references(definition, &format!("$.$defs.{name}"), &mut references);
        for (target, path) in &references {
            if !names.contains(target) {
                return Some(format!(
                    "{path}.$ref references unknown root definition {target}."
                ));
            }
        }
        graph.push((name, references.into_iter().map(|(name, _)| name).collect()));
    }
    let mut completed: Vec<String> = Vec::new();
    let mut active: Vec<String> = Vec::new();
    fn visit(
        name: &str,
        graph: &[(String, Vec<String>)],
        active: &mut Vec<String>,
        completed: &mut Vec<String>,
    ) -> Option<String> {
        if let Some(start) = active.iter().position(|entry| entry == name) {
            let mut cycle: Vec<&str> = active[start..].iter().map(String::as_str).collect();
            cycle.push(name);
            return Some(format!(
                "$defs contains a circular reference: {}.",
                cycle.join(" -> ")
            ));
        }
        if completed.iter().any(|entry| entry == name) {
            return None;
        }
        active.push(name.to_owned());
        let targets = graph
            .iter()
            .find(|(entry, _)| entry == name)
            .map(|(_, targets)| targets.clone())
            .unwrap_or_default();
        for target in targets {
            if let Some(issue) = visit(&target, graph, active, completed) {
                return Some(issue);
            }
        }
        active.pop();
        completed.push(name.to_owned());
        None
    }
    for name in &names {
        if let Some(issue) = visit(name, &graph, &mut active, &mut completed) {
            return Some(issue);
        }
    }
    None
}

fn collect_references(schema: &JsValue, path: &str, references: &mut Vec<(String, String)>) {
    let Some(node) = record(schema) else {
        return;
    };
    if let Some(name) = node
        .get("$ref")
        .and_then(JsValue::as_string)
        .map(text)
        .and_then(|reference| local_definition_name(&reference).map(str::to_owned))
    {
        references.push((name, path.to_owned()));
    }
    for key in ["properties", "patternProperties"] {
        if let Some(children) = node.get(key).and_then(record) {
            for (name, child) in children.own_entries() {
                collect_references(child, &format!("{path}.{key}.{}", text(name)), references);
            }
        }
    }
    if let Some(additional) = node
        .get("additionalProperties")
        .filter(|v| record(v).is_some())
    {
        collect_references(
            additional,
            &format!("{path}.additionalProperties"),
            references,
        );
    }
    if let Some(items) = present(node, "items") {
        collect_references(items, &format!("{path}.items"), references);
    }
    if let Some(JsValue::Array(items)) = node.get("prefixItems") {
        for (index, item) in items.iter().enumerate() {
            collect_references(item, &format!("{path}.prefixItems[{index}]"), references);
        }
    }
    for key in ["contains", "propertyNames"] {
        if let Some(child) = present(node, key) {
            collect_references(child, &format!("{path}.{key}"), references);
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(JsValue::Array(branches)) = node.get(key) {
            for (index, branch) in branches.iter().enumerate() {
                collect_references(branch, &format!("{path}.{key}[{index}]"), references);
            }
        }
    }
    for key in ["not", "if", "then", "else"] {
        if let Some(child) = present(node, key) {
            collect_references(child, &format!("{path}.{key}"), references);
        }
    }
    if let Some(children) = node.get("dependentSchemas").and_then(record) {
        for (name, child) in children.own_entries() {
            collect_references(
                child,
                &format!("{path}.dependentSchemas.{}", text(name)),
                references,
            );
        }
    }
}

fn is_supported_type(value: &JsValue) -> bool {
    let supported = |value: &JsValue| {
        value
            .as_string()
            .is_some_and(|kind| SUPPORTED_TYPES.iter().any(|name| kind.equals_str(name)))
    };
    match value {
        JsValue::String(_) => supported(value),
        JsValue::Array(items) => !items.is_empty() && items.iter().all(supported),
        _ => false,
    }
}

/// `acceptsFormDialogAnswer` for the subset this host accepts as input.
pub fn accepts_form_dialog_answer(schema: &JsValue, value: &JsValue) -> bool {
    let Some(root) = record(schema) else {
        return false;
    };
    if root
        .get("$schema")
        .and_then(JsValue::as_string)
        .is_some_and(|declared| declared.equals_str(FORM_DIALOG_DRAFT_2020_12))
    {
        return false;
    }
    let empty = JsObject::new();
    let definitions = root.get("$defs").and_then(record).unwrap_or(&empty);
    record(value).is_some() && answer_matches(value, schema, definitions)
}

fn json_equal(left: &JsValue, right: &JsValue) -> bool {
    match (left, right) {
        (JsValue::Array(left), JsValue::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(l, r)| json_equal(l, r))
        }
        (JsValue::Object(left), JsValue::Object(right)) => {
            let left = left.own_entries();
            left.len() == right.own_entries().len()
                && left.iter().all(|(key, value)| {
                    right
                        .get_key(key)
                        .is_some_and(|other| json_equal(value, other))
                })
        }
        (JsValue::Number(left), JsValue::Number(right)) => left == right,
        _ => left == right,
    }
}

fn matches_type(value: &JsValue, kind: &JsValue) -> bool {
    let candidates: Vec<&JsValue> = match kind {
        JsValue::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    candidates.into_iter().any(|candidate| {
        let Some(candidate) = candidate.as_string() else {
            return false;
        };
        match text(candidate).as_str() {
            "null" => matches!(value, JsValue::Null),
            "object" => matches!(value, JsValue::Object(_)),
            "array" => matches!(value, JsValue::Array(_)),
            "string" => matches!(value, JsValue::String(_)),
            "number" => value.as_number().is_some_and(f64::is_finite),
            "integer" => is_integer(value),
            "boolean" => matches!(value, JsValue::Bool(_)),
            _ => false,
        }
    })
}

fn answer_matches(value: &JsValue, schema: &JsValue, definitions: &JsObject) -> bool {
    let Some(node) = record(schema) else {
        return true;
    };
    let number = |key: &str| node.get(key).and_then(JsValue::as_number);
    let child = |key: &str| present(node, key);
    if let Some(reference) = present(node, "$ref") {
        let target = reference
            .as_string()
            .map(text)
            .and_then(|reference| local_definition_name(&reference).map(str::to_owned))
            .and_then(|name| definitions.get(&name).cloned());
        if !target.is_some_and(|target| answer_matches(value, &target, definitions)) {
            return false;
        }
    }
    if let Some(kind) = child("type")
        && !matches_type(value, kind)
    {
        return false;
    }
    if let Some(constant) = child("const")
        && !json_equal(value, constant)
    {
        return false;
    }
    if let Some(JsValue::Array(values)) = node.get("enum")
        && !values.iter().any(|candidate| json_equal(value, candidate))
    {
        return false;
    }
    if let JsValue::String(string) = value {
        let length = string.units().len() as f64;
        if number("minLength").is_some_and(|min| length < min)
            || number("maxLength").is_some_and(|max| length > max)
        {
            return false;
        }
        if let Some(JsValue::String(pattern)) = child("pattern")
            && !pattern_matches(&text(pattern), &text(string))
        {
            return false;
        }
        if let Some(JsValue::String(format)) = child("format")
            && !matches_string_format(&text(string), &text(format))
        {
            return false;
        }
    }
    if let JsValue::Number(n) = value {
        let n = *n;
        if number("minimum").is_some_and(|m| n < m)
            || number("maximum").is_some_and(|m| n > m)
            || number("exclusiveMinimum").is_some_and(|m| n <= m)
            || number("exclusiveMaximum").is_some_and(|m| n >= m)
            || number("multipleOf").is_some_and(|d| {
                let quotient = n / d;
                (quotient - quotient.round()).abs() >= 1e-10 || quotient.is_nan()
            })
        {
            return false;
        }
    }
    if let JsValue::Array(items) = value {
        let length = items.len() as f64;
        if number("minItems").is_some_and(|min| length < min)
            || number("maxItems").is_some_and(|max| length > max)
        {
            return false;
        }
        if node.get("uniqueItems") == Some(&JsValue::Bool(true))
            && items
                .iter()
                .enumerate()
                .any(|(index, item)| items[..index].iter().any(|prior| json_equal(prior, item)))
        {
            return false;
        }
        let prefix: &[JsValue] = match node.get("prefixItems") {
            Some(JsValue::Array(prefix)) => prefix,
            _ => &[],
        };
        if items
            .iter()
            .zip(prefix)
            .any(|(item, schema)| !answer_matches(item, schema, definitions))
        {
            return false;
        }
        if let Some(each) = child("items")
            && !items
                .iter()
                .skip(prefix.len())
                .all(|item| answer_matches(item, each, definitions))
        {
            return false;
        }
        if let Some(contains) = child("contains") {
            let matches = items
                .iter()
                .filter(|item| answer_matches(item, contains, definitions))
                .count() as f64;
            if matches < number("minContains").unwrap_or(1.0)
                || number("maxContains").is_some_and(|max| matches > max)
            {
                return false;
            }
        }
    }
    if let JsValue::Object(object) = value {
        let empty = JsObject::new();
        let properties = node.get("properties").and_then(record).unwrap_or(&empty);
        let patterns = node
            .get("patternProperties")
            .and_then(record)
            .unwrap_or(&empty);
        let keys = object.own_entries();
        let count = keys.len() as f64;
        if number("minProperties").is_some_and(|min| count < min)
            || number("maxProperties").is_some_and(|max| count > max)
        {
            return false;
        }
        if let Some(JsValue::Array(required)) = node.get("required")
            && required.iter().any(|key| {
                key.as_string()
                    .is_some_and(|key| object.get_key(key).is_none())
            })
        {
            return false;
        }
        for (key, item) in &keys {
            match properties.get_key(key) {
                Some(property) => {
                    if !answer_matches(item, property, definitions) {
                        return false;
                    }
                }
                None => match node.get("additionalProperties") {
                    Some(JsValue::Bool(false)) => return false,
                    Some(additional @ JsValue::Object(_)) => {
                        if !answer_matches(item, additional, definitions) {
                            return false;
                        }
                    }
                    _ => {}
                },
            }
            for (pattern, schema) in patterns.own_entries() {
                if pattern_matches(&text(pattern), &text(key))
                    && !answer_matches(item, schema, definitions)
                {
                    return false;
                }
            }
        }
        if let Some(dependencies) = node.get("dependentRequired").and_then(record) {
            for (trigger, required) in dependencies.own_entries() {
                if object.get_key(trigger).is_some()
                    && let JsValue::Array(required) = required
                    && required.iter().any(|dependency| {
                        dependency
                            .as_string()
                            .is_some_and(|dependency| object.get_key(dependency).is_none())
                    })
                {
                    return false;
                }
            }
        }
        if let Some(dependencies) = node.get("dependentSchemas").and_then(record) {
            for (trigger, schema) in dependencies.own_entries() {
                if object.get_key(trigger).is_some() && !answer_matches(value, schema, definitions)
                {
                    return false;
                }
            }
        }
        if let Some(names) = child("propertyNames")
            && !keys.iter().all(|(key, _)| {
                answer_matches(&JsValue::String((*key).clone()), names, definitions)
            })
        {
            return false;
        }
    }
    if let Some(JsValue::Array(branches)) = node.get("allOf")
        && !branches
            .iter()
            .all(|branch| answer_matches(value, branch, definitions))
    {
        return false;
    }
    if let Some(JsValue::Array(branches)) = node.get("anyOf")
        && !branches
            .iter()
            .any(|branch| answer_matches(value, branch, definitions))
    {
        return false;
    }
    if let Some(JsValue::Array(branches)) = node.get("oneOf")
        && branches
            .iter()
            .filter(|branch| answer_matches(value, branch, definitions))
            .count()
            != 1
    {
        return false;
    }
    if let Some(not) = child("not")
        && answer_matches(value, not, definitions)
    {
        return false;
    }
    if let Some(condition) = child("if") {
        let branch = if answer_matches(value, condition, definitions) {
            child("then")
        } else {
            child("else")
        };
        if let Some(branch) = branch
            && !answer_matches(value, branch, definitions)
        {
            return false;
        }
    }
    true
}

fn matches_string_format(value: &str, format: &str) -> bool {
    match format {
        "email" => {
            // `/^[^\s@]+@[^\s@]+\.[^\s@]+$/u`.
            let plain = |part: &str| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|c| c != '@' && !crate::runtime::tools::js_space(c))
            };
            let Some((local, domain)) = value.split_once('@') else {
                return false;
            };
            plain(local)
                && domain.char_indices().any(|(index, c)| {
                    c == '.' && plain(&domain[..index]) && plain(&domain[index + 1..])
                })
        }
        "uri" => {
            !value.chars().any(crate::runtime::tools::js_space)
                && url::Url::parse(value).is_ok_and(|url| !url.scheme().is_empty())
        }
        "uuid" => {
            let bytes = value.as_bytes();
            bytes.len() == 36
                && bytes.iter().enumerate().all(|(index, b)| match index {
                    8 | 13 | 18 | 23 => *b == b'-',
                    19 => matches!(b.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'),
                    _ => b.is_ascii_hexdigit(),
                })
        }
        "date" => is_iso_date(value),
        "time" => is_rfc3339_time(value, false),
        "date-time" => value
            .split_once('T')
            .is_some_and(|(date, time)| is_iso_date(date) && is_rfc3339_time(time, true)),
        _ => false,
    }
}

fn digits(value: &str, count: usize) -> Option<u32> {
    (value.len() == count && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn is_iso_date(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    let [year, month, day] = parts.as_slice() else {
        return false;
    };
    let (Some(year), Some(month), Some(day)) = (digits(year, 4), digits(month, 2), digits(day, 2))
    else {
        return false;
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return false;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    day <= days
}

fn is_rfc3339_time(value: &str, require_timezone: bool) -> bool {
    if value.len() < 8 || !value.is_char_boundary(8) {
        return false;
    }
    let (clock, rest) = value.split_at(8);
    let parts: Vec<&str> = clock.split(':').collect();
    let [hour, minute, second] = parts.as_slice() else {
        return false;
    };
    let (Some(hour), Some(minute), Some(second)) =
        (digits(hour, 2), digits(minute, 2), digits(second, 2))
    else {
        return false;
    };
    let rest = match rest.strip_prefix('.') {
        Some(fraction) => {
            let end = fraction
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(fraction.len());
            if end == 0 {
                return false;
            }
            &fraction[end..]
        }
        None => rest,
    };
    let timezone = match rest {
        "" => None,
        "Z" => Some(None),
        offset => {
            let bytes = offset.as_bytes();
            if bytes.len() != 6 || !matches!(bytes[0], b'+' | b'-') || bytes[3] != b':' {
                return false;
            }
            let (Some(h), Some(m)) = (digits(&offset[1..3], 2), digits(&offset[4..6], 2)) else {
                return false;
            };
            Some(Some((h, m)))
        }
    };
    if hour > 23 || minute > 59 || second > 59 || (require_timezone && timezone.is_none()) {
        return false;
    }
    match timezone {
        None | Some(None) => true,
        Some(Some((h, m))) => h <= 23 && m <= 59,
    }
}

fn issue(path: &str, message: impl Into<JsString>) -> Option<Vec<JsObject>> {
    Some(vec![object([
        ("path", string(path)),
        ("code", string("invalid_schema")),
        ("message", JsValue::String(message.into())),
    ])])
}

/// `validateRequestUserFormInput`.
pub fn validate_input(input: &JsValue) -> Result<Option<Vec<JsObject>>, ToolError> {
    match input.get("prompt") {
        Some(JsValue::String(prompt)) if trim_empty(prompt) => {
            return Ok(issue("prompt", "prompt must be a non-empty string."));
        }
        Some(JsValue::String(_)) => {}
        _ => return Err(ToolError::failed("input.prompt.trim is not a function")),
    }
    let schema = super::field(input, "schema");
    Ok(validate_form_dialog_schema(schema)
        .err()
        .and_then(|message| issue("schema", message)))
}
