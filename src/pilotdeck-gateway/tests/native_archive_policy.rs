//! Host-injected native archive authorization and filesystem retention, through
//! the public composable LocalGateway entry. No model turn or provider is used.
#[path = "common/native_ws_client.rs"]
mod archive_ws_client;

use pilotdeck_gateway::{
    extensions::NativeExtensions,
    native_process::{
        LocalGateway, LocalOptions, NativeArchiveAuthorizationError,
        NativeArchiveAuthorizationFuture, NativeArchiveAuthorizer, NativeArchivePolicy,
        NativeGatewayServices,
    },
    runtime::connection::RequestError,
    storage::{
        AsyncTranscriptAdapter, SessionStorage, SessionStorageInput, StoreResult,
        TranscriptBlobStore, TranscriptKey, TranscriptRead, TranscriptStore,
    },
};
use pilotdeck_protocol::{JsValue, parse_json, stringify_json};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{File, FileTimes},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, UNIX_EPOCH},
};

const SESSION: &str = "policy-session";
fn js(value: Value) -> JsValue {
    parse_json(&value.to_string()).unwrap()
}
fn json_value(value: &JsValue) -> Value {
    serde_json::from_str(&stringify_json(value).unwrap().unwrap()).unwrap()
}
fn assert_error(error: &RequestError, code: &str, message: &str) {
    assert_eq!(error.code, code);
    assert!(error.message.equals_str(message), "{error:?}");
}

#[derive(Default)]
struct Blobs {
    bytes: Mutex<BTreeMap<(PathBuf, String), (TranscriptKey, Vec<u8>)>>,
    reads: AtomicUsize,
}
impl TranscriptBlobStore for Blobs {
    fn write(&self, key: &TranscriptKey, name: &str, bytes: &[u8]) -> StoreResult<()> {
        self.bytes.lock().unwrap().insert(
            (key.transcript_path.clone(), name.into()),
            (key.clone(), bytes.to_vec()),
        );
        Ok(())
    }
    fn read(&self, key: &TranscriptKey, name: &str) -> StoreResult<Option<Vec<u8>>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .bytes
            .lock()
            .unwrap()
            .get(&(key.transcript_path.clone(), name.into()))
            .map(|(owner, bytes)| {
                assert_eq!(
                    owner, key,
                    "archive provider must use the complete selected host owner key"
                );
                bytes.clone()
            }))
    }
    fn delete(&self, key: &TranscriptKey, name: &str) -> StoreResult<()> {
        self.bytes
            .lock()
            .unwrap()
            .remove(&(key.transcript_path.clone(), name.into()));
        Ok(())
    }
    fn delete_all(&self, key: &TranscriptKey) -> StoreResult<()> {
        self.bytes
            .lock()
            .unwrap()
            .retain(|(path, _), _| path != &key.transcript_path);
        Ok(())
    }
}
#[derive(Default)]
struct Store {
    blobs: Arc<Blobs>,
    reads: AtomicUsize,
}
impl TranscriptStore for Store {
    fn append(&self, _: &TranscriptKey, _: &JsValue) -> StoreResult<()> {
        panic!("archive reads must not append transcript entries")
    }
    fn read(&self, _: &TranscriptKey, _: Option<u64>) -> StoreResult<TranscriptRead> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(TranscriptRead {
            entries: vec![],
            diagnostics: vec![],
        })
    }
    fn tool_result_artifacts(&self) -> Option<Arc<dyn TranscriptBlobStore>> {
        Some(self.blobs.clone())
    }
}

