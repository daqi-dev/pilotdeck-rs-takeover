//! P7 Gateway runtime tests for session ownership pinning, cross-backend
//! isolation, Kernel no-fallback (`handle_kernel_turn_failure`), bidirectional
//! lossless stationary import/rollback, and `FullLate` fail-closed negative control.
use super::opt_in_kernel::KERNEL_EXECUTION_PROFILE_ENV;
#[cfg(all(unix, feature = "kernel-resident"))]
use super::opt_in_kernel::{KERNEL_LOOP_BIN_ENV, KERNEL_RUNTIME_ROOT_ENV};
use super::*;
#[cfg(feature = "kernel-managed-model")]
use crate::{
    extensions::NativeExtensions,
    host::{
        Cancellation, ExecuteRequest, ExecuteResult, HostError, HostErrorKind, HostFuture,
        HostHandler,
    },
    runtime::{
        project::RuntimeLease,
        turn::execution_backend::{TurnExecutionBackend, TurnExecutionClient, TurnExecutionFacts},
    },
};
use crate::{
    runtime::{
        interaction::{GatewayInteractionCoordinator, InteractionOptions, PendingEntry},
        project::RuntimeError,
        session::{
            SessionContext, SessionFactory, SessionOwner, SessionOwnerOptions,
            transcript_locks::session_transcript_path,
        },
    },
    storage::{JsonlTranscriptStore, TranscriptKey, TranscriptStore},
};
use pilotdeck_protocol::{JsString, parse_json, stringify_json};
#[cfg(all(unix, feature = "kernel-resident"))]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        let short = &uuid::Uuid::new_v4().simple().to_string()[..10];
        let raw = PathBuf::from(format!("/tmp/{prefix}-{short}"));
        fs::create_dir_all(&raw).unwrap();
        Self(fs::canonicalize(&raw).unwrap_or(raw))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct CountingOpenAiServer {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn read_http_chunked_body(reader: &mut BufReader<TcpStream>) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line).unwrap_or(0) == 0 {
            break;
        }
        let hex_len = size_line.trim().split(';').next().unwrap_or("").trim();
        let Ok(chunk_len) = usize::from_str_radix(hex_len, 16) else {
            break;
        };
        if chunk_len == 0 {
            let mut trailer = String::new();
            let _ = reader.read_line(&mut trailer);
            break;
        }
        let mut chunk = vec![0u8; chunk_len];
        if reader.read_exact(&mut chunk).is_err() {
            break;
        }
        body.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        let _ = reader.read_exact(&mut crlf);
    }
    body
}

