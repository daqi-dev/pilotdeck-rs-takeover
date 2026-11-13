//! Draft 2020-12 form schemas: the baseline's `validateDraft202012FormSchema`
//! and `getDraft202012Validator` (`tool/dialog/FormDialogSchema.ts:168-203`),
//! which compile the schema with Ajv 8.20
//! `new Ajv2020({allErrors, strict, strictTuples: false, validateFormats,
//! unevaluated})` plus `ajv-formats` 3.0.1 in its default (full) mode.
//!
//! [`compile`] follows Ajv's compile pipeline in order and reports the first
//! thing Ajv throws, with Ajv's text:
//! 1. `getSchemaRefs` (anchors; `compile/resolve.ts`);
//! 2. meta-schema validation, `schema is invalid: <errorsText>` — Ajv compiles
//!    its meta-schemas with `validateFormats: false`, so the bundled copy below
//!    carries no `format` keywords; `$dynamicRef: "#meta"` always lands on the
//!    root meta-schema and is written as `$ref: "#"`. The errors come from the
//!    Ajv port in `runtime/mcp/output_schema.rs`;
//! 3. the code-generation walk (`compile/validate/index.ts`), keyword by
//!    keyword in `RULES` order, with its strict-mode and compile-time throws.
//!
//! [`accepts`] is the boolean result of the compiled validator, including
//! Ajv's evaluated-property/item tracking for `unevaluated*`.
//!
//! What this host cannot do exactly is refused with [`Refusal::Unsupported`]
//! instead of being guessed: `$id`, `$dynamicRef`/`$dynamicAnchor`/
//! `$recursiveRef`, the `formatMaximum` family, patterns this host's regex
//! engine cannot express, formats named after `Object.prototype` members and
//! `$ref`s that need URI decoding, chain through other references or land on
//! something other than a schema.
use crate::config::builtin_property;
use crate::runtime::mcp::output_schema::{
    JsRegex, code_points, js_regex, js_regex_non_unicode_error, number_format_valid,
    string_format_valid, validate_with_budget,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::OnceLock;

/// Why a Draft 2020-12 form schema was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Ajv throws this while compiling the schema.
    Ajv(String),
    /// Ajv compiles the schema, but this host cannot validate it exactly.
    Unsupported(String),
}

/// The 2020-12 meta-schema and its seven vocabularies as one document.
const META_SCHEMA: &str = r##"{
"allOf":[{"$ref":"#/$defs/core"},{"$ref":"#/$defs/applicator"},{"$ref":"#/$defs/unevaluated"},{"$ref":"#/$defs/validation"},{"$ref":"#/$defs/meta-data"},{"$ref":"#/$defs/format-annotation"},{"$ref":"#/$defs/content"}],
"type":["object","boolean"],
"properties":{
 "definitions":{"type":"object","additionalProperties":{"$ref":"#"}},
 "dependencies":{"type":"object","additionalProperties":{"anyOf":[{"$ref":"#"},{"$ref":"#/$defs/validation/$defs/stringArray"}]}},
 "$recursiveAnchor":{"$ref":"#/$defs/core/$defs/anchorString"},
 "$recursiveRef":{"$ref":"#/$defs/core/$defs/uriReferenceString"}},
"$defs":{
 "core":{"type":["object","boolean"],"properties":{
  "$id":{"$ref":"#/$defs/core/$defs/uriReferenceString","pattern":"^[^#]*#?$"},
  "$schema":{"$ref":"#/$defs/core/$defs/uriString"},
  "$ref":{"$ref":"#/$defs/core/$defs/uriReferenceString"},
  "$anchor":{"$ref":"#/$defs/core/$defs/anchorString"},
  "$dynamicRef":{"$ref":"#/$defs/core/$defs/uriReferenceString"},
  "$dynamicAnchor":{"$ref":"#/$defs/core/$defs/anchorString"},
  "$vocabulary":{"type":"object","propertyNames":{"$ref":"#/$defs/core/$defs/uriString"},"additionalProperties":{"type":"boolean"}},
  "$comment":{"type":"string"},
  "$defs":{"type":"object","additionalProperties":{"$ref":"#"}}},
  "$defs":{"anchorString":{"type":"string","pattern":"^[A-Za-z_][-A-Za-z0-9._]*$"},"uriString":{"type":"string"},"uriReferenceString":{"type":"string"}}},
 "applicator":{"type":["object","boolean"],"properties":{
  "prefixItems":{"$ref":"#/$defs/applicator/$defs/schemaArray"},
  "items":{"$ref":"#"},
  "contains":{"$ref":"#"},
  "additionalProperties":{"$ref":"#"},
  "properties":{"type":"object","additionalProperties":{"$ref":"#"}},
  "patternProperties":{"type":"object","additionalProperties":{"$ref":"#"}},
  "dependentSchemas":{"type":"object","additionalProperties":{"$ref":"#"}},
  "propertyNames":{"$ref":"#"},
  "if":{"$ref":"#"},
  "then":{"$ref":"#"},
  "else":{"$ref":"#"},
  "allOf":{"$ref":"#/$defs/applicator/$defs/schemaArray"},
  "anyOf":{"$ref":"#/$defs/applicator/$defs/schemaArray"},
  "oneOf":{"$ref":"#/$defs/applicator/$defs/schemaArray"},
  "not":{"$ref":"#"}},
  "$defs":{"schemaArray":{"type":"array","minItems":1,"items":{"$ref":"#"}}}},
 "unevaluated":{"type":["object","boolean"],"properties":{
  "unevaluatedItems":{"$ref":"#"},
  "unevaluatedProperties":{"$ref":"#"}}},
 "validation":{"type":["object","boolean"],"properties":{
  "type":{"anyOf":[{"$ref":"#/$defs/validation/$defs/simpleTypes"},{"type":"array","items":{"$ref":"#/$defs/validation/$defs/simpleTypes"},"minItems":1,"uniqueItems":true}]},
  "const":true,
  "enum":{"type":"array","items":true},
  "multipleOf":{"type":"number","exclusiveMinimum":0},
  "maximum":{"type":"number"},
  "exclusiveMaximum":{"type":"number"},
  "minimum":{"type":"number"},
  "exclusiveMinimum":{"type":"number"},
  "maxLength":{"$ref":"#/$defs/validation/$defs/nonNegativeInteger"},
  "minLength":{"$ref":"#/$defs/validation/$defs/nonNegativeIntegerDefault0"},
  "pattern":{"type":"string"},
  "maxItems":{"$ref":"#/$defs/validation/$defs/nonNegativeInteger"},
  "minItems":{"$ref":"#/$defs/validation/$defs/nonNegativeIntegerDefault0"},
  "uniqueItems":{"type":"boolean"},
  "maxContains":{"$ref":"#/$defs/validation/$defs/nonNegativeInteger"},
  "minContains":{"$ref":"#/$defs/validation/$defs/nonNegativeInteger"},
  "maxProperties":{"$ref":"#/$defs/validation/$defs/nonNegativeInteger"},
  "minProperties":{"$ref":"#/$defs/validation/$defs/nonNegativeIntegerDefault0"},
  "required":{"$ref":"#/$defs/validation/$defs/stringArray"},
  "dependentRequired":{"type":"object","additionalProperties":{"$ref":"#/$defs/validation/$defs/stringArray"}}},
  "$defs":{
   "nonNegativeInteger":{"type":"integer","minimum":0},
   "nonNegativeIntegerDefault0":{"$ref":"#/$defs/validation/$defs/nonNegativeInteger"},
   "simpleTypes":{"enum":["array","boolean","integer","null","number","object","string"]},
   "stringArray":{"type":"array","items":{"type":"string"},"uniqueItems":true}}},
 "meta-data":{"type":["object","boolean"],"properties":{
  "title":{"type":"string"},
  "description":{"type":"string"},
  "default":true,
  "deprecated":{"type":"boolean"},
  "readOnly":{"type":"boolean"},
  "writeOnly":{"type":"boolean"},
  "examples":{"type":"array","items":true}}},
 "format-annotation":{"type":["object","boolean"],"properties":{"format":{"type":"string"}}},
 "content":{"type":["object","boolean"],"properties":{
  "contentEncoding":{"type":"string"},
  "contentMediaType":{"type":"string"},
  "contentSchema":{"$ref":"#"}}}}
}"##;

/// Weighted recursion budget for the meta-schema walk. Every nesting level of
/// the form schema re-enters the whole meta-schema; Ajv itself only stops at
/// V8's stack limit, which a 64 kB schema does not reach in practice.
const META_BUDGET: usize = 4_000_000;

