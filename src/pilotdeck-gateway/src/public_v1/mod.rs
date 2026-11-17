//! Public versioned `/v1/ws` Gateway module (`WP4`, `007 §7`).

pub mod bootstrap;
pub mod runtime;
pub mod store;
pub mod ws;

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Response, StatusCode, header};
use serde_json::Value;

use crate::Body;
use crate::public_v1::runtime::PublicV1Runtime;

/// Shared state mounted on `NativeWsServer::with_public_v1` and standalone `serve_v1_gateway`.
pub struct PublicV1State {
    pub runtime: Arc<PublicV1Runtime>,
    bearer_token: String,
}

impl PublicV1State {
    pub fn new(runtime: Arc<PublicV1Runtime>, bearer_token: String) -> Self {
        Self {
            runtime,
            bearer_token,
        }
    }

    pub fn verify_token_str(&self, candidate: &str) -> bool {
        constant_time_eq(candidate.as_bytes(), self.bearer_token.as_bytes())
    }

    pub fn authorize_headers(&self, headers: &HeaderMap) -> bool {
        let Some(auth) = headers.get(header::AUTHORIZATION) else {
            return false;
        };
        let Ok(raw) = auth.to_str() else {
            return false;
        };
        let Some(token) = raw
            .strip_prefix("Bearer ")
            .or_else(|| raw.strip_prefix("bearer "))
        else {
            return false;
        };
        self.verify_token_str(token.trim())
    }

    pub fn unauthorized_http_response() -> Response<Body> {
        Self::json_response(
            StatusCode::UNAUTHORIZED,
            &serde_json::json!({
                "code": "UNAUTHENTICATED",
                "message": "missing or invalid Authorization Bearer token for /v1/ws",
            }),
        )
    }

    pub fn json_response(status: StatusCode, body: &Value) -> Response<Body> {
        let bytes = Bytes::from(serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec()));
        let boxed: Body = Full::new(bytes).map_err(|never| match never {}).boxed();
        let mut resp = Response::new(boxed);
        *resp.status_mut() = status;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json; charset=utf-8"),
        );
        resp
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Shared fixtures for the `public_v1` test modules.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub fn ensure_dir(p: &Path) {
        std::fs::create_dir_all(p).unwrap();
    }

    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "agentcpm-pd-wp4-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            ensure_dir(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
#[path = "regression_tests.rs"]
mod regression_tests;