struct Gate {
    entered: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
    release: tokio::sync::Notify,
}
#[derive(Default)]
struct Authorizer {
    calls: Mutex<Vec<JsValue>>,
    rejection: Mutex<Option<NativeArchiveAuthorizationError>>,
    gate: Mutex<Option<Arc<Gate>>>,
    deny_artifact: AtomicBool,
}
impl NativeArchiveAuthorizer for Authorizer {
    fn authorize(&self, input: JsValue) -> NativeArchiveAuthorizationFuture {
        let artifact_denied = self.deny_artifact.load(Ordering::SeqCst)
            && input
                .get("operation")
                .and_then(JsValue::as_string)
                .is_some_and(|value| value.equals_str("artifact"));
        self.calls.lock().unwrap().push(input);
        let rejection = if artifact_denied {
            Some(NativeArchiveAuthorizationError::Message(
                "artifact-only policy denied".into(),
            ))
        } else {
            self.rejection.lock().unwrap().clone()
        };
        let gate = self.gate.lock().unwrap().clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                if let Some(entered) = gate.entered.lock().unwrap().take() {
                    entered.send(()).unwrap();
                }
                gate.release.notified().await;
            }
            match rejection {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    pilot: PathBuf,
    store: Arc<Store>,
    adapter: Arc<AsyncTranscriptAdapter>,
}
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("pdrs-archive-policy-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        let pilot = root.join("home/.pilotdeck");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&pilot).unwrap();
        let registered =
            pilotdeck_gateway::session_chat_dir(&pilot, &project, &project.to_string_lossy());
        std::fs::create_dir_all(registered.parent().unwrap()).unwrap();
        std::fs::write(
            registered.parent().unwrap().join(".cwd"),
            project.to_string_lossy().as_bytes(),
        )
        .unwrap();
        // No submit_turn is issued. This syntactically valid provider is never contacted.
        std::fs::write(pilot.join("pilotdeck.yaml"), "schemaVersion: 1\nagent: {model: fixture/mock}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://127.0.0.1:1/v1\n      apiKey: unused-fixture-only\n      models: {mock: {}}\n").unwrap();
        let store = Arc::new(Store::default());
        let chat = root.join("host-owned/chats");
        let adapter = Arc::new(AsyncTranscriptAdapter::new(
            store.clone(),
            Arc::new(move |_| chat.clone()),
        ));
        Self {
            root,
            project,
            pilot,
            store,
            adapter,
        }
    }
    fn storage(&self) -> SessionStorage {
        self.adapter
            .create_session_storage(SessionStorageInput {
                project_root: self.project.clone(),
                pilot_home: self.pilot.clone(),
                session_id: SESSION.into(),
                now: None,
            })
            .unwrap()
    }
    fn start(&self, host_store: bool, policy: Option<NativeArchivePolicy>) -> LocalGateway {
        let env = [
            ("HOME", self.root.join("home")),
            ("PILOT_HOME", self.pilot.clone()),
            ("PILOTDECK_CONFIG_PATH", self.pilot.join("pilotdeck.yaml")),
        ]
        .into_iter()
        .map(|(name, path)| (name.into(), path.to_string_lossy().into_owned()))
        .collect();
        LocalGateway::start_with_services(
            LocalOptions {
                pilot_home: self.pilot.clone(),
                project: self.project.clone(),
                os_home: Some(self.root.join("home")),
                sidecar: self.root.join("never-used-sidecar"),
                env,
                builtin_skills_root: None,
                builtin_plugins_root: None,
                telemetry_context: None,
            },
            NativeExtensions::default(),
            NativeGatewayServices {
                session_storage: host_store.then(|| self.adapter.clone()),
                native_archive_policy: policy,
                ..Default::default()
            },
        )
        .unwrap()
    }
    fn params(&self) -> Value {
        json!({"sessionKey":SESSION,"projectKey":self.project})
    }
    fn artifact(&self, name: &str) -> Value {
        json!({"sessionKey":SESSION,"projectKey":self.project,"artifactName":name,"maxBytes":64})
    }
    fn read_counts(&self) -> (usize, usize) {
        (
            self.store.reads.load(Ordering::SeqCst),
            self.store.blobs.reads.load(Ordering::SeqCst),
        )
    }
    fn file(&self, name: &str, bytes: &[u8], modified_ms: u64) -> PathBuf {
        let path = self.storage().tool_results_dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_millis(modified_ms)),
            )
            .unwrap();
        path
    }
    fn stop(&self, gateway: LocalGateway) {
        gateway.shutdown(Duration::from_secs(5)).unwrap();
        drop(gateway);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn policy(authorizer: Option<Arc<Authorizer>>, retention: Option<f64>) -> NativeArchivePolicy {
    NativeArchivePolicy::new(
        authorizer.map(|authorizer| authorizer as Arc<dyn NativeArchiveAuthorizer>),
        retention,
        Some(Arc::new(|| 10_000.0)),
    )
    .unwrap()
}

#[test]
fn public_archive_authorizer_receives_three_operations_and_denies_before_provider() {
    let f = Fixture::new();
    let authorizer = Arc::new(Authorizer::default());
    *authorizer.rejection.lock().unwrap() = Some(NativeArchiveAuthorizationError::Message(
        "archive policy denied".into(),
    ));
    f.storage()
        .tool_result_artifact_storage()
        .unwrap()
        .write("body.txt", b"body")
        .unwrap();
    let gateway = f.start(true, Some(policy(Some(authorizer.clone()), None)));
    let calls = [
        ("native_archive_manifest", "manifest", f.params()),
        (
            "native_archive_entries",
            "entries",
            json!({"sessionKey":SESSION,"projectKey":f.project,"afterSequence":0,"limit":1}),
        ),
        (
            "native_archive_artifact",
            "artifact",
            f.artifact("body.txt"),
        ),
    ];
    let before = f.read_counts();
    for (method, operation, params) in &calls {
        let error = gateway.request(method, js(params.clone())).unwrap_err();
        assert_error(&error, "ARCHIVE_UNAUTHORIZED", "archive policy denied");
        let mut expected = params.clone();
        expected["operation"] = (*operation).into();
        assert_eq!(
            json_value(authorizer.calls.lock().unwrap().last().unwrap()),
            expected
        );
        assert_eq!(
            f.read_counts(),
            before,
            "denial precedes the archive provider for {operation}"
        );
    }
    *authorizer.rejection.lock().unwrap() = None;
    for (method, _, params) in calls {
        gateway.request(method, js(params)).unwrap();
    }
    assert_eq!(authorizer.calls.lock().unwrap().len(), 6);
    assert!(f.store.reads.load(Ordering::SeqCst) > before.0);
    assert_eq!(f.store.blobs.reads.load(Ordering::SeqCst), before.1 + 1);
    f.stop(gateway);
}

#[test]
fn public_archive_input_validation_precedes_authorizer_in_protocol_order() {
    let f = Fixture::new();
    let authorizer = Arc::new(Authorizer::default());
    *authorizer.rejection.lock().unwrap() = Some(NativeArchiveAuthorizationError::Denied);
    let gateway = f.start(true, Some(policy(Some(authorizer.clone()), None)));
    let before = f.read_counts();
    let invalid = [
        (
            "native_archive_manifest",
            json!({"sessionKey":" ","projectKey":f.project}),
            "INVALID_SESSION_KEY",
            "sessionKey is required.",
        ),
        (
            "native_archive_entries",
            json!({"sessionKey":SESSION,"projectKey":f.project,"afterSequence":-1,"limit":0}),
            "INVALID_ARCHIVE_CURSOR",
            "afterSequence must be a non-negative safe integer.",
        ),
        (
            "native_archive_entries",
            json!({"sessionKey":SESSION,"projectKey":f.project,"afterSequence":0,"limit":0}),
            "INVALID_ARCHIVE_LIMIT",
            "limit must be an integer between 1 and 500.",
        ),
        (
            "native_archive_artifact",
            json!({"sessionKey":SESSION,"projectKey":f.project,"artifactName":"../bad","maxBytes":0}),
            "INVALID_ARCHIVE_ARTIFACT",
            "artifactName must be a single safe artifact name.",
        ),
        (
            "native_archive_artifact",
            json!({"sessionKey":SESSION,"projectKey":f.project,"artifactName":"body.txt","maxBytes":0}),
            "INVALID_ARCHIVE_ARTIFACT_LIMIT",
            "maxBytes must be an integer between 1 and 10000000.",
        ),
    ];
    for (method, params, code, message) in invalid {
        assert_error(
            &gateway.request(method, js(params)).unwrap_err(),
            code,
            message,
        );
    }
    assert!(authorizer.calls.lock().unwrap().is_empty());
    assert_eq!(f.read_counts(), before);
    f.stop(gateway);
}

#[test]
fn public_archive_typed_policy_error_keeps_details_and_non_error_uses_default_denial() {
    let f = Fixture::new();
    let authorizer = Arc::new(Authorizer::default());
    let details = js(json!({"policy":"tenant-only","attempt":7}));
    *authorizer.rejection.lock().unwrap() =
        Some(NativeArchiveAuthorizationError::Dialog(RequestError {
            code: "HOST_POLICY_BLOCKED".into(),
            message: "tenant archive blocked".into(),
            details: Some(Box::new(details.clone())),
            validation: None,
        }));
    let gateway = f.start(true, Some(policy(Some(authorizer.clone()), None)));
    let before = f.read_counts();
    let error = gateway
        .request("native_archive_artifact", js(f.artifact("body.txt")))
        .unwrap_err();
    assert_error(&error, "HOST_POLICY_BLOCKED", "tenant archive blocked");
    assert_eq!(error.details.as_deref(), Some(&details));
    *authorizer.rejection.lock().unwrap() = Some(NativeArchiveAuthorizationError::Denied);
    let error = gateway
        .request("native_archive_manifest", js(f.params()))
        .unwrap_err();
    assert_error(
        &error,
        "ARCHIVE_UNAUTHORIZED",
        "Native transcript archive access was denied.",
    );
    assert!(error.details.is_none());
    assert_eq!(f.read_counts(), before);
    f.stop(gateway);
}

#[test]
fn public_archive_waits_for_async_authorization_before_reading_host_blob() {
    let f = Fixture::new();
    let authorizer = Arc::new(Authorizer::default());
    f.storage()
        .tool_result_artifact_storage()
        .unwrap()
        .write("body.txt", b"body")
        .unwrap();
    let (entered, observed) = std::sync::mpsc::sync_channel(1);
    let gate = Arc::new(Gate {
        entered: Mutex::new(Some(entered)),
        release: tokio::sync::Notify::new(),
    });
    *authorizer.gate.lock().unwrap() = Some(gate.clone());
    let gateway = f.start(true, Some(policy(Some(authorizer), None)));
    let before = f.read_counts();
    thread::scope(|scope| {
        let (finished, received) = std::sync::mpsc::sync_channel(1);
        let gateway_ref = &gateway;
        let params = f.artifact("body.txt");
        let worker = scope.spawn(move || {
            let result = gateway_ref.request("native_archive_artifact", js(params));
            let _ = finished.send(result);
        });
        if observed.recv_timeout(Duration::from_secs(5)).is_err() {
            gate.release.notify_one();
            gateway.shutdown(Duration::from_secs(5)).unwrap();
            worker.join().unwrap();
            panic!("authorizer future was not awaited");
        }
        let counts_while_pending = f.read_counts();
        let premature = received.try_recv();
        let was_pending = matches!(&premature, Err(std::sync::mpsc::TryRecvError::Empty));
        // Release/join before assertions, so a failed sequencing assertion
        // cannot strand our owned request future behind the test barrier.
        gate.release.notify_one();
        let result = match premature {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => received
                .recv_timeout(Duration::from_secs(5))
                .expect("released archive request completes"),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("archive request worker disconnected")
            }
        }
        .unwrap();
        worker.join().unwrap();
        assert_eq!(counts_while_pending, before);
        assert!(
            was_pending,
            "request must remain pending until host authorization resolves"
        );
        assert_eq!(json_value(&result)["content"], "Ym9keQ==");
    });
    assert_eq!(f.store.blobs.reads.load(Ordering::SeqCst), before.1 + 1);
    f.stop(gateway);
}

