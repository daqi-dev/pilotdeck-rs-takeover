//! `agentcpm-pd-client`: shared CLI client state machine for target resolution,
//! credential lookup, `pd-connect/1` over `/v1/ws`, local Gateway auto-start,
//! subscription credit tracking, and idempotency key persistence (`WP2`).

pub mod autostart;
pub mod config;
pub mod error;
pub mod idempotency;
pub mod ws_client;

use std::path::PathBuf;
use std::sync::Arc;

use agentcpm_pd_protocol::contract::OperationResponse;
use agentcpm_pd_protocol::envelope::bind_instance_id;
use agentcpm_pd_protocol::id::{IdGenerator, NodeId};
use agentcpm_pd_protocol::registry::{
    OperationRegistry, RetryKind, validation_errors_to_contract_error,
};
use serde_json::Value;

use crate::autostart::{GatewaySpawner, ProcessGatewaySpawner, ensure_local_gateway_running};
use crate::config::{ResolvedTarget, resolve_pd_home, resolve_target};
use crate::error::ClientError;
use crate::idempotency::{IdempotencyLedger, LedgerEntryStatus};
use crate::ws_client::{PdWsClient, PdWsConnectOptions};

#[derive(Clone, Default)]
pub struct PdClientOptions {
    pub home_override: Option<PathBuf>,
    pub target: Option<String>,
    pub no_start: bool,
    pub spawner: Option<Arc<dyn GatewaySpawner>>,
}

/// High-level client used uniformly by CLI commands, `api invoke`, and `api connect`.
pub struct PdClient {
    home: PathBuf,
    target: ResolvedTarget,
    id_gen: Arc<IdGenerator>,
    ledger: IdempotencyLedger,
    ws: PdWsClient,
}

impl PdClient {
    pub async fn connect(opts: PdClientOptions) -> Result<Self, ClientError> {
        let home = resolve_pd_home(opts.home_override.as_deref())?;
        let target = resolve_target(&home, opts.target.as_deref())?;

        let (ws_url, token, expected_instance_id, id_gen) = match &target {
            ResolvedTarget::Local {
                home: target_home,
                metadata,
                token,
                ..
            } => {
                let spawner: Arc<dyn GatewaySpawner> = match opts.spawner {
                    Some(s) => s,
                    None => Arc::new(ProcessGatewaySpawner::current_exe()?),
                };
                let ready = ensure_local_gateway_running(
                    target_home,
                    &metadata.instance_id,
                    opts.no_start,
                    spawner,
                )
                .await?;
                let node =
                    NodeId::new(metadata.node_id.clone()).unwrap_or_else(|_| NodeId::local());
                let id_gen = Arc::new(IdGenerator::new(node));
                (
                    ready.ws_url,
                    Some(token.clone()),
                    Some(metadata.instance_id.clone()),
                    id_gen,
                )
            }
            ResolvedTarget::Remote {
                ws_url,
                expected_instance_id,
                token,
                ..
            } => (
                ws_url.clone(),
                token.clone(),
                expected_instance_id.clone(),
                Arc::new(IdGenerator::new(NodeId::local())),
            ),
        };

        let ledger = IdempotencyLedger::new(&home);
        let ws = PdWsClient::connect(PdWsConnectOptions {
            ws_url,
            token,
            expected_instance_id,
            client_name: "agentcpm-pd".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            id_gen: id_gen.clone(),
        })
        .await?;

        Ok(Self {
            home,
            target,
            id_gen,
            ledger,
            ws,
        })
    }

    pub fn home(&self) -> &PathBuf {
        &self.home
    }

    pub fn target(&self) -> &ResolvedTarget {
        &self.target
    }

    pub fn ws(&self) -> &PdWsClient {
        &self.ws
    }

    pub fn ledger(&self) -> &IdempotencyLedger {
        &self.ledger
    }

    pub fn id_gen(&self) -> &Arc<IdGenerator> {
        &self.id_gen
    }