fn handle_counting_http(stream: TcpStream, reply_text: &str, hits: &AtomicUsize) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Ok(cloned) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(cloned);
    let mut stream = stream;
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }
    let raw_body = if chunked {
        read_http_chunked_body(&mut reader)
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        let _ = reader.read_exact(&mut buf);
        buf
    } else {
        Vec::new()
    };
    let req = String::from_utf8_lossy(&raw_body);
    let is_title = req.contains("Generate a concise title");
    let is_stream = req.contains("\"stream\":true");
    if !is_title && is_stream {
        hits.fetch_add(1, Ordering::SeqCst);
    }
    let sse = format!(
        "data: {{\"id\":\"c1\",\"model\":\"mock\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{reply_text}\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"id\":\"c1\",\"model\":\"mock\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: {{\"id\":\"c1\",\"model\":\"mock\",\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}}}\n\ndata: [DONE]\n\n"
    );
    let response = if is_stream && !is_title {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
            sse.len()
        )
    } else {
        let body = r#"{"id":"t","object":"chat.completion","model":"mock","choices":[{"index":0,"message":{"role":"assistant","content":"{\"title\":\"Test\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

impl CountingOpenAiServer {
    fn start(reply_text: &'static str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (hits_ref, stop_ref) = (hits.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stop_ref.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => handle_counting_http(stream, reply_text, &hits_ref),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            hits,
            stop,
            thread: Some(thread),
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for CountingOpenAiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn write_minimal_config(home: &Path, openai_addr: SocketAddr) -> PathBuf {
    let config_path = home.join("pilotdeck.yaml");
    fs::write(
        &config_path,
        format!(
            concat!(
                "schemaVersion: 1\n",
                "agent: {{model: fixture/mock}}\n",
                "model:\n",
                "  providers:\n",
                "    fixture:\n",
                "      protocol: openai\n",
                "      url: http://{}/v1\n",
                "      apiKey: fixture-local-only\n",
                "      models: {{mock: {{capabilities: {{supportsToolUse: true}}}}}}\n",
                "telemetry: {{enabled: false}}\n",
            ),
            openai_addr
        ),
    )
    .unwrap();
    config_path
}

fn base_local_options(
    home: &Path,
    project: &Path,
    config: &Path,
    profile: Option<&str>,
) -> LocalOptions {
    let mut env = Env::from([
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("PILOT_HOME".into(), home.to_string_lossy().into_owned()),
        (
            "PILOTDECK_CONFIG_PATH".into(),
            config.to_string_lossy().into_owned(),
        ),
    ]);
    if let Some(profile) = profile {
        env.insert(KERNEL_EXECUTION_PROFILE_ENV.into(), profile.into());
    }
    LocalOptions {
        pilot_home: home.to_path_buf(),
        project: project.to_path_buf(),
        os_home: Some(home.to_path_buf()),
        sidecar: home.join("unused"),
        env,
        builtin_skills_root: None,
        builtin_plugins_root: None,
        telemetry_context: None,
    }
}

#[cfg(feature = "kernel-managed-model")]
struct FailingMockBackend {
    owner: BackendOwner,
    error_kind: HostErrorKind,
    message: &'static str,
    calls: Arc<AtomicUsize>,
}

#[cfg(feature = "kernel-managed-model")]
impl ManagedSessionBinder for FailingMockBackend {
    fn backend_owner(&self) -> BackendOwner {
        self.owner
    }

    fn validate_installation(&self) -> ManagedFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async { Ok(()) })
    }

    fn prepare<'a>(
        &'a self,
        _admitted: &'a RuntimeLease,
        _context: &'a SessionContext,
    ) -> ManagedFuture<'a, Result<PreparedManagedSession, RuntimeError>> {
        let calls = self.calls.clone();
        let kind = self.error_kind;
        let msg = self.message;
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let backend: Arc<dyn TurnExecutionBackend> = Arc::new(MockTurnErrorBackend {
                kind,
                message: msg,
                calls: calls.clone(),
            });
            Ok(PreparedManagedSession {
                backend,
                close: Box::new(|| Box::pin(async { Ok(()) })),
            })
        })
    }
}

#[cfg(feature = "kernel-managed-model")]
struct MockTurnErrorBackend {
    kind: HostErrorKind,
    message: &'static str,
    calls: Arc<AtomicUsize>,
}

#[cfg(feature = "kernel-managed-model")]
impl TurnExecutionBackend for MockTurnErrorBackend {
    fn prepare(&self, _facts: TurnExecutionFacts) -> HostFuture<Box<dyn TurnExecutionClient>> {
        let kind = self.kind;
        let msg = self.message;
        let calls = self.calls.clone();
        Box::pin(async move {
            Ok(Box::new(MockTurnErrorClient {
                kind,
                message: msg,
                calls,
            }) as Box<dyn TurnExecutionClient>)
        })
    }
}

#[cfg(feature = "kernel-managed-model")]
struct MockTurnErrorClient {
    kind: HostErrorKind,
    message: &'static str,
    calls: Arc<AtomicUsize>,
}

#[cfg(feature = "kernel-managed-model")]
impl TurnExecutionClient for MockTurnErrorClient {
    fn module_id(&self) -> JsString {
        "mock-kernel".into()
    }

    fn execute(
        self: Box<Self>,
        _request: ExecuteRequest,
        handler: Arc<dyn HostHandler>,
        _cancellation: Cancellation,
    ) -> HostFuture<ExecuteResult> {
        let kind = self.kind;
        let msg = self.message;
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let mut scope = crate::host::TurnScope::new(handler, Duration::from_secs(5));
            scope.dispose().await;
            Err(HostError::new(kind, msg))
        })
    }
}

