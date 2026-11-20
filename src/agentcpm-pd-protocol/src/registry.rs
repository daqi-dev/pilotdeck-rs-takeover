//! Embedded 285-operation & command-tree registry (`WP1b`) built directly from
//! `state/cli-v1-{operations,command-tree,public-contract}.json`.

use std::collections::{BTreeMap, HashSet};
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::contract::OPERATION_VERSION;
use crate::envelope::is_control_method;
use crate::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, ContractError, EffectStatus, RetrySpec,
};
use crate::schema_validator::{
    ValidationError, validate_against_schema, validate_against_schema_with_defs,
};

pub const OPERATIONS_JSON: &str = include_str!("../../../state/cli-v1-operations.json");
pub const COMMAND_TREE_JSON: &str = include_str!("../../../state/cli-v1-command-tree.json");
pub const PUBLIC_CONTRACT_JSON: &str = include_str!("../../../state/cli-v1-public-contract.json");

/// The operations delivered by the S1/S2 substrate rounds (28 ids, the same
/// set the embedded `currentRound` lists today). Kept as a historical record:
/// S3 availability is derived from [`mark_implemented`] registrations, and
/// derived test assertions check that the implemented set covers this
/// baseline. The S3 integrator extends `currentRound` in the JSON at
/// milestones for bookkeeping only — it no longer gates availability.
pub const S2_IMPLEMENTED_OPERATIONS: [&str; 28] = [
    "schema.list",
    "schema.get",
    "system.capabilities",
    "system.health",
    "instance.initialize",
    "service.start",
    "service.status",
    "service.stop",
    "requests.lookup",
    "project.register",
    "project.get",
    "project.list",
    "session.create",
    "session.get",
    "session.list",
    "run.submit",
    "run.get",
    "run.list",
    "run.events",
    "run.snapshot",
    "run.cancel",
    "run.wait",
    "queue.get",
    "queue.resume",
    "interaction.list",
    "interaction.get",
    "interaction.respond",
    "approval.respond",
];

/// Process-wide set of operations this build registered a handler for.
/// Written once at startup by each registering component (the gateway's
/// `public_v1::domains` table and the CLI's `local_ops` table); read on every
/// availability gate afterwards.
fn implemented_ids() -> &'static RwLock<HashSet<&'static str>> {
    static IMPLEMENTED: OnceLock<RwLock<HashSet<&'static str>>> = OnceLock::new();
    IMPLEMENTED.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Marks operations as implemented by this build. Called once per process at
/// startup by the component that registers their handlers. Every id must
/// exist in the embedded 285-operation registry — a misspelled registration
/// id panics here at startup instead of silently answering `-32004` forever.
/// Re-marking an already-marked id is a no-op (duplicate registrations from
/// independent tables are fine).
///
/// # Panics
/// Panics when any id is not part of the embedded operation registry.
pub fn mark_implemented(ids: &[&'static str]) {
    let reg = OperationRegistry::global();
    // Validate before taking the write lock so a panicking registration
    // cannot poison the lock for the rest of the process.
    for id in ids {
        if reg.get(id).is_none() {
            panic!("mark_implemented: `{id}` is not in the embedded operation registry");
        }
    }
    let mut set = implemented_ids()
        .write()
        .expect("implemented-ids lock (startup marking)");
    for id in ids {
        set.insert(id);
    }
}

/// Whether this build registered a handler for the operation (see
/// [`mark_implemented`]). Availability gating — dispatch admission, stream
/// admission, local-command availability — derives from this set, not from the
/// JSON `currentRound` field.
pub fn is_implemented(id: &str) -> bool {
    implemented_ids()
        .read()
        .expect("implemented-ids lock (startup marking)")
        .contains(id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Sync,
    Async,
    Stream,
    Local,
}

impl ExecutionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Async => "async",
            Self::Stream => "stream",
            Self::Local => "local",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RetryKind {
    ReadOnly,
    IdempotentSubmit,
    NonReplayable,
    ConditionalWrite,
}

impl RetryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "readOnly",
            Self::IdempotentSubmit => "idempotentSubmit",
            Self::NonReplayable => "nonReplayable",
            Self::ConditionalWrite => "conditionalWrite",
        }
    }
}

