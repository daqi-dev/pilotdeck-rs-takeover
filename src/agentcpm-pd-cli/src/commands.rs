//! Table-driven argv parsing and execution for the canonical command entry and
//! `api invoke` (`WP3`, `007 §5–§6`, command tree `state/cli-v1-command-tree.json`).
//!
//! Parsing is driven entirely by [`OperationRegistry::global()`]: the 285
//! canonical command paths, the `server serve` recipe, per-operation parameter
//! bindings with schema-typed value coercion, convenience flags for
//! `run submit`, and the exit policy of `run wait`. Business dispatch goes
//! through the [`CommandClient`] abstraction so tests run against a mock
//! client while production uses `agentcpm-pd-client` over `/v1/ws`.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agentcpm_pd_client::error::ClientError;
use agentcpm_pd_client::ws_client::{PdWsSubscription, SubStreamItem};
use agentcpm_pd_client::{PdClient, PdClientOptions};
use agentcpm_pd_protocol::contract::{OPERATION_VERSION, OperationResponse, ResponseOutcome};
use agentcpm_pd_protocol::id::{IdGenerator, NodeId};
use agentcpm_pd_protocol::registry::{
    CommandRecipe, CommandSpec, ExecutionMode, OperationRegistry,
};
use agentcpm_pd_protocol::wire::MAX_FRAME_BYTES;
use serde_json::{Map, Value, json};

use crate::{
    BIN_NAME, GLOBAL_OPTIONS, GlobalOptions, OutputFormat, apply_global_option,
    capability_unavailable, exit_code_for, is_global_option, read_input_source, render_diag,
    render_value,
};

// ---------------------------------------------------------------------------
// Parsed entry types
// ---------------------------------------------------------------------------

/// `--log-format` for `api connect` (`007 §6.2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

impl LogFormat {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "invalid --log-format value `{other}` (expected text or json)"
            )),
        }
    }
}

/// One raw business-option occurrence; arity and typing are resolved after the
/// command path is known.
#[derive(Clone, Debug)]
pub struct RawOpt {
    pub flag: String,
    pub value: Option<String>,
}

/// Fully parsed top-level entry.
#[derive(Clone, Debug)]
pub enum ParsedEntry {
    Help,
    Version,
    ApiConnect {
        globals: GlobalOptions,
        log_format: LogFormat,
    },
    ApiInvoke {
        globals: GlobalOptions,
        method_id: String,
    },
    /// `terminal attach` — the dedicated per-terminal process (007 §4,
    /// t015): stdio `term.*` frames in `--machine` mode, raw tty otherwise.
    /// A transport entrypoint like `api connect`, not a business operation.
    TerminalAttach {
        globals: GlobalOptions,
        terminal_id: String,
        machine: bool,
    },
    Command {
        globals: GlobalOptions,
        path: Vec<String>,
        opts: Vec<RawOpt>,
    },
}

/// A resolved canonical command: either a direct command or a recipe (fixed
/// parameters plus the bindings of the command that shares its operation).
pub enum CommandMatch<'a> {
    Command(&'a CommandSpec),
    Recipe {
        recipe: &'a CommandRecipe,
        command: &'a CommandSpec,
    },
}

impl CommandMatch<'_> {
    pub fn command(&self) -> &CommandSpec {
        match self {
            Self::Command(spec) => spec,
            Self::Recipe { command, .. } => command,
        }
    }

    pub fn recipe(&self) -> Option<&CommandRecipe> {
        match self {
            Self::Command(_) => None,
            Self::Recipe { recipe, .. } => Some(recipe),
        }
    }
}

pub fn find_command_match<'a>(
    reg: &'a OperationRegistry,
    path: &[String],
) -> Option<CommandMatch<'a>> {
    let key: Vec<&str> = path.iter().map(String::as_str).collect();
    if let Some(spec) = reg.find_command(&key) {
        return Some(CommandMatch::Command(spec));
    }
    let recipe = reg.find_recipe(&key)?;
    let command = reg
        .commands()
        .iter()
        .find(|cmd| cmd.operation == recipe.operation)?;
    Some(CommandMatch::Recipe { recipe, command })
}

fn is_registered_prefix(reg: &OperationRegistry, candidate: &[String]) -> bool {
    reg.commands()
        .iter()
        .any(|cmd| cmd.path.starts_with(candidate))
        || reg
            .recipes()
            .iter()
            .any(|recipe| recipe.path.starts_with(candidate))
}

fn child_words(reg: &OperationRegistry, prefix: &[String]) -> Vec<String> {
    let mut words: BTreeSet<String> = BTreeSet::new();
    for cmd in reg.commands() {
        if cmd.path.starts_with(prefix) && cmd.path.len() > prefix.len() {
            words.insert(cmd.path[prefix.len()].clone());
        }
    }
    for recipe in reg.recipes() {
        if recipe.path.starts_with(prefix) && recipe.path.len() > prefix.len() {
            words.insert(recipe.path[prefix.len()].clone());
        }
    }
    words.into_iter().collect()
}

pub fn help_text() -> String {
    let reg = OperationRegistry::global();
    let mut groups: BTreeSet<String> = reg
        .commands()
        .iter()
        .map(|cmd| cmd.path[0].clone())
        .collect();
    for recipe in reg.recipes() {
        groups.insert(recipe.path[0].clone());
    }
    groups.insert("api".to_owned());

    let mut out = String::new();
    out.push_str(&format!(
        "{BIN_NAME} <command> [options] — PilotDeck public-contract CLI\n\n"
    ));
    out.push_str("entries:\n");
    out.push_str("  agentcpm-pd <command> [options]                     canonical command tree\n");
    out.push_str("  agentcpm-pd api invoke <methodId> --input-json <file|->\n");
    out.push_str("  agentcpm-pd api connect --target <name> [--log-format json]\n\n");
    out.push_str("global options:\n");
    for spec in GLOBAL_OPTIONS {
        out.push_str(&format!("  {:<18} {}\n", spec.name, spec.help));
    }
    out.push_str("\ncommand groups (see the operation registry for subcommands):\n  ");
    out.push_str(&groups.into_iter().collect::<Vec<_>>().join(" "));
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------
// argv parsing
// ---------------------------------------------------------------------------

/// Splits `tokens[i..]` after a value-taking option into its value, honoring
/// the `-` (stdin) token and rejecting option-looking values.
fn take_option_value(tokens: &[String], i: usize, flag: &str) -> Result<Option<String>, String> {
    match tokens.get(i + 1) {
        Some(next) if next == "-" || !next.starts_with('-') => Ok(Some(next.clone())),
        Some(_) => Err(format!("option `{flag}` requires a value")),
        None => Err(format!("option `{flag}` requires a value")),
    }
}

pub fn parse_entry(tokens: &[String]) -> Result<ParsedEntry, String> {
    let reg = OperationRegistry::global();
    let mut globals = GlobalOptions::default();
    let mut opts: Vec<RawOpt> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut saw_global = false;
    let mut i = 0;

    while i < tokens.len() {
        let token = tokens[i].as_str();
        if token == "--help" || token == "-h" {
            return Ok(ParsedEntry::Help);
        }
        if (token == "--version" || token == "-V") && path.is_empty() {
            return Ok(ParsedEntry::Version);
        }
        if token == "--" {
            return Err(
                "the `--` end-of-options separator is not supported; options are explicit"
                    .to_owned(),
            );
        }
        if let Some(body) = token.strip_prefix("--") {
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v.to_owned())),
                None => (format!("--{body}"), None),
            };
            if is_global_option(&name) {
                let takes_value = crate::GLOBAL_OPTIONS
                    .iter()
                    .find(|spec| spec.name == name)
                    .is_some_and(|spec| spec.takes_value);
                let had_inline = inline.is_some();
                let value = if takes_value && !had_inline {
                    take_option_value(tokens, i, &name)?
                } else {
                    inline
                };
                apply_global_option(&mut globals, &name, value.as_deref())?;
                saw_global = true;
                i += if had_inline || !takes_value { 1 } else { 2 };
                continue;
            }
            if path.is_empty() {
                return Err(format!(
                    "option `{name}` is not a global option and appears before any command word"
                ));
            }
            let value = match inline {
                Some(v) => Some(v),
                None => {
                    // Consume the following token unless it looks like another option.
                    match tokens.get(i + 1) {
                        Some(next) if next == "-" || !next.starts_with('-') => {
                            i += 1;
                            Some(next.clone())
                        }
                        _ => None,
                    }
                }
            };
            opts.push(RawOpt { flag: name, value });
            i += 1;
            continue;
        }
        if token.starts_with('-') && token.len() > 1 {
            return Err(format!(
                "unknown short option `{token}`; this CLI uses long options only"
            ));
        }

        // Command word.
        if path.is_empty() && token == "api" {
            return parse_api_entry(&tokens[i + 1..], globals);
        }
        // The dedicated terminal attach process (007 §4): a transport
        // entrypoint like `api connect`, diverted before the registry
        // command tree (which only carries the eight `terminals.*`
        // operations).
        if path.len() == 1 && path[0] == "terminal" && token == "attach" {
            return parse_terminal_attach(&tokens[i + 1..], globals);
        }
        if path.is_empty() && token == "tui" {
            return Err(
                "the `tui` entry is intentionally not provided; use canonical commands, `api invoke`, or `api connect`"
                    .to_owned(),
            );
        }
        if !opts.is_empty() {
            return Err(format!(
                "unexpected positional argument `{token}`; business input goes through typed options or --input-json, never guessed prompt text"
            ));
        }
        let mut candidate = path.clone();
        candidate.push(token.to_owned());
        if is_registered_prefix(reg, &candidate) {
            path = candidate;
        } else if find_command_match(reg, &path).is_some() {
            return Err(format!(
                "unexpected positional argument `{token}`; business input goes through typed options or --input-json, never guessed prompt text"
            ));
        } else if path.is_empty() {
            return Err(format!(
                "unknown top-level command `{token}`; run `{BIN_NAME} --help` for the command groups"
            ));
        } else {
            let children = child_words(reg, &path);
            return Err(format!(
                "unknown command `{BIN_NAME} {}`{}",
                path.join(" "),
                if children.is_empty() {
                    String::new()
                } else {
                    format!("; expected one of: {}", children.join(" "))
                }
            ));
        }
        i += 1;
    }

    if path.is_empty() {
        if opts.is_empty() && !saw_global {
            return Ok(ParsedEntry::Help);
        }
        return Err("missing command".to_owned());
    }
    if find_command_match(reg, &path).is_none() {
        let children = child_words(reg, &path);
        return Err(format!(
            "incomplete command `{BIN_NAME} {}`; expected one of: {}",
            path.join(" "),
            if children.is_empty() {
                "<none>".to_owned()
            } else {
                children.join(" ")
            }
        ));
    }
    Ok(ParsedEntry::Command {
        globals,
        path,
        opts,
    })
}