/// Data nesting bound for [`accepts`]; Ajv throws `RangeError` (and the
/// baseline answers `false`) on runaway `$ref` recursion.
const MAX_EVAL_DEPTH: usize = 512;

fn meta_schema() -> &'static JsValue {
    static META: OnceLock<JsValue> = OnceLock::new();
    META.get_or_init(|| parse_json(META_SCHEMA).expect("bundled 2020-12 meta-schema"))
}

/// `ajv.RULES.keywords`: every keyword strict mode accepts.
const KNOWN_KEYWORDS: &[&str] = &[
    "$async",
    "$dynamicAnchor",
    "$dynamicRef",
    "$recursiveAnchor",
    "$recursiveRef",
    "$schema",
    "$id",
    "$defs",
    "$vocabulary",
    "$comment",
    "definitions",
    "id",
    "$ref",
    "maximum",
    "minimum",
    "exclusiveMaximum",
    "exclusiveMinimum",
    "multipleOf",
    "maxLength",
    "minLength",
    "pattern",
    "maxProperties",
    "minProperties",
    "required",
    "maxItems",
    "minItems",
    "uniqueItems",
    "type",
    "nullable",
    "const",
    "enum",
    "not",
    "anyOf",
    "oneOf",
    "allOf",
    "if",
    "then",
    "else",
    "propertyNames",
    "additionalProperties",
    "dependencies",
    "properties",
    "patternProperties",
    "prefixItems",
    "items",
    "contains",
    "format",
    "title",
    "description",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "examples",
    "contentMediaType",
    "contentEncoding",
    "contentSchema",
    "dependentRequired",
    "dependentSchemas",
    "maxContains",
    "minContains",
    "unevaluatedProperties",
    "unevaluatedItems",
    "formatMaximum",
    "formatMinimum",
    "formatExclusiveMaximum",
    "formatExclusiveMinimum",
];

/// One entry of `ajv.RULES.all`: data types the keyword applies to and the
/// JSON types its own value must have.
struct Rule {
    keyword: &'static str,
    types: &'static [&'static str],
    schema_type: &'static [&'static str],
}

const fn rule(
    keyword: &'static str,
    types: &'static [&'static str],
    schema_type: &'static [&'static str],
) -> Rule {
    Rule {
        keyword,
        types,
        schema_type,
    }
}

const NUM: &[&str] = &["number"];
const STR: &[&str] = &["string"];
const ARR: &[&str] = &["array"];
const OBJ: &[&str] = &["object"];
const NONE: &[&str] = &[];
const SCHEMA: &[&str] = &["object", "boolean"];

/// `ajv.RULES.all` in insertion order (measured on the baseline instance).
const RULES: &[Rule] = &[
    rule("$dynamicAnchor", NONE, STR),
    rule("$dynamicRef", NONE, STR),
    rule("$recursiveAnchor", NONE, &["boolean"]),
    rule("$recursiveRef", NONE, STR),
    rule("$comment", NONE, NONE),
    rule("id", NONE, NONE),
    rule("$ref", NONE, STR),
    rule("maximum", NUM, NUM),
    rule("minimum", NUM, NUM),
    rule("exclusiveMaximum", NUM, NUM),
    rule("exclusiveMinimum", NUM, NUM),
    rule("multipleOf", NUM, NUM),
    rule("maxLength", STR, NUM),
    rule("minLength", STR, NUM),
    rule("pattern", STR, STR),
    rule("maxProperties", OBJ, NUM),
    rule("minProperties", OBJ, NUM),
    rule("required", OBJ, ARR),
    rule("maxItems", ARR, NUM),
    rule("minItems", ARR, NUM),
    rule("uniqueItems", ARR, &["boolean"]),
    rule("type", NONE, &["string", "array"]),
    rule("nullable", NONE, &["boolean"]),
    rule("const", NONE, NONE),
    rule("enum", NONE, ARR),
    rule("not", NONE, SCHEMA),
    rule("anyOf", NONE, ARR),
    rule("oneOf", NONE, ARR),
    rule("allOf", NONE, ARR),
    rule("if", NONE, SCHEMA),
    rule("then", NONE, SCHEMA),
    rule("else", NONE, SCHEMA),
    rule("propertyNames", OBJ, SCHEMA),
    rule("additionalProperties", OBJ, &["boolean", "object"]),
    rule("dependencies", OBJ, OBJ),
    rule("properties", OBJ, OBJ),
    rule("patternProperties", OBJ, OBJ),
    rule("prefixItems", ARR, ARR),
    rule("items", ARR, SCHEMA),
    rule("contains", ARR, SCHEMA),
    rule("format", &["number", "string"], STR),
    rule("dependentRequired", OBJ, OBJ),
    rule("dependentSchemas", OBJ, OBJ),
    rule("maxContains", ARR, NUM),
    rule("minContains", ARR, NUM),
    rule("unevaluatedProperties", OBJ, &["boolean", "object"]),
    rule("unevaluatedItems", ARR, &["boolean", "object"]),
    rule("formatMaximum", STR, STR),
    rule("formatMinimum", STR, STR),
    rule("formatExclusiveMaximum", STR, STR),
    rule("formatExclusiveMinimum", STR, STR),
];

/// `ajv.RULES.rules`: keyword groups in evaluation order.
const GROUPS: &[(Option<&str>, &[&str])] = &[
    (
        None,
        &[
            "$dynamicAnchor",
            "$dynamicRef",
            "$recursiveAnchor",
            "$recursiveRef",
            "$comment",
            "id",
            "$ref",
            "type",
            "nullable",
            "const",
            "enum",
            "not",
            "anyOf",
            "oneOf",
            "allOf",
            "if",
            "then",
            "else",
        ],
    ),
    (
        Some("number"),
        &[
            "maximum",
            "minimum",
            "exclusiveMaximum",
            "exclusiveMinimum",
            "multipleOf",
            "format",
        ],
    ),
    (
        Some("string"),
        &[
            "maxLength",
            "minLength",
            "pattern",
            "format",
            "formatMaximum",
            "formatMinimum",
            "formatExclusiveMaximum",
            "formatExclusiveMinimum",
        ],
    ),
    (
        Some("array"),
        &[
            "maxItems",
            "minItems",
            "prefixItems",
            "items",
            "contains",
            "uniqueItems",
            "maxContains",
            "minContains",
            "unevaluatedItems",
        ],
    ),
    (
        Some("object"),
        &[
            "maxProperties",
            "minProperties",
            "required",
            "propertyNames",
            "additionalProperties",
            "dependencies",
            "properties",
            "patternProperties",
            "dependentRequired",
            "dependentSchemas",
            "unevaluatedProperties",
        ],
    ),
];

const FORMAT_LIMITS: &[&str] = &[
    "formatMaximum",
    "formatMinimum",
    "formatExclusiveMaximum",
    "formatExclusiveMinimum",
];

/// `ajv-formats` full mode: `(name, applies to numbers)`.
const FORMATS: &[(&str, bool)] = &[
    ("date", false),
    ("time", false),
    ("date-time", false),
    ("iso-time", false),
    ("iso-date-time", false),
    ("duration", false),
    ("uri", false),
    ("uri-reference", false),
    ("uri-template", false),
    ("url", false),
    ("email", false),
    ("hostname", false),
    ("ipv4", false),
    ("ipv6", false),
    ("regex", false),
    ("uuid", false),
    ("json-pointer", false),
    ("json-pointer-uri-fragment", false),
    ("relative-json-pointer", false),
    ("byte", false),
    ("int32", true),
    ("int64", true),
    ("float", true),
    ("double", true),
    ("password", false),
    ("binary", false),
];

/// `json-schema-traverse` keys whose array values hold schemas.
const TRAVERSE_ARRAY_KEYWORDS: &[&str] = &["items", "allOf", "anyOf", "oneOf"];
/// `json-schema-traverse` keys whose object values map names to schemas.
const TRAVERSE_PROPS_KEYWORDS: &[&str] = &[
    "$defs",
    "definitions",
    "properties",
    "patternProperties",
    "dependencies",
];
/// `json-schema-traverse` keys never descended into with `allKeys`.
const TRAVERSE_SKIP_KEYWORDS: &[&str] = &[
    "default",
    "enum",
    "const",
    "required",
    "maxLength",
    "minLength",
    "pattern",
    "format",
    "maximum",
    "minimum",
    "exclusiveMaximum",
    "exclusiveMinimum",
    "multipleOf",
    "maxItems",
    "minItems",
    "uniqueItems",
    "maxProperties",
    "minProperties",
];