#[derive(Clone, Debug)]
pub struct OperationSpec {
    pub id: String,
    pub title: String,
    pub execution: ExecutionMode,
    pub retry: RetryKind,
    pub errors: Vec<String>,
    pub input_schema: Value,
    pub result_schema: Value,
    pub capability_ids: Vec<String>,
    pub declares_instance_id: bool,
    pub is_current_round: bool,
    pub is_control: bool,
    pub input_schema_digest: String,
    pub result_schema_digest: String,
}

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub path: Vec<String>,
    pub operation: String,
    pub parameter_bindings: BTreeMap<String, String>,
    pub required_input: Vec<String>,
    pub secure_input_fields: Vec<String>,
    pub derived_inputs: BTreeMap<String, String>,
    pub input_mode: String,
    pub guess_prompt_from_first_word: bool,
    pub convenience_flags: BTreeMap<String, Value>,
    pub exit_policy: Option<Value>,
    pub condition: Option<String>,
    pub caller: String,
    pub status: String,
}

#[derive(Clone, Debug)]
pub struct CommandRecipe {
    pub path: Vec<String>,
    pub operation: String,
    pub fixed_params: Map<String, Value>,
    pub description: String,
}

#[derive(Debug)]
pub struct OperationRegistry {
    operations: Vec<OperationSpec>,
    by_id: BTreeMap<String, usize>,
    commands: Vec<CommandSpec>,
    by_path: BTreeMap<Vec<String>, usize>,
    recipes: Vec<CommandRecipe>,
    recipes_by_path: BTreeMap<Vec<String>, usize>,
    current_round_ids: Vec<String>,
    control_method_ids: Vec<String>,
    aliases: BTreeMap<String, String>,
    public_contract: Value,
    contract_defs: Map<String, Value>,
    registry_digest: String,
}

#[derive(Deserialize)]
struct RawOperationsDoc {
    operations: Vec<RawOperation>,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    #[serde(rename = "currentRound")]
    current_round: RawCurrentRound,
}

#[derive(Deserialize)]
struct RawCurrentRound {
    operations: Vec<String>,
}

#[derive(Deserialize)]
struct RawOperation {
    id: String,
    title: String,
    execution: ExecutionMode,
    retry: RetryKind,
    #[serde(default)]
    errors: Vec<String>,
    input: Value,
    result: Value,
    #[serde(rename = "capabilityIds", default)]
    capability_ids: Vec<String>,
}

#[derive(Deserialize)]
struct RawCommandTreeDoc {
    commands: Vec<RawCommand>,
    #[serde(default)]
    recipes: Vec<RawRecipe>,
}

#[derive(Deserialize)]
struct RawCommand {
    path: Vec<String>,
    operation: String,
    #[serde(rename = "parameterBindings", default)]
    parameter_bindings: BTreeMap<String, String>,
    #[serde(rename = "requiredInput", default)]
    required_input: Vec<String>,
    #[serde(rename = "secureInputFields", default)]
    secure_input_fields: Vec<String>,
    #[serde(rename = "derivedInputs", default)]
    derived_inputs: BTreeMap<String, String>,
    #[serde(rename = "inputMode", default)]
    input_mode: String,
    #[serde(rename = "guessPromptFromFirstWord", default)]
    guess_prompt_from_first_word: bool,
    #[serde(rename = "convenienceFlags", default)]
    convenience_flags: BTreeMap<String, Value>,
    #[serde(rename = "exitPolicy", default)]
    exit_policy: Option<Value>,
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    caller: String,
    #[serde(default)]
    status: String,
}

#[derive(Deserialize)]
struct RawRecipe {
    path: Vec<String>,
    operation: String,
    #[serde(rename = "fixedParams", default)]
    fixed_params: Map<String, Value>,
    #[serde(default)]
    description: String,
}