    /// Invokes a remote operation with automatic durable idempotency key persistence for
    /// `idempotentSubmit` (and optional `--request-key` validation on any write).
    pub async fn invoke(
        &self,
        method_id: &str,
        mut input: Value,
        explicit_request_key: Option<&str>,
        expected_revision: Option<String>,
        timeout_override: Option<std::time::Duration>,
    ) -> Result<OperationResponse, ClientError> {
        let reg = OperationRegistry::global();
        let spec = reg
            .get(method_id)
            .ok_or_else(|| ClientError::usage(format!("unknown operation `{method_id}`")))?;

        bind_instance_id(
            &mut input,
            self.ws.bound_instance_id(),
            spec.declares_instance_id,
        )?;
        reg.validate_input(method_id, &input)
            .map_err(|errs| ClientError::Contract(validation_errors_to_contract_error(errs)))?;

        let needs_key = spec.retry == RetryKind::IdempotentSubmit || explicit_request_key.is_some();
        let key = if needs_key {
            let preview_req_id = self.id_gen.next_id();
            Some(self.ledger.prepare_key(
                method_id,
                &input,
                explicit_request_key,
                &preview_req_id,
                &self.id_gen,
            )?)
        } else {
            None
        };

        let result = self
            .ws
            .call_operation(
                method_id,
                input,
                key.clone(),
                expected_revision,
                timeout_override,
            )
            .await;

        if let Some(k) = &key {
            match &result {
                Ok(_) => {
                    let _ = self
                        .ledger
                        .update_status(k, LedgerEntryStatus::Confirmed, None);
                }
                Err(err) => {
                    let status = if err.is_result_unknown() {
                        LedgerEntryStatus::Unknown
                    } else {
                        LedgerEntryStatus::FailedSafe
                    };
                    let code = err.into_contract_error().code;
                    let _ = self.ledger.update_status(k, status, Some(&code));
                }
            }
        }

        result
    }