/// `terminal attach [--terminal-id <id>] [--machine] [globals]` (007 §4).
/// The lease token never travels through argv — attach acquires it itself.
fn parse_terminal_attach(
    rest: &[String],
    mut globals: GlobalOptions,
) -> Result<ParsedEntry, String> {
    let mut terminal_id: Option<String> = None;
    let mut machine = false;
    let mut i = 0;
    while i < rest.len() {
        let token = rest[i].as_str();
        if token == "--help" || token == "-h" {
            return Ok(ParsedEntry::Help);
        }
        if let Some(body) = token.strip_prefix("--") {
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v.to_owned())),
                None => (format!("--{body}"), None),
            };
            if name == "--terminal-id" {
                let value = inline
                    .or_else(|| rest.get(i + 1).cloned())
                    .ok_or_else(|| "option `--terminal-id` requires a value".to_owned())?;
                if value.starts_with('-') && value != "-" {
                    return Err("option `--terminal-id` requires a value".to_owned());
                }
                terminal_id = Some(value);
                i += if inline.is_some() { 1 } else { 2 };
                continue;
            }
            if name == "--machine" {
                if let Value::Bool(value) = parse_bool_flag_value(&name, inline.as_deref())? {
                    machine = value;
                }
                i += 1;
                continue;
            }
            if !is_global_option(&name) {
                return Err(format!(
                    "terminal attach accepts --terminal-id, --machine, and global options, got `{name}`"
                ));
            }
            let takes_value = crate::GLOBAL_OPTIONS
                .iter()
                .find(|spec| spec.name == name)
                .is_some_and(|spec| spec.takes_value);
            let had_inline = inline.is_some();
            let value = if takes_value && !had_inline {
                take_option_value(rest, i, &name)?
            } else {
                inline
            };
            apply_global_option(&mut globals, &name, value.as_deref())?;
            i += if had_inline || !takes_value { 1 } else { 2 };
            continue;
        }
        return Err(format!(
            "unexpected positional argument `{token}`; terminal attach takes --terminal-id, not positional input"
        ));
    }
    let terminal_id =
        terminal_id.ok_or_else(|| "terminal attach requires --terminal-id <id>".to_owned())?;
    Ok(ParsedEntry::TerminalAttach {
        globals,
        terminal_id,
        machine,
    })
}

fn parse_api_entry(rest: &[String], mut globals: GlobalOptions) -> Result<ParsedEntry, String> {
    let Some(sub) = rest.first() else {
        return Err("missing `invoke` or `connect` after `api`".to_owned());
    };
    match sub.as_str() {
        "invoke" => {
            let mut method_id: Option<String> = None;
            let mut i = 1;
            while i < rest.len() {
                let token = rest[i].as_str();
                if token == "--help" || token == "-h" {
                    return Ok(ParsedEntry::Help);
                }
                if let Some(body) = token.strip_prefix("--") {
                    let (name, inline) = match body.split_once('=') {
                        Some((n, v)) => (format!("--{n}"), Some(v.to_owned())),
                        None => (format!("--{body}"), None),
                    };
                    if !is_global_option(&name) {
                        return Err(format!(
                            "api invoke accepts only global options (input goes through --input-json), got `{name}`"
                        ));
                    }
                    let takes_value = crate::GLOBAL_OPTIONS
                        .iter()
                        .find(|spec| spec.name == name)
                        .is_some_and(|spec| spec.takes_value);
                    let had_inline = inline.is_some();
                    let value = if takes_value && !had_inline {
                        take_option_value(rest, i, &name)?
                    } else {
                        inline
                    };
                    apply_global_option(&mut globals, &name, value.as_deref())?;
                    i += if had_inline || !takes_value { 1 } else { 2 };
                    continue;
                }
                if token.starts_with('-') && token.len() > 1 {
                    return Err(format!("unknown short option `{token}`"));
                }
                if method_id.is_some() {
                    return Err(format!(
                        "unexpected positional argument `{token}`; api invoke takes exactly one methodId"
                    ));
                }
                method_id = Some(token.to_owned());
                i += 1;
            }
            let method_id =
                method_id.ok_or_else(|| "api invoke requires a methodId positional".to_owned())?;
            if globals.input_json.is_none() {
                return Err("api invoke requires --input-json <file|->".to_owned());
            }
            if OperationRegistry::global().get(&method_id).is_none() {
                return Err(format!("unknown methodId `{method_id}`"));
            }
            Ok(ParsedEntry::ApiInvoke { globals, method_id })
        }
        "connect" => {
            let mut log_format = LogFormat::Text;
            let mut i = 1;
            while i < rest.len() {
                let token = rest[i].as_str();
                if token == "--help" || token == "-h" {
                    return Ok(ParsedEntry::Help);
                }
                if let Some(body) = token.strip_prefix("--") {
                    let (name, inline) = match body.split_once('=') {
                        Some((n, v)) => (format!("--{n}"), Some(v.to_owned())),
                        None => (format!("--{body}"), None),
                    };
                    if name == "--log-format" {
                        let had_inline = inline.is_some();
                        let value = inline
                            .or_else(|| rest.get(i + 1).cloned())
                            .ok_or_else(|| "option `--log-format` requires a value".to_owned())?;
                        log_format = LogFormat::parse(&value)?;
                        i += if had_inline { 1 } else { 2 };
                        continue;
                    }
                    if !is_global_option(&name) {
                        return Err(format!(
                            "api connect accepts only --target, --log-format, and global flags, got `{name}`"
                        ));
                    }
                    let takes_value = crate::GLOBAL_OPTIONS
                        .iter()
                        .find(|spec| spec.name == name)
                        .is_some_and(|spec| spec.takes_value);
                    let had_inline = inline.is_some();
                    let value = if takes_value && !had_inline {
                        take_option_value(rest, i, &name)?
                    } else {
                        inline
                    };
                    apply_global_option(&mut globals, &name, value.as_deref())?;
                    i += if had_inline || !takes_value { 1 } else { 2 };
                    continue;
                }
                return Err(format!(
                    "unexpected positional argument `{token}`; api connect takes no positional arguments"
                ));
            }
            Ok(ParsedEntry::ApiConnect {
                globals,
                log_format,
            })
        }
        other => Err(format!(
            "unknown `api` subcommand `{other}`; expected `invoke` or `connect`"
        )),
    }
}

// ---------------------------------------------------------------------------
// Input assembly (typed options from the registry)
// ---------------------------------------------------------------------------

/// Coerced value kinds derived from the operation input schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PropKind {
    Str,
    Int,
    Num,
    Bool,
    Arr,
    Json,
}

fn kind_from_name(name: &str) -> Option<PropKind> {
    match name {
        "string" => Some(PropKind::Str),
        "integer" => Some(PropKind::Int),
        "number" => Some(PropKind::Num),
        "boolean" => Some(PropKind::Bool),
        "array" => Some(PropKind::Arr),
        _ => None,
    }
}

/// Resolves the effective value kind of a bound property, following `$ref`s and
/// the first non-`null` `anyOf`/`oneOf` variant (e.g. `anyOf: [Id, null]`).
fn prop_kind(prop: &Value, schema: &Value, depth: usize) -> PropKind {
    if depth > 4 {
        return PropKind::Json;
    }
    if let Some(kind) = prop.get("type").and_then(|t| match t {
        Value::String(s) => kind_from_name(s),
        Value::Array(variants) => variants
            .iter()
            .find_map(|v| v.as_str().and_then(kind_from_name)),
        _ => None,
    }) {
        return kind;
    }
    if let Some(reference) = prop.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap_or("");
        if let Some(def) = schema.get("$defs").and_then(|d| d.get(name)) {
            return prop_kind(def, schema, depth + 1);
        }
        return PropKind::Json;
    }
    for combinator in ["anyOf", "oneOf"] {
        if let Some(variants) = prop.get(combinator).and_then(Value::as_array) {
            for variant in variants {
                if variant.get("type").and_then(Value::as_str) == Some("null") {
                    continue;
                }
                let kind = prop_kind(variant, schema, depth + 1);
                if kind != PropKind::Json {
                    return kind;
                }
            }
        }
    }
    PropKind::Json
}