#[test]
fn public_archive_filesystem_retention_is_strictly_greater_and_default_is_unrestricted() {
    for invalid in [0.0, 0.5, 9_007_199_254_740_992.0, f64::NAN] {
        let error = NativeArchivePolicy::new(None, Some(invalid), None)
            .err()
            .expect("retention requires positive safe integer");
        assert_eq!(
            error.to_string(),
            "nativeArchiveRetentionMs must be a positive safe integer."
        );
    }
    let f = Fixture::new();
    let boundary = f.file("boundary.txt", b"boundary", 9_000);
    let expired = f.file("expired.txt", b"expired", 8_999);
    let gateway = f.start(false, Some(policy(None, Some(1_000.0))));
    let result = json_value(
        &gateway
            .request("native_archive_artifact", js(f.artifact("boundary.txt")))
            .unwrap(),
    );
    assert_eq!(
        result,
        json!({"artifactName":"boundary.txt","content":"Ym91bmRhcnk=","encoding":"base64","bytes":8,"truncated":false})
    );
    let error = gateway
        .request("native_archive_artifact", js(f.artifact("expired.txt")))
        .unwrap_err();
    assert_error(
        &error,
        "ARCHIVE_ARTIFACT_EXPIRED",
        "Archive artifact expired.txt is outside the configured retention window.",
    );
    let missing = gateway
        .request("native_archive_artifact", js(f.artifact("missing.txt")))
        .unwrap_err();
    assert_error(
        &missing,
        "ARCHIVE_ARTIFACT_NOT_FOUND",
        "Archive artifact missing.txt was not found.",
    );
    assert!(
        boundary.is_file() && expired.is_file(),
        "retention denies reading, it does not delete data"
    );
    f.stop(gateway);
    let gateway = f.start(false, None);
    let result = json_value(
        &gateway
            .request("native_archive_artifact", js(f.artifact("expired.txt")))
            .unwrap(),
    );
    assert_eq!(result["content"], "ZXhwaXJlZA==");
    assert_eq!(result["bytes"], 7);
    f.stop(gateway);
}