const REF_KEYWORDS: &[&str] = &[
    "$ref",
    "$recursiveRef",
    "$recursiveAnchor",
    "$dynamicRef",
    "$dynamicAnchor",
];

fn key_text(key: &JsString) -> String {
    String::from_utf16_lossy(key.units())
}

fn is_known_keyword(key: &JsString) -> bool {
    KNOWN_KEYWORDS.iter().any(|name| key.equals_str(name))
}

fn find_rule(keyword: &str) -> Option<&'static Rule> {
    RULES.iter().find(|rule| rule.keyword == keyword)
}

fn is_rule(key: &JsString) -> bool {
    RULES.iter().any(|rule| key.equals_str(rule.keyword))
}

/// `schema[keyword] !== undefined`.
fn present<'a>(object: &'a JsObject, keyword: &str) -> Option<&'a JsValue> {
    object.get(keyword).filter(|value| !value.is_undefined())
}

/// `schemaHasRules`.
fn has_rules(object: &JsObject) -> bool {
    object.own_entries().iter().any(|(key, _)| is_rule(key))
}

/// `schemaHasRulesButRef`.
fn has_rules_but_ref(object: &JsObject) -> bool {
    object
        .own_entries()
        .iter()
        .any(|(key, _)| !key.equals_str("$ref") && is_rule(key))
}

/// `allSchemaProperties`: own keys except `__proto__`.
fn schema_properties(value: Option<&JsValue>) -> Vec<&JsString> {
    match value {
        Some(JsValue::Object(object)) => object
            .own_entries()
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| !key.equals_str("__proto__"))
            .collect(),
        _ => Vec::new(),
    }
}

/// `typeof value == st` for one `schemaType` entry.
fn valid_schema_type(value: &JsValue, schema_type: &[&str]) -> bool {
    schema_type.is_empty()
        || schema_type.iter().any(|kind| match *kind {
            "array" => matches!(value, JsValue::Array(_)),
            "object" => matches!(value, JsValue::Object(_)),
            "string" => matches!(value, JsValue::String(_)),
            "number" => matches!(value, JsValue::Number(_)),
            "boolean" => matches!(value, JsValue::Bool(_)),
            _ => false,
        })
}

fn json_type(name: &JsString) -> Option<&'static str> {
    [
        "number", "integer", "string", "boolean", "array", "object", "null",
    ]
    .into_iter()
    .find(|kind| name.equals_str(kind))
}

/// `getSchemaTypes`, including the `nullable` checks.
fn schema_types(object: &JsObject) -> Result<Vec<&'static str>, String> {
    let mut types: Vec<&'static str> = match present(object, "type") {
        Some(JsValue::String(name)) => json_type(name).into_iter().collect(),
        Some(JsValue::Array(names)) => names
            .iter()
            .filter_map(|name| name.as_string().and_then(json_type))
            .collect(),
        _ => Vec::new(),
    };
    let nullable = present(object, "nullable");
    if types.contains(&"null") {
        if matches!(nullable, Some(JsValue::Bool(false))) {
            return Err("type: null contradicts nullable: false".into());
        }
    } else {
        if types.is_empty() && nullable.is_some() {
            return Err("\"nullable\" cannot be used without \"type\"".into());
        }
        if matches!(nullable, Some(JsValue::Bool(true))) {
            types.push("null");
        }
    }
    Ok(types)
}

/// `encodeURIComponent(escapeJsonPointer(name))`; `None` is the `URIError`
/// a lone surrogate raises.
fn escape_fragment(name: &JsString) -> Option<String> {
    let text = name.to_utf8().ok()?;
    let escaped = text.replace('~', "~0").replace('/', "~1");
    let mut out = String::new();
    for byte in escaped.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    Some(out)
}

fn strict(message: impl AsRef<str>) -> String {
    format!("strict mode: {}", message.as_ref())
}

/// Ajv's static view of evaluated properties/items (`it.props`, `it.items`):
/// absent, a compile-time constant, `true`, or a runtime variable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Evaluated {
    Undefined,
    Static,
    All,
    Dynamic,
}

/// `mergeEvaluated.props/items(gen, from, to, toName)`.
fn merge_evaluated(from: Evaluated, to: Evaluated, to_name: bool) -> Evaluated {
    let merged = match (to, from) {
        (Evaluated::Undefined, from) => from,
        (Evaluated::Dynamic, _) | (_, Evaluated::Dynamic) => Evaluated::Dynamic,
        (_, Evaluated::All) => Evaluated::All,
        _ => Evaluated::Static,
    };
    if to_name { Evaluated::Dynamic } else { merged }
}

/// One schema compilation context (`SchemaObjCxt`) as far as compile-time
/// throws depend on it.
struct Context {
    /// `errSchemaPath`.
    path: String,
    /// `dataTypes`.
    data_types: Vec<&'static str>,
    /// `definedProperties`, shared by reference like Ajv's `Set`.
    defined: Rc<RefCell<HashSet<String>>>,
    props: Evaluated,
    items: Evaluated,
}

impl Context {
    fn root(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            data_types: Vec::new(),
            defined: Rc::new(RefCell::new(HashSet::new())),
            props: Evaluated::Undefined,
            items: Evaluated::Undefined,
        }
    }

    /// A subschema applied to the same data (`allOf`, `if`, ...).
    fn same_data(&self, path: String) -> Self {
        Self {
            path,
            data_types: self.data_types.clone(),
            defined: self.defined.clone(),
            props: Evaluated::Undefined,
            items: Evaluated::Undefined,
        }
    }

    /// A subschema applied to a property, item or key. Ajv's
    /// `dataContextProps` replaces the *parent's* `definedProperties` with a
    /// fresh set that the child then shares.
    fn new_data(&mut self, path: String) -> Self {
        self.defined = Rc::new(RefCell::new(HashSet::new()));
        Self {
            path,
            data_types: Vec::new(),
            defined: self.defined.clone(),
            props: Evaluated::Undefined,
            items: Evaluated::Undefined,
        }
    }

    /// `KeywordCxt.mergeEvaluated(schemaCxt, toName)`.
    fn merge(&mut self, child: &Context, to_name: bool) {
        if self.props != Evaluated::All && child.props != Evaluated::Undefined {
            self.props = merge_evaluated(child.props, self.props, to_name);
        }
        if self.items != Evaluated::All && child.items != Evaluated::Undefined {
            self.items = merge_evaluated(child.items, self.items, to_name);
        }
    }

    /// `KeywordCxt.mergeValidEvaluated(schemaCxt, valid)`.
    fn merge_valid(&mut self, child: &Context) {
        if self.props != Evaluated::All || self.items != Evaluated::All {
            self.merge(child, true);
        }
    }

    fn keyword_path(&self, keyword: &str) -> String {
        format!("{}/{keyword}", self.path)
    }
}

type Anchors<'a> = HashMap<String, &'a JsValue>;

/// The Ajv compile pipeline for a root form schema that declares 2020-12.
pub fn compile(schema: &JsValue) -> Result<(), Refusal> {
    compile_with_anchors(schema).map(|_| ())
}

fn compile_with_anchors(schema: &JsValue) -> Result<Anchors<'_>, Refusal> {
    let Some(root) = schema.as_object() else {
        return Err(Refusal::Unsupported(
            "schema must be a JSON object.".to_owned(),
        ));
    };
    if present(root, "$id").is_some() {
        return Err(unsupported("schema.$id"));
    }
    let mut anchors = HashMap::new();
    let mut seen = HashSet::new();
    collect_anchors(schema, true, &mut anchors, &mut seen)?;
    match validate_with_budget(meta_schema(), schema, META_BUDGET) {
        Ok(None) => {}
        Ok(Some(errors)) => return Err(Refusal::Ajv(format!("schema is invalid: {errors}"))),
        Err(_) => {
            return Err(Refusal::Unsupported(
                "schema is too deeply nested for this gateway's Draft 2020-12 validator."
                    .to_owned(),
            ));
        }
    }
    let mut compiler = Compiler {
        root: schema,
        anchors: &anchors,
        compiled: HashMap::new(),
        unsupported: None,
    };
    let mut context = Context::root("#");
    compiler
        .subschema(schema, &mut context)
        .map_err(Refusal::Ajv)?;
    if let Some(message) = compiler.unsupported {
        return Err(Refusal::Unsupported(message));
    }
    Ok(anchors)
}

fn unsupported(what: &str) -> Refusal {
    Refusal::Unsupported(unsupported_message(what))
}