#[test]
fn ownership_pinning_cross_backend_isolation_and_unimported_jsonl_rejection() {
    let dir = TempDirGuard::new("pd-own-iso");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();

    // Fresh Legacy session pins to LegacyRuntime and persists across lookups.
    let legacy_rec = super::session_ownership::admit_or_pin_session(
        &home,
        &project,
        &JsString::from("legacy-1"),
        false,
        BackendOwner::LegacyRuntime,
    )
    .unwrap();
    assert_eq!(legacy_rec.owner, BackendOwner::LegacyRuntime);
    assert_eq!(
        load_session_owner(&home, "legacy-1")
            .unwrap()
            .unwrap()
            .owner,
        BackendOwner::LegacyRuntime
    );

    // Cross-backend mixing onto KernelFullLate or KernelScopedResident fails closed.
    for target in [
        BackendOwner::KernelFullLate,
        BackendOwner::KernelScopedResident,
    ] {
        let err = super::session_ownership::admit_or_pin_session(
            &home,
            &project,
            &JsString::from("legacy-1"),
            false,
            target,
        )
        .unwrap_err();
        assert!(
            err.message.contains("MIGRATION_OWNERSHIP_CONFLICT"),
            "unexpected conflict error: {}",
            err.message
        );
    }

    // KernelFullLate session rejects running on LegacyRuntime or KernelScopedResident.
    let kfl_rec = super::session_ownership::admit_or_pin_session(
        &home,
        &project,
        &JsString::from("kfl-1"),
        false,
        BackendOwner::KernelFullLate,
    )
    .unwrap();
    assert_eq!(kfl_rec.owner, BackendOwner::KernelFullLate);
    for wrong in [
        BackendOwner::LegacyRuntime,
        BackendOwner::KernelScopedResident,
    ] {
        let err = super::session_ownership::admit_or_pin_session(
            &home,
            &project,
            &JsString::from("kfl-1"),
            false,
            wrong,
        )
        .unwrap_err();
        assert!(err.message.contains("MIGRATION_OWNERSHIP_CONFLICT"));
    }

    // Pre-existing un-imported legacy .jsonl without an ownership record cannot enter Kernel.
    let unimported = JsString::from("pre-existing-legacy");
    let unimported_path = session_transcript_path(&home, &project, &unimported);
    fs::create_dir_all(unimported_path.parent().unwrap()).unwrap();
    fs::write(
        &unimported_path,
        "{\"type\":\"turn_started\",\"sessionId\":\"pre-existing-legacy\",\"turnId\":\"t1\",\"sequence\":1}\n",
    )
    .unwrap();
    let err = super::session_ownership::admit_or_pin_session(
        &home,
        &project,
        &unimported,
        false,
        BackendOwner::KernelFullLate,
    )
    .unwrap_err();
    assert!(
        err.message.contains("MIGRATION_NOT_STATIONARY"),
        "expected un-imported transcript rejection, got: {}",
        err.message
    );
}

fn register_test_project(home: &Path, project: &Path) {
    let chats = crate::session_chat_dir(home, project, &project.to_string_lossy());
    fs::create_dir_all(chats.parent().unwrap()).unwrap();
    fs::write(
        chats.parent().unwrap().join(".cwd"),
        project.to_string_lossy().as_bytes(),
    )
    .unwrap();
}