    pub async fn close(&self) {
        self.ws.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use agentcpm_pd_protocol::errors::CODE_IDEMPOTENCY_CONFLICT;
    use agentcpm_pd_protocol::wire::{
        ABI_VERSION, InitializeResult, JsonRpcRequest, JsonRpcResponse, OperationRpcResult,
        ProtocolLimits, TargetHandshakeInfo, WIRE_INITIALIZE,
    };
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    use crate::autostart::SpawnGatewayFuture;
    use crate::config::{
        ClientConfig, CredentialsFile, LocalInstanceMetadata, ReadyFile, TargetKind, TargetProfile,
        atomic_write_file, local_metadata_path, local_token_path, ready_path, save_client_config,
        save_credentials,
    };
    use crate::ws_client::unwrap_rpc_outcome;

    struct TempHome(PathBuf);

    impl TempHome {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "agentcpm-pd-wp2-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            crate::config::ensure_secure_dir(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn seed_local_home(home: &std::path::Path, instance_id: &str) {
        let meta = LocalInstanceMetadata {
            instance_id: instance_id.to_owned(),
            owner_id: "owner-local".to_owned(),
            product: "agentcpm-pd".to_owned(),
            protocol_version: agentcpm_pd_protocol::contract::OPERATION_VERSION.to_owned(),
            node_id: "local".to_owned(),
            initialized_at: "2026-11-17T00:00:00Z".to_owned(),
            initialization_revision: "init-1".to_owned(),
        };
        atomic_write_file(
            &local_metadata_path(home),
            &serde_json::to_vec_pretty(&meta).unwrap(),
            false,
        )
        .unwrap();
        atomic_write_file(&local_token_path(home), b"secret-local-token", true).unwrap();
    }

    async fn spawn_mock_ws_server(
        instance_id: String,
        delay_run_submit_ms: Option<u64>,
        drop_after_submit: bool,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let iid = instance_id.clone();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(txt))) = ws.next().await {
                        let Ok(req) = serde_json::from_str::<JsonRpcRequest>(&txt) else {
                            continue;
                        };
                        match req.method.as_str() {
                            WIRE_INITIALIZE => {
                                let res = InitializeResult {
                                    abi_version: ABI_VERSION,
                                    supported: true,
                                    api_version: agentcpm_pd_protocol::contract::OPERATION_VERSION
                                        .to_owned(),
                                    supported_features: vec!["subscriptions".to_owned()],
                                    connection_id: "conn-mock-1".to_owned(),
                                    cli: None,
                                    target: TargetHandshakeInfo {
                                        name: "local".to_owned(),
                                        instance_id: iid.clone(),
                                        node_id: "local".to_owned(),
                                        gateway_version: "0.1.0".to_owned(),
                                        schema_digest: "deadbeef".to_owned(),
                                        transport: "ws".to_owned(),
                                    },
                                    limits: ProtocolLimits::default(),
                                };
                                let frame =
                                    JsonRpcResponse::ok(req.id, serde_json::to_value(res).unwrap());
                                let _ = ws
                                    .send(Message::Text(
                                        serde_json::to_string(&frame).unwrap().into(),
                                    ))
                                    .await;
                            }
                            "run.submit" => {
                                if drop_after_submit {
                                    let _ = ws.close(None).await;
                                    break;
                                }
                                if let Some(ms) = delay_run_submit_ms {
                                    tokio::time::sleep(Duration::from_millis(ms)).await;
                                }
                                let rpc_res = OperationRpcResult {
                                    outcome:
                                        agentcpm_pd_protocol::contract::ResponseOutcome::Accepted,
                                    result: json!({
                                        "kind": "run",
                                        "runId": "local-run-1",
                                        "durable": true,
                                        "acceptedRevision": "rev-2",
                                        "deduplicated": false,
                                        "state": "queued"
                                    }),
                                };
                                let frame = JsonRpcResponse::ok(
                                    req.id,
                                    serde_json::to_value(rpc_res).unwrap(),
                                );
                                let _ = ws
                                    .send(Message::Text(
                                        serde_json::to_string(&frame).unwrap().into(),
                                    ))
                                    .await;
                            }
                            other => {
                                let rpc_res = OperationRpcResult {
                                    outcome:
                                        agentcpm_pd_protocol::contract::ResponseOutcome::Succeeded,
                                    result: json!({"echoMethod": other, "instanceId": iid}),
                                };
                                let frame = JsonRpcResponse::ok(
                                    req.id,
                                    serde_json::to_value(rpc_res).unwrap(),
                                );
                                let _ = ws
                                    .send(Message::Text(
                                        serde_json::to_string(&frame).unwrap().into(),
                                    ))
                                    .await;
                            }
                        }
                    }
                });
            }
        });
        format!("ws://{addr}/v1/ws")
    }

    #[test]
    fn t4_remote_target_failure_never_falls_back_to_local_and_rejects_plaintext_remote_ws() {
        let tmp = TempHome::new("d8");
        seed_local_home(&tmp.0, "local-019a0000-0000-7000-8000-000000000001");

        let mut targets = BTreeMap::new();
        targets.insert(
            "prod-remote".to_owned(),
            TargetProfile {
                id: "prod-remote".to_owned(),
                kind: TargetKind::Remote,
                endpoint_url: Some("wss://127.0.0.1:65432/v1/ws".to_owned()),
                data_root: None,
                expected_instance_id: Some("remote-inst-99".to_owned()),
                tls_fingerprint: None,
                credential_ref: Some("prod-tok".to_owned()),
            },
        );
        targets.insert(
            "bad-plain".to_owned(),
            TargetProfile {
                id: "bad-plain".to_owned(),
                kind: TargetKind::Remote,
                endpoint_url: Some("ws://192.168.10.201:9000/v1/ws".to_owned()),
                data_root: None,
                expected_instance_id: None,
                tls_fingerprint: None,
                credential_ref: None,
            },
        );
        save_client_config(
            &tmp.0,
            &ClientConfig {
                default_target: Some("prod-remote".to_owned()),
                targets,
            },
        )
        .unwrap();
        let mut tokens = BTreeMap::new();
        tokens.insert("prod-tok".to_owned(), "remote-bearer".to_owned());
        save_credentials(&tmp.0, &CredentialsFile { tokens }).unwrap();

        let resolved = resolve_target(&tmp.0, None).unwrap();
        assert!(!resolved.is_local());
        assert_eq!(resolved.expected_instance_id(), Some("remote-inst-99"));

        let missing = resolve_target(&tmp.0, Some("nonexistent-profile")).unwrap_err();
        assert!(matches!(missing, ClientError::Config(_)));

        let plain_err = resolve_target(&tmp.0, Some("bad-plain")).unwrap_err();
        assert_eq!(plain_err.into_contract_error().code, "FORBIDDEN");
    }

    #[test]
    fn idempotency_ledger_persists_before_dispatch_and_rejects_mismatched_payload() {
        let tmp = TempHome::new("idem");
        let ledger = IdempotencyLedger::new(&tmp.0);
        let id_gen = IdGenerator::new(NodeId::local());

        let input_a =
            json!({"instanceId": "inst-1", "sessionId": "s1", "input": {"text": "hello"}});
        let key = ledger
            .prepare_key("run.submit", &input_a, Some("key-123"), "req-1", &id_gen)
            .unwrap();
        assert_eq!(key, "key-123");

        let disk_rec = ledger.get("key-123").unwrap().unwrap();
        assert_eq!(disk_rec.status, LedgerEntryStatus::Pending);

        let reordered =
            json!({"sessionId": "s1", "input": {"text": "hello"}, "instanceId": "inst-1"});
        assert_eq!(
            ledger
                .prepare_key("run.submit", &reordered, Some("key-123"), "req-2", &id_gen)
                .unwrap(),
            "key-123"
        );

        let different =
            json!({"instanceId": "inst-1", "sessionId": "s1", "input": {"text": "goodbye"}});
        let err = ledger
            .prepare_key("run.submit", &different, Some("key-123"), "req-3", &id_gen)
            .unwrap_err();
        assert_eq!(err.into_contract_error().code, CODE_IDEMPOTENCY_CONFLICT);
    }

    struct MockSingleWinnerSpawner {
        spawn_count: Arc<AtomicUsize>,
        ws_url: String,
        instance_id: String,
    }

    impl GatewaySpawner for MockSingleWinnerSpawner {
        fn spawn(&self, home: &std::path::Path) -> SpawnGatewayFuture {
            let count = self.spawn_count.clone();
            let ws_url = self.ws_url.clone();
            let iid = self.instance_id.clone();
            let home_dir = home.to_path_buf();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(80)).await;
                let ready = ReadyFile {
                    pid: std::process::id(),
                    instance_id: iid,
                    generation: "gen-1".to_owned(),
                    protocol_version: agentcpm_pd_protocol::contract::OPERATION_VERSION.to_owned(),
                    ws_url,
                    http_url: None,
                    started_at: "2026-11-17T00:00:00Z".to_owned(),
                    server_revision: "rev-1".to_owned(),
                };
                atomic_write_file(
                    &ready_path(&home_dir),
                    &serde_json::to_vec_pretty(&ready).unwrap(),
                    false,
                )?;
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn concurrent_3x_autostart_elects_single_winner_and_t5_timeout_returns_result_unknown() {
        let tmp = TempHome::new("autostart-t5");
        let iid = "local-019a0000-0000-7000-8000-000000000002".to_owned();
        seed_local_home(&tmp.0, &iid);

        let ws_url = spawn_mock_ws_server(iid.clone(), Some(350), false).await;
        let spawns = Arc::new(AtomicUsize::new(0));
        let spawner: Arc<dyn GatewaySpawner> = Arc::new(MockSingleWinnerSpawner {
            spawn_count: spawns.clone(),
            ws_url: ws_url.clone(),
            instance_id: iid.clone(),
        });

        let (r1, r2, r3) = tokio::join!(
            PdClient::connect(PdClientOptions {
                home_override: Some(tmp.0.clone()),
                target: None,
                no_start: false,
                spawner: Some(spawner.clone()),
            }),
            PdClient::connect(PdClientOptions {
                home_override: Some(tmp.0.clone()),
                target: None,
                no_start: false,
                spawner: Some(spawner.clone()),
            }),
            PdClient::connect(PdClientOptions {
                home_override: Some(tmp.0.clone()),
                target: None,
                no_start: false,
                spawner: Some(spawner.clone()),
            }),
        );
        let c1 = r1.unwrap();
        let c2 = r2.unwrap();
        let c3 = r3.unwrap();
        assert_eq!(spawns.load(Ordering::SeqCst), 1);

        let err = c1
            .invoke(
                "run.submit",
                json!({"sessionId": "s1", "input": [{"kind": "text", "text": "slow"}]}),
                Some("k-t5"),
                None,
                Some(Duration::from_millis(40)),
            )
            .await
            .unwrap_err();
        assert!(err.is_result_unknown());
        assert_eq!(err.exit_code_for_outcome(), 3);
        let rec = c1.ledger().get("k-t5").unwrap().unwrap();
        assert_eq!(rec.status, LedgerEntryStatus::Unknown);

        let drop_url = spawn_mock_ws_server(iid.clone(), None, true).await;
        let drop_ws = PdWsClient::connect(PdWsConnectOptions::for_local(drop_url, "tok", &iid))
            .await
            .unwrap();
        let err_drop = drop_ws
            .call_operation(
                "run.submit",
                json!({"sessionId": "s1", "input": [{"kind": "text", "text": "drop"}]}),
                Some("k-drop".to_owned()),
                None,
                Some(Duration::from_secs(2)),
            )
            .await
            .unwrap_err();
        assert!(err_drop.is_result_unknown());
        assert_eq!(err_drop.exit_code_for_outcome(), 3);

        c1.close().await;
        c2.close().await;
        c3.close().await;
        drop_ws.close().await;
    }

    #[tokio::test]
    async fn t10_instance_change_during_reconnect_fails_and_no_start_blocks_boot() {
        let tmp = TempHome::new("t10");
        let old_id = "local-019a0000-0000-7000-8000-000000000010";
        let new_id = "local-019a0000-0000-7000-8000-000000000099";
        seed_local_home(&tmp.0, old_id);

        let spawner: Arc<dyn GatewaySpawner> = Arc::new(MockSingleWinnerSpawner {
            spawn_count: Arc::new(AtomicUsize::new(0)),
            ws_url: "ws://127.0.0.1:1/v1/ws".to_owned(),
            instance_id: old_id.to_owned(),
        });
        let err_no_start = PdClient::connect(PdClientOptions {
            home_override: Some(tmp.0.clone()),
            target: None,
            no_start: true,
            spawner: Some(spawner),
        })
        .await;
        assert!(matches!(
            err_no_start,
            Err(ClientError::ConnectionFailed(_))
        ));

        let changed_url = spawn_mock_ws_server(new_id.to_owned(), None, false).await;
        let err = PdWsClient::connect_with_backoff(
            PdWsConnectOptions::for_local(changed_url, "tok", old_id),
            3,
        )
        .await;
        assert!(matches!(
            err,
            Err(ClientError::InstanceChanged { ref expected, ref actual })
            if expected == old_id && actual == new_id
        ));

        let ok_url = spawn_mock_ws_server(old_id.to_owned(), None, false).await;
        let ok_ws = PdWsClient::connect(PdWsConnectOptions::for_local(ok_url, "tok", old_id))
            .await
            .unwrap();
        let resp = ok_ws
            .call_operation(
                "run.submit",
                json!({"sessionId": "s1", "input": [{"kind": "text", "text": "hi"}]}),
                Some("k-ok".to_owned()),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(unwrap_rpc_outcome(&resp)["runId"], "local-run-1");
        ok_ws.close().await;
    }

    /// Regression: `sub.open` response, event frames, and `sub.end` arriving
    /// back-to-back must not lose events to the response->registration window
    /// (parked frames are flushed in arrival order by `open_subscription`).
    #[tokio::test]
    async fn subscription_events_racing_registration_are_parked_and_delivered_in_order() {
        use crate::ws_client::SubStreamItem;
        use agentcpm_pd_protocol::contract::EventEnvelope;
        use agentcpm_pd_protocol::wire::{
            JsonRpcNotification, SubEndNotification, SubEventNotification, SubOpenResult,
            SubTerminal, WIRE_SUB_END, WIRE_SUB_EVENT, WIRE_SUB_OPEN,
        };

        let instance_id = "local-019a0000-0000-7000-8000-000000000031".to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let iid = instance_id.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(txt))) = ws.next().await {
                let Ok(req) = serde_json::from_str::<JsonRpcRequest>(&txt) else {
                    continue;
                };
                match req.method.as_str() {
                    WIRE_INITIALIZE => {
                        let res = InitializeResult {
                            abi_version: ABI_VERSION,
                            supported: true,
                            api_version: agentcpm_pd_protocol::contract::OPERATION_VERSION
                                .to_owned(),
                            supported_features: vec!["subscriptions".to_owned()],
                            connection_id: "conn-race-1".to_owned(),
                            cli: None,
                            target: TargetHandshakeInfo {
                                name: "local".to_owned(),
                                instance_id: iid.clone(),
                                node_id: "local".to_owned(),
                                gateway_version: "0.1.0".to_owned(),
                                schema_digest: "deadbeef".to_owned(),
                                transport: "ws".to_owned(),
                            },
                            limits: ProtocolLimits::default(),
                        };
                        let frame = JsonRpcResponse::ok(req.id, serde_json::to_value(res).unwrap());
                        let _ = ws
                            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
                            .await;
                    }
                    WIRE_SUB_OPEN => {
                        // Response + 5 events + end, written back-to-back so they
                        // can all land before the client registers the channel.
                        let sub_id = "sub-race-1".to_owned();
                        let open = JsonRpcResponse::ok(
                            req.id,
                            serde_json::to_value(SubOpenResult {
                                subscription_id: sub_id.clone(),
                            })
                            .unwrap(),
                        );
                        let _ = ws
                            .send(Message::Text(serde_json::to_string(&open).unwrap().into()))
                            .await;
                        for seq in 1..=5u32 {
                            let ev = SubEventNotification {
                                subscription_id: sub_id.clone(),
                                event: EventEnvelope {
                                    version: "agentcpm-pd.event/1".to_owned(),
                                    instance_id: iid.clone(),
                                    run_id: Some("run-race-1".to_owned()),
                                    operation_id: None,
                                    subscription_id: Some(sub_id.clone()),
                                    cursor: format!("cur-{seq}"),
                                    event_type: "run.progress".to_owned(),
                                    data: serde_json::Map::new(),
                                },
                            };
                            let notif = JsonRpcNotification {
                                jsonrpc: "2.0".to_owned(),
                                method: WIRE_SUB_EVENT.to_owned(),
                                params: serde_json::to_value(ev).unwrap(),
                            };
                            let _ = ws
                                .send(Message::Text(serde_json::to_string(&notif).unwrap().into()))
                                .await;
                        }
                        let end = SubEndNotification {
                            subscription_id: sub_id,
                            terminal: SubTerminal::Completed,
                            resume_cursor: Some("cur-5".to_owned()),
                            gap: None,
                            error: None,
                        };
                        let notif = JsonRpcNotification {
                            jsonrpc: "2.0".to_owned(),
                            method: WIRE_SUB_END.to_owned(),
                            params: serde_json::to_value(end).unwrap(),
                        };
                        let _ = ws
                            .send(Message::Text(serde_json::to_string(&notif).unwrap().into()))
                            .await;
                    }
                    _ => {}
                }
            }
        });

        let ws = PdWsClient::connect(PdWsConnectOptions::for_local(
            format!("ws://{addr}/v1/ws"),
            "tok",
            &instance_id,
        ))
        .await
        .unwrap();

        let mut sub = ws
            .open_subscription(
                "run.events",
                json!({"runId": "run-race-1", "follow": true}),
                Some(16),
                false,
            )
            .await
            .unwrap();

        let mut cursors = Vec::new();
        loop {
            match sub.recv().await {
                Some(SubStreamItem::Event(ev)) => cursors.push(ev.event.cursor.clone()),
                Some(SubStreamItem::End(end)) => {
                    assert_eq!(end.terminal, SubTerminal::Completed);
                    break;
                }
                None => panic!("subscription closed before the terminal frame"),
            }
        }
        assert_eq!(
            cursors,
            vec!["cur-1", "cur-2", "cur-3", "cur-4", "cur-5"],
            "events must survive the registration race in arrival order"
        );
        ws.close().await;
    }
}