#[test]
fn public_archive_host_blob_retention_is_owned_by_host_but_custom_authorization_still_applies() {
    let f = Fixture::new();
    let authorizer = Arc::new(Authorizer::default());
    f.storage()
        .tool_result_artifact_storage()
        .unwrap()
        .write("old.txt", b"host body")
        .unwrap();
    f.file("old.txt", b"expired disk decoy", 0);
    let gateway = f.start(true, Some(policy(Some(authorizer.clone()), Some(1.0))));
    let result = json_value(
        &gateway
            .request("native_archive_artifact", js(f.artifact("old.txt")))
            .unwrap(),
    );
    assert_eq!(
        result,
        json!({"artifactName":"old.txt","content":"aG9zdCBib2R5","encoding":"base64","bytes":9,"truncated":false})
    );
    let before = f.read_counts();
    *authorizer.rejection.lock().unwrap() = Some(NativeArchiveAuthorizationError::Message(
        "host custom policy denied".into(),
    ));
    let error = gateway
        .request("native_archive_artifact", js(f.artifact("old.txt")))
        .unwrap_err();
    assert_error(&error, "ARCHIVE_UNAUTHORIZED", "host custom policy denied");
    assert_eq!(f.read_counts(), before);
    assert_eq!(authorizer.calls.lock().unwrap().len(), 2);
    f.stop(gateway);
}