fn parse_scalar_list(raw: &str, item_kind: PropKind, flag: &str) -> Result<Value, String> {
    let items: Vec<Value> = raw
        .split(',')
        .map(|part| match item_kind {
            PropKind::Int => part
                .parse::<i64>()
                .map(|n| json!(n))
                .map_err(|_| format!("option `{flag}` expects comma-separated integers")),
            PropKind::Num => part
                .parse::<f64>()
                .map(|n| json!(n))
                .map_err(|_| format!("option `{flag}` expects comma-separated numbers")),
            _ => Ok(Value::String(part.to_owned())),
        })
        .collect::<Result<_, _>>()?;
    Ok(Value::Array(items))
}

fn parse_typed_value(
    prop: Option<&Value>,
    schema: &Value,
    raw: &str,
    flag: &str,
) -> Result<Value, String> {
    let kind = prop
        .map(|p| prop_kind(p, schema, 0))
        .unwrap_or(PropKind::Json);
    match kind {
        PropKind::Str => Ok(Value::String(raw.to_owned())),
        PropKind::Int => raw
            .parse::<i64>()
            .map(|n| json!(n))
            .map_err(|_| format!("option `{flag}` expects an integer, got `{raw}`")),
        PropKind::Num => raw
            .parse::<f64>()
            .map(|n| json!(n))
            .map_err(|_| format!("option `{flag}` expects a number, got `{raw}`")),
        PropKind::Bool => match raw {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            other => Err(format!(
                "option `{flag}` is a boolean flag (pass it bare for true, or =true/=false), got `{other}`"
            )),
        },
        PropKind::Arr => {
            if raw.trim_start().starts_with('[') {
                serde_json::from_str(raw)
                    .map_err(|e| format!("option `{flag}` expects a JSON array: {e}"))
            } else {
                let item_kind = prop
                    .and_then(|p| p.get("items"))
                    .map(|items| prop_kind(items, schema, 1))
                    .unwrap_or(PropKind::Str);
                parse_scalar_list(raw, item_kind, flag)
            }
        }
        PropKind::Json => serde_json::from_str(raw)
            .map_err(|e| format!("option `{flag}` expects a JSON value: {e}")),
    }
}

fn parse_bool_flag_value(flag: &str, value: Option<&str>) -> Result<Value, String> {
    match value {
        None => Ok(Value::Bool(true)),
        Some("true") => Ok(Value::Bool(true)),
        Some("false") => Ok(Value::Bool(false)),
        Some(other) => Err(format!("option `{flag}` is a boolean flag, got `{other}`")),
    }
}

pub(crate) fn local_request_id() -> String {
    static ID_GEN: OnceLock<IdGenerator> = OnceLock::new();
    ID_GEN
        .get_or_init(|| IdGenerator::new(NodeId::local()))
        .next_id()
}

/// Assembles the operation input object from parsed options.
///
/// Enforces: `--input-json` exclusivity, recipe-fixed parameters, secure fields
/// via `--input-json` only, schema-typed coercion, convenience flags for
/// `run submit` (`--text` / `--text-file`), and required-field presence
/// (derived fields like `instanceId` are injected from the bound target).
pub fn assemble_input(
    matched: &CommandMatch<'_>,
    opts: &[RawOpt],
    globals: &GlobalOptions,
) -> Result<Value, String> {
    let reg = OperationRegistry::global();
    let spec = matched.command();
    let op = reg
        .get(&spec.operation)
        .ok_or_else(|| format!("command references unknown operation `{}`", spec.operation))?;
    let input_schema = &op.input_schema;

    // Reverse binding map: flag -> field.
    let flag_to_field: BTreeMap<&str, &str> = spec
        .parameter_bindings
        .iter()
        .map(|(field, flag)| (flag.as_str(), field.as_str()))
        .collect();
    let convenience: BTreeSet<&str> = spec.convenience_flags.keys().map(String::as_str).collect();
    let secure_fields: BTreeSet<&str> = spec
        .secure_input_fields
        .iter()
        .map(String::as_str)
        .collect();
    let fixed_fields: BTreeSet<&str> = matched
        .recipe()
        .map(|recipe| recipe.fixed_params.keys().map(String::as_str).collect())
        .unwrap_or_default();

    let mut fields: Map<String, Value> = Map::new();
    let mut text_part: Option<Value> = None;

    if let Some(path) = &globals.input_json {
        if !opts.is_empty() {
            return Err(
                "--input-json cannot be combined with business options (connection globals are fine)"
                    .to_owned(),
            );
        }
        let raw = read_input_source(path)?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("--input-json did not contain valid JSON: {e}"))?;
        let Some(obj) = value.as_object() else {
            return Err("--input-json must contain a JSON object".to_owned());
        };
        return Ok(Value::Object(obj.clone()));
    }

    for opt in opts {
        let flag = opt.flag.as_str();
        if !flag_to_field.contains_key(flag) && !convenience.contains(flag) {
            return Err(format!(
                "unknown option `{flag}` for `{}`",
                spec.path.join(" ")
            ));
        }
        if convenience.contains(flag) {
            // Only `run submit` defines convenience flags in this tree.
            let value = opt
                .value
                .clone()
                .ok_or_else(|| format!("option `{flag}` requires a value"))?;
            if text_part.is_some() {
                return Err(
                    "options --text/--text-file/--input are mutually exclusive business inputs"
                        .to_owned(),
                );
            }
            if flag == "--text" {
                text_part = Some(json!([{"kind": "text", "text": value}]));
            } else if flag == "--text-file" {
                let body = read_input_source(&value)?;
                text_part = Some(json!([{"kind": "text", "text": body}]));
            } else {
                return Err(format!("unsupported convenience flag `{flag}`"));
            }
            continue;
        }
        let field = flag_to_field[flag];
        if fixed_fields.contains(field) {
            return Err(format!(
                "option `{flag}` is fixed by the `{}` recipe and cannot be overridden",
                matched
                    .recipe()
                    .map(|r| r.path.join(" "))
                    .unwrap_or_default()
            ));
        }
        if secure_fields.contains(field) {
            return Err(format!(
                "option `{flag}` carries a secret; secrets are not accepted as plain argv arguments — use --input-json with a protected file or stdin"
            ));
        }
        let prop = input_schema
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|props| props.get(field));
        let kind = prop
            .map(|p| prop_kind(p, input_schema, 0))
            .unwrap_or(PropKind::Json);
        let value = if kind == PropKind::Bool {
            parse_bool_flag_value(flag, opt.value.as_deref())?
        } else {
            let raw = opt
                .value
                .clone()
                .ok_or_else(|| format!("option `{flag}` requires a value"))?;
            parse_typed_value(prop, input_schema, &raw, flag)?
        };
        fields.insert(field.to_owned(), value);
    }

    // Recipe fixed parameters (e.g. `server serve` -> mode=foreground).
    if let Some(recipe) = matched.recipe() {
        for (field, value) in &recipe.fixed_params {
            fields.insert(field.clone(), value.clone());
        }
    }

    if let Some(part) = text_part {
        if fields.contains_key("input") {
            return Err(
                "options --text/--text-file/--input are mutually exclusive business inputs"
                    .to_owned(),
            );
        }
        fields.insert("input".to_owned(), part);
    }

    // Required fields except target-derived ones (instanceId comes from the
    // bound target and is injected/verified by the client).
    for field in &spec.required_input {
        if field == "instanceId" || spec.derived_inputs.contains_key(field) {
            continue;
        }
        if !fields.contains_key(field) {
            let flag = spec
                .parameter_bindings
                .get(field)
                .cloned()
                .unwrap_or_else(|| format!("--{field}"));
            return Err(format!(
                "missing required option `{flag}` for `{}`",
                spec.path.join(" ")
            ));
        }
    }

    Ok(Value::Object(fields))
}

// ---------------------------------------------------------------------------
// Command client abstraction (production /v1/ws client + test mocks)
// ---------------------------------------------------------------------------

/// One business invocation against the operation directory.
#[derive(Clone, Debug)]
pub struct CallRequest {
    pub method_id: String,
    pub input: Value,
    pub request_key: Option<String>,
    pub timeout: Option<Duration>,
}

/// Result of one invocation plus the idempotency key materialized for it.
#[derive(Debug)]
pub struct CallOutput {
    pub result: Result<OperationResponse, ClientError>,
    pub request_key: Option<String>,
}

pub type CallFuture = Pin<Box<dyn Future<Output = CallOutput>>>;
pub type OpenFollowFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn FollowStream>, ClientError>> + 'a>>;
pub type FollowRecvFuture<'a> = Pin<Box<dyn Future<Output = Option<FollowItem>> + 'a>>;
pub type FollowCloseFuture<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// One frame from a follow subscription.
#[derive(Clone, Debug)]
pub enum FollowItem {
    Event(Value),
    End {
        terminal: String,
        resume_cursor: Option<String>,
        error: Option<ClientError>,
    },
}

/// Abstracts the operation transport so command execution is testable against
/// a mock client (the default test path while the gateway is under parallel
/// development).
pub trait CommandClient {
    fn call(&self, req: CallRequest) -> CallFuture;
    fn open_follow(&self, method_id: &str, input: Value) -> OpenFollowFuture<'_>;
}

pub struct FollowSubscription {
    sub: Option<PdWsSubscription>,
}

impl FollowSubscription {
    fn new(sub: PdWsSubscription) -> Self {
        Self { sub: Some(sub) }
    }
}

pub trait FollowStream {
    fn recv(&mut self) -> FollowRecvFuture<'_>;
    fn close(&mut self) -> FollowCloseFuture<'_>;
}