#[test]
fn local_gateway_enforces_ownership_and_fails_closed_on_tampered_ledger() {
    let dir = TempDirGuard::new("pd-lg-own");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    register_test_project(&home, &project);

    let server = CountingOpenAiServer::start("LEGACY_OWNED_OK");
    let config = write_minimal_config(&home, server.addr);

    // 1. Run a real turn in Legacy mode -> pins session as LegacyRuntime on disk.
    let gw =
        LocalGateway::start(base_local_options(&home, &project, &config, Some("legacy"))).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let capture = events.clone();
    gw.submit_turn(
        parse_json(r#"{"sessionKey":"legacy-gw-1","channelKey":"test","message":"hi"}"#).unwrap(),
        Arc::new(move |ev| capture.lock().unwrap().push(ev)),
    )
    .unwrap();
    gw.shutdown(Duration::from_secs(10)).unwrap();
    assert_eq!(server.hits(), 1);
    assert_eq!(
        load_session_owner(&home, "legacy-gw-1")
            .unwrap()
            .unwrap()
            .owner,
        BackendOwner::LegacyRuntime
    );

    // 2. Tampers in ownership ledger fail closed immediately at LocalGateway::start.
    let store_path = ownership_store_path(&home);
    let saved_ledger = fs::read(&store_path).unwrap();
    fs::write(&store_path, b"{\"tampered\":true}\n").unwrap();
    let err =
        match LocalGateway::start(base_local_options(&home, &project, &config, Some("legacy"))) {
            Ok(_) => panic!("tampered ownership ledger must fail closed"),
            Err(err) => err,
        };
    assert!(
        err.to_string().contains("MIGRATION_OWNERSHIP_CORRUPT"),
        "unexpected tamper error: {err}"
    );
    fs::write(&store_path, saved_ledger).unwrap();

    // 3. Trying to submit that LegacyRuntime-pinned session through a Kernel binder fails closed
    //    without touching the binder or Legacy model runtime.
    #[cfg(feature = "kernel-managed-model")]
    {
        let binder_calls = Arc::new(AtomicUsize::new(0));
        let binder: Arc<dyn ManagedSessionBinder> = Arc::new(FailingMockBackend {
            owner: BackendOwner::KernelFullLate,
            error_kind: HostErrorKind::LoopFailure,
            message: "should not reach backend",
            calls: binder_calls.clone(),
        });
        let hits_before = server.hits();
        let kgw = LocalGateway::start_with_services(
            base_local_options(&home, &project, &config, Some("legacy")),
            NativeExtensions::default(),
            NativeGatewayServices {
                kernel_execution: Some(binder),
                ..Default::default()
            },
        )
        .unwrap();
        let conflict_events = Arc::new(Mutex::new(Vec::new()));
        let conflict_capture = conflict_events.clone();
        let conflict_outcome = kgw.submit_turn(
            parse_json(r#"{"sessionKey":"legacy-gw-1","channelKey":"test","message":"hi"}"#)
                .unwrap(),
            Arc::new(move |ev| {
                if let Ok(Some(s)) = stringify_json(&ev) {
                    conflict_capture.lock().unwrap().push(s);
                }
            }),
        );
        let observed = match conflict_outcome {
            Err(err) => err.to_string(),
            Ok(()) => conflict_events.lock().unwrap().join("\n"),
        };
        assert!(
            observed.contains("MIGRATION_OWNERSHIP_CONFLICT"),
            "expected cross-backend conflict error, got: {observed}"
        );
        assert_eq!(binder_calls.load(Ordering::SeqCst), 0);
        assert_eq!(server.hits(), hits_before);
        kgw.shutdown(Duration::from_secs(10)).unwrap();
    }

    server.shutdown();
}

#[cfg(feature = "kernel-managed-model")]
#[test]
fn kernel_turn_failure_and_result_unknown_never_fallback_or_replay_on_legacy() {
    for (label, kind, expected_diag) in [
        (
            "loop_failure",
            HostErrorKind::LoopFailure,
            "FailedNoFallback",
        ),
        (
            "result_unknown",
            HostErrorKind::ResultUnknown,
            "UnknownReconcileRequired",
        ),
    ] {
        let dir = TempDirGuard::new("pd-nofallback");
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&project).unwrap();
        register_test_project(&home, &project);

        let legacy_http = CountingOpenAiServer::start("MUST_NEVER_RUN_LEGACY");
        let config = write_minimal_config(&home, legacy_http.addr);

        let calls = Arc::new(AtomicUsize::new(0));
        let binder: Arc<dyn ManagedSessionBinder> = Arc::new(FailingMockBackend {
            owner: BackendOwner::KernelFullLate,
            error_kind: kind,
            message: "kernel turn exploded",
            calls: calls.clone(),
        });
        let gw = LocalGateway::start_with_services(
            base_local_options(&home, &project, &config, None),
            NativeExtensions::default(),
            NativeGatewayServices {
                kernel_execution: Some(binder),
                ..Default::default()
            },
        )
        .unwrap();

        let sid = format!("k-{label}");
        let events = Arc::new(Mutex::new(Vec::new()));
        let capture = events.clone();
        let outcome = gw.submit_turn(
            parse_json(&format!(
                r#"{{"sessionKey":"{sid}","channelKey":"test","message":"hello"}}"#
            ))
            .unwrap(),
            Arc::new(move |ev| {
                if let Ok(Some(s)) = stringify_json(&ev) {
                    capture.lock().unwrap().push(s);
                }
            }),
        );
        let observed = match outcome {
            Err(err) => err.to_string(),
            Ok(()) => events.lock().unwrap().join("\n"),
        };
        assert!(
            observed.contains("kernel turn exploded"),
            "{label} missing authentic failure message: {observed}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "prepare + execute called once"
        );
        assert_eq!(
            legacy_http.hits(),
            0,
            "{label} must never fall back to legacy OpenAI runtime"
        );

        let record = load_session_owner(&home, &sid).unwrap().unwrap();
        assert_eq!(record.owner, BackendOwner::KernelFullLate);

        let host_err = HostError::new(kind, "direct check");
        let recorded = super::session_ownership::record_kernel_turn_failure(
            &home,
            &JsString::from(sid.as_str()),
            "turn-check",
            BackendOwner::KernelFullLate,
            &host_err,
        )
        .unwrap();
        assert!(format!("{:?}", recorded.disposition).contains(expected_diag));

        gw.shutdown(Duration::from_secs(10)).unwrap();
        legacy_http.shutdown();
    }
}

#[test]
fn live_gateway_stationary_audit_rejects_active_writer_or_pending_interactions() {
    let dir = TempDirGuard::new("pd-live-audit");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();

    struct StubFactory;
    impl SessionFactory for StubFactory {
        fn create(
            &self,
            _context: SessionContext,
            _previous: Option<crate::runtime::session::LiveSessionHandle>,
        ) -> crate::runtime::refresh::BoxFuture<
            Result<crate::runtime::session::LiveSessionHandle, RuntimeError>,
        > {
            Box::pin(async { Err(RuntimeError::new("unused")) })
        }
    }

    let owner = SessionOwner::new(SessionOwnerOptions {
        factory: Arc::new(StubFactory),
        now_ms: Arc::new(|| 1_700_000_000_000),
        idle_timeout_ms: 60_000,
        idle_sweep_interval_ms: 0,
        on_evict: None,
        on_session_idle_evict: None,
    });
    let interactions = GatewayInteractionCoordinator::new(InteractionOptions::default());
    let sk = JsString::from("sess-busy");

    // Seed a valid stationary transcript on disk, then simulate a pending permission prompt.
    let path = session_transcript_path(&home, &project, &sk);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        concat!(
            r#"{"type":"turn_started","sessionId":"sess-busy","turnId":"t1","sequence":1}"#,
            "\n",
            r#"{"type":"accepted_input","sessionId":"sess-busy","turnId":"t1","sequence":2,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
            "\n",
            r#"{"type":"assistant_message","sessionId":"sess-busy","turnId":"t1","sequence":3,"message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}"#,
            "\n",
            r#"{"type":"turn_result","sessionId":"sess-busy","turnId":"t1","sequence":4,"result":{"type":"success"}}"#,
            "\n"
        ),
    )
    .unwrap();

    let reg = interactions
        .permission_bus()
        .register(
            &sk,
            PendingEntry {
                request_id: "req-1".into(),
                tool_call_id: "tc-1".into(),
                tool_name: "bash".into(),
                resolve: Arc::new(|_| {}),
                reject: Arc::new(|_| {}),
            },
            None,
        )
        .unwrap();
    let activity =
        observe_live_session_activity(&owner, &interactions, &home, &project, &sk).unwrap();
    assert_eq!(activity.pending_permission_count, 1);

    let err = import_stationary_session_into_gateway(
        &home,
        &project,
        "sess-busy",
        &activity,
        BackendOwner::KernelFullLate,
    )
    .unwrap_err();
    assert!(
        err.message.contains("MIGRATION_NOT_STATIONARY"),
        "expected pending permission rejection, got: {}",
        err.message
    );
    reg.dispose();

    // Once cleared, live activity reports stationary and import succeeds.
    let clean = observe_live_session_activity(&owner, &interactions, &home, &project, &sk).unwrap();
    assert!(!clean.has_active_writer_lease);
    assert_eq!(clean.pending_permission_count, 0);
    assert_eq!(clean.pending_question_count, 0);
}