#[test]
fn public_archive_artifact_method_cannot_spoof_authorization_or_retention_operation() {
    let hosted = Fixture::new();
    let host_authorizer = Arc::new(Authorizer::default());
    host_authorizer.deny_artifact.store(true, Ordering::SeqCst);
    hosted
        .storage()
        .tool_result_artifact_storage()
        .unwrap()
        .write("body.txt", b"protected host body")
        .unwrap();
    let gateway = hosted.start(true, Some(policy(Some(host_authorizer.clone()), None)));
    let normal = hosted.artifact("body.txt");
    let mut spoofed = normal.clone();
    spoofed["operation"] = "manifest".into();
    spoofed["policyTrace"] = "client-extra-retained".into();
    for params in [normal, spoofed] {
        let denied = gateway
            .request("native_archive_artifact", js(params.clone()))
            .unwrap_err();
        assert_error(
            &denied,
            "ARCHIVE_UNAUTHORIZED",
            "artifact-only policy denied",
        );
        assert_eq!(
            hosted.store.blobs.reads.load(Ordering::SeqCst),
            0,
            "authorization denial precedes the real host blob read"
        );
        let input = json_value(host_authorizer.calls.lock().unwrap().last().unwrap());
        assert_eq!(
            input["operation"], "artifact",
            "RPC method determines the security operation"
        );
        if params.get("policyTrace").is_some() {
            assert_eq!(input["policyTrace"], "client-extra-retained");
        }
    }
    assert_eq!(host_authorizer.calls.lock().unwrap().len(), 2);
    authenticated_ws_spoof_uses_same_host(&hosted, &gateway, &host_authorizer);
    hosted.stop(gateway);

    let filesystem = Fixture::new();
    filesystem.file("expired.txt", b"expired", 0);
    let clock_calls = Arc::new(AtomicUsize::new(0));
    let observed_clock = clock_calls.clone();
    let guarded = NativeArchivePolicy::new(
        None,
        Some(1_000.0),
        Some(Arc::new(move || {
            observed_clock.fetch_add(1, Ordering::SeqCst);
            10_000.0
        })),
    )
    .unwrap();
    let gateway = filesystem.start(false, Some(guarded));
    let normal = filesystem.artifact("expired.txt");
    let mut spoofed = normal.clone();
    spoofed["operation"] = "manifest".into();
    // The unmodified request is the same expired-file control before the
    // malicious extra field. Both must enforce the same retention window.
    for params in [normal, spoofed] {
        let expired = gateway
            .request("native_archive_artifact", js(params))
            .unwrap_err();
        assert_error(
            &expired,
            "ARCHIVE_ARTIFACT_EXPIRED",
            "Archive artifact expired.txt is outside the configured retention window.",
        );
    }
    assert_eq!(clock_calls.load(Ordering::SeqCst), 2);
    filesystem.stop(gateway);
}

