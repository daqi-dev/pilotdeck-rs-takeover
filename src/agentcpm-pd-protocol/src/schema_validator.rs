//! Pure safe JSON Schema Draft 2020-12 subset validator covering all keywords used
//! across the 285 embedded `agentcpm-pd` operation schemas and `public-contract`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_REF_DEPTH: usize = 64;
const MAX_VALIDATION_STEPS: usize = 250_000;
const MAX_VALIDATION_ERRORS: usize = 64;
const MAX_JSON_EQ_DEPTH: usize = 256;
const MAX_REGEX_CACHE_ENTRIES: usize = 1024;
const MAX_REGEX_SIZE_BYTES: usize = 1 << 20;
const MAX_REGEX_NEST_LIMIT: u32 = 64;

/// A single schema validation failure at a JSON pointer-style path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationError {
    pub path: String,
    pub message: String,
}

impl ValidationError {
    fn new(path: &str, message: impl Into<String>) -> Self {
        Self {
            path: if path.is_empty() {
                "$".to_owned()
            } else {
                path.to_owned()
            },
            message: message.into(),
        }
    }
}

/// Validates `value` against `schema` using `schema["$defs"]` as root definitions.
pub fn validate_against_schema(value: &Value, schema: &Value) -> Result<(), Vec<ValidationError>> {
    let empty = Map::new();
    let root_defs = schema
        .as_object()
        .and_then(|obj| obj.get("$defs"))
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    validate_against_schema_with_defs(value, schema, root_defs)
}

struct ValidationCtx {
    steps: usize,
    probe_mode: bool,
    abort: Option<ValidationError>,
}

impl ValidationCtx {
    fn new() -> Self {
        Self {
            steps: 0,
            probe_mode: false,
            abort: None,
        }
    }

    fn push_error(&self, errors: &mut Vec<ValidationError>, err: ValidationError) {
        if self.probe_mode {
            if errors.is_empty() {
                errors.push(err);
            }
            return;
        }
        if errors.len() < MAX_VALIDATION_ERRORS {
            errors.push(err);
        }
    }

    fn should_stop(&self, errors: &[ValidationError]) -> bool {
        self.abort.is_some()
            || (self.probe_mode && !errors.is_empty())
            || (!self.probe_mode && errors.len() >= MAX_VALIDATION_ERRORS)
    }
}

/// Validates `value` against `schema` using an explicit root `$defs` map (e.g. `public-contract.schema.$defs`).
pub fn validate_against_schema_with_defs(
    value: &Value,
    schema: &Value,
    root_defs: &Map<String, Value>,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    let mut ctx = ValidationCtx::new();
    validate_node(value, schema, root_defs, "$", 0, &mut ctx, &mut errors);
    if let Some(abort) = ctx.abort {
        if errors.len() < MAX_VALIDATION_ERRORS {
            errors.push(abort);
        } else if let Some(last) = errors.last_mut() {
            *last = abort;
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn compiled_regex(pattern: &str) -> Result<Regex, String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(re) = guard.get(pattern)
    {
        return Ok(re.clone());
    }
    let re = RegexBuilder::new(pattern)
        .size_limit(MAX_REGEX_SIZE_BYTES)
        .dfa_size_limit(MAX_REGEX_SIZE_BYTES)
        .nest_limit(MAX_REGEX_NEST_LIMIT)
        .build()
        .map_err(|e| format!("invalid schema pattern `{pattern}`: {e}"))?;
    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= MAX_REGEX_CACHE_ENTRIES && !guard.contains_key(pattern) {
            guard.clear();
        }
        guard.insert(pattern.to_owned(), re.clone());
    }
    Ok(re)
}

fn resolve_ref<'a>(
    reference: &str,
    local_schema: &'a Map<String, Value>,
    root_defs: &'a Map<String, Value>,
) -> Option<&'a Value> {
    let name = reference.strip_prefix("#/$defs/")?;
    root_defs.get(name).or_else(|| {
        local_schema
            .get("$defs")
            .and_then(Value::as_object)
            .and_then(|m| m.get(name))
    })
}

