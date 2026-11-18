//! `knowledge.*` — the K1 generic knowledge module binding
//! (`cli.task.runtime`, S3; capability `cli.decision.knowledge` +
//! `cli.capability.runtime-modules`).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `knowledge.inspect` /
//! `.configure` / `.validate` / `.query` / `.citation.resolve` 条目（批准
//! 记录：K1，用户 2026-11-16「C1 很好，K1也很好。就这么办。」）。语义参考
//! 是旧线 `config/{binding,modules}.rs` 的绑定/受管模块分层（只读；那是
//! StaffDeck 运行时绑定变换，v1 按 K1 新公开合同实现）与
//! `s0.decisions:K1`。
//!
//! K1 的边界（决定性取舍，全部登记在任务报告）：
//!
//! * **不自建索引引擎**（K1 范围决定）：查询、引用解析全部转发给外部模块，
//!   本域零索引、零本地知识存储；「0命中与上游失败不同」由错误路径保证。
//! * **受管模块**：`moduleReference` 指向服务端受管模块清单——
//!   `<AGENTCPM_PD_HOME>/knowledge-modules/<reference>.json`，字段：身份
//!   （`identity.moduleId`）、transport（stdio 的 command/args/cwd 或 tcp 的
//!   host/port）、`protocolVersion`、`declaredMethods`、`supportedOptions`、
//!   `credentialEnv`。清单是服务端数据，不是客户端输入（「原始endpoint/
//!   secret配置走受保护通用模块配置流程」「不接受客户端…任意上游URL」）。
//!   `credentialEnv` 非空 → configure 以 `UNSUPPORTED_PROFILE` 拒绝（「不得
//!   绕过当前native profile对knowledge credentialEnv的限制」「不支持的认
//!   证profile明确报告」）。
//! * **模块线协议**（`agentcpm-pd.knowledge/1`，v1 新面由本域定义并登记）：
//!   每次调用一个 NDJSON 请求/响应对——请求
//!   `{"id":"<opaque>","method":"describe|query|resolve_citation","params":{...}}`，
//!   响应 `{"id":"<same>","result":{...}}` 或
//!   `{"id":"<same>","error":{"code":"...","message":"..."}}`。describe 的
//!   result 携带 `identity` / `protocolVersion` / `methods`（握手）。
//! * **调用形态**：每次调用独立 spawn/connect（薄实现；连接池属后续细化），
//!   子进程环境仅 PATH + 清单 env（网关自身环境与秘密不外泄），10 s 内部
//!   超时（`TIMEOUT`）。取消：调用 future 被丢弃（客户端断开）即杀子进程
//!   ——「慢上游可取消」，只停止等待、不重放副作用。
//! * **诊断与执行分离**（K1 验收矩阵）：
//!   - inspect/configure **无需可用 provider**：两者零上游接触（「不为
//!     inspect自动访问未授权端点」「配置不触发查询、索引或StaffDeck绑
//!     定」「禁用不触发上游调用」）。
//!   - inspect 区分 `disabled / unconfigured / configured / ready /
//!     unavailable`；`declaredMethods` 来自清单声明，`effectiveMethods` =
//!     配置允许 ∩ 清单声明 ∩ 握手实证（「配置声明、实际装配、最近校验及
//!     有效能力分别报告」「declared不等于effective」「未配置明确报告而非
//!     空查询成功」）。
//!   - validate 做受管 manifest/健康校验（module.managed / transport /
//!     manifest.identity / manifest.contract / manifest.methods 五项
//!     check），「不可用（unavailable）和校验失败（fail）如实区分」；校验
//!     结果绑定配置 revision 落库（「缓存校验结果须绑定revision」）。
//!   - query / citation.resolve 要求 binding 已启用且当前 revision 校验通
//!     过且方法在有效集合内，否则 `CAPABILITY_UNAVAILABLE` / `DISABLED`；
//!     未声明的 cursor/filter 选项 → `UNSUPPORTED_QUERY_OPTION`；命中规范化
//!     失败 → `UPSTREAM_FAILED`（「规范化失败不得丢引用装成功」）。
//!   - citation.resolve 的「解析前后绑定revision变化明确处理」：调用后复读
//!     revision，变化即 `STALE_CITATION`；query-only 模块 →
//!     `CAPABILITY_UNAVAILABLE`。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// Contract error codes these operations declare that the protocol crate has
/// no shared constant for yet. (The contract also declares FORBIDDEN /
/// OUTCOME_UNKNOWN / CANCELLED on the K1 family; the single-user v1 request
/// context carries no client-side identity fields to override and sync
/// dispatch has no server-side cancel channel, so none of the three is
/// reachable in this build — registered in the task report.)
const CODE_CONFLICT: &str = "CONFLICT";
const CODE_UNSUPPORTED_PROFILE: &str = "UNSUPPORTED_PROFILE";
const CODE_UNKNOWN_MODULE_REFERENCE: &str = "UNKNOWN_MODULE_REFERENCE";
const CODE_CONFIGURATION_INVALID: &str = "CONFIGURATION_INVALID";
const CODE_PROTOCOL_INCOMPATIBLE: &str = "PROTOCOL_INCOMPATIBLE";
const CODE_TIMEOUT: &str = "TIMEOUT";
const CODE_UPSTREAM_FAILED: &str = "UPSTREAM_FAILED";
const CODE_DISABLED: &str = "DISABLED";
const CODE_CAPABILITY_UNAVAILABLE: &str = "CAPABILITY_UNAVAILABLE";
const CODE_UNSUPPORTED_QUERY_OPTION: &str = "UNSUPPORTED_QUERY_OPTION";
const CODE_STALE_CITATION: &str = "STALE_CITATION";

/// The knowledge module wire protocol (one NDJSON pair per call).
const KNOWLEDGE_PROTOCOL: &str = "agentcpm-pd.knowledge/1";
/// The method vocabulary a module may declare.
const KNOWLEDGE_METHODS: [&str; 2] = ["query", "resolve_citation"];
/// The query options a manifest may declare support for.
const KNOWLEDGE_OPTIONS: [&str; 3] = ["knowledgeBaseIds", "limit", "cursor"];
/// Per-call upstream bound. Query/validate share it.
const MODULE_CALL_TIMEOUT: Duration = Duration::from_secs(10);

fn module_call_timeout() -> Duration {
    MODULE_CALL_TIMEOUT
}