#[test]
fn bidirectional_stationary_import_and_rollback_lossless_idempotent_and_restorable() {
    let dir = TempDirGuard::new("pd-mig-gw");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();

    let src_id = "legacy-bidi";
    let src_js = JsString::from(src_id);
    let src_path = session_transcript_path(&home, &project, &src_js);
    fs::create_dir_all(src_path.parent().unwrap()).unwrap();

    let jsonl = concat!(
        r#"{"type":"turn_started","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.000Z","sequence":1}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.001Z","sequence":2,"messages":[{"role":"user","content":[{"type":"text","text":"中文\ud800😀"}]}]}"#,
        "\n",
        r#"{"type":"step_started","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.002Z","step":1,"sequence":3}"#,
        "\n",
        r#"{"type":"tool_call","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.003Z","step":1,"call":{"id":"c1","name":"read_file"},"sequence":4}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.004Z","sequence":5,"message":{"role":"assistant","content":[{"type":"tool_call","id":"c1","name":"read_file","input":{"path":"a"}}]}}"#,
        "\n",
        r#"{"type":"tool_result","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.005Z","step":1,"result":{"toolCallId":"c1"},"sequence":6}"#,
        "\n",
        r#"{"type":"tool_result_message","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.006Z","sequence":7,"message":{"role":"user","content":[{"type":"tool_result","toolCallId":"c1","content":[{"type":"text","text":"ok-\ud800"}]}]}}"#,
        "\n",
        r#"{"type":"step_completed","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.007Z","step":1,"outcome":"completed","sequence":8}"#,
        "\n",
        r#"{"type":"step_started","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.008Z","step":2,"sequence":9}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.009Z","sequence":10,"message":{"role":"assistant","content":[{"type":"text","text":"FINAL_MARKER"}]}}"#,
        "\n",
        r#"{"type":"step_completed","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.010Z","step":2,"outcome":"completed","sequence":11}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"legacy-bidi","turnId":"t1","createdAt":"2026-11-16T00:00:00.011Z","sequence":12,"result":{"type":"success"}}"#,
        "\n"
    );
    fs::write(&src_path, jsonl.as_bytes()).unwrap();
    let original_bytes = fs::read(&src_path).unwrap();

    // Pin source as LegacyRuntime first.
    super::session_ownership::admit_or_pin_session(
        &home,
        &project,
        &src_js,
        false,
        BackendOwner::LegacyRuntime,
    )
    .unwrap();

    // Import LegacyRuntime -> KernelFullLate.
    let imported = import_stationary_session_into_gateway(
        &home,
        &project,
        src_id,
        &LiveSessionActivity::default(),
        BackendOwner::KernelFullLate,
    )
    .unwrap();
    assert_ne!(imported.mapping.target_session_id, src_id);
    assert_eq!(imported.mapping.target_owner, BackendOwner::KernelFullLate);
    assert_eq!(imported.audit_entry_count, 12);
    assert_eq!(imported.audit_message_count, 4);
    assert_eq!(fs::read(&src_path).unwrap(), original_bytes);
    assert_eq!(
        load_session_owner(&home, src_id).unwrap().unwrap().owner,
        BackendOwner::LegacyRuntime
    );
    assert_eq!(
        load_session_owner(&home, &imported.mapping.target_session_id)
            .unwrap()
            .unwrap()
            .owner,
        BackendOwner::KernelFullLate
    );

    // Idempotent second run produces exact same mapping.
    let imported_again = import_stationary_session_into_gateway(
        &home,
        &project,
        src_id,
        &LiveSessionActivity::default(),
        BackendOwner::KernelFullLate,
    )
    .unwrap();
    assert_eq!(imported_again.mapping, imported.mapping);

    // Rollback KernelFullLate -> LegacyRuntime produces a distinct rollback target.
    let rolled_back = rollback_session_in_gateway(
        &home,
        &project,
        &imported.mapping.target_session_id,
        &LiveSessionActivity::default(),
        BackendOwner::LegacyRuntime,
        1,
    )
    .unwrap();
    assert_ne!(rolled_back.mapping.target_session_id, src_id);
    assert_ne!(
        rolled_back.mapping.target_session_id,
        imported.mapping.target_session_id
    );
    assert!(rolled_back.mapping.is_rollback);
    assert_eq!(
        rolled_back.mapping.target_owner,
        BackendOwner::LegacyRuntime
    );
    assert_eq!(
        load_session_owner(&home, &imported.mapping.target_session_id)
            .unwrap()
            .unwrap()
            .owner,
        BackendOwner::KernelFullLate
    );
    assert_eq!(
        load_session_owner(&home, &rolled_back.mapping.target_session_id)
            .unwrap()
            .unwrap()
            .owner,
        BackendOwner::LegacyRuntime
    );

    // Gateway JsonlTranscriptStore reads the rolled-back .jsonl losslessly including lone surrogate \ud800.
    let store = JsonlTranscriptStore::new();
    let rb_js = JsString::from(rolled_back.mapping.target_session_id.as_str());
    let read = store
        .read(
            &TranscriptKey {
                project_root: project.clone(),
                pilot_home: home.clone(),
                session_id: rb_js.clone(),
                transcript_path: rolled_back.target_transcript_path.clone(),
            },
            None,
        )
        .unwrap();
    assert_eq!(read.entries.len(), 12);
    let dumped: Vec<String> = read
        .entries
        .iter()
        .filter_map(|e| stringify_json(e).ok().flatten())
        .collect();
    let joined = dumped.join("\n");
    assert!(joined.contains("FINAL_MARKER"));
    assert!(joined.contains("\\ud800"));
}

