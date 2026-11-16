//! 已认证 UDS 的 L-C 客户端；身份来自宿主对实际连接的认证，不接受 principal 参数。
use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// 服务端实际握手事实；仅可读取，不接受请求构造。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttestedConnection {
    principal: String,
    tenant: Option<String>,
}
impl AttestedConnection {
    pub fn principal(&self) -> &str {
        &self.principal
    }
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }
}

pub struct KernelConnection {
    stream: BufReader<UnixStream>,
    next_id: u64,
    timeout: Duration,
    in_flight: bool,
    usable: bool,
    attested: AttestedConnection,
}
struct PendingOwner(Option<Arc<dyn agentcpm_host::TrustedProjectOwner>>);
impl Drop for PendingOwner {
    fn drop(&mut self) {
        if let Some(owner) = self.0.take() {
            owner.revoke();
        }
    }
}
// A cancelled scoped bind cannot drop a genuinely bound Host owner while
// its close/release is unconfirmed. Keep the handle retained on Unknown.
struct ScopedBindGuard(Option<agentcpm_host::TrustedProjectSession>);
impl Drop for ScopedBindGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.revoke();
            std::mem::forget(handle);
        }
    }
}
impl KernelConnection {
    async fn connect_and_initialize(
        path: &std::path::Path,
        timeout: Duration,
        expected_uid: Option<u32>,
        client_features: Value,
    ) -> Result<(Self, Value), HostError> {
        if timeout.is_zero() || !path.is_absolute() {
            return Err(protocol(
                "Kernel UDS path must be absolute and timeout must be positive.",
            ));
        }
        let stream = tokio::time::timeout(timeout, UnixStream::connect(path))
            .await
            .map_err(|_| protocol("Kernel authenticated connection timed out."))?
            .map_err(|e| protocol(format!("Kernel authenticated connection failed: {e}")))?;
        if let Some(expected_uid) = expected_uid {
            let peer = stream.peer_cred().map_err(|e| {
                protocol(format!("Kernel UDS peer credential inspection failed: {e}"))
            })?;
            if peer.uid() != expected_uid {
                return Err(protocol(
                    "Kernel UDS peer UID does not match the explicitly configured deployment UID.",
                ));
            }
        }
        let mut wire = Self {
            stream: BufReader::new(stream),
            next_id: 0,
            timeout,
            in_flight: false,
            usable: true,
            attested: AttestedConnection {
                principal: String::new(),
                tenant: None,
            },
        };
        let init = wire
            .call(
                "initialize",
                json!({
                    "abi_version": 2,
                    "api_version": agentcpm_iface::API_VERSION,
                    "features": client_features,
                }),
            )
            .await?;
        wire.attested = attested_connection(&init)?;
        Ok((wire, init))
    }

    pub async fn connect_uds(path: &std::path::Path, timeout: Duration) -> Result<Self, HostError> {
        let (wire, init) = Self::connect_and_initialize(
            path,
            timeout,
            None,
            json!([
                "model-session/1.0",
                agentcpm_iface::model_session::MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
            ]),
        )
        .await?;
        installed_scheduler(&init)?;
        Ok(wire)
    }

    /// Scoped resident connection over attested UDS. Validates peer UID (when
    /// supplied), per-connection attestation, atomic journal, bound loop/controller
    /// refs, and explicit scoped features without requiring `LOOP_SETTLEMENT_FEATURE`.
    pub(crate) async fn connect_scoped_attested(
        path: &std::path::Path,
        timeout: Duration,
        expected_uid: Option<u32>,
        required_features: &[String],
        expected_loop: &str,
        expected_controller: &str,
    ) -> Result<Self, HostError> {
        if required_features.is_empty()
            || expected_loop.is_empty()
            || expected_controller.is_empty()
        {
            return Err(protocol(
                "Scoped Kernel connection requires non-empty feature and binding expectations.",
            ));
        }
        let client_features = if required_features
            .iter()
            .any(|f| f == p::model_session_resources::FEATURE)
        {
            json!([
                "model-session/1.0",
                agentcpm_iface::model_session::MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
            ])
        } else {
            json!([])
        };
        let (wire, init) =
            Self::connect_and_initialize(path, timeout, expected_uid, client_features).await?;
        validate_scoped_scheduler(&init, required_features, expected_loop, expected_controller)?;
        Ok(wire)
    }