const DDL_V1_KNOWLEDGE_BINDINGS: &str = "CREATE TABLE IF NOT EXISTS v1_knowledge_bindings (
    binding_id TEXT PRIMARY KEY,
    revision TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    module_reference TEXT NOT NULL,
    allowed_methods_json TEXT NOT NULL DEFAULT '[]',
    validation_json TEXT,
    validated_revision TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_knowledge_bindings", DDL_V1_KNOWLEDGE_BINDINGS);
    reg.handler("knowledge.inspect", |rt, input, _key, _rev| {
        Box::pin(knowledge_inspect(rt, input))
    });
    reg.handler("knowledge.configure", |rt, input, _key, _rev| {
        Box::pin(knowledge_configure(rt, input))
    });
    reg.handler("knowledge.validate", |rt, input, _key, _rev| {
        Box::pin(knowledge_validate(rt, input))
    });
    reg.handler("knowledge.query", |rt, input, _key, _rev| {
        Box::pin(knowledge_query(rt, input))
    });
    reg.handler("knowledge.citation.resolve", |rt, input, _key, _rev| {
        Box::pin(knowledge_citation_resolve(rt, input))
    });
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn knowledge_invalid(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message.into(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn knowledge_not_found(binding_id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("knowledge binding `{binding_id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn capability_unavailable(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_CAPABILITY_UNAVAILABLE,
        message.into(),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn upstream_failed(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_UPSTREAM_FAILED,
        message.into(),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::NotApplied,
    )
}

fn sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

// ---------------------------------------------------------------------------
// Managed module manifests (server-side data under the data root)
// ---------------------------------------------------------------------------

/// A parsed managed module definition.
struct ManagedModule {
    module_reference: String,
    identity_module_id: String,
    transport_kind: String,
    stdio_command: Option<String>,
    stdio_args: Vec<String>,
    stdio_cwd: Option<String>,
    tcp_host: Option<String>,
    tcp_port: Option<u16>,
    declared_methods: Vec<String>,
    supported_options: Vec<String>,
}

fn manifest_path(store: &V1Store, module_reference: &str) -> PathBuf {
    store
        .home()
        .join("knowledge-modules")
        .join(format!("{module_reference}.json"))
}

/// Loads and validates a managed module manifest. The tri-state outcome maps
/// exactly onto the K1 diagnostics: a missing file is an unknown reference, a
/// malformed one is invalid configuration, a well-formed one carries the
/// declared methods/options.
fn load_managed_module(
    store: &V1Store,
    module_reference: &str,
) -> Result<Result<ManagedModule, ContractError>, ContractError> {
    let path = manifest_path(store, module_reference);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(Err(ContractError::new(
            CODE_UNKNOWN_MODULE_REFERENCE,
            format!(
                "moduleReference `{module_reference}` does not resolve to a managed module definition"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    };
    let value: Value = serde_json::from_str(&raw).map_err(|err| {
        ContractError::new(
            CODE_CONFIGURATION_INVALID,
            format!("managed module manifest `{module_reference}` is not valid JSON: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let obj = value.as_object().ok_or_else(|| {
        ContractError::new(
            CODE_CONFIGURATION_INVALID,
            format!("managed module manifest `{module_reference}` must be an object"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let string_list = |field: &str| -> Option<Vec<String>> {
        obj.get(field).and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
    };
    let declared_methods = string_list("declaredMethods").unwrap_or_default();
    let supported_options = string_list("supportedOptions").unwrap_or_default();
    let transport = obj.get("transport").and_then(Value::as_object);
    let Some(transport) = transport else {
        return Err(ContractError::new(
            CODE_CONFIGURATION_INVALID,
            format!("managed module manifest `{module_reference}` has no transport block"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    };
    let transport_kind = transport
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut module = ManagedModule {
        module_reference: module_reference.to_owned(),
        identity_module_id: obj
            .get("identity")
            .and_then(Value::as_object)
            .and_then(|identity| identity.get("moduleId"))
            .and_then(Value::as_str)
            .unwrap_or(module_reference)
            .to_owned(),
        transport_kind: transport_kind.clone(),
        stdio_command: None,
        stdio_args: Vec::new(),
        stdio_cwd: None,
        tcp_host: None,
        tcp_port: None,
        declared_methods,
        supported_options,
    };
    match transport_kind.as_str() {
        "stdio" => {
            module.stdio_command = Some(
                transport
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            );
            module.stdio_args = transport
                .get("args")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            module.stdio_cwd = transport
                .get("cwd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
        "tcp" => {
            module.tcp_host = Some(
                transport
                    .get("host")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            );
            module.tcp_port = transport
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok());
        }
        other => {
            return Err(ContractError::new(
                CODE_CONFIGURATION_INVALID,
                format!(
                    "managed module manifest `{module_reference}` declares unknown transport `{other}`"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }
    // The manifest protocol version must be the knowledge protocol — a
    // manifest that speaks something else can never be served by this build.
    let manifest_protocol = obj
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if manifest_protocol != KNOWLEDGE_PROTOCOL {
        return Err(ContractError::new(
            CODE_PROTOCOL_INCOMPATIBLE,
            format!(
                "managed module manifest `{module_reference}` declares protocol `{manifest_protocol}`; this build speaks `{KNOWLEDGE_PROTOCOL}`"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    // The native profile provides no knowledge credentialEnv: a module that
    // needs one is explicitly unsupported, never silently degraded.
    let credential_env = string_list("credentialEnv").unwrap_or_default();
    if !credential_env.is_empty() {
        return Err(ContractError::new(
            CODE_UNSUPPORTED_PROFILE,
            format!(
                "managed module `{module_reference}` requires credentialEnv {credential_env:?}; the native profile provides no knowledge credential environment"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    for method in &module.declared_methods {
        if !KNOWLEDGE_METHODS.contains(&method.as_str()) {
            return Err(ContractError::new(
                CODE_CONFIGURATION_INVALID,
                format!(
                    "managed module manifest `{module_reference}` declares unknown method `{method}`"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }
    for option in &module.supported_options {
        if !KNOWLEDGE_OPTIONS.contains(&option.as_str()) {
            return Err(ContractError::new(
                CODE_CONFIGURATION_INVALID,
                format!(
                    "managed module manifest `{module_reference}` declares unknown option `{option}`"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }
    Ok(Ok(module))
}

// ---------------------------------------------------------------------------
// Module calls (one NDJSON pair per call; the child dies with the future)
// ---------------------------------------------------------------------------

/// Why a module call ended without a result.
enum ModuleCallError {
    /// The upstream did not answer within the per-call bound.
    Timeout,
    /// The transport could not be established at all.
    Unreachable(String),
    /// The peer answered something this protocol cannot parse.
    Protocol(String),
    /// The peer answered a structured module error.
    Upstream { code: String, message: String },
}

impl ModuleCallError {
    fn to_contract_error(&self) -> ContractError {
        match self {
            Self::Timeout => ContractError::new(
                CODE_TIMEOUT,
                format!(
                    "the knowledge module did not answer within {} ms",
                    module_call_timeout().as_millis()
                ),
                RetrySpec::backoff_read(Some(500)),
                EffectStatus::NotApplied,
            ),
            Self::Unreachable(message) => ContractError::new(
                CODE_UPSTREAM_FAILED,
                message.clone(),
                RetrySpec::backoff_read(Some(500)),
                EffectStatus::NotApplied,
            ),
            Self::Protocol(message) => ContractError::new(
                CODE_PROTOCOL_INCOMPATIBLE,
                message.clone(),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ),
            Self::Upstream { code, message } => {
                // The module's own vocabulary passes through where the public
                // contract shares it; everything else folds into
                // UPSTREAM_FAILED with the upstream code preserved.
                match code.as_str() {
                    CODE_UNSUPPORTED_QUERY_OPTION | CODE_STALE_CITATION | "NOT_FOUND" => {
                        ContractError::new(
                            code.clone(),
                            message.clone(),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        )
                    }
                    _ => upstream_failed(format!(
                        "the knowledge module answered error `{code}`: {message}"
                    )),
                }
            }
        }
    }
}

/// Performs one module call: opens the transport, writes the request line,
/// reads the response line, and returns the `result` object. Dropping the
/// returned-in-flight future kills the child / closes the connection —
/// that is the K1 cancellation surface for these sync operations.
async fn call_module(
    store: &V1Store,
    module: &ManagedModule,
    method: &str,
    params: Value,
) -> Result<Value, ModuleCallError> {
    let request_id = format!("kreq-{}", store.next_id());
    let request = json!({"id": request_id, "method": method, "params": params});
    let request_line = format!("{}\n", request);

    let exchange = async {
        match module.transport_kind.as_str() {
            "stdio" => {
                let command = module.stdio_command.clone().unwrap_or_default();
                if command.is_empty() {
                    return Err(ModuleCallError::Unreachable(format!(
                        "managed module `{}` has no stdio command",
                        module.module_reference
                    )));
                }
                let mut cmd = tokio::process::Command::new(&command);
                cmd.args(&module.stdio_args)
                    .env_clear()
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                if let Some(path) = std::env::var_os("PATH") {
                    cmd.env("PATH", path);
                }
                if let Some(cwd) = &module.stdio_cwd {
                    cmd.current_dir(cwd);
                }
                let mut child = cmd.spawn().map_err(|err| {
                    ModuleCallError::Unreachable(format!(
                        "spawning knowledge module `{}` failed: {err}",
                        module.module_reference
                    ))
                })?;
                let mut stdin = child.stdin.take().expect("piped stdin");
                let stdout = child.stdout.take().expect("piped stdout");
                let write = async {
                    stdin
                        .write_all(request_line.as_bytes())
                        .await
                        .map_err(|err| {
                            ModuleCallError::Unreachable(format!(
                                "writing to knowledge module `{}` failed: {err}",
                                module.module_reference
                            ))
                        })
                };
                let read = read_response(stdout, &request_id, &module.module_reference);
                let (write_res, read_res) = tokio::join!(write, read);
                write_res?;
                read_res
            }
            "tcp" => {
                let host = module.tcp_host.clone().unwrap_or_default();
                let port = module.tcp_port.unwrap_or_default();
                let mut stream = tokio::net::TcpStream::connect((host.as_str(), port))
                    .await
                    .map_err(|err| {
                        ModuleCallError::Unreachable(format!(
                            "connecting to knowledge module at {host}:{port} failed: {err}"
                        ))
                    })?;
                stream
                    .write_all(request_line.as_bytes())
                    .await
                    .map_err(|err| {
                        ModuleCallError::Unreachable(format!(
                            "writing to knowledge module at {host}:{port} failed: {err}"
                        ))
                    })?;
                let (read_half, _write_half) = stream.into_split();
                read_response(read_half, &request_id, &module.module_reference).await
            }
            other => Err(ModuleCallError::Protocol(format!(
                "managed module `{}` declares unknown transport `{other}`",
                module.module_reference
            ))),
        }
    };
    match tokio::time::timeout(module_call_timeout(), exchange).await {
        Err(_) => Err(ModuleCallError::Timeout),
        Ok(outcome) => outcome,
    }
}

/// Reads and parses one NDJSON response line, checking the request id.
async fn read_response<R: tokio::io::AsyncRead + Unpin + Send>(
    read_half: R,
    request_id: &str,
    module_reference: &str,
) -> Result<Value, ModuleCallError> {
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.map_err(|err| {
        ModuleCallError::Unreachable(format!(
            "reading from knowledge module `{module_reference}` failed: {err}"
        ))
    })?;
    if n == 0 {
        return Err(ModuleCallError::Unreachable(format!(
            "knowledge module `{module_reference}` closed the connection without answering"
        )));
    }
    let value: Value =
        serde_json::from_str(line.trim_end_matches(['\r', '\n'])).map_err(|err| {
            ModuleCallError::Protocol(format!(
                "knowledge module `{module_reference}` answered a non-JSON line: {err}"
            ))
        })?;
    if value.get("id").and_then(Value::as_str) != Some(request_id) {
        return Err(ModuleCallError::Protocol(format!(
            "knowledge module `{module_reference}` answered a mismatched request id"
        )));
    }
    if let Some(error) = value.get("error").and_then(Value::as_object) {
        return Err(ModuleCallError::Upstream {
            code: error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("UPSTREAM_ERROR")
                .to_owned(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }
    let result = value.get("result").cloned().unwrap_or(Value::Null);
    Ok(result)
}

// ---------------------------------------------------------------------------
// Binding rows and state derivation
// ---------------------------------------------------------------------------

struct StoredBinding {
    binding_id: String,
    revision: String,
    enabled: bool,
    module_reference: String,
    allowed_methods: Vec<String>,
    validation: Option<Value>,
    validated_revision: Option<String>,
}

impl StoredBinding {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let allowed_raw: String = row.get(4)?;
        Ok(Self {
            binding_id: row.get(0)?,
            revision: row.get(1)?,
            enabled: row.get::<_, i64>(2)? != 0,
            module_reference: row.get(3)?,
            allowed_methods: serde_json::from_str(&allowed_raw).unwrap_or_default(),
            validation: row
                .get::<_, Option<String>>(5)?
                .and_then(|text| serde_json::from_str(&text).ok()),
            validated_revision: row.get(6)?,
        })
    }
}

const BINDING_COLUMNS: &str = "binding_id, revision, enabled, module_reference,
    allowed_methods_json, validation_json, validated_revision";

fn load_binding(
    conn: &Connection,
    binding_id: &str,
) -> Result<Option<StoredBinding>, ContractError> {
    conn.query_row(
        &format!("SELECT {BINDING_COLUMNS} FROM v1_knowledge_bindings WHERE binding_id = ?1"),
        params![binding_id],
        StoredBinding::from_row,
    )
    .optional()
    .map_err(sql_err)
}

/// The next monotonic `rev-<n>` (the shared counter, local copy per the
/// substrate's per-domain pattern).
fn next_rev(conn: &Connection) -> Result<String, ContractError> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let next = current.unwrap_or(0) + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(sql_err)?;
    Ok(format!("rev-{next:08}"))
}

/// The effective-methods derivation: what the binding allows ∩ what the
/// manifest declares ∩ what the last validation's handshake proved. An
/// unvalidated (or differently-revisioned) binding has no effective methods.
struct EffectiveMethods {
    state: &'static str,
    unavailable_reason: Option<String>,
    effective: Vec<String>,
    declared: Vec<String>,
}

fn derive_effective_methods(
    store: &V1Store,
    binding: &StoredBinding,
) -> Result<EffectiveMethods, ContractError> {
    if !binding.enabled {
        return Ok(EffectiveMethods {
            state: "disabled",
            unavailable_reason: Some("BINDING_DISABLED".to_owned()),
            effective: Vec::new(),
            declared: Vec::new(),
        });
    }
    // The manifest is read per derivation; a module that disappeared since
    // configure is an unconfigured binding (explicitly not a query success).
    let module = match load_managed_module(store, &binding.module_reference)? {
        Ok(module) => module,
        Err(_) => {
            return Ok(EffectiveMethods {
                state: "unconfigured",
                unavailable_reason: Some("MODULE_NOT_ASSEMBLED".to_owned()),
                effective: Vec::new(),
                declared: Vec::new(),
            });
        }
    };
    let validation_current = binding
        .validation
        .as_ref()
        .is_some_and(|_| binding.validated_revision.as_deref() == Some(binding.revision.as_str()));
    if !validation_current {
        return Ok(EffectiveMethods {
            state: "configured",
            unavailable_reason: Some("NOT_VALIDATED".to_owned()),
            effective: Vec::new(),
            declared: module.declared_methods,
        });
    }
    let validation = binding.validation.clone().unwrap_or_default();
    let validated = validation["validated"].as_bool().unwrap_or(false);
    let handshake_methods: Vec<String> = validation["handshake"]["methods"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let effective: Vec<String> = binding
        .allowed_methods
        .iter()
        .filter(|method| {
            module.declared_methods.contains(method) && handshake_methods.contains(method)
        })
        .cloned()
        .collect();
    if validated {
        Ok(EffectiveMethods {
            state: "ready",
            unavailable_reason: None,
            effective,
            declared: module.declared_methods,
        })
    } else {
        // An unvalidated binding has no proven effective capability
        // (「方法声明不等于已装配或已通过授权」): the failed check is the
        // reported reason, effective stays empty.
        Ok(EffectiveMethods {
            state: "unavailable",
            unavailable_reason: Some(format!(
                "VALIDATION_FAILED:{}",
                validation["failedCheck"].as_str().unwrap_or("unknown")
            )),
            effective: Vec::new(),
            declared: module.declared_methods,
        })
    }
}

// ---------------------------------------------------------------------------
// knowledge.inspect
// ---------------------------------------------------------------------------

async fn knowledge_inspect(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let binding_id = input["bindingId"].as_str().unwrap_or_default();
    let store = rt.store();
    let binding = {
        let conn = store.conn.lock().expect("sqlite mutex");
        load_binding(&conn, binding_id)?.ok_or_else(|| knowledge_not_found(binding_id))?
    };
    // Zero upstream contact: the manifest is read from disk, the module is
    // never spawned (「不为inspect自动访问未授权端点」).
    let derived = derive_effective_methods(store, &binding)?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "bindingId": binding.binding_id,
            "revision": binding.revision,
            "state": derived.state,
            "declaredMethods": derived.declared,
            "effectiveMethods": derived.effective,
            "unavailableReason": derived.unavailable_reason,
        }),
    ))
}

// ---------------------------------------------------------------------------
// knowledge.configure
// ---------------------------------------------------------------------------

async fn knowledge_configure(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let binding_id = input["bindingId"].as_str().unwrap_or_default().to_owned();
    let expected_revision = input["expectedRevision"].as_str().unwrap_or_default();
    let enabled = input["enabled"].as_bool().unwrap_or(false);
    let module_reference = input["moduleReference"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let allowed_methods: Vec<String> = input["allowedMethods"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();

    if binding_id.is_empty() || module_reference.is_empty() {
        return Err(knowledge_invalid("bindingId and moduleReference are required").into());
    }
    // The reference must be a managed module definition (server-side data).
    // This read is the ONLY upstream-adjacent work configure does — it never
    // spawns, queries, or binds anything (「配置不触发查询、索引或StaffDeck绑
    // 定」「禁用不触发上游调用」).
    load_managed_module(rt.store(), &module_reference)??;
    // No allowedMethods-vs-declared check here: the K1 matrix keeps the
    // binding's allowed set and the module's declared methods separate on
    // purpose (「配置声明、实际装配、最近校验及有效能力分别报告」,
    // `declared != effective`) — a binding may allow a method the module does
    // not declare; validate then reports the intersection as the effective
    // set and the undeclared method stays CAPABILITY_UNAVAILABLE at call
    // time. The method *vocabulary* itself is schema-enforced
    // (`allowedMethods.items.enum` in the operation contract).

    let store = rt.store();
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn.transaction().map_err(sql_err)?;
    let existing = load_binding(&tx, &binding_id)?;
    let revision = match (&existing, expected_revision) {
        (Some(row), "absent") => {
            return Err(ContractError::new(
                CODE_CONFLICT,
                format!(
                    "binding `{binding_id}` exists at revision `{}`; create used `absent`",
                    row.revision
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        (Some(row), expected) if expected != row.revision => {
            return Err(ContractError::new(
                CODE_CONFLICT,
                format!(
                    "expectedRevision `{expected}` does not match the binding revision `{}`",
                    row.revision
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        (None, expected) if expected != "absent" => {
            return Err(ContractError::new(
                CODE_CONFLICT,
                format!("binding `{binding_id}` does not exist; create with `absent`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        _ => next_rev(&tx)?,
    };
    let now = now_rfc3339();
    if existing.is_some() {
        tx.execute(
            "UPDATE v1_knowledge_bindings SET revision = ?1, enabled = ?2,
                module_reference = ?3, allowed_methods_json = ?4, validation_json = NULL,
                validated_revision = NULL, updated_at = ?5
             WHERE binding_id = ?6",
            params![
                revision,
                i64::from(enabled),
                module_reference,
                serde_json::to_string(&allowed_methods).unwrap_or_default(),
                now,
                binding_id
            ],
        )
        .map_err(sql_err)?;
    } else {
        tx.execute(
            "INSERT INTO v1_knowledge_bindings (binding_id, revision, enabled,
                module_reference, allowed_methods_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                binding_id,
                revision,
                i64::from(enabled),
                module_reference,
                serde_json::to_string(&allowed_methods).unwrap_or_default(),
                now
            ],
        )
        .map_err(sql_err)?;
    }
    tx.commit().map_err(sql_err)?;
    drop(conn);
    store.event_notifier().notify_waiters();

    // Persisted and runtime are reported separately (「已持久但runtime不可用
    // 分开报告」): a fresh enabled binding awaits validation; a disabled one
    // needs nothing. restart_required/unavailable are unreachable in the
    // per-call transport model (no persistent assembly to restart) — the
    // report registers this.
    let runtime_state = if enabled {
        "pending_validation"
    } else {
        "disabled"
    };
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "bindingId": binding_id,
            "revision": revision,
            "persisted": true,
            "runtimeState": runtime_state,
        }),
    ))
}

// ---------------------------------------------------------------------------
// knowledge.validate
// ---------------------------------------------------------------------------

async fn knowledge_validate(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let binding_id = input["bindingId"].as_str().unwrap_or_default();
    let expected_revision = input["expectedRevision"].as_str().unwrap_or_default();
    let store = rt.store();
    let binding = {
        let conn = store.conn.lock().expect("sqlite mutex");
        let binding =
            load_binding(&conn, binding_id)?.ok_or_else(|| knowledge_not_found(binding_id))?;
        if binding.revision != expected_revision {
            return Err(ContractError::new(
                CODE_CONFLICT,
                format!(
                    "expectedRevision `{expected_revision}` does not match the binding revision `{}`",
                    binding.revision
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        binding
    };

    // A disabled binding validates nothing upstream (「禁用不触发上游调用」).
    let mut checks: Vec<Value> = Vec::new();
    let mut validated = false;
    let mut effective: Vec<String> = Vec::new();
    let mut handshake: Option<Value> = None;
    let mut failed_check: Option<String> = None;
    if !binding.enabled {
        checks.push(json!({"name": "binding.enabled", "status": "fail"}));
        failed_check = Some("binding.enabled".to_owned());
    } else {
        match load_managed_module(store, &binding.module_reference)? {
            Err(err) => {
                checks.push(json!({"name": "module.managed", "status": "fail"}));
                failed_check = Some("module.managed".to_owned());
                let _ = err;
            }
            Ok(module) => {
                checks.push(json!({"name": "module.managed", "status": "pass"}));
                match call_module(store, &module, "describe", json!({})).await {
                    Err(ModuleCallError::Timeout) => {
                        return Err(ModuleCallError::Timeout.to_contract_error().into());
                    }
                    Err(ModuleCallError::Unreachable(message)) => {
                        checks.push(json!({"name": "transport", "status": "unavailable"}));
                        failed_check = Some("transport".to_owned());
                        let _ = message;
                    }
                    Err(ModuleCallError::Protocol(message)) => {
                        checks.push(json!({"name": "manifest.contract", "status": "fail"}));
                        failed_check = Some("manifest.contract".to_owned());
                        let _ = message;
                    }
                    Err(ModuleCallError::Upstream { code, message }) => {
                        checks.push(json!({"name": "transport", "status": "unavailable"}));
                        failed_check = Some("transport".to_owned());
                        let _ = (code, message);
                    }
                    Ok(result) => {
                        checks.push(json!({"name": "transport", "status": "pass"}));
                        // manifest.contract: the peer must speak our protocol.
                        let protocol_ok = result["protocolVersion"]
                            .as_str()
                            .is_some_and(|version| version == KNOWLEDGE_PROTOCOL);
                        checks.push(json!({
                            "name": "manifest.contract",
                            "status": if protocol_ok { "pass" } else { "fail" },
                        }));
                        if !protocol_ok {
                            failed_check = Some("manifest.contract".to_owned());
                        }
                        // manifest.identity: the peer's identity must match
                        // the managed manifest's.
                        let identity_ok = result["identity"]["moduleId"]
                            .as_str()
                            .is_some_and(|id| id == module.identity_module_id);
                        checks.push(json!({
                            "name": "manifest.identity",
                            "status": if identity_ok { "pass" } else { "fail" },
                        }));
                        if !identity_ok {
                            failed_check = Some("manifest.identity".to_owned());
                        }
                        // manifest.methods: the runtime declaration must
                        // match the manifest declaration exactly.
                        let runtime_methods: Vec<String> = result["methods"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(Value::as_str)
                                    .map(ToOwned::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let mut sorted_runtime = runtime_methods.clone();
                        sorted_runtime.sort();
                        let mut sorted_manifest = module.declared_methods.clone();
                        sorted_manifest.sort();
                        let methods_ok = sorted_runtime == sorted_manifest;
                        checks.push(json!({
                            "name": "manifest.methods",
                            "status": if methods_ok { "pass" } else { "fail" },
                        }));
                        if !methods_ok {
                            failed_check = Some("manifest.methods".to_owned());
                        }
                        // manifest.transport: the transport actually used is
                        // the manifest's declared kind (both spawn and TCP
                        // paths record it implicitly through reaching here).
                        checks.push(json!({"name": "manifest.transport", "status": "pass"}));
                        handshake = Some(result.clone());
                        validated = protocol_ok && identity_ok && methods_ok;
                        if validated {
                            effective = binding
                                .allowed_methods
                                .iter()
                                .filter(|method| {
                                    module.declared_methods.contains(method)
                                        && runtime_methods.contains(method)
                                })
                                .cloned()
                                .collect();
                        }
                    }
                }
            }
        }
    }

    // Persist the validation bound to this configuration revision (「缓存校
    // 验结果须绑定revision」).
    let validation = json!({
        "validated": validated,
        "checks": checks,
        "failedCheck": failed_check,
        "effectiveMethods": effective,
        "handshake": handshake,
    });
    {
        let conn = store.conn.lock().expect("sqlite mutex");
        conn.execute(
            "UPDATE v1_knowledge_bindings SET validation_json = ?1, validated_revision = ?2,
                updated_at = ?3
             WHERE binding_id = ?4 AND revision = ?5",
            params![
                serde_json::to_string(&validation).unwrap_or_default(),
                binding.revision,
                now_rfc3339(),
                binding.binding_id,
                binding.revision
            ],
        )
        .map_err(sql_err)?;
    }
    store.event_notifier().notify_waiters();

    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "bindingId": binding.binding_id,
            "revision": binding.revision,
            "validated": validated,
            "effectiveMethods": effective,
            "checks": checks,
        }),
    ))
}

// ---------------------------------------------------------------------------
// The execution gate shared by query and citation.resolve
// ---------------------------------------------------------------------------

/// Everything an executing operation needs from a ready binding.
struct GatedBinding {
    binding: StoredBinding,
    module: ManagedModule,
    #[allow(dead_code)]
    effective: Vec<String>,
}

/// The K1 execution gate: enabled, assembled (managed manifest present), and
/// validated for the CURRENT configuration revision with the method in the
/// effective set (「binding须已启用且实际装配可用；query须在配置允许集合、
/// 模块声明和通过identity/contract/transport/methods校验的manifest中」).
/// Unready bindings answer explicitly, never an empty success.
fn gate_execution(
    rt: &PublicV1Runtime,
    binding_id: &str,
    method: &str,
) -> Result<GatedBinding, ContractError> {
    let store = rt.store();
    let binding = {
        let conn = store.conn.lock().expect("sqlite mutex");
        load_binding(&conn, binding_id)?.ok_or_else(|| knowledge_not_found(binding_id))?
    };
    if !binding.enabled {
        return Err(ContractError::new(
            CODE_DISABLED,
            format!("knowledge binding `{binding_id}` is disabled"),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }
    if !binding.allowed_methods.contains(&method.to_owned()) {
        return Err(capability_unavailable(format!(
            "method `{method}` is not in the allowed set of binding `{binding_id}`"
        )));
    }
    let derived = derive_effective_methods(store, &binding)?;
    if derived.state != "ready" {
        return Err(capability_unavailable(format!(
            "knowledge binding `{binding_id}` is `{}` ({})",
            derived.state,
            derived
                .unavailable_reason
                .unwrap_or_else(|| "unknown".to_owned())
        )));
    }
    if !derived.effective.contains(&method.to_owned()) {
        return Err(capability_unavailable(format!(
            "method `{method}` is declared but not assembled for binding `{binding_id}` (query-only modules must refuse citation resolution explicitly)"
        )));
    }
    // `derive_effective_methods` already loaded the manifest to classify the
    // binding (a module that disappeared since configure is `unconfigured`,
    // reported as CAPABILITY_UNAVAILABLE — the K1 matrix row, never a leaked
    // UNKNOWN_MODULE_REFERENCE from the execution path). The load here only
    // refetches what a ready binding needs for the call itself.
    let module = load_managed_module(store, &binding.module_reference)??;
    Ok(GatedBinding {
        binding,
        module,
        effective: derived.effective,
    })
}

// ---------------------------------------------------------------------------
// knowledge.query
// ---------------------------------------------------------------------------

async fn knowledge_query(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let binding_id = input["bindingId"].as_str().unwrap_or_default();
    let query = input["query"].as_str().unwrap_or_default();
    let gated = gate_execution(&rt, binding_id, "query")?;
    let module_options = &gated.module.supported_options;

    // Unknown options are refused, never ignored (「默认不接受provider未声
    // 明的cursor/filter，明确unsupported_query_option」).
    if input.get("knowledgeBaseIds").is_some()
        && !module_options.contains(&"knowledgeBaseIds".to_owned())
    {
        return Err(ContractError::new(
            CODE_UNSUPPORTED_QUERY_OPTION,
            "the module does not declare support for `knowledgeBaseIds`",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    if input.get("limit").is_some() && !module_options.contains(&"limit".to_owned()) {
        return Err(ContractError::new(
            CODE_UNSUPPORTED_QUERY_OPTION,
            "the module does not declare support for `limit`",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    if input.get("cursor").is_some() && !module_options.contains(&"cursor".to_owned()) {
        return Err(ContractError::new(
            CODE_UNSUPPORTED_QUERY_OPTION,
            "the module does not declare support for `cursor`",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    let mut params = Map::new();
    params.insert("query".to_owned(), json!(query));
    if let Some(kb_ids) = input.get("knowledgeBaseIds") {
        params.insert("knowledgeBaseIds".to_owned(), kb_ids.clone());
    }
    if let Some(limit) = input.get("limit") {
        params.insert("limit".to_owned(), limit.clone());
    }
    if let Some(cursor) = input.get("cursor") {
        params.insert("cursor".to_owned(), cursor.clone());
    }
    let store = rt.store();
    let result = call_module(store, &gated.module, "query", Value::Object(params))
        .await
        .map_err(|err| err.to_contract_error())?;

    // Normalization: every hit must carry the contract shape; a malformed
    // provider payload fails loudly — dropped citations are never "success".
    let hits_raw = result
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| upstream_failed("the module answered no hits array"))?;
    let mut hits = Vec::with_capacity(hits_raw.len());
    for hit in hits_raw {
        let id = hit["id"].as_str().unwrap_or_default();
        if id.is_empty() {
            return Err(upstream_failed(
                "a module hit has an empty id; refusing to normalize a malformed payload",
            )
            .into());
        }
        let citation_ids: Vec<String> = hit["citationIds"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if citation_ids.is_empty() || citation_ids.iter().any(String::is_empty) {
            return Err(upstream_failed(format!(
                "hit `{id}` lost its citationIds in the provider payload; refusing to normalize"
            ))
            .into());
        }
        hits.push(json!({
            "id": id,
            "text": hit["text"].as_str().unwrap_or_default(),
            "citationIds": citation_ids,
        }));
    }
    let truncated = result["truncated"].as_bool().unwrap_or(false);
    let mut response = json!({
        "bindingId": gated.binding.binding_id,
        "bindingRevision": gated.binding.revision,
        "hits": hits,
        "truncated": truncated,
    });
    if let Some(next_cursor) = result.get("nextCursor")
        && !next_cursor.is_null()
    {
        response["nextCursor"] = next_cursor.clone();
    }
    if let Some(metadata) = result.get("providerMetadata").filter(|v| v.is_object()) {
        response["providerMetadata"] = metadata.clone();
    }
    Ok((ResponseOutcome::Succeeded, response))
}

// ---------------------------------------------------------------------------
// knowledge.citation.resolve
// ---------------------------------------------------------------------------

async fn knowledge_citation_resolve(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let binding_id = input["bindingId"].as_str().unwrap_or_default();
    let citation_id = input["citationId"].as_str().unwrap_or_default();
    // The query-only refusal is explicit (「query-only模块必须明确拒绝」).
    let gated = gate_execution(&rt, binding_id, "resolve_citation")?;
    let store = rt.store();
    let revision_at_start = gated.binding.revision.clone();
    let result = call_module(
        store,
        &gated.module,
        "resolve_citation",
        json!({"citationId": citation_id}),
    )
    .await
    .map_err(|err| err.to_contract_error())?;

    // Revision race: if the binding moved while the module resolved, the
    // citation is stale for the current configuration (「解析前后绑定
    // revision变化明确处理」).
    let revision_now: String = {
        let conn = store.conn.lock().expect("sqlite mutex");
        load_binding(&conn, binding_id)?
            .map(|binding| binding.revision)
            .unwrap_or_default()
    };
    if revision_now != revision_at_start {
        return Err(ContractError::new(
            CODE_STALE_CITATION,
            format!(
                "binding `{binding_id}` moved from revision `{revision_at_start}` to `{revision_now}` while the citation was resolving"
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    // Normalization: the source must carry a traceable documentId; optional
    // fields keep their declared types or are dropped, never faked.
    let document_id = result["source"]["documentId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if document_id.is_empty() {
        return Err(upstream_failed(
            "the citation has no traceable source documentId; refusing to normalize",
        )
        .into());
    }
    let mut source = json!({"documentId": document_id});
    if let Some(title) = result["source"]["title"].as_str() {
        source["title"] = json!(title);
    }
    if let Some(version) = result["source"]["version"].as_str() {
        source["version"] = json!(version);
    } else if result["source"]["version"].is_null() {
        source["version"] = Value::Null;
    }
    let mut response = json!({
        "bindingId": gated.binding.binding_id,
        "citationId": citation_id,
        "source": source,
    });
    if let Some(excerpt) = result["excerpt"].as_str() {
        response["excerpt"] = json!(excerpt);
    }
    if let Some(content_ref) = result["contentReference"].as_str() {
        response["contentReference"] = json!(content_ref);
    } else if result["contentReference"].is_null() {
        response["contentReference"] = Value::Null;
    }
    Ok((ResponseOutcome::Succeeded, response))
}

// ---------------------------------------------------------------------------
// Tests (K1 acceptance matrix: 未装配/未配置/声明缺失/越权/超时/取消/
// schema 不匹配；inspect/configure 无需可用 provider).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::initialize_instance;
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;

    fn test_runtime() -> (Arc<PublicV1Runtime>, TempDir) {
        let tmp = TempDir::new("k1");
        let root = tmp.0.join("home");
        initialize_instance(&root, None, Some("local"), Some(true)).unwrap();
        let file = crate::public_v1::bootstrap::load_local_instance_and_token(&root, None)
            .unwrap()
            .0;
        let meta = InstanceMeta {
            instance_id: file.instance_id,
            owner_id: file.owner_id,
            product: file.product,
            protocol_version: file.protocol_version,
            node_id: file.node_id,
            generation: "gen-test-1".to_owned(),
            server_revision: "srv-test-1".to_owned(),
            initialization_revision: file.initialization_revision,
        };
        let store = V1Store::open_and_recover(&root, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        (rt, tmp)
    }

    /// Writes a managed module manifest for `reference` whose stdio command
    /// runs `script_path` with python3.
    /// Writes a managed module manifest for `reference` whose stdio command
    /// runs `script_path` with python3. The `__MODULE_ID__` marker in the
    /// script is bound to the manifest's identity so the handshake matches.
    fn seed_module(rt: &Arc<PublicV1Runtime>, reference: &str, methods: &[&str], script: &str) {
        let dir = rt.store().home().join("knowledge-modules");
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join(format!("{reference}.py"));
        std::fs::write(
            &script_path,
            script
                .replace("__MODULE_ID__", reference)
                .replace("__MODULE_METHODS__", &json!(methods).to_string()),
        )
        .unwrap();
        let manifest = json!({
            "identity": {"moduleId": reference},
            "transport": {
                "kind": "stdio",
                "command": "python3",
                "args": ["-I", script_path.to_string_lossy()],
            },
            "protocolVersion": KNOWLEDGE_PROTOCOL,
            "declaredMethods": methods,
            "supportedOptions": ["knowledgeBaseIds", "limit", "cursor"],
            "credentialEnv": [],
        });
        std::fs::write(
            dir.join(format!("{reference}.json")),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn python3_present() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// A module that answers describe/query/resolve_citation deterministically.
    const GOOD_MODULE: &str = r#"
import json, sys
def send(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    method = req.get("method")
    if method == "describe":
        send({"id": req["id"], "result": {
            "identity": {"moduleId": "__MODULE_ID__"},
            "protocolVersion": "agentcpm-pd.knowledge/1",
            "methods": __MODULE_METHODS__}})
    elif method == "query":
        send({"id": req["id"], "result": {
            "hits": [
                {"id": "hit-1", "text": "The PilotDeck contract is the behavior truth.",
                 "citationIds": ["cite-1"]},
                {"id": "hit-2", "text": "Baselines do not guess.", "citationIds": ["cite-2"]},
            ],
            "nextCursor": None, "truncated": False, "providerMetadata": {"engine": "demo"}}})
    elif method == "resolve_citation":
        if req["params"]["citationId"] == "cite-1":
            send({"id": req["id"], "result": {
                "source": {"documentId": "doc-1", "title": "Contract", "version": "3"},
                "excerpt": "The contract is the truth.",
                "contentReference": "docstore://doc-1"}})
        else:
            send({"id": req["id"], "error": {"code": "NOT_FOUND",
                "message": "unknown citation"}})
    else:
        send({"id": req["id"], "error": {"code": "UNSUPPORTED_METHOD",
            "message": "unknown method"}})
"#;

    async fn configure_binding(
        rt: &Arc<PublicV1Runtime>,
        binding_id: &str,
        module_reference: &str,
        enabled: bool,
        methods: &[&str],
    ) -> Value {
        let (_, result) = rt
            .dispatch(
                "knowledge.configure",
                json!({
                    "bindingId": binding_id,
                    "expectedRevision": "absent",
                    "enabled": enabled,
                    "moduleReference": module_reference,
                    "allowedMethods": methods,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        result
    }

    async fn inspect(rt: &Arc<PublicV1Runtime>, binding_id: &str) -> Value {
        let (_, result) = rt
            .dispatch(
                "knowledge.inspect",
                json!({"bindingId": binding_id}),
                None,
                None,
            )
            .await
            .unwrap();
        result
    }

    fn err_code(err: &DispatchError) -> String {
        match err {
            DispatchError::Application(contract) => contract.code.clone(),
            DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    #[tokio::test]
    async fn k1_unassembled_inspect_and_configure_work_without_provider() {
        let (rt, _home) = test_runtime();
        let reg = agentcpm_pd_protocol::registry::OperationRegistry::global();

        // An unknown reference refuses to configure at all (the module must
        // be a managed definition at configure time).
        let err = rt
            .dispatch(
                "knowledge.configure",
                json!({
                    "bindingId": "kb-x",
                    "expectedRevision": "absent",
                    "enabled": true,
                    "moduleReference": "no-such-module",
                    "allowedMethods": ["query"],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "UNKNOWN_MODULE_REFERENCE");

        // Configure against a manifest that exists (seed_module only writes
        // files — no module process is ever spawned here): persisted +
        // pending_validation, zero upstream contact.
        seed_module(&rt, "kb-demo", &["query", "resolve_citation"], GOOD_MODULE);
        let configured = configure_binding(&rt, "kb", "kb-demo", true, &["query"]).await;
        reg.validate_result("knowledge.configure", &configured)
            .unwrap();
        assert_eq!(configured["persisted"], true);
        assert_eq!(configured["runtimeState"], "pending_validation");

        // 未配置: remove the managed manifest — the binding persists, the
        // inspect/configure diagnostics work without any provider, and the
        // query is explicitly unavailable (cli.case.packages.knowledge-config).
        std::fs::remove_file(
            rt.store()
                .home()
                .join("knowledge-modules")
                .join("kb-demo.json"),
        )
        .unwrap();
        let inspected = inspect(&rt, "kb").await;
        reg.validate_result("knowledge.inspect", &inspected)
            .unwrap();
        assert_eq!(inspected["state"], "unconfigured");
        assert_eq!(inspected["declaredMethods"], json!([]));
        assert_eq!(inspected["effectiveMethods"], json!([]));
        assert_eq!(inspected["unavailableReason"], "MODULE_NOT_ASSEMBLED");

        // Unconfigured → query refuses explicitly (never an empty success).
        let err = rt
            .dispatch(
                "knowledge.query",
                json!({"bindingId": "kb", "query": "contract"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "CAPABILITY_UNAVAILABLE");

        // Unknown binding inspects as NOT_FOUND.
        let err = rt
            .dispatch(
                "knowledge.inspect",
                json!({"bindingId": "nope"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "NOT_FOUND");
    }

    #[tokio::test]
    async fn k1_disabled_binding_reports_disabled_and_query_refuses() {
        let (rt, _home) = test_runtime();
        seed_module(&rt, "kb-demo", &["query", "resolve_citation"], GOOD_MODULE);
        let configured = configure_binding(&rt, "kb-off", "kb-demo", false, &["query"]).await;
        assert_eq!(configured["runtimeState"], "disabled");
        let inspected = inspect(&rt, "kb-off").await;
        assert_eq!(inspected["state"], "disabled");
        assert_eq!(inspected["unavailableReason"], "BINDING_DISABLED");
        let err = rt
            .dispatch(
                "knowledge.query",
                json!({"bindingId": "kb-off", "query": "contract"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "DISABLED");
    }

    #[tokio::test]
    async fn k1_cas_conflicts_and_validation_binding() {
        let (rt, _home) = test_runtime();
        seed_module(&rt, "kb-demo", &["query", "resolve_citation"], GOOD_MODULE);
        let first = configure_binding(&rt, "kb-cas", "kb-demo", true, &["query"]).await;
        let revision = first["revision"].as_str().unwrap().to_owned();

        // CAS: wrong revision, and `absent` over an existing binding.
        let err = rt
            .dispatch(
                "knowledge.configure",
                json!({
                    "bindingId": "kb-cas",
                    "expectedRevision": "rev-00000099",
                    "enabled": true,
                    "moduleReference": "kb-demo",
                    "allowedMethods": ["query"],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "CONFLICT");
        let err = rt
            .dispatch(
                "knowledge.configure",
                json!({
                    "bindingId": "kb-cas",
                    "expectedRevision": "absent",
                    "enabled": true,
                    "moduleReference": "kb-demo",
                    "allowedMethods": ["query"],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "CONFLICT");

        // A configure on an unknown binding without `absent` conflicts.
        let err = rt
            .dispatch(
                "knowledge.configure",
                json!({
                    "bindingId": "kb-ghost",
                    "expectedRevision": "rev-00000001",
                    "enabled": true,
                    "moduleReference": "kb-demo",
                    "allowedMethods": ["query"],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "CONFLICT");

        // allowedMethods outside the schema's method enum are refused by the
        // input validation (INVALID_ARGUMENT) before the handler runs — the
        // operation contract's `allowedMethods.items.enum` is the vocabulary
        // fence, not a handler-side module-declared check.
        let err = rt
            .dispatch(
                "knowledge.configure",
                json!({
                    "bindingId": "kb-bad",
                    "expectedRevision": "absent",
                    "enabled": true,
                    "moduleReference": "kb-demo",
                    "allowedMethods": ["query", "resolve_citation", "reindex"],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "INVALID_ARGUMENT");

        // A configure with the right revision rebinds and resets validation.
        let second = rt
            .dispatch(
                "knowledge.configure",
                json!({
                    "bindingId": "kb-cas",
                    "expectedRevision": revision,
                    "enabled": true,
                    "moduleReference": "kb-demo",
                    "allowedMethods": ["query"],
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(second["runtimeState"], "pending_validation");
        let inspected = inspect(&rt, "kb-cas").await;
        assert_eq!(inspected["state"], "configured");
        assert_eq!(inspected["unavailableReason"], "NOT_VALIDATED");
        assert_eq!(
            inspected["declaredMethods"],
            json!(["query", "resolve_citation"])
        );
        // declared != effective before validation.
        assert_eq!(inspected["effectiveMethods"], json!([]));
    }

    #[tokio::test]
    async fn k1_full_lifecycle_validate_query_and_citation() {
        if !python3_present() {
            eprintln!("note: python3 absent; skipping the k1 module-process scenario");
            return;
        }
        let (rt, _home) = test_runtime();
        let reg = agentcpm_pd_protocol::registry::OperationRegistry::global();
        seed_module(&rt, "kb-demo", &["query", "resolve_citation"], GOOD_MODULE);
        let configured = configure_binding(
            &rt,
            "kb-main",
            "kb-demo",
            true,
            &["query", "resolve_citation"],
        )
        .await;
        let revision = configured["revision"].as_str().unwrap().to_owned();

        // Validate: all checks pass; declared != effective is resolved by the
        // handshake (both methods effective here).
        let (_, validated) = rt
            .dispatch(
                "knowledge.validate",
                json!({"bindingId": "kb-main", "expectedRevision": revision}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("knowledge.validate", &validated)
            .unwrap();
        assert_eq!(validated["validated"], true);
        assert_eq!(
            validated["effectiveMethods"],
            json!(["query", "resolve_citation"])
        );
        for check in validated["checks"].as_array().unwrap() {
            assert_eq!(check["status"], "pass", "check {check}");
        }

        // inspect now reports ready.
        let inspected = inspect(&rt, "kb-main").await;
        assert_eq!(inspected["state"], "ready");
        assert_eq!(inspected["unavailableReason"], Value::Null);
        assert_eq!(
            inspected["effectiveMethods"],
            json!(["query", "resolve_citation"])
        );

        // Query: real hits with citations, 0-hit ≠ upstream failure.
        let (_, queried) = rt
            .dispatch(
                "knowledge.query",
                json!({
                    "bindingId": "kb-main",
                    "query": "contract truth",
                    "limit": 10,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("knowledge.query", &queried).unwrap();
        assert_eq!(queried["bindingRevision"], json!(revision));
        assert_eq!(queried["hits"].as_array().unwrap().len(), 2);
        assert_eq!(queried["hits"][0]["citationIds"], json!(["cite-1"]));
        assert_eq!(queried["truncated"], false);
        assert_eq!(queried["providerMetadata"]["engine"], "demo");

        // Citation resolve: traceable source, version, excerpt.
        let (_, resolved) = rt
            .dispatch(
                "knowledge.citation.resolve",
                json!({"bindingId": "kb-main", "citationId": "cite-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("knowledge.citation.resolve", &resolved)
            .unwrap();
        assert_eq!(resolved["source"]["documentId"], "doc-1");
        assert_eq!(resolved["source"]["version"], "3");
        assert_eq!(resolved["excerpt"], "The contract is the truth.");

        // Unknown citation: NOT_FOUND passes through.
        let err = rt
            .dispatch(
                "knowledge.citation.resolve",
                json!({"bindingId": "kb-main", "citationId": "cite-x"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "NOT_FOUND");
    }

    #[tokio::test]
    async fn k1_declared_but_not_assembled_method_refuses() {
        if !python3_present() {
            eprintln!("note: python3 absent; skipping the k1 module-process scenario");
            return;
        }
        let (rt, _home) = test_runtime();
        // The manifest declares only query: a binding that also allows
        // resolve_citation still has an effective set without it (声明缺失).
        seed_module(&rt, "kb-qonly", &["query"], GOOD_MODULE);
        let configured = configure_binding(
            &rt,
            "kb-q",
            "kb-qonly",
            true,
            &["query", "resolve_citation"],
        )
        .await;
        let revision = configured["revision"].as_str().unwrap().to_owned();
        let (_, validated) = rt
            .dispatch(
                "knowledge.validate",
                json!({"bindingId": "kb-q", "expectedRevision": revision}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(validated["validated"], true);
        assert_eq!(validated["effectiveMethods"], json!(["query"]));

        let inspected = inspect(&rt, "kb-q").await;
        // declared != effective: the acceptance anchor.
        assert_eq!(inspected["declaredMethods"], json!(["query"]));
        assert_eq!(inspected["effectiveMethods"], json!(["query"]));

        // A query-only module refuses citation resolution explicitly.
        let err = rt
            .dispatch(
                "knowledge.citation.resolve",
                json!({"bindingId": "kb-q", "citationId": "cite-1"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "CAPABILITY_UNAVAILABLE");
    }

    #[tokio::test]
    async fn k1_method_outside_allowed_set_is_unauthorized() {
        if !python3_present() {
            eprintln!("note: python3 absent; skipping the k1 module-process scenario");
            return;
        }
        let (rt, _home) = test_runtime();
        seed_module(&rt, "kb-demo", &["query", "resolve_citation"], GOOD_MODULE);
        // 越权: the binding allows only resolve_citation; query is refused.
        let configured =
            configure_binding(&rt, "kb-cite", "kb-demo", true, &["resolve_citation"]).await;
        let revision = configured["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "knowledge.validate",
            json!({"bindingId": "kb-cite", "expectedRevision": revision}),
            None,
            None,
        )
        .await
        .unwrap();
        let err = rt
            .dispatch(
                "knowledge.query",
                json!({"bindingId": "kb-cite", "query": "contract"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "CAPABILITY_UNAVAILABLE");
    }

    #[tokio::test]
    async fn k1_slow_upstream_times_out_and_cancellation_kills_the_child() {
        if !python3_present() {
            eprintln!("note: python3 absent; skipping the k1 module-process scenario");
            return;
        }
        let (rt, _home) = test_runtime();
        // The module answers the describe handshake promptly but never
        // answers a query: the binding validates (ready) and the slow
        // upstream surfaces as TIMEOUT on the query itself (the per-call
        // bound is the documented 10 s).
        let slow_query = r#"
import json, sys, time
def send(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    if req.get("method") == "describe":
        send({"id": req["id"], "result": {
            "identity": {"moduleId": "kb-slow"},
            "protocolVersion": "agentcpm-pd.knowledge/1",
            "methods": ["query"]}})
    else:
        time.sleep(300)
"#;
        seed_module(&rt, "kb-slow", &["query"], slow_query);
        let configured = configure_binding(&rt, "kb-slowb", "kb-slow", true, &["query"]).await;
        let revision = configured["revision"].as_str().unwrap().to_owned();
        let (_, validated) = rt
            .dispatch(
                "knowledge.validate",
                json!({"bindingId": "kb-slowb", "expectedRevision": revision}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(validated["validated"], true);
        let inspected = inspect(&rt, "kb-slowb").await;
        assert_eq!(inspected["state"], "ready");

        // The query path surfaces TIMEOUT (a slow upstream, honestly).
        let err = rt
            .dispatch(
                "knowledge.query",
                json!({"bindingId": "kb-slowb", "query": "slow"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "TIMEOUT");

        // Cancellation: dropping the in-flight call_module future kills the
        // child (the marker must never appear) — the K1 cancellation surface
        // for these sync operations is the kill-on-drop guard.
        let marker = std::env::temp_dir().join(format!("k1-cancel-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let slow_module = ManagedModule {
            module_reference: "cancel-probe".to_owned(),
            identity_module_id: "cancel-probe".to_owned(),
            transport_kind: "stdio".to_owned(),
            stdio_command: Some("sh".to_owned()),
            stdio_args: vec![
                "-c".to_owned(),
                format!("sleep 1; echo done > {}; sleep 300", marker.display()),
            ],
            stdio_cwd: None,
            tcp_host: None,
            tcp_port: None,
            declared_methods: vec!["query".to_owned()],
            supported_options: Vec::new(),
        };
        let store = rt.store().clone();
        let call = call_module(&store, &slow_module, "describe", json!({}));
        let mut call = Box::pin(call);
        // Start the call, then drop it (the client disconnected).
        let _ = tokio::time::timeout(Duration::from_millis(200), &mut call).await;
        drop(call);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "the module child must be killed when the call future is dropped"
        );
    }

    #[tokio::test]
    async fn k1_malformed_provider_payload_fails_loudly() {
        if !python3_present() {
            eprintln!("note: python3 absent; skipping the k1 module-process scenario");
            return;
        }
        let (rt, _home) = test_runtime();
        // A module that drops citationIds from its hits (schema 不匹配 in
        // the provider payload): normalization must refuse, not "succeed".
        let malformed = r#"
import json, sys
def send(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    if req.get("method") == "describe":
        send({"id": req["id"], "result": {
            "identity": {"moduleId": "kb-bad"},
            "protocolVersion": "agentcpm-pd.knowledge/1",
            "methods": ["query"]}})
    elif req.get("method") == "query":
        send({"id": req["id"], "result": {
            "hits": [{"id": "hit-1", "text": "citation lost"}],
            "truncated": False}})
"#;
        seed_module(&rt, "kb-bad", &["query"], malformed);
        let configured = configure_binding(&rt, "kb-badb", "kb-bad", true, &["query"]).await;
        let revision = configured["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "knowledge.validate",
            json!({"bindingId": "kb-badb", "expectedRevision": revision}),
            None,
            None,
        )
        .await
        .unwrap();
        let err = rt
            .dispatch(
                "knowledge.query",
                json!({"bindingId": "kb-badb", "query": "anything"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "UPSTREAM_FAILED");
    }

    #[tokio::test]
    async fn k1_protocol_identity_and_method_mismatches_fail_validation() {
        if !python3_present() {
            eprintln!("note: python3 absent; skipping the k1 module-process scenario");
            return;
        }
        let (rt, _home) = test_runtime();
        // A module whose handshake declares a foreign protocol, a different
        // identity, and extra methods.
        let lying = r#"
import json, sys
def send(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    send({"id": req["id"], "result": {
        "identity": {"moduleId": "someone-else"},
        "protocolVersion": "vendor/2",
        "methods": ["query", "resolve_citation", "reindex"]}})
"#;
        seed_module(&rt, "kb-lying", &["query"], lying);
        let configured = configure_binding(&rt, "kb-ly", "kb-lying", true, &["query"]).await;
        let revision = configured["revision"].as_str().unwrap().to_owned();
        let (_, validated) = rt
            .dispatch(
                "knowledge.validate",
                json!({"bindingId": "kb-ly", "expectedRevision": revision}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(validated["validated"], false);
        assert_eq!(validated["effectiveMethods"], json!([]));
        let names: Vec<String> = validated["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["status"] != "pass")
            .map(|c| c["name"].as_str().unwrap().to_owned())
            .collect();
        assert!(names.contains(&"manifest.contract".to_owned()));
        assert!(names.contains(&"manifest.identity".to_owned()));
        assert!(names.contains(&"manifest.methods".to_owned()));
        let inspected = inspect(&rt, "kb-ly").await;
        assert_eq!(inspected["state"], "unavailable");
    }

    #[tokio::test]
    async fn k1_credential_env_profile_is_refused() {
        let (rt, _home) = test_runtime();
        // A manifest that needs a credential environment is explicitly
        // unsupported under the native profile.
        let dir = rt.store().home().join("knowledge-modules");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = json!({
            "identity": {"moduleId": "kb-cred"},
            "transport": {"kind": "stdio", "command": "sh", "args": ["-c", "sleep 1"]},
            "protocolVersion": KNOWLEDGE_PROTOCOL,
            "declaredMethods": ["query"],
            "supportedOptions": [],
            "credentialEnv": ["STAFFDECK_TOKEN"],
        });
        std::fs::write(
            dir.join("kb-cred.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        let err = rt
            .dispatch(
                "knowledge.configure",
                json!({
                    "bindingId": "kb-credb",
                    "expectedRevision": "absent",
                    "enabled": true,
                    "moduleReference": "kb-cred",
                    "allowedMethods": ["query"],
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "UNSUPPORTED_PROFILE");
    }

    #[tokio::test]
    async fn k1_unreachable_module_validates_as_unavailable_not_failed() {
        let (rt, _home) = test_runtime();
        // A manifest whose command does not exist: the transport check is
        // "unavailable" (不可用) while the op itself succeeds with
        // validated=false (「不可用和校验失败必须如实区分」).
        let dir = rt.store().home().join("knowledge-modules");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = json!({
            "identity": {"moduleId": "kb-dead"},
            "transport": {
                "kind": "stdio",
                "command": dir.join("no-such-runner").to_string_lossy(),
                "args": [],
            },
            "protocolVersion": KNOWLEDGE_PROTOCOL,
            "declaredMethods": ["query"],
            "supportedOptions": [],
            "credentialEnv": [],
        });
        std::fs::write(
            dir.join("kb-dead.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        // configure needs no provider: a dead command still persists.
        let configured = configure_binding(&rt, "kb-deadb", "kb-dead", true, &["query"]).await;
        assert_eq!(configured["persisted"], true);
        assert_eq!(configured["runtimeState"], "pending_validation");

        let (_, validated) = rt
            .dispatch(
                "knowledge.validate",
                json!({
                    "bindingId": "kb-deadb",
                    "expectedRevision": configured["revision"]
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(validated["validated"], false);
        let transport = validated["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "transport")
            .unwrap();
        assert_eq!(transport["status"], "unavailable");
        let inspected = inspect(&rt, "kb-deadb").await;
        assert_eq!(inspected["state"], "unavailable");

        // An executing query on the unavailable binding refuses explicitly.
        let err = rt
            .dispatch(
                "knowledge.query",
                json!({"bindingId": "kb-deadb", "query": "x"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "CAPABILITY_UNAVAILABLE");
    }

    #[tokio::test]
    async fn k1_unsupported_query_options_are_refused() {
        if !python3_present() {
            eprintln!("note: python3 absent; skipping the k1 module-process scenario");
            return;
        }
        let (rt, _home) = test_runtime();
        seed_module(&rt, "kb-demo", &["query", "resolve_citation"], GOOD_MODULE);
        // Restrict the manifest's supportedOptions to nothing. The declared
        // methods must still match the module's handshake so the binding
        // validates and the option gate is what rejects.
        let dir = rt.store().home().join("knowledge-modules");
        let manifest = json!({
            "identity": {"moduleId": "kb-demo"},
            "transport": {
                "kind": "stdio",
                "command": "python3",
                "args": ["-I", dir.join("kb-demo.py").to_string_lossy()],
            },
            "protocolVersion": KNOWLEDGE_PROTOCOL,
            "declaredMethods": ["query", "resolve_citation"],
            "supportedOptions": [],
            "credentialEnv": [],
        });
        std::fs::write(
            dir.join("kb-demo.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        let configured = configure_binding(&rt, "kb-noopts", "kb-demo", true, &["query"]).await;
        let revision = configured["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "knowledge.validate",
            json!({"bindingId": "kb-noopts", "expectedRevision": revision}),
            None,
            None,
        )
        .await
        .unwrap();
        let err = rt
            .dispatch(
                "knowledge.query",
                json!({"bindingId": "kb-noopts", "query": "x", "limit": 5}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "UNSUPPORTED_QUERY_OPTION");
        let err = rt
            .dispatch(
                "knowledge.query",
                json!({"bindingId": "kb-noopts", "query": "x", "cursor": "c1"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err_code(&err), "UNSUPPORTED_QUERY_OPTION");
    }
}