fn unsupported_message(what: &str) -> String {
    format!("{what} is not supported by this gateway's Draft 2020-12 form validator.")
}

/// `getSchemaRefs`: registers `$anchor`/`$dynamicAnchor` names below the root
/// (`json-schema-traverse` with `allKeys`) and refuses nested `$id`s.
fn collect_anchors<'a>(
    value: &'a JsValue,
    is_root: bool,
    anchors: &mut Anchors<'a>,
    seen: &mut HashSet<String>,
) -> Result<(), Refusal> {
    let JsValue::Object(object) = value else {
        return Ok(());
    };
    if !is_root {
        if matches!(object.get("$id"), Some(JsValue::String(_))) {
            return Err(unsupported("A nested $id"));
        }
        for keyword in ["$anchor", "$dynamicAnchor"] {
            let Some(JsValue::String(anchor)) = object.get(keyword) else {
                continue;
            };
            let anchor = key_text(anchor);
            let mut chars = anchor.chars();
            let valid = chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
            if !valid {
                return Err(Refusal::Ajv(format!("invalid anchor \"{anchor}\"")));
            }
            let reference = format!("#{anchor}");
            if !seen.insert(reference.clone()) {
                return Err(Refusal::Ajv(format!(
                    "reference \"{reference}\" resolves to more than one schema"
                )));
            }
            anchors.insert(reference, value);
        }
    }
    for (key, child) in object.own_entries() {
        match child {
            JsValue::Array(items) => {
                if TRAVERSE_ARRAY_KEYWORDS
                    .iter()
                    .any(|name| key.equals_str(name))
                {
                    for item in items {
                        collect_anchors(item, false, anchors, seen)?;
                    }
                }
            }
            _ if TRAVERSE_PROPS_KEYWORDS
                .iter()
                .any(|name| key.equals_str(name)) =>
            {
                if let JsValue::Object(map) = child {
                    for (_, item) in map.own_entries() {
                        collect_anchors(item, false, anchors, seen)?;
                    }
                }
            }
            _ if !TRAVERSE_SKIP_KEYWORDS
                .iter()
                .any(|name| key.equals_str(name)) =>
            {
                collect_anchors(child, false, anchors, seen)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// `hasRef`: any reference keyword anywhere below, data included.
fn has_ref(value: &JsValue) -> bool {
    match value {
        JsValue::Object(object) => object.own_entries().iter().any(|(key, child)| {
            REF_KEYWORDS.iter().any(|name| key.equals_str(name)) || has_ref(child)
        }),
        JsValue::Array(items) => items.iter().any(has_ref),
        _ => false,
    }
}

enum Resolved<'a> {
    Root,
    Target(&'a JsValue),
    Missing,
    Unsupported,
}

/// `resolveRef` for the local references [`compile`] supports.
fn resolve<'a>(root: &'a JsValue, anchors: &Anchors<'a>, reference: &str) -> Resolved<'a> {
    if reference == "#" || reference == "#/" {
        return Resolved::Root;
    }
    let Some(fragment) = reference.strip_prefix('#') else {
        return Resolved::Unsupported;
    };
    if !fragment.starts_with('/') {
        return match anchors.get(reference) {
            Some(target) => Resolved::Target(target),
            None => Resolved::Missing,
        };
    }
    // uri-js percent handling and JavaScript property lookup are not ported:
    // only plain JSON-pointer text is resolved here.
    if !fragment
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-._~!$&'()*+,;=:@/".contains(c))
    {
        return Resolved::Unsupported;
    }
    let mut node = root;
    for raw in fragment[1..].split('/') {
        let segment = raw.replace("~1", "/").replace("~0", "~");
        node = match node {
            JsValue::Object(object) => {
                if builtin_property(&JsString::from(segment.as_str())) {
                    return Resolved::Unsupported;
                }
                match object.get(&segment) {
                    Some(child) => child,
                    None => return Resolved::Missing,
                }
            }
            JsValue::Array(items) => {
                let index = (segment == "0"
                    || (!segment.starts_with('0')
                        && !segment.is_empty()
                        && segment.bytes().all(|b| b.is_ascii_digit())))
                .then(|| segment.parse::<usize>().ok())
                .flatten();
                match index {
                    Some(index) => match items.get(index) {
                        Some(child) => child,
                        None => return Resolved::Missing,
                    },
                    None if segment == "length" => return Resolved::Unsupported,
                    None => return Resolved::Missing,
                }
            }
            JsValue::Bool(_) => return Resolved::Missing,
            _ => return Resolved::Unsupported,
        };
    }
    if std::ptr::eq(node, root) {
        return Resolved::Missing;
    }
    match node {
        JsValue::Object(object)
            if present(object, "$ref").is_some() && !has_rules_but_ref(object) =>
        {
            // Ajv follows a pure-`$ref` target to its own target.
            Resolved::Unsupported
        }
        JsValue::Object(_) | JsValue::Bool(_) => Resolved::Target(node),
        _ => Resolved::Unsupported,
    }
}

struct Compiler<'a, 'b> {
    root: &'a JsValue,
    anchors: &'b Anchors<'a>,
    /// Separately compiled `$ref` targets and their final evaluated state
    /// (`None` while still compiling).
    compiled: HashMap<usize, Option<(Evaluated, Evaluated)>>,
    unsupported: Option<String>,
}

impl Compiler<'_, '_> {
    fn note_unsupported(&mut self, what: String) {
        if self.unsupported.is_none() {
            self.unsupported = Some(unsupported_message(&what));
        }
    }

    /// Ajv tracks evaluated properties/items in generated `var`s once they
    /// depend on the data (`anyOf`, `oneOf`, `if`/`then`/`else`,
    /// `dependentSchemas`, `patternProperties`, recursive `$ref`s). Which
    /// names those variables hold follows the generated code's control flow
    /// and hoisting (measured: after `if`/`else`, the `if` subschema's
    /// properties count only on the `then` path), not the specification, so
    /// `unevaluated*` over such state is refused rather than approximated.
    fn note_dynamic_evaluation(&mut self, keyword: &str, context: &Context) {
        self.note_unsupported(format!(
            "schema keyword {keyword} at {} (after anyOf/oneOf/if/dependentSchemas/patternProperties or a recursive $ref)",
            context.path
        ));
    }

    /// `checkUnknownRules`.
    fn check_unknown_rules(&self, object: &JsObject) -> Result<(), String> {
        for (key, _) in object.own_entries() {
            if !is_known_keyword(key) {
                return Err(strict(format!("unknown keyword: \"{}\"", key_text(key))));
            }
        }
        Ok(())
    }

    /// `alwaysValidSchema`.
    fn always_valid(&self, schema: &JsValue) -> Result<bool, String> {
        match schema {
            JsValue::Bool(valid) => Ok(*valid),
            JsValue::Object(object) => {
                if object.is_empty() {
                    return Ok(true);
                }
                self.check_unknown_rules(object)?;
                Ok(!has_rules(object))
            }
            _ => Ok(true),
        }
    }

    /// `subschemaCode` / `validateFunctionCode`.
    fn subschema(&mut self, schema: &JsValue, context: &mut Context) -> Result<(), String> {
        let JsValue::Object(object) = schema else {
            return Ok(());
        };
        self.check_unknown_rules(object)?;
        if !has_rules(object) {
            return Ok(());
        }
        let types = schema_types(object)?;
        if present(object, "$ref").is_some_and(truthy) && !has_rules_but_ref(object) {
            return self.keyword(object, "$ref", context);
        }
        self.check_strict_types(object, &types, context)?;
        for (_, keywords) in GROUPS {
            if !keywords.iter().any(|name| present(object, name).is_some()) {
                continue;
            }
            for name in *keywords {
                if present(object, name).is_some() {
                    self.keyword(object, name, context)?;
                }
            }
        }
        Ok(())
    }

    /// `checkStrictTypes`.
    fn check_strict_types(
        &self,
        object: &JsObject,
        types: &[&'static str],
        context: &mut Context,
    ) -> Result<(), String> {
        let error = |context: &Context, message: String| {
            Err(strict(format!(
                "{message} at \"{}\" (strictTypes)",
                context.path
            )))
        };
        if !types.is_empty() {
            if context.data_types.is_empty() {
                context.data_types = types.to_vec();
            } else {
                for kind in types {
                    if !includes_type(&context.data_types, kind) {
                        return error(
                            context,
                            format!(
                                "type \"{kind}\" not allowed by context \"{}\"",
                                context.data_types.join(",")
                            ),
                        );
                    }
                }
                let mut narrowed = Vec::new();
                for kind in &context.data_types {
                    if includes_type(types, kind) {
                        narrowed.push(*kind);
                    } else if types.contains(&"integer") && *kind == "number" {
                        narrowed.push("integer");
                    }
                }
                context.data_types = narrowed;
            }
        }
        if types.len() > 1 && !(types.len() == 2 && types.contains(&"null")) {
            return error(
                context,
                "use allowUnionTypes to allow union type keyword".to_owned(),
            );
        }
        for rule in RULES {
            if rule.types.is_empty() || present(object, rule.keyword).is_none() {
                continue;
            }
            let applicable = rule.types.iter().any(|kind| {
                context.data_types.contains(kind)
                    || (*kind == "number" && context.data_types.contains(&"integer"))
            });
            if !applicable {
                return error(
                    context,
                    format!(
                        "missing type \"{}\" for keyword \"{}\"",
                        rule.types.join(","),
                        rule.keyword
                    ),
                );
            }
        }
        Ok(())
    }

    /// `keywordCode` for one keyword (`KeywordCxt` constructor + `code`).
    fn keyword(
        &mut self,
        object: &JsObject,
        keyword: &str,
        context: &mut Context,
    ) -> Result<(), String> {
        let value = present(object, keyword).expect("keyword is present");
        if FORMAT_LIMITS.contains(&keyword) && present(object, "format").is_none() {
            return Err(format!(
                "parent schema must have dependencies of {keyword}: format"
            ));
        }
        if let Some(rule) = find_rule(keyword)
            && !valid_schema_type(value, rule.schema_type)
        {
            let types = rule
                .schema_type
                .iter()
                .map(|kind| format!("\"{kind}\""))
                .collect::<Vec<_>>()
                .join(",");
            return Err(format!("{keyword} value must be [{types}]"));
        }
        match keyword {
            "$dynamicAnchor" | "$dynamicRef" | "$recursiveRef" | "$recursiveAnchor" => {
                self.note_unsupported(format!("schema keyword {keyword} at {}", context.path));
            }
            "id" => {
                return Err("NOT SUPPORTED: keyword \"id\", use \"$id\" for schema ID".into());
            }
            "$ref" => self.reference(value, context)?,
            "enum" => {
                if matches!(value, JsValue::Array(items) if items.is_empty()) {
                    return Err("enum must have non-empty array".into());
                }
            }
            "not" => {
                if !self.always_valid(value)? {
                    let mut child = context.same_data(context.keyword_path("not"));
                    self.subschema(value, &mut child)?;
                }
            }
            "anyOf" | "oneOf" | "allOf" => self.composite(keyword, value, context)?,
            "if" => self.if_keyword(object, value, context)?,
            "then" | "else" => {
                if present(object, "if").is_none() {
                    return Err(strict(format!("\"{keyword}\" without \"if\" is ignored")));
                }
            }
            "format" => self.format(value, context)?,
            "pattern" => self.use_pattern(value, context)?,
            "formatMaximum"
            | "formatMinimum"
            | "formatExclusiveMaximum"
            | "formatExclusiveMinimum" => {
                self.note_unsupported(format!("schema keyword {keyword} at {}", context.path));
            }
            "prefixItems" => {
                let items = array(value);
                if !items.is_empty() && context.items != Evaluated::All {
                    context.items = merge_evaluated(Evaluated::Static, context.items, false);
                }
                for (index, item) in items.iter().enumerate() {
                    if self.always_valid(item)? {
                        continue;
                    }
                    let mut child =
                        context.new_data(format!("{}/prefixItems/{index}", context.path));
                    self.subschema(item, &mut child)?;
                }
            }
            "items" => {
                context.items = Evaluated::All;
                // `false` compiles to a length check (after `prefixItems`) or
                // to an always-failing boolean subschema; neither can throw.
                if !self.always_valid(value)? && value.as_object().is_some() {
                    let mut child = context.new_data(context.keyword_path("items"));
                    self.subschema(value, &mut child)?;
                }
            }
            "contains" => {
                let min = present(object, "minContains")
                    .and_then(JsValue::as_number)
                    .unwrap_or(1.0);
                let max = present(object, "maxContains").and_then(JsValue::as_number);
                if max.is_none() && min == 0.0 {
                    return Err(strict(
                        "\"minContains\" == 0 without \"maxContains\": \"contains\" keyword ignored",
                    ));
                }
                if max.is_some_and(|max| min > max) {
                    return Err(strict(
                        "\"minContains\" > \"maxContains\" is always invalid",
                    ));
                }
                if !self.always_valid(value)? {
                    context.items = Evaluated::All;
                    let mut child = context.new_data(context.keyword_path("contains"));
                    self.subschema(value, &mut child)?;
                }
            }
            "maxContains" | "minContains" => {
                if present(object, "contains").is_none() {
                    return Err(strict(format!(
                        "\"{keyword}\" without \"contains\" is ignored"
                    )));
                }
            }
            "unevaluatedItems" => {
                if context.items == Evaluated::Dynamic {
                    self.note_dynamic_evaluation(keyword, context);
                }
                if context.items != Evaluated::All {
                    if value.as_object().is_some() && !self.always_valid(value)? {
                        let mut child = context.new_data(context.keyword_path("unevaluatedItems"));
                        self.subschema(value, &mut child)?;
                    }
                    context.items = Evaluated::All;
                }
            }
            "required" => {
                let required = array(value);
                let properties = present(object, "properties").and_then(JsValue::as_object);
                for key in required {
                    let Some(key) = key.as_string() else {
                        continue;
                    };
                    let declared = properties.is_some_and(|properties| {
                        properties.get_key(key).is_some() || builtin_property(key)
                    });
                    if !declared && !context.defined.borrow().contains(&key_text(key)) {
                        return Err(strict(format!(
                            "required property \"{}\" is not defined at \"{}\" (strictRequired)",
                            key_text(key),
                            context.path
                        )));
                    }
                }
            }
            "propertyNames" => {
                if !self.always_valid(value)? {
                    let mut child = context.new_data(context.keyword_path("propertyNames"));
                    child.data_types = vec!["string"];
                    self.subschema(value, &mut child)?;
                }
            }
            "additionalProperties" => {
                context.props = Evaluated::All;
                if !self.always_valid(value)? {
                    for pattern in schema_properties(present(object, "patternProperties")) {
                        self.compile_pattern(&key_text(pattern), context)?;
                    }
                    if value.as_object().is_some() {
                        let mut child =
                            context.new_data(context.keyword_path("additionalProperties"));
                        self.subschema(value, &mut child)?;
                    }
                }
            }
            "dependencies" => {
                if let JsValue::Object(map) = value {
                    for (name, dependency) in map.own_entries() {
                        if name.equals_str("__proto__")
                            || matches!(dependency, JsValue::Array(_))
                            || self.always_valid(dependency)?
                        {
                            continue;
                        }
                        let path = self.child_path(context, "dependencies", name)?;
                        let mut child = context.same_data(path);
                        self.subschema(dependency, &mut child)?;
                        context.merge_valid(&child);
                    }
                }
            }
            "properties" => self.properties(value, context)?,
            "patternProperties" => self.pattern_properties(object, value, context)?,
            "dependentSchemas" => {
                if let JsValue::Object(map) = value {
                    for (name, dependency) in map.own_entries() {
                        if self.always_valid(dependency)? {
                            continue;
                        }
                        let path = self.child_path(context, "dependentSchemas", name)?;
                        let mut child = context.same_data(path);
                        self.subschema(dependency, &mut child)?;
                        context.merge_valid(&child);
                    }
                }
            }
            "unevaluatedProperties" => {
                if context.props == Evaluated::Dynamic {
                    self.note_dynamic_evaluation(keyword, context);
                }
                if context.props != Evaluated::All
                    && !matches!(value, JsValue::Bool(false))
                    && !self.always_valid(value)?
                {
                    let mut child = context.new_data(context.keyword_path("unevaluatedProperties"));
                    self.subschema(value, &mut child)?;
                }
                context.props = Evaluated::All;
            }
            _ => {}
        }
        Ok(())
    }

    fn child_path(
        &self,
        context: &Context,
        keyword: &str,
        name: &JsString,
    ) -> Result<String, String> {
        let escaped = escape_fragment(name).ok_or_else(|| "URI malformed".to_owned())?;
        Ok(format!("{}/{keyword}/{escaped}", context.path))
    }

    fn composite(
        &mut self,
        keyword: &str,
        value: &JsValue,
        context: &mut Context,
    ) -> Result<(), String> {
        let branches = array(value);
        if keyword == "anyOf" {
            // `schema.some(alwaysValidSchema)` stops at the first match.
            for branch in branches {
                if self.always_valid(branch)? {
                    break;
                }
            }
        }
        for (index, branch) in branches.iter().enumerate() {
            if keyword != "anyOf" && self.always_valid(branch)? {
                continue;
            }
            let mut child = context.same_data(format!("{}/{keyword}/{index}", context.path));
            self.subschema(branch, &mut child)?;
            match keyword {
                "anyOf" => context.merge_valid(&child),
                "oneOf" => context.merge(&child, true),
                _ => context.merge(&child, false),
            }
        }
        Ok(())
    }

    fn if_keyword(
        &mut self,
        object: &JsObject,
        value: &JsValue,
        context: &mut Context,
    ) -> Result<(), String> {
        let then_schema = present(object, "then");
        let else_schema = present(object, "else");
        if then_schema.is_none() && else_schema.is_none() {
            return Err(strict("\"if\" without \"then\" and \"else\" is ignored"));
        }
        let has_then = match then_schema {
            Some(schema) => !self.always_valid(schema)?,
            None => false,
        };
        let has_else = match else_schema {
            Some(schema) => !self.always_valid(schema)?,
            None => false,
        };
        if !has_then && !has_else {
            return Ok(());
        }
        let mut child = context.same_data(context.keyword_path("if"));
        self.subschema(value, &mut child)?;
        context.merge(&child, false);
        for (keyword, schema, has) in [
            ("then", then_schema, has_then),
            ("else", else_schema, has_else),
        ] {
            if let (true, Some(schema)) = (has, schema) {
                let mut child = context.same_data(context.keyword_path(keyword));
                self.subschema(schema, &mut child)?;
                context.merge_valid(&child);
            }
        }
        Ok(())
    }

    fn properties(&mut self, value: &JsValue, context: &mut Context) -> Result<(), String> {
        let JsValue::Object(map) = value else {
            return Ok(());
        };
        let names = schema_properties(Some(value));
        for name in &names {
            context.defined.borrow_mut().insert(key_text(name));
        }
        if !names.is_empty() && context.props != Evaluated::All {
            context.props = merge_evaluated(Evaluated::Static, context.props, false);
        }
        let mut checked = Vec::new();
        for name in names {
            let schema = map.get_key(name).expect("own property");
            if !self.always_valid(schema)? {
                checked.push((name, schema));
            }
        }
        for (name, schema) in checked {
            let path = self.child_path(context, "properties", name)?;
            let mut child = context.new_data(path);
            self.subschema(schema, &mut child)?;
            context.defined.borrow_mut().insert(key_text(name));
        }
        Ok(())
    }

    fn pattern_properties(
        &mut self,
        object: &JsObject,
        value: &JsValue,
        context: &mut Context,
    ) -> Result<(), String> {
        let JsValue::Object(map) = value else {
            return Ok(());
        };
        let patterns = schema_properties(Some(value));
        let mut always_valid = Vec::new();
        for pattern in &patterns {
            always_valid.push(self.always_valid(map.get_key(pattern).expect("own property"))?);
        }
        if patterns.is_empty()
            || (always_valid.iter().all(|valid| *valid) && context.props == Evaluated::All)
        {
            return Ok(());
        }
        let declared = present(object, "properties").and_then(JsValue::as_object);
        if context.props != Evaluated::All {
            context.props = Evaluated::Dynamic;
        }
        for (pattern, always_valid) in patterns.into_iter().zip(always_valid) {
            let source = key_text(pattern);
            if let Some(declared) = declared {
                if let Some(detail) = js_regex_non_unicode_error(&source) {
                    return Err(format!("Invalid regular expression: /{source}/: {detail}"));
                }
                match js_regex(&source) {
                    JsRegex::Compiled(regex) => {
                        for (name, _) in declared.own_entries() {
                            let name = key_text(name);
                            if regex.is_match(&name) {
                                return Err(strict(format!(
                                    "property {name} matches pattern {source} (use allowMatchingProperties)"
                                )));
                            }
                        }
                    }
                    _ => self.note_unsupported(format!(
                        "patternProperties pattern {source:?} at {}",
                        context.path
                    )),
                }
            }
            self.compile_pattern(&source, context)?;
            if !always_valid {
                let path = self.child_path(context, "patternProperties", pattern)?;
                let mut child = context.new_data(path);
                self.subschema(map.get_key(pattern).expect("own property"), &mut child)?;
            }
        }
        Ok(())
    }

    /// `usePattern`: `new RegExp(pattern, "u")`.
    fn compile_pattern(&mut self, source: &str, context: &Context) -> Result<(), String> {
        match js_regex(source) {
            JsRegex::Invalid(detail) => {
                Err(format!("Invalid regular expression: /{source}/u: {detail}"))
            }
            JsRegex::JsOnly => {
                self.note_unsupported(format!("pattern {source:?} at {}", context.path));
                Ok(())
            }
            JsRegex::Compiled(_) => Ok(()),
        }
    }

    fn use_pattern(&mut self, value: &JsValue, context: &Context) -> Result<(), String> {
        match value.as_string() {
            Some(source) => self.compile_pattern(&key_text(source), context),
            None => Ok(()),
        }
    }

    fn format(&mut self, value: &JsValue, context: &Context) -> Result<(), String> {
        let Some(name) = value.as_string() else {
            return Ok(());
        };
        if builtin_property(name) {
            self.note_unsupported(format!("format {:?} at {}", key_text(name), context.path));
            return Ok(());
        }
        if !FORMATS.iter().any(|(known, _)| name.equals_str(known)) {
            return Err(format!(
                "unknown format \"{}\" ignored in schema at path \"{}\"",
                key_text(name),
                context.path
            ));
        }
        Ok(())
    }

    fn reference(&mut self, value: &JsValue, context: &mut Context) -> Result<(), String> {
        let Some(reference) = value.as_string() else {
            return Ok(());
        };
        let reference = key_text(reference);
        match resolve(self.root, self.anchors, &reference) {
            Resolved::Root => {
                // The root validator is still compiling: its evaluated
                // state is read at run time.
                for to in [&mut context.props, &mut context.items] {
                    if *to != Evaluated::All {
                        *to = Evaluated::Dynamic;
                    }
                }
            }
            Resolved::Missing => {
                return Err(format!("can't resolve reference {reference} from id #"));
            }
            Resolved::Unsupported => {
                self.note_unsupported(format!("$ref {reference:?} at {}", context.path));
            }
            Resolved::Target(target) if !has_ref(target) => {
                let mut child = Context {
                    path: reference,
                    data_types: Vec::new(),
                    defined: context.defined.clone(),
                    props: Evaluated::Undefined,
                    items: Evaluated::Undefined,
                };
                self.subschema(target, &mut child)?;
                context.merge(&child, false);
            }
            Resolved::Target(target) => {
                let key = target as *const JsValue as usize;
                if !self.compiled.contains_key(&key) {
                    self.compiled.insert(key, None);
                    let mut root = Context::root("#");
                    self.subschema(target, &mut root)?;
                    self.compiled.insert(key, Some((root.props, root.items)));
                }
                match self.compiled[&key] {
                    Some((props, items)) => {
                        for (from, to) in [(props, &mut context.props), (items, &mut context.items)]
                        {
                            if *to == Evaluated::All {
                                continue;
                            }
                            *to = match from {
                                Evaluated::Undefined => *to,
                                Evaluated::Dynamic => merge_evaluated(from, *to, true),
                                _ => merge_evaluated(from, *to, false),
                            };
                        }
                    }
                    None => {
                        for to in [&mut context.props, &mut context.items] {
                            if *to != Evaluated::All {
                                *to = Evaluated::Dynamic;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn array(value: &JsValue) -> &[JsValue] {
    match value {
        JsValue::Array(items) => items,
        _ => &[],
    }
}

fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::String(text) => !text.is_empty(),
        JsValue::Bool(value) => *value,
        JsValue::Number(number) => *number != 0.0 && !number.is_nan(),
        JsValue::Null | JsValue::Undefined => false,
        _ => true,
    }
}

/// `includesType`.
fn includes_type(types: &[&'static str], kind: &str) -> bool {
    types.contains(&kind) || (kind == "integer" && types.contains(&"number"))
}

// ---------------------------------------------------------------------------
// Validation (the compiled validator's boolean result)
// ---------------------------------------------------------------------------

/// Evaluated properties and items, as Ajv tracks them for `unevaluated*`.
#[derive(Default)]
struct Annotations {
    all_props: bool,
    props: HashSet<Vec<u16>>,
    all_items: bool,
    items: usize,
}

impl Annotations {
    fn merge(&mut self, other: Annotations) {
        self.all_props |= other.all_props;
        self.props.extend(other.props);
        self.all_items |= other.all_items;
        self.items = self.items.max(other.items);
    }
}

/// Whether the form answer passes the compiled Draft 2020-12 validator. A
/// schema that does not compile answers `false`, like the baseline's
/// fail-closed `catch`.
pub fn accepts(schema: &JsValue, value: &JsValue) -> bool {
    let Ok(anchors) = compile_with_anchors(schema) else {
        return false;
    };
    let evaluator = Evaluator {
        root: schema,
        anchors: &anchors,
    };
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(scope, || evaluator.eval(schema, value, 0).0)
            .map(|handle| handle.join().unwrap_or(false))
            .unwrap_or(false)
    })
}

struct Evaluator<'a, 'b> {
    root: &'a JsValue,
    anchors: &'b Anchors<'a>,
}

fn always_valid(schema: &JsValue) -> bool {
    match schema {
        JsValue::Bool(valid) => *valid,
        JsValue::Object(object) => object.is_empty() || !has_rules(object),
        _ => true,
    }
}

fn type_matches(kind: &str, data: &JsValue) -> bool {
    match kind {
        "null" => matches!(data, JsValue::Null),
        "boolean" => matches!(data, JsValue::Bool(_)),
        "number" => data.as_number().is_some_and(f64::is_finite),
        "integer" => data
            .as_number()
            .is_some_and(|n| n.is_finite() && n.fract() == 0.0),
        "string" => matches!(data, JsValue::String(_)),
        "array" => matches!(data, JsValue::Array(_)),
        "object" => matches!(data, JsValue::Object(_)),
        _ => false,
    }
}

/// `fast-deep-equal` on JSON values.
fn deep_equal(left: &JsValue, right: &JsValue) -> bool {
    match (left, right) {
        (JsValue::Array(a), JsValue::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| deep_equal(a, b))
        }
        (JsValue::Object(a), JsValue::Object(b)) => {
            a.len() == b.len()
                && a.entries().iter().all(|(key, value)| {
                    b.get_key(key).is_some_and(|other| deep_equal(value, other))
                })
        }
        (JsValue::Number(a), JsValue::Number(b)) => a == b,
        (a, b) => a == b,
    }
}

/// `data[name] !== undefined` on a JSON object.
fn has_property(data: &JsObject, name: &JsString) -> bool {
    data.get_key(name).is_some() || builtin_property(name)
}

/// Ajv's `multipleOf`: `data / divisor !== parseInt(data / divisor)`.
fn multiple_of(value: f64, divisor: f64) -> bool {
    let quotient = value / divisor;
    quotient.is_finite() && quotient.abs() < 1e21 && quotient.fract() == 0.0
}

fn regex_test(source: &JsString, text: &JsString) -> bool {
    let (Ok(source), Ok(text)) = (source.to_utf8(), text.to_utf8()) else {
        return false;
    };
    match js_regex(&source) {
        JsRegex::Compiled(regex) => regex.is_match(&text),
        _ => false,
    }
}

impl Evaluator<'_, '_> {
    fn eval(&self, schema: &JsValue, data: &JsValue, depth: usize) -> (bool, Annotations) {
        if depth > MAX_EVAL_DEPTH {
            return (false, Annotations::default());
        }
        let object = match schema {
            JsValue::Bool(valid) => return (*valid, Annotations::default()),
            JsValue::Object(object) if has_rules(object) => object,
            _ => return (true, Annotations::default()),
        };
        let mut ann = Annotations::default();
        if present(object, "$ref").is_some_and(truthy) && !has_rules_but_ref(object) {
            let valid = self.reference(object, data, depth, &mut ann);
            return (valid, ann);
        }
        let mut valid = true;
        // untyped group
        if present(object, "$ref").is_some() {
            valid &= self.reference(object, data, depth, &mut ann);
        }
        let types = schema_types(object).unwrap_or_default();
        if !types.is_empty() && !types.iter().any(|kind| type_matches(kind, data)) {
            valid = false;
        }
        if let Some(expected) = present(object, "const") {
            valid &= deep_equal(expected, data);
        }
        if let Some(JsValue::Array(options)) = present(object, "enum") {
            valid &= options.iter().any(|option| deep_equal(option, data));
        }
        if let Some(schema) = present(object, "not") {
            valid &= !self.eval(schema, data, depth + 1).0;
        }
        if let Some(JsValue::Array(branches)) = present(object, "anyOf") {
            let mut any = false;
            for branch in branches {
                let (ok, branch_ann) = self.eval(branch, data, depth + 1);
                if ok {
                    any = true;
                    ann.merge(branch_ann);
                }
            }
            valid &= any;
        }
        if let Some(JsValue::Array(branches)) = present(object, "oneOf") {
            let mut passing = 0;
            for branch in branches {
                let (ok, branch_ann) = self.eval(branch, data, depth + 1);
                if ok {
                    passing += 1;
                    ann.merge(branch_ann);
                }
            }
            valid &= passing == 1;
        }
        if let Some(JsValue::Array(branches)) = present(object, "allOf") {
            for branch in branches {
                let (ok, branch_ann) = self.eval(branch, data, depth + 1);
                valid &= ok;
                ann.merge(branch_ann);
            }
        }
        if let Some(condition) = present(object, "if") {
            let then_schema = present(object, "then").filter(|s| !always_valid(s));
            let else_schema = present(object, "else").filter(|s| !always_valid(s));
            if then_schema.is_some() || else_schema.is_some() {
                let (matched, if_ann) = self.eval(condition, data, depth + 1);
                // Ajv merges the `if` subschema's evaluated state whether or
                // not it matched.
                ann.merge(if_ann);
                let branch = if matched { then_schema } else { else_schema };
                if let Some(branch) = branch {
                    let (ok, branch_ann) = self.eval(branch, data, depth + 1);
                    valid &= ok;
                    if ok {
                        ann.merge(branch_ann);
                    }
                }
            }
        }
        match data {
            JsValue::Number(number) => valid &= self.number_rules(object, *number),
            JsValue::String(text) => valid &= self.string_rules(object, text),
            JsValue::Array(items) => valid &= self.array_rules(object, items, depth, &mut ann),
            JsValue::Object(map) => valid &= self.object_rules(object, map, data, depth, &mut ann),
            _ => {}
        }
        (valid, ann)
    }

    fn reference(
        &self,
        object: &JsObject,
        data: &JsValue,
        depth: usize,
        ann: &mut Annotations,
    ) -> bool {
        let Some(reference) = present(object, "$ref").and_then(JsValue::as_string) else {
            return true;
        };
        let target = match resolve(self.root, self.anchors, &key_text(reference)) {
            Resolved::Root => self.root,
            Resolved::Target(target) => target,
            Resolved::Missing | Resolved::Unsupported => return false,
        };
        let (ok, target_ann) = self.eval(target, data, depth + 1);
        ann.merge(target_ann);
        ok
    }

    fn number_rules(&self, object: &JsObject, value: f64) -> bool {
        let limit = |keyword: &str| present(object, keyword).and_then(JsValue::as_number);
        if limit("maximum").is_some_and(|max| value > max)
            || limit("minimum").is_some_and(|min| value < min)
            || limit("exclusiveMaximum").is_some_and(|max| value >= max)
            || limit("exclusiveMinimum").is_some_and(|min| value <= min)
            || limit("multipleOf").is_some_and(|divisor| !multiple_of(value, divisor))
        {
            return false;
        }
        if let Some(format) = present(object, "format").and_then(JsValue::as_string) {
            let format = key_text(format);
            if FORMATS
                .iter()
                .any(|(name, numeric)| *numeric && *name == format)
                && number_format_valid(&format, value) == Some(false)
            {
                return false;
            }
        }
        true
    }

    fn string_rules(&self, object: &JsObject, text: &JsString) -> bool {
        let length = code_points(text) as f64;
        let limit = |keyword: &str| present(object, keyword).and_then(JsValue::as_number);
        if limit("maxLength").is_some_and(|max| length > max)
            || limit("minLength").is_some_and(|min| length < min)
        {
            return false;
        }
        if let Some(pattern) = present(object, "pattern").and_then(JsValue::as_string)
            && !regex_test(pattern, text)
        {
            return false;
        }
        if let Some(format) = present(object, "format").and_then(JsValue::as_string) {
            let format = key_text(format);
            if FORMATS
                .iter()
                .any(|(name, numeric)| !*numeric && *name == format)
            {
                let Ok(text) = text.to_utf8() else {
                    return false;
                };
                if string_format_valid(&format, &text) == Some(false) {
                    return false;
                }
            }
        }
        true
    }

    fn array_rules(
        &self,
        object: &JsObject,
        items: &[JsValue],
        depth: usize,
        ann: &mut Annotations,
    ) -> bool {
        let mut valid = true;
        let length = items.len() as f64;
        let limit = |keyword: &str| present(object, keyword).and_then(JsValue::as_number);
        if limit("maxItems").is_some_and(|max| length > max)
            || limit("minItems").is_some_and(|min| length < min)
        {
            valid = false;
        }
        let prefix = match present(object, "prefixItems") {
            Some(JsValue::Array(prefix)) => prefix.as_slice(),
            _ => &[],
        };
        for (item, schema) in items.iter().zip(prefix) {
            valid &= self.eval(schema, item, depth + 1).0;
        }
        if !prefix.is_empty() {
            ann.items = ann.items.max(prefix.len());
        }
        if let Some(schema) = present(object, "items") {
            ann.all_items = true;
            for item in items.iter().skip(prefix.len()) {
                valid &= self.eval(schema, item, depth + 1).0;
            }
        }
        if let Some(schema) = present(object, "contains") {
            let min = limit("minContains").unwrap_or(1.0);
            let max = limit("maxContains");
            if always_valid(schema) {
                valid &= length >= min && max.is_none_or(|max| length <= max);
            } else {
                ann.all_items = true;
                let matches = items
                    .iter()
                    .filter(|item| self.eval(schema, item, depth + 1).0)
                    .count() as f64;
                valid &= matches >= min && max.is_none_or(|max| matches <= max);
            }
        }
        if matches!(present(object, "uniqueItems"), Some(JsValue::Bool(true))) {
            for (index, item) in items.iter().enumerate() {
                if items[..index].iter().any(|prior| deep_equal(prior, item)) {
                    valid = false;
                    break;
                }
            }
        }
        if let Some(schema) = present(object, "unevaluatedItems")
            && !ann.all_items
        {
            for item in items.iter().skip(ann.items) {
                valid &= self.eval(schema, item, depth + 1).0;
            }
            ann.all_items = true;
        }
        valid
    }

    fn object_rules(
        &self,
        object: &JsObject,
        map: &JsObject,
        data: &JsValue,
        depth: usize,
        ann: &mut Annotations,
    ) -> bool {
        let mut valid = true;
        let keys = map.own_entries();
        let count = keys.len() as f64;
        let limit = |keyword: &str| present(object, keyword).and_then(JsValue::as_number);
        if limit("maxProperties").is_some_and(|max| count > max)
            || limit("minProperties").is_some_and(|min| count < min)
        {
            valid = false;
        }
        if let Some(JsValue::Array(required)) = present(object, "required") {
            valid &= required
                .iter()
                .filter_map(JsValue::as_string)
                .all(|name| has_property(map, name));
        }
        if let Some(schema) = present(object, "propertyNames") {
            for (key, _) in &keys {
                valid &= self
                    .eval(schema, &JsValue::String((*key).clone()), depth + 1)
                    .0;
            }
        }
        let declared = schema_properties(present(object, "properties"));
        let patterns = schema_properties(present(object, "patternProperties"));
        if let Some(schema) = present(object, "additionalProperties") {
            ann.all_props = true;
            for (key, value) in &keys {
                let additional = !declared.iter().any(|name| name == key)
                    && !patterns.iter().any(|pattern| regex_test(pattern, key));
                if additional {
                    valid &= self.eval(schema, value, depth + 1).0;
                }
            }
        }
        if let Some(JsValue::Object(dependencies)) = present(object, "dependencies") {
            for (name, dependency) in dependencies.own_entries() {
                if name.equals_str("__proto__") || !has_property(map, name) {
                    continue;
                }
                match dependency {
                    JsValue::Array(names) => {
                        valid &= names
                            .iter()
                            .filter_map(JsValue::as_string)
                            .all(|name| has_property(map, name));
                    }
                    schema if !always_valid(schema) => {
                        let (ok, dep_ann) = self.eval(schema, data, depth + 1);
                        valid &= ok;
                        if ok {
                            ann.merge(dep_ann);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(JsValue::Object(properties)) = present(object, "properties") {
            for name in &declared {
                ann.props.insert(name.units().to_vec());
                let schema = properties.get_key(name).expect("own property");
                if let Some(value) = map.get_key(name) {
                    valid &= self.eval(schema, value, depth + 1).0;
                }
            }
        }
        if let Some(JsValue::Object(pattern_map)) = present(object, "patternProperties") {
            let all_always_valid = patterns
                .iter()
                .all(|pattern| always_valid(pattern_map.get_key(pattern).expect("own property")));
            if !patterns.is_empty() && !(all_always_valid && ann.all_props) {
                for pattern in &patterns {
                    let schema = pattern_map.get_key(pattern).expect("own property");
                    for (key, value) in &keys {
                        if regex_test(pattern, key) {
                            ann.props.insert(key.units().to_vec());
                            valid &= self.eval(schema, value, depth + 1).0;
                        }
                    }
                }
            }
        }
        if let Some(JsValue::Object(dependent)) = present(object, "dependentRequired") {
            for (name, names) in dependent.own_entries() {
                if !has_property(map, name) {
                    continue;
                }
                if let JsValue::Array(names) = names {
                    valid &= names
                        .iter()
                        .filter_map(JsValue::as_string)
                        .all(|name| has_property(map, name));
                }
            }
        }
        if let Some(JsValue::Object(dependent)) = present(object, "dependentSchemas") {
            for (name, schema) in dependent.own_entries() {
                if always_valid(schema) || !has_property(map, name) {
                    continue;
                }
                let (ok, dep_ann) = self.eval(schema, data, depth + 1);
                valid &= ok;
                if ok {
                    ann.merge(dep_ann);
                }
            }
        }
        if let Some(schema) = present(object, "unevaluatedProperties")
            && !ann.all_props
        {
            for (key, value) in &keys {
                if !ann.props.contains(key.units()) {
                    valid &= self.eval(schema, value, depth + 1).0;
                }
            }
            ann.all_props = true;
        }
        valid
    }
}

#[cfg(test)]
mod tests {
    use super::super::form_dialog::{accepts_form_dialog_answer, validate_form_dialog_schema};
    use super::*;

    /// `generate.mjs` run against the fixed baseline.
    const ORACLE: &str = include_str!("../../../../tests/fixtures/form-dialog-2020/oracle.json");

    /// Schemas the baseline accepts that this host refuses explicitly.
    const UNSUPPORTED: &[&str] = &[
        "format-limit",
        "dynamic-ref",
        "root-id",
        "nested-id",
        "unevaluated-anyof",
        "unevaluated-if",
        "unevaluated-pattern",
    ];

    #[test]
    fn schemas_and_answers_match_the_baseline_oracle() {
        let cases = parse_json(ORACLE).expect("oracle");
        let mut failures = Vec::new();
        for case in array(&cases) {
            let name = key_text(case.get("name").and_then(JsValue::as_string).unwrap());
            let schema = case.get("schema").unwrap();
            let actual = validate_form_dialog_schema(schema);
            if UNSUPPORTED.contains(&name.as_str()) {
                match &actual {
                    Err(message) if message.contains("is not supported by this gateway") => {}
                    other => failures.push(format!(
                        "{name}: expected an explicit refusal, got {other:?}"
                    )),
                }
                continue;
            }
            let expected = case
                .get("message")
                .and_then(JsValue::as_string)
                .map(key_text);
            if actual.clone().err() != expected {
                failures.push(format!(
                    "{name}: schema\n  baseline {expected:?}\n  rust     {actual:?}"
                ));
                continue;
            }
            for answer in array(case.get("answers").unwrap()) {
                let value = answer.get("value").unwrap();
                let accepted = answer.get("accepted").and_then(JsValue::as_bool).unwrap();
                if accepts_form_dialog_answer(schema, value) != accepted {
                    failures.push(format!(
                        "{name}: answer {} baseline accepted={accepted}",
                        pilotdeck_protocol::stringify_json(value)
                            .ok()
                            .flatten()
                            .unwrap_or_default()
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