impl FollowStream for FollowSubscription {
    fn recv(&mut self) -> FollowRecvFuture<'_> {
        let Some(sub) = self.sub.as_mut() else {
            return Box::pin(async { None });
        };
        Box::pin(async move {
            match sub.recv().await {
                Some(SubStreamItem::Event(ev)) => Some(FollowItem::Event(
                    serde_json::to_value(&ev.event).unwrap_or(Value::Null),
                )),
                Some(SubStreamItem::End(end)) => Some(FollowItem::End {
                    terminal: serde_json::to_value(end.terminal)
                        .ok()
                        .and_then(|v| v.as_str().map(ToOwned::to_owned))
                        .unwrap_or_else(|| "error".to_owned()),
                    resume_cursor: end.resume_cursor,
                    error: end.error.map(ClientError::Contract),
                }),
                None => None,
            }
        })
    }

    fn close(&mut self) -> FollowCloseFuture<'_> {
        match self.sub.take() {
            Some(sub) => Box::pin(async move {
                let _ = sub.close().await;
            }),
            None => Box::pin(async {}),
        }
    }
}

/// Production [`CommandClient`] over `agentcpm-pd-client` (`/v1/ws`), shared by
/// the argv and `api invoke` entries.
pub struct PdCommandClient {
    pub client: Arc<PdClient>,
}

impl CommandClient for PdCommandClient {
    fn call(&self, req: CallRequest) -> CallFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let reg = OperationRegistry::global();
            let request_key = match &req.request_key {
                Some(key) => Some(key.clone()),
                None if reg.requires_idempotency_key(&req.method_id) => {
                    Some(client.id_gen().next_id())
                }
                None => None,
            };
            let result = client
                .invoke(
                    &req.method_id,
                    req.input,
                    request_key.as_deref(),
                    None,
                    req.timeout,
                )
                .await;
            CallOutput {
                result,
                request_key,
            }
        })
    }

    fn open_follow(&self, method_id: &str, input: Value) -> OpenFollowFuture<'_> {
        let ws = self.client.ws().clone();
        let method_id = method_id.to_owned();
        Box::pin(async move {
            let sub = ws.open_subscription(&method_id, input, None, true).await?;
            Ok(Box::new(FollowSubscription::new(sub)) as Box<dyn FollowStream>)
        })
    }
}

async fn connect_client(globals: &GlobalOptions) -> Result<Arc<PdCommandClient>, ClientError> {
    let pd = PdClient::connect(PdClientOptions {
        home_override: None,
        target: globals.target.clone(),
        no_start: globals.no_start,
        spawner: None,
    })
    .await?;
    Ok(Arc::new(PdCommandClient {
        client: Arc::new(pd),
    }))
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Invocation {
    method_id: String,
    input: Value,
    format: OutputFormat,
    request_key: Option<String>,
    report_generated_key: bool,
    timeout: Option<Duration>,
    wait_exit_policy: bool,
}

fn write_envelope(
    out: &mut dyn Write,
    format: OutputFormat,
    resp: &OperationResponse,
) -> std::io::Result<()> {
    let value = serde_json::to_value(resp).unwrap_or(Value::Null);
    write!(out, "{}", render_value(format, &value))
}

/// Reports an envelope write failure; stdout going away (broken pipe, closed
/// consumer) must fail the command instead of exiting 0 (review P2-11).
const EXIT_OK_CODE: i32 = 0;

pub(crate) fn write_envelope_or_fail(
    out: &mut dyn Write,
    format: OutputFormat,
    resp: &OperationResponse,
) -> i32 {
    match write_envelope(out, format, resp) {
        Ok(()) => EXIT_OK_CODE,
        Err(e) => {
            eprint!(
                "{}",
                render_diag(
                    Some(format),
                    &format!("failed to write the result envelope to stdout: {e}"),
                )
            );
            agentcpm_pd_protocol::errors::EXIT_FAILED
        }
    }
}

pub(crate) fn local_response(request_id: String, result: Value) -> OperationResponse {
    OperationResponse {
        version: OPERATION_VERSION.to_owned(),
        request_id,
        outcome: ResponseOutcome::Succeeded,
        result: Some(result),
        error: None,
    }
}

fn failed_response(request_id: String, err: &ClientError) -> OperationResponse {
    OperationResponse {
        version: OPERATION_VERSION.to_owned(),
        request_id,
        outcome: ResponseOutcome::Failed,
        result: None,
        error: Some(err.into_contract_error()),
    }
}

pub(crate) fn write_error(out: &mut dyn Write, format: OutputFormat, err: &ClientError) -> i32 {
    // P2-11: a failed error-envelope write must not exit 0.
    let write_failed =
        write_envelope(out, format, &failed_response(local_request_id(), err)).is_err();
    eprint!("{}", render_diag(Some(format), &format!("error: {err}")));
    if write_failed {
        eprint!(
            "{}",
            render_diag(Some(format), "failed to write the error envelope to stdout")
        );
        return agentcpm_pd_protocol::errors::EXIT_FAILED;
    }
    exit_code_for(err)
}

/// Exit policy for `run.wait` (`007` command tree `exitPolicy`): 0 only when
/// `timedOut=false` and `run.state=succeeded`; known failure/cancel/interrupt
/// exit 1; `result_unknown` exits 3; SIGINT exits 130 (handled by the caller).
fn run_wait_exit_code(result: &Value, format: OutputFormat) -> i32 {
    let timed_out = result
        .get("timedOut")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let state = result
        .get("run")
        .and_then(|run| run.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match state {
        "succeeded" if !timed_out => 0,
        "failed" | "cancelled" | "interrupted" => 1,
        "result_unknown" => 3,
        other => {
            eprint!(
                "{}",
                render_diag(
                    Some(format),
                    &format!(
                        "run.wait observation returned without success: state=`{other}` timedOut={timed_out}; run state unchanged"
                    ),
                )
            );
            1
        }
    }
}

async fn execute_invocation(
    client: &dyn CommandClient,
    inv: Invocation,
    out: &mut dyn Write,
) -> i32 {
    let request = CallRequest {
        method_id: inv.method_id.clone(),
        input: inv.input.clone(),
        request_key: inv.request_key.clone(),
        timeout: inv.timeout,
    };
    let output = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            eprint!("{}", render_diag(Some(inv.format), "interrupted; observation stopped, background runs are never cancelled by this CLI"));
            return 130;
        }
        output = client.call(request) => output,
    };
    match output.result {
        Ok(resp) => {
            if let Err(e) = write_envelope(out, inv.format, &resp) {
                eprint!(
                    "{}",
                    render_diag(
                        Some(inv.format),
                        &format!("failed to write the result envelope to stdout: {e}"),
                    )
                );
                return agentcpm_pd_protocol::errors::EXIT_FAILED;
            }
            if inv.report_generated_key
                && let Some(key) = &output.request_key
            {
                eprint!(
                    "{}",
                    render_diag(
                        Some(inv.format),
                        &format!(
                            "idempotency key `{key}` persisted; query acceptance with `requests.lookup`"
                        ),
                    )
                );
            }
            if inv.wait_exit_policy {
                return run_wait_exit_code(
                    resp.result.as_ref().unwrap_or(&Value::Null),
                    inv.format,
                );
            }
            0
        }
        Err(err) => write_error(out, inv.format, &err),
    }
}

/// Standard recovery guidance when a follow subscription is rejected at open
/// with `CURSOR_EXPIRED` (`007 §6.4`): snapshot, take the checkpoint cursor,
/// re-open from it. Shared by the open-rejection and mid-stream gap paths.
fn cursor_expired_guidance(context: &str) -> String {
    format!(
        "{context}: the requested cursor has expired; recover with `run snapshot --run-id <id>`, take snapshot.checkpoint.cursor, and re-open the subscription with --after-cursor set to it"
    )
}

fn follow_terminal_exit(
    terminal: &str,
    resume_cursor: Option<&str>,
    error: Option<&ClientError>,
    format: OutputFormat,
) -> i32 {
    match terminal {
        "completed" | "closed" | "shutdown" => 0,
        "slow_consumer" => {
            eprint!(
                "{}",
                render_diag(
                    Some(format),
                    &format!(
                        "subscription ended: slow consumer{}; re-subscribe with --after-cursor to resume without gaps",
                        resume_cursor
                            .map(|c| format!(" (resumeCursor `{c}`)"))
                            .unwrap_or_default()
                    ),
                )
            );
            1
        }
        "gap" => {
            eprint!(
                "{}",
                render_diag(
                    Some(format),
                    &format!(
                        "subscription ended{}; {}",
                        resume_cursor
                            .map(|c| format!(" (resumeCursor `{c}`)"))
                            .unwrap_or_default(),
                        cursor_expired_guidance("cursor gap mid-stream"),
                    ),
                )
            );
            1
        }
        "disconnected" => {
            eprint!(
                "{}",
                render_diag(Some(format), "subscription ended: upstream connection lost"),
            );
            1
        }
        _ => {
            eprint!(
                "{}",
                render_diag(
                    Some(format),
                    &format!(
                        "subscription ended: {terminal}{}",
                        error.map(|e| format!(" ({e})")).unwrap_or_default()
                    ),
                )
            );
            1
        }
    }
}