fn validate_node(
    value: &Value,
    schema: &Value,
    root_defs: &Map<String, Value>,
    path: &str,
    depth: usize,
    ctx: &mut ValidationCtx,
    errors: &mut Vec<ValidationError>,
) {
    if ctx.should_stop(errors) {
        return;
    }
    ctx.steps = ctx.steps.saturating_add(1);
    if ctx.steps > MAX_VALIDATION_STEPS {
        ctx.abort = Some(ValidationError::new(
            path,
            "schema validation exceeded maximum step budget",
        ));
        return;
    }
    if depth > MAX_REF_DEPTH {
        ctx.abort = Some(ValidationError::new(
            path,
            "schema $ref nesting exceeded maximum depth",
        ));
        return;
    }

    match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            ctx.push_error(
                errors,
                ValidationError::new(path, "value disallowed by false schema"),
            );
            return;
        }
        Value::Object(_) => {}
        _ => {
            ctx.push_error(
                errors,
                ValidationError::new(path, "invalid non-object schema node"),
            );
            return;
        }
    }

    let obj = schema.as_object().expect("checked Object above");

    // In Draft 2020-12, if this subschema declares its own `$defs`, prefer lookup in root_defs
    // and fall back to local `$defs`. Merge only when overridden so recursion preserves root_defs.
    let merged_defs_owned: Option<Map<String, Value>> = obj
        .get("$defs")
        .and_then(Value::as_object)
        .map(|local_defs| {
            let mut merged = root_defs.clone();
            for (k, v) in local_defs {
                merged.entry(k.clone()).or_insert_with(|| v.clone());
            }
            merged
        });
    let active_defs = merged_defs_owned.as_ref().unwrap_or(root_defs);

    if let Some(reference) = obj.get("$ref").and_then(Value::as_str) {
        if let Some(target) = resolve_ref(reference, obj, active_defs) {
            validate_node(value, target, active_defs, path, depth + 1, ctx, errors);
        } else {
            ctx.push_error(
                errors,
                ValidationError::new(path, format!("unresolved $ref `{reference}`")),
            );
        }
        if ctx.should_stop(errors) {
            return;
        }
    }

    if let Some(const_val) = obj.get("const")
        && !json_eq(value, const_val)
    {
        ctx.push_error(
            errors,
            ValidationError::new(path, format!("must equal constant {const_val}")),
        );
        if ctx.should_stop(errors) {
            return;
        }
    }

    if let Some(enum_vals) = obj.get("enum").and_then(Value::as_array)
        && !enum_vals.iter().any(|candidate| json_eq(value, candidate))
    {
        ctx.push_error(
            errors,
            ValidationError::new(path, "must match one of enum values"),
        );
        if ctx.should_stop(errors) {
            return;
        }
    }

    if let Some(type_val) = obj.get("type")
        && !matches_type(value, type_val)
    {
        ctx.push_error(
            errors,
            ValidationError::new(path, format!("expected JSON type {type_val}")),
        );
        if ctx.should_stop(errors) {
            return;
        }
    }

    if let Some(all_of) = obj.get("allOf").and_then(Value::as_array) {
        for sub in all_of {
            validate_node(value, sub, active_defs, path, depth + 1, ctx, errors);
            if ctx.should_stop(errors) {
                return;
            }
        }
    }

    if let Some(any_of) = obj.get("anyOf").and_then(Value::as_array) {
        let mut matched = false;
        let saved_probe = ctx.probe_mode;
        ctx.probe_mode = true;
        for sub in any_of {
            let mut probe = Vec::new();
            validate_node(value, sub, active_defs, path, depth + 1, ctx, &mut probe);
            if ctx.abort.is_some() {
                ctx.probe_mode = saved_probe;
                return;
            }
            if probe.is_empty() {
                matched = true;
                break;
            }
        }
        ctx.probe_mode = saved_probe;
        if !matched {
            ctx.push_error(
                errors,
                ValidationError::new(path, "must match at least one branch of anyOf"),
            );
            if ctx.should_stop(errors) {
                return;
            }
        }
    }

    if let Some(one_of) = obj.get("oneOf").and_then(Value::as_array) {
        let mut matches = 0usize;
        let saved_probe = ctx.probe_mode;
        ctx.probe_mode = true;
        for sub in one_of {
            let mut probe = Vec::new();
            validate_node(value, sub, active_defs, path, depth + 1, ctx, &mut probe);
            if ctx.abort.is_some() {
                ctx.probe_mode = saved_probe;
                return;
            }
            if probe.is_empty() {
                matches += 1;
                if matches > 1 {
                    break;
                }
            }
        }
        ctx.probe_mode = saved_probe;
        if matches != 1 {
            ctx.push_error(
                errors,
                ValidationError::new(
                    path,
                    format!("must match exactly one branch of oneOf (matched {matches})"),
                ),
            );
            if ctx.should_stop(errors) {
                return;
            }
        }
    }

    if let Some(not_schema) = obj.get("not") {
        let mut probe = Vec::new();
        let saved_probe = ctx.probe_mode;
        ctx.probe_mode = true;
        validate_node(
            value,
            not_schema,
            active_defs,
            path,
            depth + 1,
            ctx,
            &mut probe,
        );
        ctx.probe_mode = saved_probe;
        if ctx.abort.is_some() {
            return;
        }
        if probe.is_empty() {
            ctx.push_error(
                errors,
                ValidationError::new(path, "must not match `not` subschema"),
            );
            if ctx.should_stop(errors) {
                return;
            }
        }
    }

    if let Some(if_schema) = obj.get("if") {
        let mut probe = Vec::new();
        let saved_probe = ctx.probe_mode;
        ctx.probe_mode = true;
        validate_node(
            value,
            if_schema,
            active_defs,
            path,
            depth + 1,
            ctx,
            &mut probe,
        );
        ctx.probe_mode = saved_probe;
        if ctx.abort.is_some() {
            return;
        }
        if probe.is_empty() {
            if let Some(then_schema) = obj.get("then") {
                validate_node(
                    value,
                    then_schema,
                    active_defs,
                    path,
                    depth + 1,
                    ctx,
                    errors,
                );
            }
        } else if let Some(else_schema) = obj.get("else") {
            validate_node(
                value,
                else_schema,
                active_defs,
                path,
                depth + 1,
                ctx,
                errors,
            );
        }
        if ctx.should_stop(errors) {
            return;
        }
    }

    match value {
        Value::String(s) => validate_string(s, obj, path, ctx, errors),
        Value::Number(n) => validate_number(n, obj, path, ctx, errors),
        Value::Array(arr) => validate_array(arr, obj, active_defs, path, depth, ctx, errors),
        Value::Object(map) => validate_object(map, obj, active_defs, path, depth, ctx, errors),
        _ => {}
    }
}