fn authenticated_ws_spoof_uses_same_host(
    fixture: &Fixture,
    gateway: &LocalGateway,
    authorizer: &Arc<Authorizer>,
) {
    // LocalGateway remains alive throughout: it owns the runtime serving this
    // cloned TurnGateway. The auxiliary runtime drives WS tasks. Normal server
    // stop also shuts down this same host, so this helper is deliberately last
    // for the hosted fixture; the outer shutdown is only repeated cleanup.
    let host = gateway.turn_gateway();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let calls_before = authorizer.calls.lock().unwrap().len();
    let blob_reads_before = fixture.store.blobs.reads.load(Ordering::SeqCst);
    let (hello, manifest, rejected, address) = runtime.block_on(async {
        let server = pilotdeck_gateway::native_ws::NativeWsServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            host,
            "archive-policy-owned-token".into(),
            "archive-policy-test".into(),
        )
        .await
        .unwrap();
        let address = server.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server_task = tokio::spawn(server.run(
            async {
                let _ = stopped.await;
            },
            Duration::from_secs(5),
        ));
        let mut client = archive_ws_client::Client::connect(address).await;
        client
            .send(
                json!({"type":"hello","protocolVersion":"1.2","clientName":"ui",
            "clientVersion":"archive-policy-test","token":"archive-policy-owned-token"}),
            )
            .await;
        let hello = tokio::time::timeout(Duration::from_secs(5), client.read_json())
            .await
            .unwrap();
        client
            .send(json!({"type":"request","id":"archive-ws-manifest",
            "method":"native_archive_manifest","params":fixture.params()}))
            .await;
        let manifest = archive_ws_response(&mut client, "archive-ws-manifest").await;
        let mut params = fixture.artifact("body.txt");
        params["operation"] = "manifest".into();
        params["policyTrace"] = "ws-extra-retained".into();
        client
            .send(json!({"type":"request","id":"archive-ws-spoof",
            "method":"native_archive_artifact","params":params}))
            .await;
        let rejected = archive_ws_response(&mut client, "archive-ws-spoof").await;
        client.close().await;
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        (hello, manifest, rejected, address)
    });
    assert_eq!(
        hello["type"], "hello_ok",
        "token-authenticated WS handshake, not just HTTP Upgrade"
    );
    assert_eq!(manifest["ok"], true);
    assert_eq!(
        manifest["result"]["format"], "native_transcript_entries",
        "ordinary allowed archive RPC reaches the same production host"
    );
    assert_eq!(rejected["ok"], false);
    assert_eq!(rejected["error"]["code"], "ARCHIVE_UNAUTHORIZED");
    assert_eq!(rejected["error"]["message"], "artifact-only policy denied");
    assert_eq!(
        fixture.store.blobs.reads.load(Ordering::SeqCst),
        blob_reads_before
    );
    let inputs = authorizer.calls.lock().unwrap();
    assert_eq!(inputs.len(), calls_before + 2);
    assert_eq!(json_value(&inputs[calls_before])["operation"], "manifest");
    let actual = json_value(&inputs[calls_before + 1]);
    assert_eq!(actual["operation"], "artifact");
    assert_eq!(actual["policyTrace"], "ws-extra-retained");
    assert_eq!(actual["sessionKey"], SESSION);
    assert_eq!(
        actual["projectKey"],
        fixture.project.to_string_lossy().as_ref()
    );
    assert!(
        std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err(),
        "owned WS listener is closed after joined drain"
    );
}

async fn archive_ws_response(client: &mut archive_ws_client::Client, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = client.read_json().await;
            if frame["type"] == "response" {
                assert_eq!(frame["id"], id);
                return frame;
            }
            assert_eq!(
                frame["type"], "notification",
                "only ordinary notifications may precede an RPC response"
            );
        }
    })
    .await
    .expect("bounded authenticated archive response")
}