#[cfg(test)]
#[path = "owned_turn_tests.rs"]
mod owned_turn_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use agentcpm_pd_protocol::registry::OperationRegistry;
    use serde_json::json;

    use crate::public_v1::bootstrap::{
        initialize_instance, inspect_service_status, load_local_instance_and_token,
    };
    use crate::public_v1::runtime::DispatchError;
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;

    #[tokio::test]
    async fn all_28_ops_and_s3_capability_unavailable_and_crash_recovery_validate_against_schemas()
    {
        let reg = OperationRegistry::global();
        let tmp = TempDir::new("28ops");
        let root_dir = tmp.0.join("pd-home");
        let init_res = initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        assert!(
            reg.validate_result("instance.initialize", &init_res)
                .is_ok()
        );

        let (file, token) = load_local_instance_and_token(&root_dir, None).unwrap();
        assert!(!token.is_empty());
        let iid = file.instance_id.clone();

        let status_stopped = inspect_service_status(&root_dir, Some(&iid)).unwrap();
        assert!(
            reg.validate_result("service.status", &status_stopped)
                .is_ok()
        );
        assert_eq!(status_stopped["serviceState"], "stopped");

        let meta = InstanceMeta {
            instance_id: iid.clone(),
            owner_id: file.owner_id.clone(),
            product: file.product.clone(),
            protocol_version: file.protocol_version.clone(),
            node_id: file.node_id.clone(),
            generation: "gen-test-1".to_owned(),
            server_revision: "srv-test-1".to_owned(),
            initialization_revision: file.initialization_revision.clone(),
        };
        let store = V1Store::open_and_recover(&root_dir, meta.clone()).unwrap();
        let rt = PublicV1Runtime::new(store.clone());

        // 1. system.capabilities + system.health + schema.*
        let (_, caps) = rt
            .dispatch(
                "system.capabilities",
                json!({"instanceId": iid}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("system.capabilities", &caps).unwrap();

        let (_, health) = rt
            .dispatch("system.health", json!({"instanceId": iid}), None, None)
            .await
            .unwrap();
        reg.validate_result("system.health", &health).unwrap();

        let (_, sl) = rt
            .dispatch("schema.list", json!({}), None, None)
            .await
            .unwrap();
        reg.validate_result("schema.list", &sl).unwrap();

        let (_, sg) = rt
            .dispatch("schema.get", json!({"methodId": "run.submit"}), None, None)
            .await
            .unwrap();
        reg.validate_result("schema.get", &sg).unwrap();

        // 2. S3 op -> -32004 CAPABILITY_UNAVAILABLE
        let s3_err = rt
            .dispatch(
                "terminals.create",
                json!({"projectId": "p1", "rows": 24, "columns": 80}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(s3_err, DispatchError::Rpc { code: -32004, .. }));

        // 3. project.{register,get,list} & session.{create,get,list}
        let (_, preg) = rt
            .dispatch(
                "project.register",
                json!({
                    "instanceId": iid,
                    "projectId": "proj-1",
                    "name": "Demo",
                    "environmentId": "local",
                    "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
                }),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("project.register", &preg).unwrap();

        let (_, pget) = rt
            .dispatch(
                "project.get",
                json!({"instanceId": iid, "projectId": "proj-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("project.get", &pget).unwrap();

        let (_, plist) = rt
            .dispatch("project.list", json!({"instanceId": iid}), None, None)
            .await
            .unwrap();
        reg.validate_result("project.list", &plist).unwrap();

        let (_, sc) = rt
            .dispatch(
                "session.create",
                json!({"instanceId": iid, "sessionId": "sess-1", "projectId": "proj-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("session.create", &sc).unwrap();

        let (_, sg_res) = rt
            .dispatch(
                "session.get",
                json!({"instanceId": iid, "sessionId": "sess-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("session.get", &sg_res).unwrap();

        let (_, slist) = rt
            .dispatch(
                "session.list",
                json!({"instanceId": iid, "projectId": "proj-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("session.list", &slist).unwrap();

        // 4. run.submit + idempotent deduplication + requests.lookup + run.wait + events + snapshot + list + get
        let (_, rsub) = rt
            .dispatch(
                "run.submit",
                json!({"instanceId": iid, "sessionId": "sess-1", "input": [{"kind": "text", "text": "hello world"}]}),
                Some("key-run-1"),
                None,
            )
            .await
            .unwrap();
        reg.validate_result("run.submit", &rsub).unwrap();
        assert_eq!(rsub["deduplicated"], false);
        let run1 = rsub["runId"].as_str().unwrap().to_owned();

        let (_, rsub_dup) = rt
            .dispatch(
                "run.submit",
                json!({"instanceId": iid, "sessionId": "sess-1", "input": [{"kind": "text", "text": "hello world"}]}),
                Some("key-run-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(rsub_dup["deduplicated"], true);
        assert_eq!(rsub_dup["runId"], run1);

        let (_, lookup) = rt
            .dispatch(
                "requests.lookup",
                json!({"instanceId": iid, "methodId": "run.submit", "idempotencyKey": "key-run-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("requests.lookup", &lookup).unwrap();
        assert_eq!(lookup["requestState"], "accepted");

        let (_, rwait) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run1, "timeoutMs": 5000}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("run.wait", &rwait).unwrap();
        assert_eq!(rwait["run"]["state"], "succeeded");

        let (_, rget) = rt
            .dispatch(
                "run.get",
                json!({"instanceId": iid, "runId": run1}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("run.get", &rget).unwrap();

        let (_, rlist) = rt
            .dispatch(
                "run.list",
                json!({"instanceId": iid, "scope": {"kind": "session", "projectId": "proj-1", "sessionId": "sess-1"}}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("run.list", &rlist).unwrap();

        let (_, revents) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": run1}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("run.events", &revents).unwrap();

        let (_, rsnap) = rt
            .dispatch(
                "run.snapshot",
                json!({"instanceId": iid, "runId": run1}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("run.snapshot", &rsnap).unwrap();
        assert_eq!(rsnap["snapshot"]["sections"].as_array().unwrap().len(), 3);

        // 5. Approval & Question interactions + cancel
        let (_, r_app) = rt
            .dispatch(
                "run.submit",
                json!({"instanceId": iid, "sessionId": "sess-1", "input": [{"kind": "text", "text": "[pd-driver:approval]"}]}),
                Some("key-app-1"),
                None,
            )
            .await
            .unwrap();
        let run_app = r_app["runId"].as_str().unwrap().to_owned();
        tokio::time::sleep(Duration::from_millis(40)).await;

        let (_, ilist) = rt
            .dispatch(
                "interaction.list",
                json!({"instanceId": iid, "state": "pending"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("interaction.list", &ilist).unwrap();
        let ia = &ilist["items"].as_array().unwrap()[0];
        let ia_id = ia["interactionId"].as_str().unwrap().to_owned();
        let ia_rev = ia["revision"].as_str().unwrap().to_owned();

        let (_, iget) = rt
            .dispatch(
                "interaction.get",
                json!({"instanceId": iid, "interactionId": ia_id}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("interaction.get", &iget).unwrap();

        let (_, app_resp) = rt
            .dispatch(
                "approval.respond",
                json!({"instanceId": iid, "interactionId": ia_id, "expectedRevision": ia_rev, "decision": "allow"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("approval.respond", &app_resp).unwrap();

        let (_, r_app_wait) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run_app, "timeoutMs": 5000}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(r_app_wait["run"]["state"], "succeeded");

        // Question interaction
        let (_, r_q) = rt
            .dispatch(
                "run.submit",
                json!({"instanceId": iid, "sessionId": "sess-1", "input": [{"kind": "text", "text": "[pd-driver:question]"}]}),
                Some("key-q-1"),
                None,
            )
            .await
            .unwrap();
        let run_q = r_q["runId"].as_str().unwrap().to_owned();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let (_, iq_list) = rt
            .dispatch(
                "interaction.list",
                json!({"instanceId": iid, "kind": "question", "state": "pending"}),
                None,
                None,
            )
            .await
            .unwrap();
        let q_item = &iq_list["items"].as_array().unwrap()[0];
        let (_, iresp) = rt
            .dispatch(
                "interaction.respond",
                json!({
                    "instanceId": iid,
                    "interactionId": q_item["interactionId"],
                    "expectedRevision": q_item["revision"],
                    "answer": {"text": "yes"}
                }),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("interaction.respond", &iresp).unwrap();
        let (_, r_q_wait) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run_q, "timeoutMs": 5000}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(r_q_wait["run"]["state"], "succeeded");

        // Failure -> queue pause -> queue.get -> queue.resume + cancel
        let (_, r_fail) = rt
            .dispatch(
                "run.submit",
                json!({"instanceId": iid, "sessionId": "sess-1", "input": [{"kind": "text", "text": "[pd-driver:fail]"}]}),
                Some("key-fail-1"),
                None,
            )
            .await
            .unwrap();
        let fail_id = r_fail["runId"].as_str().unwrap().to_owned();
        let _ = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": fail_id, "timeoutMs": 5000}),
                None,
                None,
            )
            .await
            .unwrap();

        let (_, qget) = rt
            .dispatch(
                "queue.get",
                json!({"instanceId": iid, "sessionId": "sess-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("queue.get", &qget).unwrap();
        assert_eq!(qget["queue"]["paused"], true);

        let (_, qres) = rt
            .dispatch(
                "queue.resume",
                json!({
                    "instanceId": iid,
                    "sessionId": "sess-1",
                    "expectedRevision": qget["queue"]["revision"],
                    "acknowledgedPauseReason": qget["queue"]["pauseReason"],
                }),
                None,
                None,
            )
            .await
            .unwrap();
        reg.validate_result("queue.resume", &qres).unwrap();
        assert_eq!(qres["queue"]["paused"], false);

        let (_, r_long) = rt
            .dispatch(
                "run.submit",
                json!({"instanceId": iid, "sessionId": "sess-1", "input": [{"kind": "text", "text": "[pd-driver:sleep=5000]"}]}),
                Some("key-cancel-1"),
                None,
            )
            .await
            .unwrap();
        let long_id = r_long["runId"].as_str().unwrap().to_owned();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (_, rcancel) = rt
            .dispatch(
                "run.cancel",
                json!({"instanceId": iid, "runId": long_id}),
                Some("key-can-req-1"),
                None,
            )
            .await
            .unwrap();
        reg.validate_result("run.cancel", &rcancel).unwrap();
        let (_, r_c_wait) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": long_id, "timeoutMs": 5000}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(r_c_wait["run"]["state"], "cancelled");

        // 6. Startup crash recovery test (D9): simulate running run -> reopen SQLite -> interrupted + paused
        let (_, r_crash) = rt
            .dispatch(
                "run.submit",
                json!({"instanceId": iid, "sessionId": "sess-1", "input": [{"kind": "text", "text": "[pd-driver:sleep=5000]"}]}),
                Some("key-crash-1"),
                None,
            )
            .await
            .unwrap();
        let crash_id = r_crash["runId"].as_str().unwrap().to_owned();
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(rt);
        drop(store);

        let store2 = V1Store::open_and_recover(&root_dir, meta).unwrap();
        let recovered = store2.get_run(&crash_id).unwrap();
        assert_eq!(
            recovered.state,
            agentcpm_pd_protocol::contract::RunState::Interrupted
        );
        let q_after = store2.get_queue("sess-1", None).unwrap();
        assert_eq!(q_after["queue"]["paused"], true);
        assert_eq!(q_after["queue"]["pauseReason"], "gateway_restart");
    }

    #[tokio::test]
    async fn standalone_v1_gateway_enforces_bearer_auth_and_serves_ws_initialize_and_gap() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

        use crate::public_v1::store::{RunTransition, parse_cursor};
        use agentcpm_pd_protocol::contract::RunState;

        let tmp = TempDir::new("ws");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (file, token) = load_local_instance_and_token(&root_dir, None).unwrap();
        let iid = file.instance_id.clone();

        // Seed a finished run directly through the store so the gap path has a
        // real event history with known cursors, then mark everything at or
        // before the first progress cursor as trimmed (`earliest_cursor_seq`).
        let (gap_run_id, expired_cursor, live_after_cursor, earliest_cursor) = {
            let meta = InstanceMeta {
                instance_id: file.instance_id.clone(),
                owner_id: file.owner_id.clone(),
                product: file.product.clone(),
                protocol_version: file.protocol_version.clone(),
                node_id: file.node_id.clone(),
                generation: "gen-test-1".to_owned(),
                server_revision: "srv-test-1".to_owned(),
                initialization_revision: file.initialization_revision.clone(),
            };
            let seed = V1Store::open_and_recover(&root_dir, meta).unwrap();
            seed.register_project(&json!({
                "instanceId": iid,
                "projectId": "proj-ws",
                "name": "Gap",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
            }))
            .unwrap();
            seed.create_session(&json!({
                "instanceId": iid, "sessionId": "sess-ws", "projectId": "proj-ws"
            }))
            .unwrap();
            let disposition = seed
                .submit_run(
                    &json!({
                        "instanceId": iid,
                        "sessionId": "sess-ws",
                        "input": [{"kind": "text", "text": "gap seed"}]
                    }),
                    Some("key-gap-seed-1"),
                )
                .unwrap();
            let run_id = match disposition {
                crate::public_v1::store::SubmitDisposition::NewRun { run_id, .. } => run_id,
                other => panic!("expected a new seeded run, got {other:?}"),
            };
            seed.transition_run_state(
                &run_id,
                RunTransition::new(
                    RunState::Running,
                    "run.started",
                    &json!({"runId": run_id, "sessionId": "sess-ws"}),
                ),
            )
            .unwrap();
            let first_cursor = seed
                .append_run_progress_event(
                    &run_id,
                    "sess-ws",
                    "run.progress",
                    &json!({"runId": run_id, "delta": "chunk-1"}),
                )
                .unwrap();
            let second_cursor = seed
                .append_run_progress_event(
                    &run_id,
                    "sess-ws",
                    "run.progress",
                    &json!({"runId": run_id, "delta": "chunk-2"}),
                )
                .unwrap();
            seed.transition_run_state(
                &run_id,
                RunTransition::new(
                    RunState::Succeeded,
                    "run.completed",
                    &json!({"runId": run_id}),
                )
                .outcome_code("SUCCEEDED"),
            )
            .unwrap();
            // Retention boundary between the two progress cursors: resuming
            // from the first one is expired (gap), resuming from the second
            // one still replays the retained terminal tail.
            let first_seq = parse_cursor(&first_cursor).unwrap();
            let second_seq = parse_cursor(&second_cursor).unwrap();
            assert_eq!(
                second_seq,
                first_seq + 1,
                "seeded history must be contiguous"
            );
            let earliest = second_seq + 1;
            seed.set_earliest_cursor_seq_for_test(earliest).unwrap();
            (
                run_id,
                first_cursor,
                second_cursor,
                crate::public_v1::store::format_cursor(earliest),
            )
        };

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let home_srv = root_dir.clone();
        let server_task = tokio::spawn(async move {
            let _ = crate::public_v1::bootstrap::serve_v1_gateway(
                home_srv,
                Some(iid),
                crate::public_v1::bootstrap::default_loopback_addr(),
                async move {
                    let _ = stop_rx.await;
                },
            )
            .await;
        });

        // P3 (wp4 review): bound the ready.json poll — a failed server start
        // used to hang the test forever instead of failing.
        let ready_file = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Ok(raw) = std::fs::read_to_string(root_dir.join("run").join("ready.json"))
                    && let Ok(rf) =
                        serde_json::from_str::<crate::public_v1::bootstrap::ReadyFile>(&raw)
                {
                    return rf;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .expect("server wrote ready.json within 15s");

        // Unauthenticated WS upgrade must be rejected (HTTP 401)
        let bad_res = tokio_tungstenite::connect_async(&ready_file.ws_url).await;
        assert!(
            bad_res.is_err(),
            "unauthenticated /v1/ws must fail HTTP 401"
        );

        // Authenticated WS upgrade succeeds and speaks pd-connect/1
        let mut req = ready_file.ws_url.as_str().into_client_request().unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();

        // Calling operation before initialize returns -32002
        let pre_init = json!({
            "jsonrpc": "2.0",
            "id": "r0",
            "method": "system.health",
            "params": {"input": {}}
        });
        ws.send(Message::Text(pre_init.to_string().into()))
            .await
            .unwrap();
        let Some(Ok(Message::Text(r0_txt))) = ws.next().await else {
            panic!("expected -32002 frame");
        };
        let r0_val: Value = serde_json::from_str(&r0_txt).unwrap();
        assert_eq!(r0_val["error"]["code"], -32002);

        // Initialize succeeds
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": "r1",
            "method": "initialize",
            "params": {
                "abi_version": 1,
                "api_version": "agentcpm-pd.operation/1",
                "features": ["subscriptions"],
                "client": {"name": "wp4-test", "version": "0.1.0"}
            }
        });
        ws.send(Message::Text(init_req.to_string().into()))
            .await
            .unwrap();
        let Some(Ok(Message::Text(r1_txt))) = ws.next().await else {
            panic!("expected init response");
        };
        let r1_val: Value = serde_json::from_str(&r1_txt).unwrap();
        assert_eq!(r1_val["result"]["target"]["instanceId"], file.instance_id);

        // S3 stream op (`terminals.output`) returns -32004 CAPABILITY_UNAVAILABLE
        let term_sub = json!({
            "jsonrpc": "2.0",
            "id": "r2",
            "method": "sub.open",
            "params": {"stream": "terminals.output", "input": {"instanceId": file.instance_id, "terminalId": "t1"}}
        });
        ws.send(Message::Text(term_sub.to_string().into()))
            .await
            .unwrap();
        let Some(Ok(Message::Text(r2_txt))) = ws.next().await else {
            panic!("expected -32004 frame");
        };
        let r2_val: Value = serde_json::from_str(&r2_txt).unwrap();
        assert_eq!(r2_val["error"]["code"], -32004);

        // Gap rejection (P1 wp4 review): subscribing with a cursor older than
        // the retention boundary is refused at open with -32000
        // CURSOR_EXPIRED and `details.gap` carrying the `$defs.gap` object
        // (007 §6.4 / public-contract transportContract); only mid-subscription
        // gaps end via `sub.end`. The standard recovery is run.snapshot ->
        // checkpoint.cursor -> sub.open.
        let gap_sub = json!({
            "jsonrpc": "2.0",
            "id": "r3",
            "method": "sub.open",
            "params": {
                "stream": "run.events",
                "input": {"runId": gap_run_id, "afterCursor": expired_cursor, "follow": true},
                "credit": 4
            }
        });
        ws.send(Message::Text(gap_sub.to_string().into()))
            .await
            .unwrap();
        let Some(Ok(Message::Text(frame_txt))) = ws.next().await else {
            panic!("expected the expired-cursor sub.open rejection");
        };
        let gap_reject: Value = serde_json::from_str(&frame_txt).unwrap();
        assert_eq!(gap_reject["id"], "r3", "unexpected frame {gap_reject}");
        assert!(
            gap_reject.get("error").is_some(),
            "expired cursor at open must be an error response: {gap_reject}"
        );
        {
            let error = &gap_reject["error"];
            assert_eq!(
                error["code"], -32000,
                "application error channel: {gap_reject}"
            );
            let data = &error["data"];
            assert_eq!(data["code"], "CURSOR_EXPIRED");
            let gap = &data["details"]["gap"];
            assert_eq!(gap["kind"], "gap");
            assert_eq!(gap["requestedCursor"], expired_cursor.as_str());
            assert_eq!(gap["earliestCursor"], earliest_cursor.as_str());
            assert_eq!(gap["recovery"], "fetch_snapshot_then_resume");
            assert!(
                gap["snapshotRevision"]
                    .as_str()
                    .is_some_and(|rev| !rev.is_empty()),
                "gap descriptor carries the run snapshot revision"
            );
        }

        // Positive control + wire-order regression: a subscription from the
        // newest retained cursor replays the retained tail and only then
        // completes. 007 §6.4 (`completed`: 事件全部送达): every `sub.event`
        // of the subscription must precede its `sub.end` on the wire — a
        // control-lane terminal that overtakes queued events makes consumers
        // that stop at `sub.end` drop the tail.
        let live_sub = json!({
            "jsonrpc": "2.0",
            "id": "r4",
            "method": "sub.open",
            "params": {
                "stream": "run.events",
                "input": {"runId": gap_run_id, "afterCursor": live_after_cursor, "follow": true},
                "credit": 16
            }
        });
        ws.send(Message::Text(live_sub.to_string().into()))
            .await
            .unwrap();
        let live_events = tokio::time::timeout(Duration::from_secs(5), async {
            let mut live_events = 0;
            loop {
                let Some(Ok(Message::Text(frame_txt))) = ws.next().await else {
                    panic!("expected replayed events or sub.end");
                };
                let frame: Value = serde_json::from_str(&frame_txt).unwrap();
                if frame.get("id").is_some() {
                    assert_eq!(frame["id"], "r4");
                    assert!(frame["error"].is_null(), "sub.open must succeed: {frame}");
                    continue;
                }
                match frame["method"].as_str() {
                    Some("sub.event") => {
                        let event = &frame["params"]["event"];
                        assert_eq!(event["runId"], gap_run_id.as_str());
                        assert!(
                            event["cursor"]
                                .as_str()
                                .is_some_and(|c| c > live_after_cursor.as_str()),
                            "only retained events replay: {event}"
                        );
                        live_events += 1;
                    }
                    Some("sub.end") => {
                        assert_eq!(frame["params"]["terminal"], "completed");
                        break live_events;
                    }
                    Some("conn.heartbeat") => {}
                    other => panic!("unexpected notification {other:?}: {frame}"),
                }
            }
        })
        .await
        .expect("retained tail replays and the subscription completes");
        assert!(
            live_events >= 1,
            "retained tail must replay before sub.end(completed), got {live_events}"
        );

        let _ = ws.close(None).await;
        let _ = stop_tx.send(());
        let _ = server_task.await;
    }
}