async fn execute_follow(client: &dyn CommandClient, inv: Invocation, out: &mut dyn Write) -> i32 {
    let open = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            eprint!("{}", render_diag(Some(inv.format), "interrupted; observation stopped, background runs are never cancelled by this CLI"));
            return 130;
        }
        open = client.open_follow(&inv.method_id, inv.input.clone()) => open,
    };
    let mut stream = match open {
        Ok(stream) => stream,
        Err(err) => {
            eprint!(
                "{}",
                render_diag(
                    Some(inv.format),
                    &format!("failed to open subscription: {err}")
                )
            );
            return exit_code_for(&err);
        }
    };

    let deadline = inv.timeout.map(|d| tokio::time::Instant::now() + d);
    loop {
        let item = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                stream.close().await;
                eprint!("{}", render_diag(Some(inv.format), "interrupted; observation stopped, background runs are never cancelled by this CLI"));
                return 130;
            }
            _ = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            }, if deadline.is_some() => {
                stream.close().await;
                eprint!("{}", render_diag(Some(inv.format), "observation timed out; the subscription was closed but the run continues"));
                return 1;
            }
            item = stream.recv() => item,
        };
        match item {
            Some(FollowItem::Event(event)) => {
                let mut line = serde_json::to_string(&event)
                    .unwrap_or_else(|_| serde_json::to_string(&Value::Null).unwrap_or_default());
                if line.len() > MAX_FRAME_BYTES {
                    line.truncate(MAX_FRAME_BYTES);
                }
                if writeln!(out, "{line}").is_err() {
                    stream.close().await;
                    return 1;
                }
            }
            Some(FollowItem::End {
                terminal,
                resume_cursor,
                error,
            }) => {
                return follow_terminal_exit(
                    &terminal,
                    resume_cursor.as_deref(),
                    error.as_ref(),
                    inv.format,
                );
            }
            None => {
                eprint!(
                    "{}",
                    render_diag(
                        Some(inv.format),
                        "subscription closed without a terminal frame",
                    ),
                );
                return 1;
            }
        }
    }
}

/// Entry for canonical commands: assembly is table-driven, dispatch follows
/// the operation's execution mode.
pub async fn run_canonical(globals: GlobalOptions, path: Vec<String>, opts: Vec<RawOpt>) -> i32 {
    let reg = OperationRegistry::global();
    let Some(matched) = find_command_match(reg, &path) else {
        eprintln!("{BIN_NAME}: internal error: command path vanished from the registry");
        return 1;
    };
    let method_id = matched.command().operation.clone();
    let input = match assemble_input(&matched, &opts, &globals) {
        Ok(input) => input,
        Err(message) => {
            eprintln!("{BIN_NAME}: usage error: {message}");
            return 2;
        }
    };
    dispatch_operation(&globals, &method_id, input).await
}

/// Entry for `api invoke <methodId> --input-json <file|->`: same registry,
/// same schemas, same dispatch as the canonical entry (`A6` equivalence).
pub async fn run_api_invoke(globals: GlobalOptions, method_id: String) -> i32 {
    let reg = OperationRegistry::global();
    if reg.get(&method_id).is_none() {
        eprintln!("{BIN_NAME}: usage error: unknown methodId `{method_id}`");
        return 2;
    }
    let Some(path) = globals.input_json.clone() else {
        eprintln!("{BIN_NAME}: usage error: api invoke requires --input-json <file|->");
        return 2;
    };
    let raw = match read_input_source(&path) {
        Ok(raw) => raw,
        Err(message) => {
            eprintln!("{BIN_NAME}: usage error: {message}");
            return 2;
        }
    };
    let input: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("{BIN_NAME}: usage error: --input-json did not contain valid JSON: {e}");
            return 2;
        }
    };
    if !input.is_object() {
        eprintln!("{BIN_NAME}: usage error: --input-json must contain a JSON object");
        return 2;
    }
    dispatch_operation(&globals, &method_id, input).await
}

/// Dispatch by execution mode: local operations run in-process; stream
/// operations follow NDJSON when `follow=true`; everything else is unary.
async fn dispatch_operation(globals: &GlobalOptions, method_id: &str, mut input: Value) -> i32 {
    let reg = OperationRegistry::global();
    let Some(op) = reg.get(method_id) else {
        eprintln!("{BIN_NAME}: usage error: unknown methodId `{method_id}`");
        return 2;
    };

    // `--timeout` is the observation window for operations that declare their
    // own `timeoutMs` (e.g. `run.wait`), mapped onto the input so the server
    // waits that long; the client deadline then derives from it. Elsewhere it
    // stays the request deadline.
    let mut effective_globals = globals.clone();
    if let Some(timeout_ms) = globals.timeout_ms
        && op
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|props| props.contains_key("timeoutMs"))
        && input.get("timeoutMs").is_none()
        && let Some(obj) = input.as_object_mut()
    {
        obj.insert("timeoutMs".to_owned(), json!(timeout_ms));
        effective_globals.timeout_ms = None;
    }

    match op.execution {
        ExecutionMode::Local => {
            let mut sink = std::io::stdout().lock();
            run_local_operation(&effective_globals, method_id, input, &mut sink).await
        }
        ExecutionMode::Stream => {
            let follow = input
                .get("follow")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if follow {
                // Subscriptions force `follow=true` and never accept `page`.
                if input.get("page").is_some() {
                    eprintln!(
                        "{BIN_NAME}: usage error: `page` cannot be combined with --follow; paginate with a plain unary call instead"
                    );
                    return 2;
                }
                run_follow(&effective_globals, method_id, input).await
            } else {
                run_unary(&effective_globals, method_id, input).await
            }
        }
        ExecutionMode::Sync | ExecutionMode::Async => {
            run_unary(&effective_globals, method_id, input).await
        }
    }
}

fn invocation_from_globals(
    globals: &GlobalOptions,
    method_id: &str,
    input: Value,
    format: OutputFormat,
) -> Invocation {
    let reg = OperationRegistry::global();
    Invocation {
        method_id: method_id.to_owned(),
        input,
        format,
        request_key: globals.request_key.clone(),
        report_generated_key: globals.request_key.is_none()
            && reg.requires_idempotency_key(method_id),
        timeout: globals.timeout_ms.map(Duration::from_millis),
        wait_exit_policy: method_id == "run.wait",
    }
}

async fn run_unary(globals: &GlobalOptions, method_id: &str, input: Value) -> i32 {
    let format = OutputFormat::resolve_unary(globals.output);
    let client = match connect_client(globals).await {
        Ok(client) => client,
        Err(err) => {
            let mut sink = std::io::stdout().lock();
            return write_error(&mut sink, format, &err);
        }
    };
    let inv = invocation_from_globals(globals, method_id, input, format);
    let mut sink = std::io::stdout().lock();
    execute_invocation(client.as_ref(), inv, &mut sink).await
}

async fn run_follow(globals: &GlobalOptions, method_id: &str, input: Value) -> i32 {
    let format = match OutputFormat::resolve_stream(globals.output) {
        Ok(format) => format,
        Err(message) => {
            eprintln!("{BIN_NAME}: usage error: {message}");
            return 2;
        }
    };
    let client = match connect_client(globals).await {
        Ok(client) => client,
        Err(err) => {
            let mut sink = std::io::stdout().lock();
            return write_error(&mut sink, format, &err);
        }
    };
    let inv = invocation_from_globals(globals, method_id, input, format);
    let mut sink = std::io::stdout().lock();
    execute_follow(client.as_ref(), inv, &mut sink).await
}

// ---------------------------------------------------------------------------
// Local operations
// ---------------------------------------------------------------------------