#[cfg(all(unix, feature = "kernel-resident"))]
#[test]
fn full_late_opt_in_and_auto_discovery_fail_closed_when_host_lacks_settlement_feature() {
    let dir = TempDirGuard::new("pd-fl-neg");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    let root_dir = dir.path().join("kr");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();

    let server = CountingOpenAiServer::start("MUST_NOT_REACH_LEGACY");
    let config = write_minimal_config(&home, server.addr);
    let exe = dir.path().join("pilotdeck-native-loop");
    fs::write(&exe, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();

    // Exercise both explicit `full-late` and unset `P7.default` auto-discovery.
    for (idx, profile) in [Some("full-late"), None].into_iter().enumerate() {
        let kr = root_dir.join(format!("r{idx}"));
        fs::create_dir_all(&kr).unwrap();
        let mut opts = base_local_options(&home, &project, &config, profile);
        opts.env.insert(
            KERNEL_LOOP_BIN_ENV.into(),
            exe.to_string_lossy().into_owned(),
        );
        opts.env.insert(
            KERNEL_RUNTIME_ROOT_ENV.into(),
            kr.to_string_lossy().into_owned(),
        );

        let err = match LocalGateway::start(opts) {
            Ok(_) => panic!("FullLate against fb54c74a8 host must fail closed"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("Unsupported Kernel integration")
                || err.to_string().contains("scheduler.settlement-data.v1"),
            "expected missing LOOP_SETTLEMENT_FEATURE rejection, got: {err}"
        );
        assert_eq!(server.hits(), 0);
    }

    server.shutdown();
}