impl OperationRegistry {
    /// Returns the process-wide embedded registry parsed once from compiled JSON assets.
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<OperationRegistry> = OnceLock::new();
        INSTANCE.get_or_init(Self::load_embedded)
    }

    fn load_embedded() -> Self {
        let ops_doc: RawOperationsDoc =
            serde_json::from_str(OPERATIONS_JSON).expect("valid embedded cli-v1-operations.json");
        let tree_doc: RawCommandTreeDoc = serde_json::from_str(COMMAND_TREE_JSON)
            .expect("valid embedded cli-v1-command-tree.json");
        let public_contract: Value = serde_json::from_str(PUBLIC_CONTRACT_JSON)
            .expect("valid embedded cli-v1-public-contract.json");

        let contract_defs = public_contract
            .get("schema")
            .and_then(|s| s.get("$defs"))
            .and_then(Value::as_object)
            .cloned()
            .expect("public-contract schema.$defs must exist");

        let current_round_ids = ops_doc.current_round.operations.clone();
        let current_round_set: std::collections::BTreeSet<&str> =
            current_round_ids.iter().map(String::as_str).collect();

        let mut operations = Vec::with_capacity(ops_doc.operations.len());
        let mut by_id = BTreeMap::new();
        let mut control_method_ids = Vec::new();

        for raw in ops_doc.operations {
            let declares_instance_id = raw
                .input
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|props| props.contains_key("instanceId"));
            let is_current_round = current_round_set.contains(raw.id.as_str());
            let is_control = is_control_method(&raw.id);
            if is_control {
                control_method_ids.push(raw.id.clone());
            }

            let input_schema_digest = sha256_value(&raw.input);
            let result_schema_digest = sha256_value(&raw.result);
            let idx = operations.len();
            by_id.insert(raw.id.clone(), idx);
            operations.push(OperationSpec {
                id: raw.id,
                title: raw.title,
                execution: raw.execution,
                retry: raw.retry,
                errors: raw.errors,
                input_schema: raw.input,
                result_schema: raw.result,
                capability_ids: raw.capability_ids,
                declares_instance_id,
                is_current_round,
                is_control,
                input_schema_digest,
                result_schema_digest,
            });
        }

        let mut commands = Vec::with_capacity(tree_doc.commands.len());
        let mut by_path = BTreeMap::new();
        for raw in tree_doc.commands {
            let idx = commands.len();
            by_path.insert(raw.path.clone(), idx);
            commands.push(CommandSpec {
                path: raw.path,
                operation: raw.operation,
                parameter_bindings: raw.parameter_bindings,
                required_input: raw.required_input,
                secure_input_fields: raw.secure_input_fields,
                derived_inputs: raw.derived_inputs,
                input_mode: raw.input_mode,
                guess_prompt_from_first_word: raw.guess_prompt_from_first_word,
                convenience_flags: raw.convenience_flags,
                exit_policy: raw.exit_policy,
                condition: raw.condition,
                caller: raw.caller,
                status: raw.status,
            });
        }

        let mut recipes = Vec::with_capacity(tree_doc.recipes.len());
        let mut recipes_by_path = BTreeMap::new();
        for raw in tree_doc.recipes {
            let idx = recipes.len();
            recipes_by_path.insert(raw.path.clone(), idx);
            recipes.push(CommandRecipe {
                path: raw.path,
                operation: raw.operation,
                fixed_params: raw.fixed_params,
                description: raw.description,
            });
        }

        let mut hasher = Sha256::new();
        hasher.update(OPERATIONS_JSON.as_bytes());
        hasher.update([0u8]);
        hasher.update(COMMAND_TREE_JSON.as_bytes());
        let registry_digest = format!("{:x}", hasher.finalize());

        Self {
            operations,
            by_id,
            commands,
            by_path,
            recipes,
            recipes_by_path,
            current_round_ids,
            control_method_ids,
            aliases: ops_doc.aliases,
            public_contract,
            contract_defs,
            registry_digest,
        }
    }

    pub fn operations(&self) -> &[OperationSpec] {
        &self.operations
    }

    pub fn commands(&self) -> &[CommandSpec] {
        &self.commands
    }

    pub fn recipes(&self) -> &[CommandRecipe] {
        &self.recipes
    }

    pub fn current_round_ids(&self) -> &[String] {
        &self.current_round_ids
    }

    pub fn control_method_ids(&self) -> &[String] {
        &self.control_method_ids
    }

    pub fn aliases(&self) -> &BTreeMap<String, String> {
        &self.aliases
    }

    pub fn public_contract(&self) -> &Value {
        &self.public_contract
    }

    pub fn contract_defs(&self) -> &Map<String, Value> {
        &self.contract_defs
    }

    pub fn registry_digest(&self) -> &str {
        &self.registry_digest
    }

    pub fn get(&self, method_id: &str) -> Option<&OperationSpec> {
        self.by_id.get(method_id).map(|idx| &self.operations[*idx])
    }

    pub fn find_command(&self, path: &[&str]) -> Option<&CommandSpec> {
        let key: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
        self.by_path.get(&key).map(|idx| &self.commands[*idx])
    }

    pub fn find_recipe(&self, path: &[&str]) -> Option<&CommandRecipe> {
        let key: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
        self.recipes_by_path
            .get(&key)
            .map(|idx| &self.recipes[*idx])
    }

    pub fn declares_instance_id(&self, method_id: &str) -> bool {
        self.get(method_id)
            .is_some_and(|op| op.declares_instance_id)
    }

    pub fn requires_idempotency_key(&self, method_id: &str) -> bool {
        self.get(method_id)
            .is_some_and(|op| op.retry == RetryKind::IdempotentSubmit)
    }

    pub fn schema_digest(&self, method_id: &str) -> Option<&str> {
        self.get(method_id)
            .map(|op| op.input_schema_digest.as_str())
    }

    pub fn validate_input(
        &self,
        method_id: &str,
        input: &Value,
    ) -> Result<(), Vec<ValidationError>> {
        let Some(op) = self.get(method_id) else {
            return Err(vec![ValidationError {
                path: "$".to_owned(),
                message: format!("unknown operation `{method_id}`"),
            }]);
        };
        validate_against_schema(input, &op.input_schema)
    }

    pub fn validate_result(
        &self,
        method_id: &str,
        result: &Value,
    ) -> Result<(), Vec<ValidationError>> {
        let Some(op) = self.get(method_id) else {
            return Err(vec![ValidationError {
                path: "$".to_owned(),
                message: format!("unknown operation `{method_id}`"),
            }]);
        };
        validate_against_schema(result, &op.result_schema)
    }

    pub fn validate_contract_def(
        &self,
        def_name: &str,
        value: &Value,
    ) -> Result<(), Vec<ValidationError>> {
        let Some(schema) = self.contract_defs.get(def_name) else {
            return Err(vec![ValidationError {
                path: "$".to_owned(),
                message: format!("unknown contract definition `{def_name}`"),
            }]);
        };
        validate_against_schema_with_defs(value, schema, &self.contract_defs)
    }

    /// Executes `schema.list` locally from the embedded registry.
    pub fn handle_schema_list(&self, input: &Value) -> Result<Value, ContractError> {
        self.validate_input("schema.list", input)
            .map_err(validation_errors_to_contract_error)?;

        let obj = input.as_object();
        if let Some(version) = obj
            .and_then(|m| m.get("schemaVersion"))
            .and_then(Value::as_str)
            && version != OPERATION_VERSION
        {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("unsupported schemaVersion `{version}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let domain = obj.and_then(|m| m.get("domain")).and_then(Value::as_str);
        let page = obj.and_then(|m| m.get("page")).and_then(Value::as_object);
        let limit = page
            .and_then(|p| p.get("limit"))
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 500) as usize;
        let offset = match page.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
            Some(cursor) => cursor.parse::<usize>().map_err(|_| {
                ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    format!("invalid pagination cursor `{cursor}`"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?,
            None => 0,
        };

        let mut method_ids: Vec<&str> = self
            .operations
            .iter()
            .filter(|op| match domain {
                Some(d) => op.id.split('.').next() == Some(d),
                None => true,
            })
            .map(|op| op.id.as_str())
            .collect();
        method_ids.sort_unstable();

        if offset > method_ids.len() {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                "pagination cursor beyond available range",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let end = offset.saturating_add(limit).min(method_ids.len());
        let slice: Vec<&str> = method_ids[offset..end].to_vec();
        let complete = end >= method_ids.len();
        let next_cursor = if complete {
            Value::Null
        } else {
            Value::String(end.to_string())
        };

        Ok(json!({
            "methodIds": slice,
            "schemaVersion": OPERATION_VERSION,
            "pageInfo": {
                "snapshotRevision": format!("schema-{}", &self.registry_digest[..12]),
                "nextCursor": next_cursor,
                "complete": complete,
            }
        }))
    }

    /// Executes `schema.get` locally from the embedded registry.
    pub fn handle_schema_get(&self, input: &Value) -> Result<Value, ContractError> {
        self.validate_input("schema.get", input)
            .map_err(validation_errors_to_contract_error)?;

        let obj = input.as_object().expect("validated schema.get object");
        if let Some(version) = obj.get("schemaVersion").and_then(Value::as_str)
            && version != OPERATION_VERSION
        {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("unsupported schemaVersion `{version}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let method_id = obj
            .get("methodId")
            .and_then(Value::as_str)
            .expect("validated required methodId");
        let Some(op) = self.get(method_id) else {
            return Err(ContractError::new(
                CODE_NOT_FOUND,
                format!("unknown methodId `{method_id}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        };

        Ok(json!({
            "methodId": op.id,
            "schemaVersion": OPERATION_VERSION,
            "inputSchema": op.input_schema,
            "resultSchema": op.result_schema,
            "errorCodes": op.errors,
            "execution": op.execution.as_str(),
            "retry": op.retry.as_str(),
        }))
    }
}

fn sha256_value(v: &Value) -> String {
    let serialized = serde_json::to_string(v).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    format!("{:x}", hasher.finalize())
}

const MAX_REPORTED_VALIDATION_ERRORS: usize = 32;
const MAX_VALIDATION_FIELD_BYTES: usize = 512;

fn truncate_utf8_bytes(mut s: String, max_bytes: usize) -> String {
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("...");
    }
    s
}

pub fn validation_errors_to_contract_error(errors: Vec<ValidationError>) -> ContractError {
    let total_errors = errors.len();
    let capped: Vec<ValidationError> = errors
        .into_iter()
        .take(MAX_REPORTED_VALIDATION_ERRORS)
        .map(|err| ValidationError {
            path: truncate_utf8_bytes(err.path, MAX_VALIDATION_FIELD_BYTES),
            message: truncate_utf8_bytes(err.message, MAX_VALIDATION_FIELD_BYTES),
        })
        .collect();
    let summary = capped
        .iter()
        .take(3)
        .map(|e| format!("{}: {}", e.path, e.message))
        .collect::<Vec<_>>()
        .join("; ");
    let mut details = Map::new();
    details.insert(
        "validationErrors".to_owned(),
        serde_json::to_value(&capped).unwrap_or(Value::Null),
    );
    if total_errors > capped.len() {
        details.insert("validationErrorsTruncated".to_owned(), json!(true));
        details.insert("totalValidationErrors".to_owned(), json!(total_errors));
    }
    let msg = if summary.is_empty() {
        "schema validation failed".to_owned()
    } else {
        summary
    };
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        msg,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
    .with_details(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_registry_has_285_ops_11_control_ops_and_derived_round_consistency() {
        let reg = OperationRegistry::global();
        assert_eq!(reg.operations().len(), 285);
        assert_eq!(reg.commands().len(), 285);
        assert_eq!(reg.control_method_ids().len(), 11);

        // currentRound consistency is derived, not a fixed count: the parsed
        // list equals the embedded JSON and stays a subset of the full set.
        // The S3 integrator extends currentRound at milestones (record
        // keeping only); the count is not a design constant.
        let raw: Value = serde_json::from_str(OPERATIONS_JSON).expect("operations json");
        let json_round: Vec<String> = raw["currentRound"]["operations"]
            .as_array()
            .expect("currentRound.operations")
            .iter()
            .map(|v| v.as_str().expect("currentRound id").to_owned())
            .collect();
        assert_eq!(reg.current_round_ids(), json_round.as_slice());
        for id in reg.current_round_ids() {
            assert!(reg.get(id).is_some(), "missing currentRound op {id}");
        }
        for id in reg.control_method_ids() {
            assert!(reg.get(id).is_some(), "missing control op {id}");
        }
        for cmd in reg.commands() {
            assert!(
                reg.get(&cmd.operation).is_some(),
                "command {:?} references missing op {}",
                cmd.path,
                cmd.operation
            );
        }

        // Spot-check conditional instanceId injection metadata
        assert!(reg.declares_instance_id("run.submit"));
        assert!(reg.declares_instance_id("system.capabilities"));
        assert!(!reg.declares_instance_id("schema.list"));
        assert!(!reg.declares_instance_id("schema.get"));
        assert!(!reg.declares_instance_id("service.status"));

        assert!(reg.requires_idempotency_key("run.submit"));
        assert!(reg.requires_idempotency_key("run.cancel"));
        assert!(!reg.requires_idempotency_key("run.get"));
        assert!(!reg.requires_idempotency_key("project.register"));

        // Implemented set (S3 substrate): the S2 baseline constant is within
        // the full 285 set and stays inside currentRound (the milestone
        // bookkeeping never drops the substrate baseline), and marking it
        // makes exactly those ids answer `is_implemented` (this test binary
        // registers nothing else).
        let current_round: std::collections::BTreeSet<&str> =
            json_round.iter().map(String::as_str).collect();
        for id in S2_IMPLEMENTED_OPERATIONS {
            assert!(
                reg.get(id).is_some(),
                "S2 constant op {id} missing from the registry"
            );
            assert!(
                current_round.contains(id),
                "S2 constant op {id} dropped from currentRound"
            );
        }
        mark_implemented(&S2_IMPLEMENTED_OPERATIONS);
        for id in S2_IMPLEMENTED_OPERATIONS {
            assert!(is_implemented(id), "marked S2 op {id}");
        }
        assert!(!is_implemented("terminals.create"));
    }

    #[test]
    fn mark_implemented_dedupes_repeated_registrations() {
        // Independent tables may register the same id twice; duplicates are
        // a no-op, and the id stays implemented.
        mark_implemented(&["schema.list", "schema.list"]);
        assert!(is_implemented("schema.list"));
    }

    #[test]
    #[should_panic(expected = "not in the embedded operation registry")]
    fn mark_implemented_panics_on_unknown_id() {
        // Validation fires before the lock is taken, so a panicking
        // registration cannot poison the global set for other tests.
        mark_implemented(&["definitely.not.an.operation"]);
    }

    #[test]
    fn public_contract_positive_and_negative_examples_match_validator() {
        let reg = OperationRegistry::global();
        let pc = reg.public_contract();

        let examples = pc["examples"].as_array().expect("pc.examples");
        assert_eq!(examples.len(), 9);
        for ex in examples {
            let name = ex["name"].as_str().unwrap();
            let def = ex["schema"].as_str().unwrap();
            let val = &ex["value"];
            let res = reg.validate_contract_def(def, val);
            assert!(
                res.is_ok(),
                "positive contract example `{name}` against `{def}` failed: {res:?}"
            );
        }

        let negatives = pc["negativeExamples"]
            .as_array()
            .expect("pc.negativeExamples");
        assert_eq!(negatives.len(), 9);
        for ex in negatives {
            let name = ex["name"].as_str().unwrap();
            let def = ex["schema"].as_str().unwrap();
            let val = &ex["value"];
            let res = reg.validate_contract_def(def, val);
            assert!(
                res.is_err(),
                "negative contract example `{name}` against `{def}` unexpectedly passed"
            );
        }

        let op_examples = pc["operationExamples"]
            .as_array()
            .expect("pc.operationExamples");
        assert_eq!(op_examples.len(), 8);
        for ex in op_examples {
            let name = ex["name"].as_str().unwrap();
            let op_id = ex["operation"].as_str().unwrap();
            let field = ex["field"].as_str().unwrap();
            let expected_valid = ex["valid"].as_bool().unwrap();
            let val = &ex["value"];
            let res = match field {
                "input" => reg.validate_input(op_id, val),
                "result" => reg.validate_result(op_id, val),
                other => panic!("unexpected example field {other}"),
            };
            assert_eq!(
                res.is_ok(),
                expected_valid,
                "operationExample `{name}` ({op_id}.{field}) expected valid={expected_valid}, got {res:?}"
            );
        }
    }

    #[test]
    fn schema_list_and_schema_get_roundtrip_and_validate() {
        let reg = OperationRegistry::global();

        let page1 = reg
            .handle_schema_list(&json!({"page": {"limit": 100}}))
            .expect("page1");
        assert!(reg.validate_result("schema.list", &page1).is_ok());
        assert_eq!(page1["methodIds"].as_array().unwrap().len(), 100);
        assert_eq!(page1["pageInfo"]["complete"], false);
        let cursor1 = page1["pageInfo"]["nextCursor"].as_str().unwrap();

        let page2 = reg
            .handle_schema_list(&json!({"page": {"limit": 100, "cursor": cursor1}}))
            .expect("page2");
        assert!(reg.validate_result("schema.list", &page2).is_ok());
        let cursor2 = page2["pageInfo"]["nextCursor"].as_str().unwrap();

        let page3 = reg
            .handle_schema_list(&json!({"page": {"limit": 100, "cursor": cursor2}}))
            .expect("page3");
        assert!(reg.validate_result("schema.list", &page3).is_ok());
        assert_eq!(page3["methodIds"].as_array().unwrap().len(), 85);
        assert_eq!(page3["pageInfo"]["complete"], true);
        assert!(page3["pageInfo"]["nextCursor"].is_null());

        let filtered = reg
            .handle_schema_list(&json!({"domain": "schema"}))
            .expect("domain filter");
        let ids: Vec<&str> = filtered["methodIds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["schema.export", "schema.get", "schema.list"]);

        for id in reg.current_round_ids() {
            let res = reg
                .handle_schema_get(&json!({"methodId": id}))
                .expect("schema.get");
            assert!(
                reg.validate_result("schema.get", &res).is_ok(),
                "schema.get result invalid for {id}"
            );
        }

        let err = reg
            .handle_schema_get(&json!({"methodId": "does.not.exist"}))
            .unwrap_err();
        assert_eq!(err.code, CODE_NOT_FOUND);
    }

    #[test]
    fn negative_controls_catch_injected_schema_violations() {
        let reg = OperationRegistry::global();

        // Unknown field rejected when additionalProperties: false
        let err = reg
            .validate_input("schema.list", &json!({"instanceId": "local-1"}))
            .unwrap_err();
        assert!(err.iter().any(|e| e.path.contains("instanceId")));

        // Valid vs invalid date-time on real schema field (`$defs.interaction.expiresAt`)
        let valid_interaction = json!({
            "instanceId": "local-1",
            "projectId": "proj-1",
            "runId": "run-1",
            "interactionId": "ia-1",
            "revision": "rev-00000001",
            "kind": "approval",
            "state": "pending",
            "choices": ["allow", "deny"],
            "expiresAt": "2026-01-01T12:34:56Z"
        });
        assert!(
            reg.validate_contract_def("interaction", &valid_interaction)
                .is_ok(),
            "baseline interaction must pass"
        );
        for bad_ts in [
            "not-a-timestamp",
            "2026-01-01T12:34:中Z",
            "+202-+1-+2T+1:+2:+3++1:+2",
        ] {
            let mut bad_ia = valid_interaction.clone();
            bad_ia["expiresAt"] = json!(bad_ts);
            let errs = reg
                .validate_contract_def("interaction", &bad_ia)
                .expect_err("invalid date-time must fail");
            assert!(
                errs.iter()
                    .any(|e| e.path.contains("expiresAt") && e.message.contains("date-time")),
                "expected expiresAt date-time error for `{bad_ts}`, got {errs:?}"
            );
        }

        // Conditional mismatch in system.capabilities result (`available: true` requires `supported: true`)
        let valid_caps = json!({
            "identity": {
                "instanceId": "inst-1",
                "ownerId": "owner-1",
                "product": "agentcpm-pd",
                "protocolVersion": "agentcpm-pd.operation/1"
            },
            "generation": "gen-1",
            "runtimeKind": "native_rust",
            "configurationState": "ready",
            "capabilities": [{
                "methodId": "run.submit",
                "schemaVersion": "agentcpm-pd.operation/1",
                "supported": true,
                "configured": true,
                "authorized": true,
                "available": true,
                "reason": null,
                "requiredPermission": [],
                "limits": []
            }]
        });
        assert!(
            reg.validate_result("system.capabilities", &valid_caps)
                .is_ok(),
            "valid system.capabilities must pass"
        );
        let mut bad_caps = valid_caps.clone();
        bad_caps["capabilities"][0]["supported"] = json!(false);
        let cap_ready = reg
            .validate_result("system.capabilities", &bad_caps)
            .expect_err("available:true with supported:false must trigger if/then error");
        assert!(
            cap_ready
                .iter()
                .any(|e| e.path.contains("capabilities[0].supported")
                    && e.message.contains("constant")),
            "expected conditional const error on capabilities[0].supported, got {cap_ready:?}"
        );
    }

    #[test]
    fn schema_validator_and_error_bounds_protect_against_dos_and_oversize() {
        use crate::schema_validator::{is_valid_rfc3339_date_time, validate_against_schema};
        use crate::wire::MAX_FRAME_BYTES;

        assert!(is_valid_rfc3339_date_time("2026-11-21T00:00:00.123+08:00"));
        assert!(!is_valid_rfc3339_date_time("2026-01-01T12:34:中Z"));
        assert!(!is_valid_rfc3339_date_time("+202-+1-+2T+1:+2:+3++1:+2"));

        // `not` + excessive $ref depth must abort fail-closed instead of bypassing `not`.
        let mut defs = serde_json::Map::new();
        defs.insert("Loop".to_owned(), json!({"$ref": "#/$defs/Loop"}));
        let not_deep = json!({
            "$defs": defs,
            "not": {"$ref": "#/$defs/Loop"}
        });
        let err = validate_against_schema(&json!("anything"), &not_deep).unwrap_err();
        assert!(
            err.iter().any(|e| e.message.contains("depth")),
            "expected depth abort on `not` recursion, got {err:?}"
        );

        // Diamond DAG with exponential branches must terminate within step budget.
        let mut dag_defs = serde_json::Map::new();
        for i in 0..30 {
            let next = format!("#/$defs/N{}", i + 1);
            dag_defs.insert(
                format!("N{i}"),
                json!({"anyOf": [{"$ref": next}, {"$ref": next}]}),
            );
        }
        dag_defs.insert("N30".to_owned(), json!({"type": "integer"}));
        let dag_schema = json!({"$defs": dag_defs, "$ref": "#/$defs/N0"});
        let dag_err = validate_against_schema(&json!("not-an-int"), &dag_schema).unwrap_err();
        assert!(
            dag_err.iter().any(|e| e.message.contains("step budget")),
            "expected step budget abort on diamond DAG, got {dag_err:?}"
        );

        // Large uniqueItems array runs in linear time.
        let unique_schema =
            json!({"type": "array", "uniqueItems": true, "items": {"type": "string"}});
        let big_unique: Vec<Value> = (0..10_000).map(|i| json!(format!("item-{i}"))).collect();
        assert!(validate_against_schema(&Value::Array(big_unique), &unique_schema).is_ok());

        // Huge validation error list stays bounded well below MAX_FRAME_BYTES and 256-char message.
        let reg = OperationRegistry::global();
        let mut bad_map = serde_json::Map::new();
        for i in 0..500 {
            bad_map.insert(format!("unknown_{i}_{}", "x".repeat(300)), json!(i));
        }
        let errs = reg
            .validate_input("schema.list", &Value::Object(bad_map))
            .unwrap_err();
        let ce = validation_errors_to_contract_error(errs);
        assert!(ce.message.chars().count() <= 256);
        let encoded = serde_json::to_string(&ce).unwrap();
        assert!(encoded.len() < MAX_FRAME_BYTES / 4);
    }
}