/// Dispatches an `execution='local'` operation through the `local_ops`
/// table (S3 substrate). Availability derives from the registered handlers —
/// the table marks its ids implemented in the protocol crate's global set —
/// and an unregistered local operation answers `CAPABILITY_UNAVAILABLE`
/// with the same envelope and message as before.
async fn run_local_operation(
    globals: &GlobalOptions,
    method_id: &str,
    input: Value,
    out: &mut dyn Write,
) -> i32 {
    let format = OutputFormat::resolve_unary(globals.output);
    match crate::local_ops::LocalOpRegistry::global().lookup(method_id) {
        Some(handler) => handler(globals, &input, format, out).await,
        None => {
            let err = capability_unavailable(
                method_id,
                "this local operation is not implemented in the current round (S3+)",
            );
            write_error(out, format, &err)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (mock-client path; the production gateway is exercised on cb8/mbp2)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use agentcpm_pd_protocol::errors::{
        CODE_CAPABILITY_UNAVAILABLE, CODE_RESULT_UNKNOWN, EXIT_RESULT_UNKNOWN, EXIT_USAGE,
        EffectStatus, RetrySpec,
    };

    fn tokens(list: &[&str]) -> Vec<String> {
        list.iter().map(|token| (*token).to_owned()).collect()
    }

    fn command_of(entry: Result<ParsedEntry, String>) -> (GlobalOptions, Vec<String>, Vec<RawOpt>) {
        match entry.expect("parse ok") {
            ParsedEntry::Command {
                globals,
                path,
                opts,
            } => (globals, path, opts),
            other => panic!("expected command entry, got {other:?}"),
        }
    }

    /// Parses tokens and assembles the command input (parse + assemble path).
    fn assemble_of(argv: &[&str]) -> Result<Value, String> {
        let (globals, path, opts) = command_of(parse_entry(&tokens(argv)));
        let reg = OperationRegistry::global();
        let matched = find_command_match(reg, &path).expect("command match");
        assemble_input(&matched, &opts, &globals)
    }

    // ---- parsing ---------------------------------------------------------

    #[test]
    fn parse_rejects_tui_unknown_top_level_and_incomplete_paths() {
        for bad in [
            vec!["tui"],
            vec!["frobnicate", "--x", "1"],
            vec!["run"],
            vec!["run", "nope"],
            vec!["git", "identity"],
            vec!["run", "submit", "stray-positional"],
        ] {
            let err = parse_entry(&tokens(&bad)).expect_err("must be rejected");
            assert!(!err.is_empty(), "case {bad:?}: {err}");
        }

        let err = parse_entry(&tokens(&["tui"])).expect_err("tui rejected");
        assert!(err.contains("tui"), "{err}");
        let err = parse_entry(&tokens(&["frobnicate"])).expect_err("unknown rejected");
        assert!(err.contains("unknown top-level command"), "{err}");
    }

    #[test]
    fn parse_canonical_command_with_global_and_business_options() {
        let (globals, path, opts) = command_of(parse_entry(&tokens(&[
            "run",
            "list",
            "--target",
            "local",
            "--output",
            "json",
            "--states",
            "queued,running",
            "--no-start",
        ])));
        assert_eq!(path, vec!["run", "list"]);
        assert_eq!(globals.target.as_deref(), Some("local"));
        assert_eq!(globals.output, Some(OutputFormat::Json));
        assert!(globals.no_start);
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].flag, "--states");
        assert_eq!(opts[0].value.as_deref(), Some("queued,running"));

        // globals may also precede the command words
        let (globals, path, _) = command_of(parse_entry(&tokens(&[
            "--target", "remote", "sessions", "list",
        ])));
        assert_eq!(path, vec!["sessions", "list"]);
        assert_eq!(globals.target.as_deref(), Some("remote"));
    }

    #[test]
    fn parse_unknown_option_and_business_option_before_command_rejected() {
        let err =
            assemble_of(&["run", "list", "--bogus", "1"]).expect_err("unknown business option");
        assert!(err.contains("unknown option"), "{err}");
        let err =
            parse_entry(&tokens(&["--bogus", "run", "list"])).expect_err("option before command");
        assert!(err.contains("before any command word"), "{err}");
        let err = parse_entry(&tokens(&["-x", "run"])).expect_err("short option");
        assert!(err.contains("short option"), "{err}");
    }

    #[test]
    fn parse_run_submit_convenience_flags_are_exclusive() {
        // --text alone is fine
        let (_, _, opts) = command_of(parse_entry(&tokens(&[
            "run",
            "submit",
            "--session-id",
            "s1",
            "--text",
            "hello",
        ])));
        let reg = OperationRegistry::global();
        let matched = find_command_match(reg, &["run".into(), "submit".into()]).unwrap();
        let input = assemble_input(&matched, &opts, &GlobalOptions::default()).unwrap();
        assert_eq!(input["input"], json!([{"kind": "text", "text": "hello"}]));

        // --text + --input
        let err = assemble_of(&[
            "run",
            "submit",
            "--session-id",
            "s1",
            "--text",
            "a",
            "--input",
            "[]",
        ])
        .expect_err("mutually exclusive");
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn parse_run_submit_requires_exactly_one_business_input() {
        let reg = OperationRegistry::global();
        let matched = find_command_match(reg, &["run".into(), "submit".into()]).unwrap();
        // none of --text/--text-file/--input (with sessionId satisfied)
        let opts = vec![RawOpt {
            flag: "--session-id".into(),
            value: Some("s1".into()),
        }];
        let err =
            assemble_input(&matched, &opts, &GlobalOptions::default()).expect_err("missing input");
        assert!(err.contains("missing required option `--input`"), "{err}");

        // --text-file - reads stdin; here we point at a file
        let dir = std::env::temp_dir().join(format!("agentcpm-pd-cli-text-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("body.txt");
        std::fs::write(&file, "from file").unwrap();
        let opts = vec![
            RawOpt {
                flag: "--session-id".into(),
                value: Some("s1".into()),
            },
            RawOpt {
                flag: "--text-file".into(),
                value: Some(file.to_string_lossy().into_owned()),
            },
        ];
        let input = assemble_input(&matched, &opts, &GlobalOptions::default()).unwrap();
        assert_eq!(
            input["input"],
            json!([{"kind": "text", "text": "from file"}])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_input_json_excludes_business_options() {
        let err = assemble_of(&[
            "run",
            "submit",
            "--session-id",
            "s1",
            "--input-json",
            "-",
            "--text",
            "hi",
        ])
        .expect_err("input-json exclusivity");
        assert!(err.contains("--input-json cannot be combined"), "{err}");
    }

    #[test]
    fn parse_run_events_follow_flag_and_missing_required() {
        let (_, _, opts) = command_of(parse_entry(&tokens(&["run", "events", "--follow"])));
        assert_eq!(opts[0].flag, "--follow");
        assert_eq!(opts[0].value, None);

        let reg = OperationRegistry::global();
        let matched = find_command_match(reg, &["run".into(), "events".into()]).unwrap();
        let err =
            assemble_input(&matched, &[], &GlobalOptions::default()).expect_err("runId required");
        assert!(err.contains("missing required option `--run-id`"), "{err}");
    }

    #[test]
    fn parse_secure_fields_reject_plain_argv() {
        let err = assemble_of(&["credentials", "write", "--secret-file", "hunter2"])
            .expect_err("secret over argv rejected");
        assert!(err.contains("secret"), "{err}");
    }

    #[test]
    fn parse_typed_values_by_schema_kind() {
        let reg = OperationRegistry::global();

        // integer: background-tasks list --limit 10
        let matched = find_command_match(reg, &["background-tasks".into(), "list".into()]).unwrap();
        let opts = vec![
            RawOpt {
                flag: "--session-id".into(),
                value: Some("s1".into()),
            },
            RawOpt {
                flag: "--limit".into(),
                value: Some("10".into()),
            },
        ];
        let input = assemble_input(&matched, &opts, &GlobalOptions::default()).unwrap();
        assert_eq!(input["limit"], json!(10));

        // boolean: access-keys set-enabled --enabled (bare flag)
        let matched =
            find_command_match(reg, &["access-keys".to_owned(), "set-enabled".to_owned()]).unwrap();
        let opts = vec![
            RawOpt {
                flag: "--key-id".into(),
                value: Some("k1".into()),
            },
            RawOpt {
                flag: "--expected-revision".into(),
                value: Some("rev-1".into()),
            },
            RawOpt {
                flag: "--enabled".into(),
                value: None,
            },
        ];
        let input = assemble_input(&matched, &opts, &GlobalOptions::default()).unwrap();
        assert_eq!(input["enabled"], json!(true));

        // object via JSON: schema list --page '{"limit":5}'
        let matched = find_command_match(reg, &["schema".into(), "list".into()]).unwrap();
        let opts = vec![RawOpt {
            flag: "--page".into(),
            value: Some(r#"{"limit":5}"#.into()),
        }];
        let input = assemble_input(&matched, &opts, &GlobalOptions::default()).unwrap();
        assert_eq!(input["page"]["limit"], json!(5));

        // array<string> comma form (run list requires only the derived instanceId)
        let matched = find_command_match(reg, &["run".into(), "list".into()]).unwrap();
        let opts = vec![RawOpt {
            flag: "--states".into(),
            value: Some("queued,running".into()),
        }];
        let input = assemble_input(&matched, &opts, &GlobalOptions::default()).unwrap();
        assert_eq!(input["states"], json!(["queued", "running"]));

        // integer coercion failure
        let matched = find_command_match(reg, &["background-tasks".into(), "list".into()]).unwrap();
        let opts = vec![
            RawOpt {
                flag: "--session-id".into(),
                value: Some("s1".into()),
            },
            RawOpt {
                flag: "--limit".into(),
                value: Some("ten".into()),
            },
        ];
        let err =
            assemble_input(&matched, &opts, &GlobalOptions::default()).expect_err("bad integer");
        assert!(err.contains("expects an integer"), "{err}");
    }

    #[test]
    fn parse_recipe_server_serve_fixes_mode() {
        let (_, path, _) = command_of(parse_entry(&tokens(&["server", "serve"])));
        assert_eq!(path, vec!["server", "serve"]);

        let reg = OperationRegistry::global();
        let matched = find_command_match(reg, &path).unwrap();
        let input = assemble_input(&matched, &[], &GlobalOptions::default()).unwrap();
        assert_eq!(input["mode"], json!("foreground"));

        let opts = vec![RawOpt {
            flag: "--mode".into(),
            value: Some("background".into()),
        }];
        let err = assemble_input(&matched, &opts, &GlobalOptions::default())
            .expect_err("mode is fixed by recipe");
        assert!(err.contains("fixed by the `server serve` recipe"), "{err}");
    }

    #[test]
    fn parse_api_invoke_and_api_connect() {
        match parse_entry(&tokens(&[
            "api",
            "invoke",
            "run.submit",
            "--input-json",
            "-",
        ]))
        .unwrap()
        {
            ParsedEntry::ApiInvoke { method_id, globals } => {
                assert_eq!(method_id, "run.submit");
                assert_eq!(globals.input_json.as_deref(), Some("-"));
            }
            other => panic!("expected api invoke, got {other:?}"),
        }
        // missing --input-json / unknown method / bad subcommand
        assert!(parse_entry(&tokens(&["api", "invoke", "run.submit"])).is_err());
        assert!(
            parse_entry(&tokens(&[
                "api",
                "invoke",
                "no.such.op",
                "--input-json",
                "-"
            ]))
            .is_err()
        );
        assert!(parse_entry(&tokens(&["api", "frobnicate"])).is_err());
        assert!(parse_entry(&tokens(&["api", "invoke", "a", "b", "--input-json", "-"])).is_err());

        match parse_entry(&tokens(&[
            "api",
            "connect",
            "--target",
            "local",
            "--log-format",
            "json",
        ]))
        .unwrap()
        {
            ParsedEntry::ApiConnect {
                globals,
                log_format,
            } => {
                assert_eq!(globals.target.as_deref(), Some("local"));
                assert_eq!(log_format, LogFormat::Json);
            }
            other => panic!("expected api connect, got {other:?}"),
        }
        assert!(parse_entry(&tokens(&["api", "connect", "--log-format", "yaml"])).is_err());
        assert!(parse_entry(&tokens(&["api", "connect", "stray"])).is_err());
    }

    #[test]
    fn parse_help_and_version_and_bare_invocation() {
        assert!(matches!(
            parse_entry(&tokens(&["--help"])).unwrap(),
            ParsedEntry::Help
        ));
        assert!(matches!(
            parse_entry(&tokens(&["run", "list", "--help"])).unwrap(),
            ParsedEntry::Help
        ));
        assert!(matches!(
            parse_entry(&tokens(&["--version"])).unwrap(),
            ParsedEntry::Version
        ));
        assert!(matches!(parse_entry(&[]).unwrap(), ParsedEntry::Help));
        assert!(parse_entry(&tokens(&["--target", "local"])).is_err());
        assert!(parse_entry(&tokens(&["--"])).is_err());
    }

    // ---- mock client ------------------------------------------------------

    struct MockCommandClient {
        invoke_results: Mutex<VecDeque<Result<OperationResponse, ClientError>>>,
        follow_items: Mutex<VecDeque<FollowItem>>,
        recorded: Mutex<Vec<CallRequest>>,
    }

    impl MockCommandClient {
        fn new() -> Self {
            Self {
                invoke_results: Mutex::new(VecDeque::new()),
                follow_items: Mutex::new(VecDeque::new()),
                recorded: Mutex::new(Vec::new()),
            }
        }

        fn push_ok(&self, result: Value) {
            self.invoke_results
                .lock()
                .unwrap()
                .push_back(Ok(OperationResponse {
                    version: OPERATION_VERSION.to_owned(),
                    request_id: "req-mock".to_owned(),
                    outcome: ResponseOutcome::Accepted,
                    result: Some(result),
                    error: None,
                }));
        }

        fn push_err(&self, err: ClientError) {
            self.invoke_results.lock().unwrap().push_back(Err(err));
        }

        fn push_follow(&self, items: Vec<FollowItem>) {
            self.follow_items.lock().unwrap().extend(items);
        }
    }

    struct MockFollowStream {
        items: VecDeque<FollowItem>,
    }

    impl FollowStream for MockFollowStream {
        fn recv(&mut self) -> FollowRecvFuture<'_> {
            let next = self.items.pop_front();
            Box::pin(async move { next })
        }

        fn close(&mut self) -> FollowCloseFuture<'_> {
            self.items.clear();
            Box::pin(async {})
        }
    }

    impl CommandClient for MockCommandClient {
        fn call(&self, req: CallRequest) -> CallFuture {
            let result = {
                let mut results = self.invoke_results.lock().unwrap();
                self.recorded.lock().unwrap().push(req.clone());
                results
                    .pop_front()
                    .expect("mock invoke result must be queued")
            };
            let request_key = match req.request_key {
                Some(key) => Some(key),
                None if OperationRegistry::global().requires_idempotency_key(&req.method_id) => {
                    Some("local-mock-key".to_owned())
                }
                None => None,
            };
            Box::pin(async move {
                CallOutput {
                    result,
                    request_key,
                }
            })
        }

        fn open_follow(&self, method_id: &str, input: Value) -> OpenFollowFuture<'_> {
            let items = self.follow_items.lock().unwrap().clone();
            let method_id = method_id.to_owned();
            Box::pin(async move {
                // mirror the production rule: follow inputs never carry `page`
                if input.get("page").is_some() {
                    return Err(ClientError::usage("page not allowed on subscriptions"));
                }
                let _ = &method_id;
                Ok(Box::new(MockFollowStream { items }) as Box<dyn FollowStream>)
            })
        }
    }

    fn inv(method_id: &str, input: Value) -> Invocation {
        Invocation {
            method_id: method_id.to_owned(),
            input,
            format: OutputFormat::Json,
            request_key: None,
            report_generated_key: false,
            timeout: None,
            wait_exit_policy: method_id == "run.wait",
        }
    }

    #[tokio::test]
    async fn execute_invocation_prints_envelope_and_exits_zero() {
        let mock = MockCommandClient::new();
        mock.push_ok(json!({"kind": "run", "runId": "local-r1", "state": "queued"}));
        let mut out: Vec<u8> = Vec::new();
        let code = execute_invocation(
            &mock,
            inv("run.submit", json!({"sessionId": "s1"})),
            &mut out,
        )
        .await;
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).unwrap();
        let value: Value = serde_json::from_str(printed.trim()).unwrap();
        assert_eq!(value["outcome"], "accepted");
        assert_eq!(value["result"]["runId"], "local-r1");
        assert_eq!(value["version"], OPERATION_VERSION);
    }

    #[tokio::test]
    async fn execute_invocation_run_wait_exit_policy() {
        fn wait_result(state: &str, timed_out: bool) -> Value {
            json!({"conditionMet": !timed_out, "timedOut": timed_out, "run": {"state": state, "runId": "r"}})
        }
        for (state, timed_out, expected) in [
            ("succeeded", false, 0),
            ("failed", false, 1),
            ("cancelled", false, 1),
            ("interrupted", false, 1),
            ("result_unknown", false, 3),
            ("running", true, 1),
        ] {
            let mock = MockCommandClient::new();
            mock.push_ok(wait_result(state, timed_out));
            let mut out: Vec<u8> = Vec::new();
            let code =
                execute_invocation(&mock, inv("run.wait", json!({"runId": "r"})), &mut out).await;
            assert_eq!(code, expected, "state={state} timedOut={timed_out}");
            assert!(
                String::from_utf8(out).unwrap().contains("\"timedOut\""),
                "envelope printed for run.wait"
            );
        }
    }

    #[tokio::test]
    async fn execute_invocation_error_paths_print_failed_envelope_and_exit_codes() {
        // usage-class contract error -> exit 2
        let mock = MockCommandClient::new();
        mock.push_err(ClientError::usage("bad input"));
        let mut out: Vec<u8> = Vec::new();
        let code = execute_invocation(&mock, inv("run.submit", json!({})), &mut out).await;
        assert_eq!(code, EXIT_USAGE);
        let value: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        assert_eq!(value["outcome"], "failed");
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");

        // result-unknown -> exit 3
        let mock = MockCommandClient::new();
        mock.push_err(ClientError::Contract(
            agentcpm_pd_protocol::errors::ContractError::result_unknown(Some("key-1"), "timed out"),
        ));
        let mut out: Vec<u8> = Vec::new();
        let code = execute_invocation(&mock, inv("run.submit", json!({})), &mut out).await;
        assert_eq!(code, EXIT_RESULT_UNKNOWN);
        let value: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        assert_eq!(value["error"]["code"], CODE_RESULT_UNKNOWN);
        assert_eq!(value["error"]["details"]["idempotencyKey"], "key-1");
    }

    #[tokio::test]
    async fn execute_follow_emits_ndjson_and_maps_terminal_exit_codes() {
        let events = |n: u32| {
            (0..n)
                .map(|i| {
                    FollowItem::Event(json!({
                        "version": "agentcpm-pd.event/1",
                        "cursor": format!("cur-{i}"),
                        "type": "run.started",
                    }))
                })
                .collect::<Vec<_>>()
        };

        // completed -> 0
        let mock = MockCommandClient::new();
        mock.push_follow(events(2));
        mock.push_follow(vec![FollowItem::End {
            terminal: "completed".to_owned(),
            resume_cursor: Some("cur-1".to_owned()),
            error: None,
        }]);
        let mut out: Vec<u8> = Vec::new();
        let code = execute_follow(
            &mock,
            inv("run.events", json!({"runId": "r", "follow": true})),
            &mut out,
        )
        .await;
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = printed.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["type"], "run.started");

        // error terminal -> 1; slow_consumer -> 1
        let mock = MockCommandClient::new();
        mock.push_follow(vec![FollowItem::End {
            terminal: "error".to_owned(),
            resume_cursor: None,
            error: Some(ClientError::Contract(
                agentcpm_pd_protocol::errors::ContractError::new(
                    CODE_CAPABILITY_UNAVAILABLE,
                    "stream failed",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ),
            )),
        }]);
        let mut out: Vec<u8> = Vec::new();
        let code = execute_follow(
            &mock,
            inv("run.events", json!({"runId": "r", "follow": true})),
            &mut out,
        )
        .await;
        assert_eq!(code, 1);

        let mock = MockCommandClient::new();
        mock.push_follow(vec![FollowItem::End {
            terminal: "slow_consumer".to_owned(),
            resume_cursor: Some("cur-9".to_owned()),
            error: None,
        }]);
        let mut out: Vec<u8> = Vec::new();
        let code = execute_follow(
            &mock,
            inv("run.events", json!({"runId": "r", "follow": true})),
            &mut out,
        )
        .await;
        assert_eq!(code, 1);

        // open failure -> mapped exit code
        let client = MockFailOpen;
        let mut out: Vec<u8> = Vec::new();
        let code = execute_follow(
            &client,
            inv("run.events", json!({"runId": "r", "follow": true})),
            &mut out,
        )
        .await;
        assert_eq!(code, 1);
    }

    struct MockFailOpen;

    impl CommandClient for MockFailOpen {
        fn call(&self, _req: CallRequest) -> CallFuture {
            Box::pin(async {
                CallOutput {
                    result: Ok(OperationResponse {
                        version: OPERATION_VERSION.to_owned(),
                        request_id: "r".to_owned(),
                        outcome: ResponseOutcome::Succeeded,
                        result: Some(Value::Null),
                        error: None,
                    }),
                    request_key: None,
                }
            })
        }

        fn open_follow(&self, _method_id: &str, _input: Value) -> OpenFollowFuture<'_> {
            Box::pin(async {
                Err(ClientError::Contract(
                    agentcpm_pd_protocol::errors::ContractError::capability_unavailable(
                        "terminals.output",
                        "not in this round",
                    ),
                ))
            })
        }
    }

    // ---- local operations -------------------------------------------------

    #[tokio::test]
    async fn local_schema_operations_serve_from_the_embedded_registry() {
        let globals = GlobalOptions::default();
        let mut out: Vec<u8> = Vec::new();
        let code = run_local_operation(
            &globals,
            "schema.list",
            json!({"page": {"limit": 3}}),
            &mut out,
        )
        .await;
        assert_eq!(code, 0);
        let value: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        assert_eq!(value["outcome"], "succeeded");
        assert_eq!(value["result"]["methodIds"].as_array().unwrap().len(), 3);

        let mut out: Vec<u8> = Vec::new();
        let code = run_local_operation(
            &globals,
            "schema.get",
            json!({"methodId": "run.submit"}),
            &mut out,
        )
        .await;
        assert_eq!(code, 0);
        let value: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        assert_eq!(value["result"]["methodId"], "run.submit");
        assert!(value["result"]["inputSchema"]["properties"]["sessionId"].is_object());

        let mut out: Vec<u8> = Vec::new();
        let code = run_local_operation(&globals, "schema.export", json!({}), &mut out).await;
        assert_eq!(code, 0);
        let value: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        assert_eq!(value["result"]["schemaVersion"], OPERATION_VERSION);
        assert_eq!(
            value["result"]["canonicalSha256"].as_str().unwrap().len(),
            64
        );
        assert!(value["result"]["document"].is_object());

        // invalid schema.get input -> usage exit 2
        let mut out: Vec<u8> = Vec::new();
        let code = run_local_operation(&globals, "schema.get", json!({"bogus": 1}), &mut out).await;
        assert_eq!(code, 2);
    }

    #[tokio::test]
    async fn local_lifecycle_without_embedded_gateway_feature_is_unavailable() {
        #[cfg(feature = "embedded-gateway")]
        {
            // with the feature the real bootstrap runs; covered by the
            // temp-home test below instead.
        }
        #[cfg(not(feature = "embedded-gateway"))]
        {
            let globals = GlobalOptions::default();
            let mut out: Vec<u8> = Vec::new();
            let code = run_local_operation(
                &globals,
                "instance.initialize",
                json!({"dataRoot": "/nonexistent-agentcpm-pd-test"}),
                &mut out,
            )
            .await;
            assert_eq!(code, 1);
            let value: Value =
                serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
            assert_eq!(value["error"]["code"], CODE_CAPABILITY_UNAVAILABLE);
            assert!(
                value["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("embedded-gateway")
            );
        }
    }

    #[cfg(feature = "embedded-gateway")]
    #[tokio::test]
    async fn local_instance_init_creates_home_and_reports_identity() {
        use agentcpm_pd_client::config::local_metadata_path;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agentcpm-pd-cli-init-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let globals = GlobalOptions::default();
        let mut out: Vec<u8> = Vec::new();
        let code = run_local_operation(
            &globals,
            "instance.initialize",
            json!({"dataRoot": dir.to_string_lossy(), "nodeId": "local"}),
            &mut out,
        )
        .await;
        assert_eq!(code, 0);
        let value: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        assert_eq!(value["outcome"], "succeeded");
        assert_eq!(value["result"]["identity"]["nodeId"], "local");
        assert!(value["result"]["identity"]["instanceId"].is_string());
        assert!(local_metadata_path(&dir).exists());

        // re-initialization is rejected (ALREADY_EXISTS -> exit 1)
        let mut out: Vec<u8> = Vec::new();
        let code = run_local_operation(
            &globals,
            "instance.initialize",
            json!({"dataRoot": dir.to_string_lossy()}),
            &mut out,
        )
        .await;
        assert_eq!(code, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_operations_outside_current_round_are_unavailable() {
        // Registry-derived probe (substrate dynamic-count pattern): the first
        // `execution='local'` operation this build's local_ops table has not
        // registered. `target.list` gained a handler in t005, so the old
        // fixed id would break — the derived probe stays valid as the table
        // grows and skips once every local op is implemented. The registry is
        // forced first so the implemented set is current for the `find`.
        use agentcpm_pd_protocol::registry::{ExecutionMode, is_implemented};
        let _ = crate::local_ops::LocalOpRegistry::global();
        let probe = OperationRegistry::global()
            .operations()
            .iter()
            .find(|op| op.execution == ExecutionMode::Local && !is_implemented(&op.id))
            .map(|op| op.id.clone());
        if let Some(method_id) = probe {
            let globals = GlobalOptions::default();
            let mut out: Vec<u8> = Vec::new();
            let code = run_local_operation(&globals, &method_id, json!({}), &mut out).await;
            assert_eq!(code, 1);
            let value: Value =
                serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
            assert_eq!(value["error"]["code"], CODE_CAPABILITY_UNAVAILABLE);
        }
    }
}

#[cfg(test)]
mod review_fix_tests {
    use super::*;
    use agentcpm_pd_protocol::contract::ResponseOutcome;
    use agentcpm_pd_protocol::errors::{EffectStatus, RetrySpec};

    /// Minimal inline mock answering one successful invocation.
    struct CannedOkClient;

    impl CommandClient for CannedOkClient {
        fn call(&self, _req: CallRequest) -> CallFuture {
            Box::pin(async {
                CallOutput {
                    result: Ok(OperationResponse {
                        version: OPERATION_VERSION.to_owned(),
                        request_id: "req-mock".to_owned(),
                        outcome: ResponseOutcome::Accepted,
                        result: Some(json!({"kind": "run", "runId": "r1"})),
                        error: None,
                    }),
                    request_key: None,
                }
            })
        }

        fn open_follow(&self, _method_id: &str, _input: Value) -> OpenFollowFuture<'_> {
            Box::pin(async { Err(ClientError::usage("no streams in this mock")) })
        }
    }

    #[test]
    fn read_input_source_rejects_oversized_files() {
        use agentcpm_pd_protocol::wire::MAX_FRAME_BYTES;
        let dir = std::env::temp_dir().join(format!("agentcpm-pd-cli-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.json");
        std::fs::write(&big, vec![b'x'; MAX_FRAME_BYTES + 1]).unwrap();
        let err = crate::read_input_source(&big.to_string_lossy())
            .expect_err("oversized input must be rejected");
        assert!(err.contains("1 MiB"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct FailWriter;

    impl std::io::Write for FailWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("stdout gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cursor_expired_guidance_names_the_standard_recovery_flow() {
        let text = cursor_expired_guidance("subscription open was rejected");
        assert!(text.contains("run snapshot"), "{text}");
        assert!(text.contains("checkpoint.cursor"), "{text}");
        assert!(text.contains("--after-cursor"), "{text}");
    }

    struct CursorExpiredOpenClient;

    impl CommandClient for CursorExpiredOpenClient {
        fn call(&self, _req: CallRequest) -> CallFuture {
            Box::pin(async {
                CallOutput {
                    result: Ok(OperationResponse {
                        version: OPERATION_VERSION.to_owned(),
                        request_id: "r".to_owned(),
                        outcome: ResponseOutcome::Succeeded,
                        result: Some(Value::Null),
                        error: None,
                    }),
                    request_key: None,
                }
            })
        }

        fn open_follow(&self, _method_id: &str, _input: Value) -> OpenFollowFuture<'_> {
            Box::pin(async {
                Err(ClientError::Contract(
                    agentcpm_pd_protocol::errors::ContractError::new(
                        "CURSOR_EXPIRED",
                        "requested cursor expired",
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )
                    .with_details(serde_json::Map::from_iter([(
                        "gap".to_owned(),
                        json!({"kind": "retention", "recovery": "fetch_snapshot_then_resume"}),
                    )])),
                ))
            })
        }
    }

    #[tokio::test]
    async fn cursor_expired_open_rejection_exits_failed() {
        let client = CursorExpiredOpenClient;
        let mut out: Vec<u8> = Vec::new();
        let code = execute_follow(
            &client,
            inv_success("run.events", json!({"runId": "r", "follow": true})),
            &mut out,
        )
        .await;
        // Known failure, not result-unknown and not usage.
        assert_eq!(code, agentcpm_pd_protocol::errors::EXIT_FAILED);
        assert!(out.is_empty(), "no frames are printed for a rejected open");
    }

    #[tokio::test]
    async fn stdout_write_failure_exits_failed_not_zero() {
        let mock = CannedOkClient;
        let mut out = FailWriter;
        let code = execute_invocation(
            &mock,
            inv_success("run.submit", json!({"sessionId": "s1"})),
            &mut out,
        )
        .await;
        assert_eq!(code, agentcpm_pd_protocol::errors::EXIT_FAILED);
    }

    fn inv_success(method_id: &str, input: Value) -> Invocation {
        Invocation {
            method_id: method_id.to_owned(),
            input,
            format: OutputFormat::Json,
            request_key: None,
            report_generated_key: false,
            timeout: None,
            wait_exit_policy: false,
        }
    }
}