fn validate_string(
    s: &str,
    obj: &Map<String, Value>,
    path: &str,
    ctx: &ValidationCtx,
    errors: &mut Vec<ValidationError>,
) {
    let char_len = s.chars().count() as u64;
    if let Some(min_len) = obj.get("minLength").and_then(Value::as_u64)
        && char_len < min_len
    {
        ctx.push_error(
            errors,
            ValidationError::new(
                path,
                format!("string length {char_len} is less than minLength {min_len}"),
            ),
        );
    }
    if let Some(max_len) = obj.get("maxLength").and_then(Value::as_u64)
        && char_len > max_len
    {
        ctx.push_error(
            errors,
            ValidationError::new(
                path,
                format!("string length {char_len} exceeds maxLength {max_len}"),
            ),
        );
    }
    if let Some(pattern) = obj.get("pattern").and_then(Value::as_str) {
        match compiled_regex(pattern) {
            Ok(re) => {
                if !re.is_match(s) {
                    ctx.push_error(
                        errors,
                        ValidationError::new(
                            path,
                            format!("string does not match pattern `{pattern}`"),
                        ),
                    );
                }
            }
            Err(err) => ctx.push_error(errors, ValidationError::new(path, err)),
        }
    }
    if let Some(fmt) = obj.get("format").and_then(Value::as_str)
        && fmt == "date-time"
        && !is_valid_rfc3339_date_time(s)
    {
        ctx.push_error(
            errors,
            ValidationError::new(path, "string is not a valid RFC 3339 date-time"),
        );
    }
}