    async fn create_and_bind_handle(
        &mut self,
        sessions: &agentcpm_host::TrustedProjectSessions,
        owner: Arc<crate::native_process::project_model_owner::ProjectModelOwner>,
        catalog: Vec<agentcpm_host::TrustedModelCatalogEntry>,
    ) -> Result<(agentcpm_host::TrustedProjectSession, Option<String>), HostError> {
        let mut owner = PendingOwner(Some(owner));
        let reply = self
            .call(
                "session.create",
                json!({
                    "project_owner": true,
                    "idempotency_key": uuid::Uuid::new_v4().to_string(),
                }),
            )
            .await?;
        let data = &reply["data"];
        let reference = data["creation_ref"].as_str().ok_or_else(|| {
            protocol("Kernel did not issue an authenticated project creation reference.")
        })?;
        let creation = sessions.claim_created(reference).map_err(kernel_error)?;
        let handle = sessions
            .bind_owner(creation, owner.0.take().expect("pending owner"), catalog)
            .map_err(kernel_error)?;
        Ok((handle, data["session_id"].as_str().map(str::to_owned)))
    }

    /// Consume the authenticated create receipt with the Host issuer and fail
    /// closed with real close/release confirmation if handle verification fails.
    pub(crate) async fn bind_project_session(
        &mut self,
        sessions: &agentcpm_host::TrustedProjectSessions,
        owner: Arc<crate::native_process::project_model_owner::ProjectModelOwner>,
        catalog: Vec<agentcpm_host::TrustedModelCatalogEntry>,
    ) -> Result<agentcpm_host::TrustedProjectSession, HostError> {
        let (handle, session_id) = self
            .create_and_bind_handle(sessions, owner, catalog)
            .await?;
        let mut bind_guard = ScopedBindGuard(Some(handle.clone()));
        if session_id.as_deref() != Some(handle.session_id()) || handle.validate_live().is_err() {
            handle.revoke();
            let receipt = self
                .call(
                    "session.close",
                    json!({
                        "session_id": handle.session_id(),
                        "generation": handle.generation(),
                        "reason": "scoped project bind rejected",
                    }),
                )
                .await;
            if !receipt.as_ref().is_ok_and(|receipt| {
                receipt["data"]["closed"] == true
                    && receipt["data"]["session_id"].as_str() == Some(handle.session_id())
            }) {
                return Err(unknown(
                    "Scoped rejected binding cleanup unconfirmed; owner retained.",
                ));
            }
            if tokio::time::timeout(self.timeout, async {
                while !handle.is_released() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .is_err()
            {
                return Err(unknown(
                    "Scoped rejected binding release unconfirmed; owner retained.",
                ));
            }
            drop(bind_guard.0.take());
            return Err(protocol("Scoped creation identity mismatch or revoked."));
        }
        drop(bind_guard.0.take());
        Ok(handle)
    }

    pub fn attested(&self) -> &AttestedConnection {
        &self.attested
    }

    /// 必须在真实 session prepare 中使用创建回复的一次引用；不能用业务 session key 替代。
    pub async fn create_project_session(
        mut self,
        sessions: &agentcpm_host::TrustedProjectSessions,
        owner: Arc<crate::native_process::project_model_owner::ProjectModelOwner>,
        catalog: Vec<agentcpm_host::TrustedModelCatalogEntry>,
        options: KernelExecutionOptions,
    ) -> Result<Arc<KernelSessionBackend>, HostError> {
        let product_owner = owner.clone();
        if options.loop_binding.is_empty()
            || options.controller_binding.is_empty()
            || options.module_id.is_empty()
            || options.cleanup_timeout.is_zero()
        {
            return Err(protocol(
                "Kernel execution requires explicit bindings and a positive cleanup budget.",
            ));
        }
        let (handle, session_id) = self
            .create_and_bind_handle(sessions, owner, catalog)
            .await?;
        let backend = Arc::new(KernelSessionBackend {
            inner: Arc::new(SessionInner {
                connection: tokio::sync::Mutex::new(self),
                handle,
                product_owner,
                options,
                turn: AtomicU64::new(0),
                event_cursor: AtomicU64::new(0),
                lease_epoch: AtomicU64::new(0),
                close_notify: tokio::sync::Notify::new(),
                active: AtomicBool::new(false),
                closing: AtomicBool::new(false),
                active_call: Mutex::new(None),
                runtime: tokio::runtime::Handle::current(),
            }),
        });
        // 从bind到返回之间无await；任何同步拒绝由backend的真实RPC清理所有者接手。
        if session_id.as_deref() != Some(backend.inner.handle.session_id()) {
            return Err(protocol(
                "Authenticated project handle does not match the creation reply.",
            ));
        }
        backend.inner.handle.validate_live().map_err(kernel_error)?;
        Ok(backend)
    }

    pub(super) async fn call(&mut self, method: &str, params: Value) -> Result<Value, HostError> {
        if !self.usable || self.in_flight {
            self.usable = false;
            let _ = self.stream.get_mut().shutdown().await;
            return Err(unknown(
                "Kernel connection has an unconfirmed prior request; replay is prohibited.",
            ));
        }
        // 若future被取消，此位仍为true，下次调用关闭连接而不误读迟到回复。
        self.in_flight = true;
        let timeout = self.timeout;
        let result = tokio::time::timeout(timeout, self.call_inner(method, params))
            .await
            .unwrap_or_else(|_| {
                Err(unknown(
                    "Kernel RPC deadline expired; outcome is not confirmed.",
                ))
            });
        self.in_flight = false;
        if result
            .as_ref()
            .is_err_and(|e| e.kind == HostErrorKind::ResultUnknown)
        {
            self.usable = false;
            let _ = self.stream.get_mut().shutdown().await;
        }
        result
    }
    async fn call_inner(&mut self, method: &str, params: Value) -> Result<Value, HostError> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| protocol("Kernel RPC identity exhausted."))?;
        let id = self.next_id;
        let mut bytes =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .map_err(|_| protocol("Kernel request is not JSON."))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(protocol(
                "Kernel request exceeds the configured frame bound.",
            ));
        }
        bytes.push(b'\n');
        self.stream
            .get_mut()
            .write_all(&bytes)
            .await
            .map_err(|e| unknown(format!("Kernel request delivery failed: {e}")))?;
        self.stream
            .get_mut()
            .flush()
            .await
            .map_err(|e| unknown(format!("Kernel request flush failed: {e}")))?;
        loop {
            let mut line = Vec::new();
            loop {
                let available = self
                    .stream
                    .fill_buf()
                    .await
                    .map_err(|e| unknown(format!("Kernel reply read failed: {e}")))?;
                if available.is_empty() {
                    return Err(unknown("Kernel connection closed before the reply."));
                }
                let count = available
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(available.len(), |i| i + 1);
                if line.len().saturating_add(count) > MAX_FRAME_BYTES {
                    return Err(unknown("Kernel reply exceeds the frame bound."));
                }
                let end = available[count - 1] == b'\n';
                line.extend_from_slice(&available[..count]);
                self.stream.consume(count);
                if end {
                    break;
                }
            }
            let reply: Value = serde_json::from_slice(&line)
                .map_err(|_| unknown("Kernel reply is invalid JSON."))?;
            if reply["jsonrpc"] != "2.0" {
                return Err(unknown("Kernel reply protocol mismatch."));
            }
            if reply.get("id").is_none() {
                continue;
            }
            if reply["id"] != id {
                return Err(unknown("Kernel reply has a different request identity."));
            }
            if let Some(error) = reply.get("error") {
                return Err(rpc_rejection(method, error));
            }
            return reply
                .get("result")
                .cloned()
                .ok_or_else(|| unknown("Kernel reply has no result."));
        }
    }
}