fn validate_number(
    n: &serde_json::Number,
    obj: &Map<String, Value>,
    path: &str,
    ctx: &ValidationCtx,
    errors: &mut Vec<ValidationError>,
) {
    let Some(val) = n.as_f64() else {
        ctx.push_error(
            errors,
            ValidationError::new(path, "number cannot be represented as f64"),
        );
        return;
    };
    if let Some(min) = obj.get("minimum").and_then(Value::as_f64)
        && val < min
    {
        ctx.push_error(
            errors,
            ValidationError::new(path, format!("number {val} is less than minimum {min}")),
        );
    }
    if let Some(max) = obj.get("maximum").and_then(Value::as_f64)
        && val > max
    {
        ctx.push_error(
            errors,
            ValidationError::new(path, format!("number {val} exceeds maximum {max}")),
        );
    }
}

fn validate_array(
    arr: &[Value],
    obj: &Map<String, Value>,
    root_defs: &Map<String, Value>,
    path: &str,
    depth: usize,
    ctx: &mut ValidationCtx,
    errors: &mut Vec<ValidationError>,
) {
    let len = arr.len() as u64;
    if let Some(min_items) = obj.get("minItems").and_then(Value::as_u64)
        && len < min_items
    {
        ctx.push_error(
            errors,
            ValidationError::new(
                path,
                format!("array length {len} is less than minItems {min_items}"),
            ),
        );
    }
    if let Some(max_items) = obj.get("maxItems").and_then(Value::as_u64)
        && len > max_items
    {
        ctx.push_error(
            errors,
            ValidationError::new(
                path,
                format!("array length {len} exceeds maxItems {max_items}"),
            ),
        );
    }
    if obj.get("uniqueItems").and_then(Value::as_bool) == Some(true) && arr.len() > 1 {
        let mut seen: HashMap<String, usize> = HashMap::with_capacity(arr.len().min(1024));
        for (idx, item) in arr.iter().enumerate() {
            match canonical_unique_key(item, MAX_JSON_EQ_DEPTH) {
                Ok(key) => {
                    if let Some(prev_idx) = seen.insert(key, idx) {
                        ctx.push_error(
                            errors,
                            ValidationError::new(
                                path,
                                format!(
                                    "array items at indices {prev_idx} and {idx} are not unique"
                                ),
                            ),
                        );
                        break;
                    }
                }
                Err(()) => {
                    ctx.abort = Some(ValidationError::new(
                        path,
                        "array item nesting exceeded maximum depth",
                    ));
                    return;
                }
            }
        }
    }
    if ctx.should_stop(errors) {
        return;
    }
    if let Some(items_schema) = obj.get("items") {
        for (idx, item) in arr.iter().enumerate() {
            let item_path = format!("{path}[{idx}]");
            validate_node(
                item,
                items_schema,
                root_defs,
                &item_path,
                depth + 1,
                ctx,
                errors,
            );
            if ctx.should_stop(errors) {
                return;
            }
        }
    }
}

fn validate_object(
    map: &Map<String, Value>,
    obj: &Map<String, Value>,
    root_defs: &Map<String, Value>,
    path: &str,
    depth: usize,
    ctx: &mut ValidationCtx,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(required) = obj.get("required").and_then(Value::as_array) {
        for req in required {
            if let Some(key) = req.as_str()
                && !map.contains_key(key)
            {
                let child_path = format!("{path}.{key}");
                ctx.push_error(
                    errors,
                    ValidationError::new(&child_path, format!("missing required property `{key}`")),
                );
                if ctx.should_stop(errors) {
                    return;
                }
            }
        }
    }

    let props = obj.get("properties").and_then(Value::as_object);
    let add_props = obj.get("additionalProperties");

    for (key, val) in map {
        let child_path = format!("{path}.{key}");
        if let Some(prop_schema) = props.and_then(|p| p.get(key)) {
            validate_node(
                val,
                prop_schema,
                root_defs,
                &child_path,
                depth + 1,
                ctx,
                errors,
            );
        } else if let Some(additional) = add_props {
            match additional {
                Value::Bool(true) => {}
                Value::Bool(false) => ctx.push_error(
                    errors,
                    ValidationError::new(
                        &child_path,
                        format!(
                            "unknown property `{key}` disallowed by additionalProperties: false"
                        ),
                    ),
                ),
                Value::Object(_) => {
                    validate_node(
                        val,
                        additional,
                        root_defs,
                        &child_path,
                        depth + 1,
                        ctx,
                        errors,
                    );
                }
                _ => {}
            }
        }
        if ctx.should_stop(errors) {
            return;
        }
    }
}