fn scheduler_supports(init: &Value, required_features: &[&str]) -> bool {
    let scheduler = &init["server_capabilities"]["scheduler"];
    init["abi_version"] == 2
        && scheduler["protocol"] == p::PROTOCOL
        && scheduler["atomic_journal"] == true
        && scheduler["features"].as_array().is_some_and(|features| {
            required_features.iter().all(|required| {
                features
                    .iter()
                    .any(|value| value.as_str() == Some(*required))
            })
        })
}

fn installed_scheduler(init: &Value) -> Result<(), HostError> {
    if !scheduler_supports(
        init,
        &[
            p::INPUT_LIMITS_FEATURE,
            p::OUTPUT_DEMAND_FEATURE,
            p::SETTLEMENT_DATA_FEATURE,
            p::LOOP_SETTLEMENT_FEATURE,
        ],
    ) {
        return Err(protocol(
            "Unsupported Kernel integration: required public execution features or atomic journal are unavailable.",
        ));
    }
    Ok(())
}

fn validate_scoped_scheduler(
    init: &Value,
    required_features: &[String],
    expected_loop: &str,
    expected_controller: &str,
) -> Result<(), HostError> {
    let scheduler = &init["server_capabilities"]["scheduler"];
    let required: Vec<&str> = required_features.iter().map(String::as_str).collect();
    if !scheduler_supports(init, &required)
        || scheduler["default_loop_ref"].as_str() != Some(expected_loop)
        || scheduler["input_controller_ref"].as_str() != Some(expected_controller)
    {
        return Err(protocol(
            "Scoped Kernel resident does not advertise the required atomic Scheduler, bindings, or scoped features.",
        ));
    }
    Ok(())
}
fn attested_connection(reply: &Value) -> Result<AttestedConnection, HostError> {
    let security = &reply["transport_security"];
    let principal = security["principal"]
        .as_str()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| protocol("Kernel connection has no authenticated principal."))?;
    if security["kind"] != "unix"
        || security["attested"] != true
        || security["principal_scope"] != "per-connection"
        || reply["transport_security_required"] != true
    {
        return Err(protocol(
            "Kernel UDS connection did not preserve strict per-connection authentication.",
        ));
    }
    let tenant = security["tenant"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    Ok(AttestedConnection {
        principal: principal.to_owned(),
        tenant,
    })
}
fn rpc_rejection(method: &str, raw: &Value) -> HostError {
    let message = raw["message"]
        .as_str()
        .unwrap_or("Kernel rejected the request.");
    let mut error = HostError::new(HostErrorKind::Callback, message);
    // 保留正式JSON诊断；绝不从Debug或文本猜测接纳事实。
    error.diagnostic = json!({"method":method,"rpc_error":raw}).to_string();
    if let Some(reason) = raw["data"]["reason"].as_str() {
        let mut module = crate::host::ModuleError::new(message);
        module.code = Some(reason.into());
        error.module_error = Some(Box::new(module));
    }
    error
}
/// 这些原因在正式execution.input接纳前产生；其他错误可能已接纳，保持Unknown。
pub(super) fn input_failure(mut error: HostError) -> HostError {
    let raw = serde_json::from_str::<Value>(&error.diagnostic).unwrap_or(Value::Null);
    let detail = &raw["rpc_error"]["data"];
    if detail["domain"] == "scheduler"
        && matches!(
            detail["reason"].as_str(),
            Some(
                "DEADLINE"
                    | "INVALID_PARAMS"
                    | "SESSION_REQUIRED"
                    | "SESSION_CLOSED"
                    | "STALE_GENERATION"
                    | "SESSION_FORBIDDEN"
                    | "POLICY_DENIED"
                    | "PERMIT_REUSED"
            )
        )
    {
        return error;
    }
    error.kind = HostErrorKind::ResultUnknown;
    error.accepted = true;
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_server_attested_uds_identity_can_be_used() {
        let good = json!({"transport_security_required":true,"transport_security":{"kind":"unix","attested":true,"principal_scope":"per-connection","principal":"gateway","tenant":"deployment"}});
        assert_eq!(
            attested_connection(&good).unwrap().tenant(),
            Some("deployment")
        );
        for (key, value) in [
            ("attested", json!(false)),
            ("kind", json!("loopback")),
            ("principal_scope", json!("per-listener")),
            ("principal", json!("")),
        ] {
            let mut bad = good.clone();
            bad["transport_security"][key] = value;
            assert!(attested_connection(&bad).is_err());
        }
        let mut relaxed = good;
        relaxed["transport_security_required"] = json!(false);
        assert!(attested_connection(&relaxed).is_err());
    }
    #[test]
    fn advertised_features_without_actual_atomic_store_are_rejected() {
        let good = json!({"abi_version":2,"server_capabilities":{"scheduler":{"protocol":p::PROTOCOL,"atomic_journal":true,"features":[p::INPUT_LIMITS_FEATURE,p::OUTPUT_DEMAND_FEATURE,p::SETTLEMENT_DATA_FEATURE,p::LOOP_SETTLEMENT_FEATURE]}}});
        assert!(installed_scheduler(&good).is_ok());
        for value in [Value::Null, json!(false)] {
            let mut bad = good.clone();
            bad["server_capabilities"]["scheduler"]["atomic_journal"] = value;
            assert!(installed_scheduler(&bad).is_err());
        }
        let mut full_without_snapshot = good.clone();
        full_without_snapshot["server_capabilities"]["scheduler"]["features"] = json!([
            p::INPUT_LIMITS_FEATURE,
            p::OUTPUT_DEMAND_FEATURE,
            p::LOOP_SETTLEMENT_FEATURE
        ]);
        assert!(installed_scheduler(&full_without_snapshot).is_err());
        let mut missing = good;
        missing["server_capabilities"]["scheduler"]["features"] = json!([p::INPUT_LIMITS_FEATURE]);
        assert!(installed_scheduler(&missing).is_err());
    }
    #[test]
    fn input_deadline_is_definite_but_unknown_or_wrong_domain_is_not() {
        for reason in ["DEADLINE", "INVALID_PARAMS", "POLICY_DENIED"] {
            let raw = json!({"code":-32000,"message":"refused","data":{"domain":"scheduler","reason":reason}});
            let error = input_failure(rpc_rejection("execution.input", &raw));
            assert!(!error.accepted);
            assert_eq!(error.kind, HostErrorKind::Callback);
            assert_eq!(error.module_error.unwrap().code, Some(reason.into()));
        }
        for raw in [
            json!({"message":"uncertain","data":{"domain":"scheduler","reason":"OUTCOME_UNKNOWN"}}),
            json!({"message":"DEADLINE","data":{"domain":"other","reason":"DEADLINE"}}),
        ] {
            let error = input_failure(rpc_rejection("execution.input", &raw));
            assert!(error.accepted);
            assert_eq!(error.kind, HostErrorKind::ResultUnknown);
        }
    }

    #[test]
    fn scoped_scheduler_requires_bindings_atomic_journal_and_all_scoped_features() {
        let required: Vec<String> = [
            p::INPUT_LIMITS_FEATURE,
            p::OUTPUT_DEMAND_FEATURE,
            p::tool_batch::FEATURE,
            p::model_session_resources::FEATURE,
            p::context_history::FEATURE,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let good = json!({
            "abi_version": 2,
            "server_capabilities": {
                "scheduler": {
                    "protocol": p::PROTOCOL,
                    "atomic_journal": true,
                    "default_loop_ref": "pilotdeck",
                    "input_controller_ref": "controller",
                    "features": required,
                }
            }
        });
        assert!(validate_scoped_scheduler(&good, &required, "pilotdeck", "controller").is_ok());

        for field in ["default_loop_ref", "input_controller_ref"] {
            let mut wrong = good.clone();
            wrong["server_capabilities"]["scheduler"][field] = json!("other");
            assert!(
                validate_scoped_scheduler(&wrong, &required, "pilotdeck", "controller").is_err()
            );
        }
        for i in 0..required.len() {
            let mut missing = good.clone();
            let mut features = required.clone();
            features.remove(i);
            missing["server_capabilities"]["scheduler"]["features"] = json!(features);
            assert!(
                validate_scoped_scheduler(&missing, &required, "pilotdeck", "controller").is_err(),
                "missing feature {} must fail closed",
                required[i]
            );
        }
        let mut non_atomic = good;
        non_atomic["server_capabilities"]["scheduler"]["atomic_journal"] = json!(false);
        assert!(
            validate_scoped_scheduler(&non_atomic, &required, "pilotdeck", "controller").is_err()
        );
    }

    #[tokio::test]
    async fn connect_scoped_attested_rejects_mismatched_expected_uid() {
        let short_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::path::PathBuf::from("/tmp").join(format!("ws-uid-{}", &short_id[..12]));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("k.s");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let accept = tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let bogus_uid = rustix::process::geteuid().as_raw().wrapping_add(1);
        let err = KernelConnection::connect_scoped_attested(
            &sock,
            Duration::from_secs(2),
            Some(bogus_uid),
            &[p::tool_batch::FEATURE.into()],
            "pilotdeck",
            "controller",
        )
        .await
        .err()
        .expect("mismatched expected_uid must be rejected");
        assert_eq!(err.kind, HostErrorKind::Protocol);
        assert!(err.message.contains("UID"));
        let _ = accept.await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