fn matches_type(value: &Value, expected: &Value) -> bool {
    match expected {
        Value::String(t) => matches_single_type(value, t),
        Value::Array(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .any(|t| matches_single_type(value, t)),
        _ => false,
    }
}

fn matches_single_type(value: &Value, expected: &str) -> bool {
    match expected {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "number" => value.is_number(),
        "integer" => match value {
            Value::Number(n) => {
                n.is_i64()
                    || n.is_u64()
                    || n.as_f64()
                        .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
            }
            _ => false,
        },
        _ => false,
    }
}

fn canonical_unique_key(val: &Value, max_depth: usize) -> Result<String, ()> {
    enum Frame<'a> {
        Val(&'a Value, usize),
        Key(String),
        Lit(&'static str),
    }

    let mut out = String::new();
    let mut stack = vec![Frame::Val(val, 0)];
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Lit(s) => out.push_str(s),
            Frame::Key(k) => {
                out.push_str(&k);
                out.push(':');
            }
            Frame::Val(v, depth) => {
                if depth > max_depth {
                    return Err(());
                }
                match v {
                    Value::Null => out.push('n'),
                    Value::Bool(b) => out.push(if *b { 'T' } else { 'F' }),
                    Value::String(s) => {
                        out.push('S');
                        out.push_str(&serde_json::to_string(s).unwrap_or_default());
                    }
                    Value::Number(n) => {
                        out.push('N');
                        if let Some(i) = n.as_i64() {
                            out.push_str(&format!("i:{i}"));
                        } else if let Some(u) = n.as_u64() {
                            if u <= i64::MAX as u64 {
                                out.push_str(&format!("i:{u}"));
                            } else {
                                out.push_str(&format!("u:{u}"));
                            }
                        } else if let Some(f) = n.as_f64() {
                            if f == 0.0 {
                                out.push_str("i:0");
                            } else if f.is_finite()
                                && f.fract() == 0.0
                                && (-9_007_199_254_740_991.0..=9_007_199_254_740_991.0).contains(&f)
                            {
                                out.push_str(&format!("i:{}", f as i64));
                            } else {
                                out.push_str(&format!("f:{:016x}", f.to_bits()));
                            }
                        } else {
                            out.push_str(&n.to_string());
                        }
                    }
                    Value::Array(arr) => {
                        out.push('[');
                        stack.push(Frame::Lit("]"));
                        for (idx, item) in arr.iter().enumerate().rev() {
                            stack.push(Frame::Val(item, depth + 1));
                            if idx > 0 {
                                stack.push(Frame::Lit(","));
                            }
                        }
                    }
                    Value::Object(map) => {
                        out.push('{');
                        stack.push(Frame::Lit("}"));
                        let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                        entries.sort_by_key(|(k, _)| *k);
                        for (idx, (k, child)) in entries.into_iter().enumerate().rev() {
                            stack.push(Frame::Val(child, depth + 1));
                            stack.push(Frame::Key(serde_json::to_string(k).unwrap_or_default()));
                            if idx > 0 {
                                stack.push(Frame::Lit(","));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn json_eq(a: &Value, b: &Value) -> bool {
    let mut stack = vec![(a, b, 0usize)];
    while let Some((lhs, rhs, depth)) = stack.pop() {
        if depth > MAX_JSON_EQ_DEPTH {
            return false;
        }
        match (lhs, rhs) {
            (Value::Null, Value::Null) => {}
            (Value::Bool(x), Value::Bool(y)) if x == y => {}
            (Value::String(x), Value::String(y)) if x == y => {}
            (Value::Number(x), Value::Number(y)) => {
                let eq = if let (Some(xi), Some(yi)) = (x.as_i64(), y.as_i64()) {
                    xi == yi
                } else if let (Some(xu), Some(yu)) = (x.as_u64(), y.as_u64()) {
                    xu == yu
                } else if let (Some(xf), Some(yf)) = (x.as_f64(), y.as_f64()) {
                    xf == yf
                } else {
                    false
                };
                if !eq {
                    return false;
                }
            }
            (Value::Array(xs), Value::Array(ys)) if xs.len() == ys.len() => {
                for (x, y) in xs.iter().zip(ys.iter()) {
                    stack.push((x, y, depth + 1));
                }
            }
            (Value::Object(xm), Value::Object(ym)) if xm.len() == ym.len() => {
                for (k, xv) in xm {
                    let Some(yv) = ym.get(k) else {
                        return false;
                    };
                    stack.push((xv, yv, depth + 1));
                }
            }
            _ => return false,
        }
    }
    true
}

fn parse_ascii_digits_u32(slice: &str) -> Option<u32> {
    if slice.is_empty() || !slice.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    slice.parse::<u32>().ok()
}

/// Validates RFC 3339 `date-time`: `YYYY-MM-DD[T|t]HH:MM:SS(.frac)?(Z|z|[+-]HH:MM)`.
pub fn is_valid_rfc3339_date_time(s: &str) -> bool {
    if !s.is_ascii() || s.len() < 20 {
        return false;
    }
    let Some((date_part, rest)) = s.split_once(['T', 't']) else {
        return false;
    };
    let date_bytes = date_part.as_bytes();
    if date_bytes.len() != 10 || date_bytes[4] != b'-' || date_bytes[7] != b'-' {
        return false;
    }
    let Some(year) = parse_ascii_digits_u32(&date_part[0..4]) else {
        return false;
    };
    let Some(month) = parse_ascii_digits_u32(&date_part[5..7]) else {
        return false;
    };
    let Some(day) = parse_ascii_digits_u32(&date_part[8..10]) else {
        return false;
    };
    if !(1..=12).contains(&month) {
        return false;
    }
    let is_leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap => 29,
        2 => 28,
        _ => return false,
    };
    if day == 0 || day > max_day {
        return false;
    }

    if rest.len() < 9 {
        return false;
    }
    let rb = rest.as_bytes();
    if rb[2] != b':' || rb[5] != b':' {
        return false;
    }
    let Some(hour) = parse_ascii_digits_u32(&rest[0..2]) else {
        return false;
    };
    let Some(min) = parse_ascii_digits_u32(&rest[3..5]) else {
        return false;
    };
    let Some(sec) = parse_ascii_digits_u32(&rest[6..8]) else {
        return false;
    };
    if hour > 23 || min > 59 || sec > 60 {
        return false;
    }

    let mut tail = &rest[8..];
    if let Some(after_dot) = tail.strip_prefix('.') {
        let frac_len = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
        if frac_len == 0 {
            return false;
        }
        tail = &after_dot[frac_len..];
    }

    if tail == "Z" || tail == "z" {
        return true;
    }
    let tb = tail.as_bytes();
    if tb.len() != 6 || (tb[0] != b'+' && tb[0] != b'-') || tb[3] != b':' {
        return false;
    }
    let Some(tz_h) = parse_ascii_digits_u32(&tail[1..3]) else {
        return false;
    };
    let Some(tz_m) = parse_ascii_digits_u32(&tail[4..6]) else {
        return false;
    };
    tz_h <= 23 && tz_m <= 59
}
