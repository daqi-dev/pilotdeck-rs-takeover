//! End-to-end stitched integration suite over MintHub-downloaded `pilotdeck-rs/*` bundles.
//!
//! Covers:
//! 1. `agentcpm-host` `components/install.json` mount + SHA-256 tamper fail-closed over UDS with
//!    compiled `pilotdeck-native-loop` (`pilotdeck-rs/iface@0.1.1`).
//! 2. Cross-package `pilotdeck-session-migration@0.1.0` + `pilotdeck-context-iface@0.1.0` +
//!    `pilotdeck-context-core@0.1.0` + `pilotdeck-context-runtime@0.1.0` +
//!    `pilotdeck-tokenizer@0.1.0` + `pilotdeck-protocol@0.1.0` migration, CAS ledger,
//!    read-only compaction/snip projection, `StoreContract`, and fail-closed Kernel failure handling.
//! 3. Live `agentcpm-host` + `pilotdeck-native-loop` (`pilotdeck-iface@0.1.1`) +
//!    `pilotdeck-model-{iface,runtime,core}` + `pilotdeck-trusted-http-host` +
//!    `pilotdeck-http-transport` + `pilotdeck-tool-{iface,core}@0.1.2` full turn & builtin
//!    workspace tools E2E.
#![cfg(unix)]
#![forbid(unsafe_code)]

use agentcpm_core::guard::SecretRegistry;
use agentcpm_host::{
    EmbedOptions, EmbedTransport, ExternalBackends, ExternalToolBinding, ManagedModelTransport,
    ResidentComposition, ResidentShutdown, ResidentShutdownRequest, SchedulerAssembly,
    SchedulerComponent, SchedulerImplementation, StartupOptions, TrustedModelBindingResolver,
    TrustedModelCatalogEntry, TrustedModelServices, TrustedModelSetup, TrustedToolBatchBackend,
    TrustedToolBatchExecution, TrustedToolBatchInstallation, TrustedToolBatchSetup,
    TrustedToolFinalCandidate, TrustedToolFinalResolution, TrustedToolLifecycle,
    TrustedToolPreparation, TrustedUdsIdentityMapping, serve_resident_composed,
};
use agentcpm_iface::{
    BoxFuture,
    context::{CompactionPersistenceContract, ContextContract, TokenCounter},
    env::http_pull::{
        HttpExchange, HttpFailure, HttpFailureKind, HttpFailurePhase, HttpPullContext,
        HttpPullRequest, HttpPullTransport, HttpRequestPreparation, HttpScopeCleanup,
    },
    model_session::{
        ConfigRevision, ModelBinding, ModelExecutionContext, ModelSession, ModelSessionConfig,
        ModelSessionFactory, SessionCapabilities,
    },
    store::{EventRecord, StoreContract},
    tool::{ToolCall, ToolContract, ToolSpec},
    types::{CallMeta, Cursor, ERR_APP, Error as IfaceError, ErrorCategory, NodeId, SessionId},
};
use agentcpm_scheduler_protocol::{self as p, context_history::OpaquePayload};
use pilotdeck_context_core::{MicroCompactionEngineOptions, SnipCallOptions, SnipEngineOptions};
use pilotdeck_context_iface::{
    AuthoritativeContextLedger, ContextOperationKind, KIND_CONTEXT_INPUT, KIND_TOOL_REGISTERED,
    LedgerTransition, PilotDeckCompactionPersistence, PilotDeckContextContract,
    PilotDeckTokenCounter, SNAPSHOT_CONTENT_TYPE,
};
use pilotdeck_context_runtime::boundary_codec;
use pilotdeck_http_transport::executor::ScopeExecutor;
use pilotdeck_model_core::codec::normalize_error::JsError;
use pilotdeck_model_iface::{
    ModelJobServices, ModelJobServicesProvider, SharedPilotDeckModelFactory,
};
use pilotdeck_model_runtime::{
    driver_transport::HostOperationError,
    headers::{HeaderMaterializeResult, ModelHeaderService, RequestMode},
    services::ModelHostServices,
};
use pilotdeck_protocol::{JsString, JsValue, parse_json, stringify_json};
use pilotdeck_session_migration::{
    BackendOwner, KernelFailureDisposition, LiveSessionActivity, SessionOwnershipStore,
    audit_jsonl_transcript, handle_kernel_turn_failure, import_stationary_session,
};
use pilotdeck_tokenizer::count_tokens;
use pilotdeck_tool_core::{ToolCallCandidate, WorkspaceBoundary, partition_tool_batch};
use pilotdeck_tool_iface::{WORKSPACE_TOOL_NAMES, WorkspaceToolConfig, workspace_tool_contract};
use pilotdeck_trusted_http_host::{
    ApprovedOrigins, JobHeaders, TrustedHttpRuntime, TrustedJobSnapshot, TrustedProjectLease,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::any::Any;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process::Command;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    mpsc,
};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WAIT: Duration = Duration::from_secs(30);
const REAP_WAIT: Duration = Duration::from_secs(3);

static ROOT_SEQ: AtomicU64 = AtomicU64::new(1);

struct StitchedRoot {
    path: PathBuf,
    owner_uid: u32,
    nonce: String,
}

impl StitchedRoot {
    fn new(tag: &str) -> Self {
        let seq = ROOT_SEQ.fetch_add(1, Ordering::SeqCst);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000_000;
        let base = if std::env::temp_dir().as_os_str().len() <= 24 {
            std::env::temp_dir()
        } else {
            PathBuf::from("/tmp")
        };
        let nonce = format!("pdse-{}-{seq}-{stamp}", std::process::id());
        let path = base.join(format!("pd_se2e_{}_{seq}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("data")).unwrap();
        fs::create_dir_all(path.join("components")).unwrap();
        let owner_uid = fs::metadata(&path).unwrap().uid();
        assert!(
            path.join("k.sock").as_os_str().len() < 100,
            "UDS path `{}` exceeds Unix socket safety margin",
            path.join("k.sock").display()
        );
        Self {
            path,
            owner_uid,
            nonce,
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.path.join("data")
    }

    fn components_dir(&self) -> PathBuf {
        self.path.join("components")
    }

    fn uds_path(&self) -> PathBuf {
        self.path.join("k.sock")
    }

    fn kernel_toml(&self) -> PathBuf {
        self.path.join("kernel.toml")
    }

    fn loop_binding(&self) -> String {
        format!("loop-{}", self.nonce)
    }

    fn controller_binding(&self) -> String {
        format!("ctrl-{}", self.nonce)
    }

    fn write_kernel_toml(&self) {
        fs::write(
            self.kernel_toml(),
            concat!(
                "storage_dir = \"storage\"\n",
                "guard = \"deny-all\"\n",
                "exec_backend = \"none\"\n",
                "allow_ops = \"model.register,session.new,session.create,session.close,",
                "session.lease_acquire,session.lease_renew,session.get,session.deliver,",
                "session.stream_open,execution.open,execution.input,execution.get,",
                "execution.events,execution.output_ack,execution.control,tool.list,tool.register,tool.exec,operation.get,",
                "model.session_open,model.session_get,model.input_append,model.input_commit,",
                "model.events_read,model.delivery_advance,model.session_close,",
                "model.session_abort,model.response_interrupt\"\n",
            ),
        )
        .unwrap();
    }

    fn copy_stitched_components(&self, src_components: &Path) {
        copy_dir_recursive(src_components, &self.components_dir());
    }

    fn install_native_loop_copy(&self) -> (PathBuf, String) {
        let source = stitched_native_loop_bin();
        let program = self.path.join("pilotdeck-native-loop");
        fs::copy(&source, &program).expect("copy compiled stitched pilotdeck-native-loop");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        #[cfg(target_os = "macos")]
        {
            assert!(
                Command::new("codesign")
                    .args(["--force", "--sign", "-"])
                    .arg(&program)
                    .status()
                    .expect("codesign copied pilotdeck-native-loop")
                    .success(),
                "codesign failed for {}",
                program.display()
            );
        }
        let version = Command::new(&program)
            .arg("--version")
            .output()
            .expect("verify stitched pilotdeck-native-loop --version");
        assert!(version.status.success());
        let digest = sha256_file(&program);
        (program, digest)
    }

    fn embed_options(
        &self,
        ready_flag: Option<Arc<AtomicBool>>,
        ready_tx: Option<mpsc::Sender<PathBuf>>,
    ) -> EmbedOptions {
        let mut options = EmbedOptions::new(
            StartupOptions {
                config: Some(self.kernel_toml()),
                data_dir: self.data_dir(),
                components_dir: self.components_dir(),
            },
            EmbedTransport::Uds(self.uds_path()),
        );
        options.trusted_uds_identity = Some(
            TrustedUdsIdentityMapping::new(self.owner_uid, "pilotdeck-app", "tenant-stitched")
                .expect("valid trusted UDS mapping"),
        );
        options.require_attested_transport = true;
        options.require_attested_tenant = true;
        options.on_ready = Some(Box::new(move |notice| {
            if let Some(flag) = &ready_flag {
                flag.store(true, Ordering::SeqCst);
            }
            if let (Some(tx), Some(uds)) = (&ready_tx, notice.uds) {
                let _ = tx.send(uds);
            }
        }));
        options
    }
}

impl Drop for StitchedRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    fs::set_permissions(dst, fs::Permissions::from_mode(0o755)).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_recursive(&src_path, &dst_path);
        } else {
            fs::copy(&src_path, &dst_path).unwrap();
            let mode = (entry.metadata().unwrap().permissions().mode() & 0o777) & !0o022;
            fs::set_permissions(&dst_path, fs::Permissions::from_mode(mode)).unwrap();
        }
    }
}

fn stitched_components_dir() -> PathBuf {
    let dir = std::env::var_os("STITCHED_COMPONENTS_DIR")
        .map(PathBuf::from)
        .expect("STITCHED_COMPONENTS_DIR must point to scaffolded MintHub components directory");
    assert!(
        dir.join("install.json").is_file(),
        "missing install.json under {}",
        dir.display()
    );
    dir
}

fn stitched_native_loop_bin() -> PathBuf {
    let bin = std::env::var_os("STITCHED_NATIVE_LOOP_BIN")
        .map(PathBuf::from)
        .expect("STITCHED_NATIVE_LOOP_BIN must point to pilotdeck-native-loop built from MintHub tarball");
    assert!(
        bin.is_file(),
        "missing compiled pilotdeck-native-loop at {}",
        bin.display()
    );
    bin
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).expect("read artifact for sha256");
    format!("{:x}", Sha256::digest(&bytes))
}

fn collect_sock_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_socket()
            || path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| matches!(ext, "sock" | "uds"))
        {
            found.push(path);
        } else if meta.file_type().is_dir() {
            found.extend(collect_sock_files(&path));
        }
    }
    found
}

fn find_live_processes(nonce: &str) -> Vec<String> {
    let output = Command::new("ps")
        .args(["-eo", "pid=,ppid=,stat=,args="])
        .output()
        .expect("run ps");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || !trimmed.contains(nonce) {
                return None;
            }
            let mut fields = trimmed.split_whitespace();
            let _pid = fields.next()?;
            let _ppid = fields.next()?;
            let stat = fields.next()?;
            if stat.starts_with('Z') {
                return None;
            }
            Some(trimmed.to_owned())
        })
        .collect()
}

fn assert_clean_teardown(root: &StitchedRoot) {
    assert!(collect_sock_files(&root.path).is_empty());
    let deadline = Instant::now() + REAP_WAIT;
    loop {
        let leaked = find_live_processes(&root.nonce);
        if leaked.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "leaked children: {leaked:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn loop_config_arg(loop_binding: &str, controller_binding: &str) -> String {
    json!({
        "profile": "pilotdeck.scoped-native-loop.v1",
        "loop_binding": loop_binding,
        "controller_binding": controller_binding,
    })
    .to_string()
}

fn controller_config_arg(loop_binding: &str, controller_binding: &str) -> String {
    json!({
        "profile": "pilotdeck.product-controller.v1",
        "loop_binding": loop_binding,
        "controller_binding": controller_binding,
    })
    .to_string()
}

fn valid_scheduler_assembly(
    program: &Path,
    sha256: &str,
    loop_binding: &str,
    controller_binding: &str,
) -> SchedulerAssembly {
    SchedulerAssembly {
        protocol: p::PROTOCOL.into(),
        profile: p::SessionConfig {
            input_controller_ref: Some(controller_binding.into()),
            default_loop_ref: loop_binding.into(),
            allowed_bindings: [loop_binding.into(), controller_binding.into()].into(),
        },
        bindings: vec![
            SchedulerComponent {
                binding: p::Binding {
                    id: loop_binding.into(),
                    revision: "stitched-loop-v1".into(),
                    kind: p::Kind::Loop,
                    effect: p::Effect::Model,
                    permissions: p::Permissions {
                        invoke: [controller_binding.into()].into(),
                        publish: false,
                        state_namespace: None,
                        resources: [
                            p::tool_batch::PREPARE,
                            p::tool_batch::STATUS,
                            p::tool_batch::CLOSE,
                            p::product_reads::PLAN_TODO,
                            p::product_reads::MODEL_METADATA,
                            "model.session_open",
                            "model.session_get",
                            "model.input_append",
                            "model.input_commit",
                            "model.events_read",
                            "model.delivery_advance",
                            "model.session_close",
                            "model.session_abort",
                            "model.response_interrupt",
                        ]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    },
                },
                provider: SchedulerImplementation::Native {
                    program: program.to_path_buf(),
                    args: vec![
                        "--pilotdeck-scoped-native-loop-v1".into(),
                        loop_config_arg(loop_binding, controller_binding),
                    ],
                    sha256: sha256.to_owned(),
                },
                controller: false,
            },
            SchedulerComponent {
                binding: p::Binding {
                    id: controller_binding.into(),
                    revision: "stitched-ctrl-v1".into(),
                    kind: p::Kind::Service,
                    effect: p::Effect::State,
                    permissions: p::Permissions {
                        invoke: [loop_binding.into()].into(),
                        publish: true,
                        state_namespace: None,
                        resources: Default::default(),
                    },
                },
                provider: SchedulerImplementation::Native {
                    program: program.to_path_buf(),
                    args: vec![
                        "--pilotdeck-product-controller-v1".into(),
                        controller_config_arg(loop_binding, controller_binding),
                    ],
                    sha256: sha256.to_owned(),
                },
                controller: true,
            },
        ],
    }
}

struct UdsWire {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    id: u64,
}

impl UdsWire {
    fn connect(uds_path: &Path) -> Self {
        let writer = UnixStream::connect(uds_path).expect("connect to resident UDS");
        writer.set_read_timeout(Some(WAIT)).unwrap();
        writer.set_write_timeout(Some(WAIT)).unwrap();
        let mut wire = Self {
            reader: BufReader::new(writer.try_clone().unwrap()),
            writer,
            id: 0,
        };
        let init = wire.call(
            "initialize",
            json!({
                "abi_version": 2,
                "api_version": agentcpm_iface::API_VERSION,
                "features": [
                    "model-session/1.0",
                    agentcpm_iface::model_session::MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
                ],
            }),
        );
        assert_eq!(init["abi_version"], 2);
        assert_eq!(
            init["server_capabilities"]["scheduler"]["protocol"],
            p::PROTOCOL
        );
        wire
    }

    fn raw_call(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.writer,
            "{}",
            json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        )
        .unwrap();
        self.writer.flush().unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(
                self.reader.read_line(&mut line).unwrap(),
                0,
                "{method}: UDS connection closed"
            );
            let frame: Value = serde_json::from_str(&line).expect("valid JSON-RPC frame");
            if frame["id"] != self.id {
                continue;
            }
            return frame;
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let frame = self.raw_call(method, params);
        assert!(frame.get("error").is_none(), "{method} failed: {frame}");
        frame
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method} missing result: {frame}"))
    }

    fn expect_error(&mut self, method: &str, params: Value) -> Value {
        let frame = self.raw_call(method, params);
        frame
            .get("error")
            .cloned()
            .unwrap_or_else(|| panic!("{method} unexpectedly succeeded: {frame}"))
    }

    fn wait_terminal(&mut self, session_id: &str, call_ref: &str) -> Value {
        let end = Instant::now() + WAIT;
        loop {
            let view = self.call(
                "execution.get",
                json!({"session_id": session_id, "call_ref": call_ref}),
            );
            if matches!(
                view["state"].as_str(),
                Some("completed" | "failed" | "cancelled" | "unknown")
            ) {
                return view;
            }
            assert!(Instant::now() < end, "execution timed out: {view}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

struct NoopToolLifecycle;

impl TrustedToolLifecycle for NoopToolLifecycle {
    fn stop(&self) {}

    fn is_released(&self) -> bool {
        true
    }
}

struct NoopToolBatchBackend;

impl TrustedToolBatchBackend for NoopToolBatchBackend {
    fn validate_envelope(
        &self,
        _: &OpaquePayload,
    ) -> Result<Arc<dyn Any + Send + Sync>, IfaceError> {
        Err(IfaceError::new(
            ErrorCategory::Execution,
            ERR_APP,
            "unused in pure model turn",
        ))
    }

    fn start(&self, _: TrustedToolPreparation) -> Result<TrustedToolBatchExecution, IfaceError> {
        Err(IfaceError::new(
            ErrorCategory::Execution,
            ERR_APP,
            "unused in pure model turn",
        ))
    }

    fn resolve_final(
        &self,
        _: &TrustedToolPreparation,
        _: &TrustedToolFinalCandidate,
    ) -> Result<TrustedToolFinalResolution, IfaceError> {
        Err(IfaceError::new(
            ErrorCategory::Execution,
            ERR_APP,
            "unused in pure model turn",
        ))
    }
}

struct NoopToolBatchSetup;

impl TrustedToolBatchSetup for NoopToolBatchSetup {
    fn build(&self) -> Result<TrustedToolBatchInstallation, IfaceError> {
        Ok(TrustedToolBatchInstallation {
            backend: Arc::new(NoopToolBatchBackend),
            lifecycle: Arc::new(NoopToolLifecycle),
        })
    }
}

struct HostGuard {
    uds_path: PathBuf,
    shutdown: ResidentShutdown,
    thread: Option<std::thread::JoinHandle<i32>>,
}

impl HostGuard {
    fn spawn(
        root: &StitchedRoot,
        assembly: SchedulerAssembly,
        tools: Vec<ExternalToolBinding>,
        managed_model: Option<Arc<dyn TrustedModelSetup>>,
    ) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let options = root.embed_options(None, Some(ready_tx));
        let mut composition = ResidentComposition::new(ExternalBackends {
            scheduler_assembly: Some(assembly),
            tools,
            ..Default::default()
        })
        .with_trusted_tool_batches(Arc::new(NoopToolBatchSetup));
        if let Some(setup) = managed_model {
            composition = composition.with_managed_model(setup);
        }
        let (composition, shutdown) = composition.with_shutdown_control();

        let thread = std::thread::spawn(move || serve_resident_composed(options, composition));
        let uds_path = ready_rx
            .recv_timeout(WAIT)
            .expect("host failed to emit ready UDS path within timeout");
        Self {
            uds_path,
            shutdown,
            thread: Some(thread),
        }
    }

    fn shutdown(mut self) -> i32 {
        assert_eq!(
            self.shutdown.request_shutdown(),
            ResidentShutdownRequest::Requested
        );
        self.thread.take().unwrap().join().unwrap()
    }
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.shutdown.request_shutdown();
            let _ = thread.join();
        }
    }
}

type MutexVec = Mutex<Vec<Value>>;

struct LocalMockHttp {
    addr: SocketAddr,
    requests: Arc<MutexVec>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LocalMockHttp {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(MutexVec::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (req_ref, stop_ref) = (requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stop_ref.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => handle_mock_http(stream, &req_ref),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(err) => panic!("mock HTTP accept failed: {err}"),
                }
            }
        });
        Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn shutdown(mut self) -> Vec<Value> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for LocalMockHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_mock_http(mut stream: TcpStream, requests: &MutexVec) {
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut length = None;
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let length = length.expect("Content-Length required");
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    requests.lock().unwrap().push(json!({
        "request_line": first.trim_end(),
        "body": serde_json::from_slice::<Value>(&body).unwrap(),
    }));
    let frames = [
        json!({"id":"stitched-resp","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"Stitched MintHub "},"finish_reason":null}]}),
        json!({"id":"stitched-resp","model":"m","choices":[{"index":0,"delta":{"content":"packages OK."},"finish_reason":null}]}),
        json!({"id":"stitched-resp","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"id":"stitched-resp","model":"m","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":5,"total_tokens":16}}),
    ];
    let sse = format!(
        "{}data: [DONE]\n\n",
        frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>()
    );
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        sse.len(),
        sse
    )
    .unwrap();
    stream.flush().unwrap();
}

struct StitchedModelLease(Arc<AtomicUsize>);
impl TrustedProjectLease for StitchedModelLease {}
impl Drop for StitchedModelLease {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct StitchedJobHost(Arc<JobHeaders>);
impl ModelHeaderService for StitchedJobHost {
    type Rejection = HttpFailure;
    type Preparation = HttpRequestPreparation;
    fn materialize(
        &self,
        provider: &JsValue,
        body: &JsValue,
        mode: RequestMode,
    ) -> HeaderMaterializeResult<Self::Rejection, Self::Preparation> {
        self.0.materialize(provider, body, mode)
    }
}
impl ModelHostServices for StitchedJobHost {
    fn dump_request_if_enabled(
        &self,
        _: &JsValue,
        _: &JsValue,
    ) -> Result<(), HostOperationError<JsError, HttpFailure>> {
        Ok(())
    }
    fn parse_retry_after_date(&self, _: &JsString) -> Result<Option<f64>, JsString> {
        Ok(None)
    }
    fn format_epoch_iso(&self, _: i64) -> String {
        "2026-11-16T00:00:00.000Z".into()
    }
    fn streaming_duration_limit(&self) -> Option<String> {
        None
    }
    fn diagnostic(&self, _: &str) {}
}

struct StitchedJobProvider {
    runtime: Arc<TrustedHttpRuntime>,
    resolver: TrustedModelBindingResolver,
    provider: JsValue,
    private_provider: JsValue,
    config: JsValue,
    origin: String,
    drops: Arc<AtomicUsize>,
}

impl ModelJobServicesProvider for StitchedJobProvider {
    fn supports_binding(&self, binding: &ModelBinding) -> bool {
        binding.model == "m"
            && binding.endpoint_ref.as_deref() == Some("p")
            && binding.extensions.is_empty()
    }
    fn for_job(
        &self,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        revision: ConfigRevision,
    ) -> Result<ModelJobServices, HttpFailure> {
        let scope = HttpPullContext {
            session_id: context.session_id().into(),
            model_session_id: context.model_session_id().into(),
            job_id: context.job_id().into(),
            principal: context.principal().into(),
            deadline: context.deadline(),
            cancel: context.cancel(),
        };
        let resolved = self.resolver.resolve(&scope)?;
        if resolved.binding != *binding || resolved.config_revision != revision {
            return Err(HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Denied,
                "stitched binding mismatch",
            ));
        }
        let headers = self.runtime.job_headers(TrustedJobSnapshot {
            context: scope,
            binding: resolved,
            private_provider: self.private_provider.clone(),
            provider_view: self.provider.clone(),
            canonical_model: JsValue::String("m".into()),
            approved_wire_model: Some(JsValue::String("m".into())),
            secrets: Arc::new(SecretRegistry::new([(
                "stitched/key".into(),
                "stitched-secret".into(),
            )])),
            lease: Arc::new(StitchedModelLease(self.drops.clone())),
            approved_origins: ApprovedOrigins::new([self.origin.clone()])?,
        })?;
        Ok(ModelJobServices {
            safe_config: self.config.clone(),
            host: Arc::new(StitchedJobHost(headers)),
            options: Default::default(),
        })
    }
}

struct StitchedModelSetup {
    origin: String,
    executor: tokio::runtime::Handle,
    drops: Arc<AtomicUsize>,
    runtime: OnceLock<Arc<TrustedHttpRuntime>>,
    resolver: OnceLock<TrustedModelBindingResolver>,
}

impl StitchedModelSetup {
    fn new(origin: String, executor: tokio::runtime::Handle, drops: Arc<AtomicUsize>) -> Self {
        Self {
            origin,
            executor,
            drops,
            runtime: OnceLock::new(),
            resolver: OnceLock::new(),
        }
    }
}

fn setup_err(msg: &'static str) -> IfaceError {
    IfaceError::new(ErrorCategory::Internal, -32603, msg)
}

impl TrustedModelSetup for StitchedModelSetup {
    fn uses_project_sessions(&self) -> bool {
        false
    }
    fn catalog(&self) -> Result<Vec<TrustedModelCatalogEntry>, IfaceError> {
        Ok(vec![TrustedModelCatalogEntry {
            binding: ModelBinding::new("m".into(), Some("p".into()), vec![]),
            capabilities: vec!["text".into()],
        }])
    }
    fn transport(
        &self,
        services: TrustedModelServices,
    ) -> Result<ManagedModelTransport, IfaceError> {
        self.resolver
            .set(services.bindings.clone())
            .map_err(|_| setup_err("resolver already initialized"))?;
        let runtime = Arc::new(TrustedHttpRuntime::new(services, None));
        let port = runtime.managed_transport();
        self.runtime
            .set(runtime)
            .map_err(|_| setup_err("runtime already initialized"))?;
        Ok(port)
    }
    fn factory(
        &self,
        transport: Arc<dyn HttpPullTransport>,
    ) -> Result<Arc<dyn ModelSessionFactory>, IfaceError> {
        // Construct ScopeExecutor from pilotdeck-http-transport so it stays linked
        // and exercised alongside TrustedHttpRuntime across package seams.
        let _http_exec = ScopeExecutor::default();
        let runtime = self
            .runtime
            .get()
            .cloned()
            .ok_or_else(|| setup_err("missing runtime"))?;
        let resolver = self
            .resolver
            .get()
            .cloned()
            .ok_or_else(|| setup_err("missing resolver"))?;
        let config = parse_json(&format!(
            r#"{{"providers":{{"p":{{"id":"p","protocol":"openai","url":"{}","models":{{"m":{{"id":"m","capabilities":{{"supportsStreaming":true,"supportsSystemPrompt":true}},"multimodal":{{"input":["text"]}}}}}}}}}}}}"#,
            self.origin
        ))
        .map_err(|_| setup_err("invalid config"))?;
        let provider = config
            .get("providers")
            .and_then(|providers| providers.get("p"))
            .cloned()
            .ok_or_else(|| setup_err("missing provider"))?;
        let mut private_provider = provider.clone();
        if let JsValue::Object(fields) = &mut private_provider {
            fields.insert("apiKey".into(), "stitched-secret".into());
        }
        let factory = SharedPilotDeckModelFactory {
            jobs: Arc::new(StitchedJobProvider {
                runtime,
                resolver,
                provider,
                private_provider,
                config,
                origin: self.origin.clone(),
                drops: self.drops.clone(),
            }),
            transport,
            executor: self.executor.clone(),
        };
        Ok(Arc::new(ModelFactoryPassthrough(factory)))
    }
}

struct ModelFactoryPassthrough(SharedPilotDeckModelFactory);
impl ModelSessionFactory for ModelFactoryPassthrough {
    fn capabilities(&self) -> SessionCapabilities {
        self.0.capabilities()
    }
    fn supported_profiles(&self) -> Vec<String> {
        self.0.supported_profiles()
    }
    fn open(
        &self,
        config: ModelSessionConfig,
    ) -> BoxFuture<'_, Result<Arc<dyn ModelSession>, IfaceError>> {
        self.0.open(config)
    }
}

#[allow(dead_code)]
struct GuardedTransportCheck(Arc<dyn HttpPullTransport>);
impl HttpPullTransport for GuardedTransportCheck {
    fn start(
        &self,
        request: HttpPullRequest,
        context: HttpPullContext,
    ) -> Result<HttpExchange, HttpFailure> {
        self.0.start(request, context)
    }
    fn close_scope(
        &self,
        context: HttpPullContext,
    ) -> Result<Arc<dyn HttpScopeCleanup>, HttpFailure> {
        self.0.close_scope(context)
    }
}

#[derive(Default)]
struct MemoryStore {
    sessions: Mutex<BTreeMap<SessionId, Vec<EventRecord>>>,
}

impl StoreContract for MemoryStore {
    fn append(&self, mut record: EventRecord) -> BoxFuture<'_, Result<Cursor, IfaceError>> {
        let session = record.session.clone();
        Box::pin(async move {
            let mut sessions = self.sessions.lock().unwrap();
            let log = sessions.entry(session).or_default();
            let cursor = log.len() as Cursor + 1;
            record.cursor = cursor;
            log.push(record);
            Ok(cursor)
        })
    }

    fn read(
        &self,
        session: &SessionId,
        from: Cursor,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<EventRecord>, IfaceError>> {
        let session = session.clone();
        Box::pin(async move {
            let sessions = self.sessions.lock().unwrap();
            let mut out: Vec<EventRecord> = sessions
                .get(&session)
                .map(|log| {
                    log.iter()
                        .filter(|event| event.cursor >= from)
                        .take(if limit == 0 { usize::MAX } else { limit })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            out.sort_by_key(|event| event.cursor);
            Ok(out)
        })
    }

    fn last_cursor(&self, session: &SessionId) -> BoxFuture<'_, Result<Cursor, IfaceError>> {
        let session = session.clone();
        Box::pin(async move {
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .get(&session)
                .map_or(0, |log| log.len() as Cursor))
        })
    }

    fn checkpoint(&self, _session: &SessionId) -> BoxFuture<'_, Result<String, IfaceError>> {
        Box::pin(async { Ok("cp-stitched".to_string()) })
    }

    fn verify_checkpoint(
        &self,
        _session: &SessionId,
    ) -> BoxFuture<'_, Result<Option<(Cursor, String)>, IfaceError>> {
        Box::pin(async { Ok(None) })
    }

    fn create_session(&self, session: &SessionId) -> BoxFuture<'_, Result<(), IfaceError>> {
        let session = session.clone();
        Box::pin(async move {
            self.sessions.lock().unwrap().entry(session).or_default();
            Ok(())
        })
    }

    fn contains_session(&self, session: &SessionId) -> BoxFuture<'_, Result<bool, IfaceError>> {
        let session = session.clone();
        Box::pin(async move { Ok(self.sessions.lock().unwrap().contains_key(&session)) })
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionId>, IfaceError>> {
        Box::pin(async move { Ok(self.sessions.lock().unwrap().keys().cloned().collect()) })
    }

    fn node(&self) -> Option<&NodeId> {
        None
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("expected synchronous completion"),
    }
}

#[test]
fn test_01_minthub_install_site_ledger_mount_and_tamper_fail_closed() {
    let src_components = stitched_components_dir();
    let ledger: Value =
        serde_json::from_str(&fs::read_to_string(src_components.join("install.json")).unwrap())
            .unwrap();
    assert_eq!(ledger["schema"], "agentcpm.kernel/install@1");
    let pkg_names: Vec<&str> = ledger["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(
        pkg_names
            .iter()
            .filter(|n| n.starts_with("pilotdeck-rs/"))
            .count(),
        20,
        "all 20 pilotdeck-rs MintHub packages must appear in install.json"
    );

    // 1. Tampering a MintHub-downloaded manifest without updating install.json must fail closed.
    let tampered_root = StitchedRoot::new("tamper");
    tampered_root.write_kernel_toml();
    tampered_root.copy_stitched_components(&src_components);
    let iface_manifest = tampered_root
        .components_dir()
        .join("pilotdeck-rs__iface@0.1.1")
        .join("mint.toml");
    assert!(iface_manifest.is_file());
    let mut raw = fs::read_to_string(&iface_manifest).unwrap();
    raw.push_str("\n# tampered\n");
    fs::write(&iface_manifest, raw).unwrap();

    let (prog_bad, sha_bad) = tampered_root.install_native_loop_copy();
    let assembly_bad = valid_scheduler_assembly(
        &prog_bad,
        &sha_bad,
        &tampered_root.loop_binding(),
        &tampered_root.controller_binding(),
    );
    let ready_bad = Arc::new(AtomicBool::new(false));
    let code_bad = serve_resident_composed(
        tampered_root.embed_options(Some(ready_bad.clone()), None),
        ResidentComposition::new(ExternalBackends {
            scheduler_assembly: Some(assembly_bad),
            ..Default::default()
        }),
    );
    assert_ne!(code_bad, 0, "tampered MintHub bundle must fail closed");
    assert!(!ready_bad.load(Ordering::SeqCst));
    assert!(UnixStream::connect(tampered_root.uds_path()).is_err());
    assert_clean_teardown(&tampered_root);

    // 2. Clean stitched components/install.json + compiled pilotdeck-native-loop boots cleanly.
    let clean_root = StitchedRoot::new("clean");
    clean_root.write_kernel_toml();
    clean_root.copy_stitched_components(&src_components);
    let (prog_ok, sha_ok) = clean_root.install_native_loop_copy();
    let assembly_ok = valid_scheduler_assembly(
        &prog_ok,
        &sha_ok,
        &clean_root.loop_binding(),
        &clean_root.controller_binding(),
    );
    let guard = HostGuard::spawn(&clean_root, assembly_ok, Vec::new(), None);
    {
        let mut wire = UdsWire::connect(&guard.uds_path);
        let session = wire.call("session/new", json!({}));
        assert!(session["session_id"].as_str().is_some());
        let denied = wire.expect_error("shutdown", json!({}));
        assert_eq!(denied["code"], -32013);
    }
    assert_eq!(guard.shutdown(), 0);
    assert_clean_teardown(&clean_root);
}

#[test]
fn test_02_stitched_session_migration_context_cas_compaction_and_tokenizer() {
    let dir = StitchedRoot::new("mig");
    let ownership_path = dir.path.join("ownership.json");
    let mut store =
        SessionOwnershipStore::open(&ownership_path, BackendOwner::KernelScopedResident).unwrap();
    let sid = "stitched-mig-session-1";
    let pinned = store
        .pin_session(sid, BackendOwner::KernelScopedResident)
        .unwrap();
    assert_eq!(pinned.owner, BackendOwner::KernelScopedResident);

    let big_body = "stitched package token payload ".repeat(48);
    let jsonl = format!(
        concat!(
            r#"{{"type":"turn_started","sessionId":"{sid}","turnId":"turn-1","sequence":1}}"#,
            "\n",
            r#"{{"type":"accepted_input","sessionId":"{sid}","turnId":"turn-1","sequence":2,"messages":[{{"role":"user","content":[{{"type":"text","text":"inspect files 😀"}}]}}]}}"#,
            "\n",
            r#"{{"type":"tool_call","sessionId":"{sid}","turnId":"turn-1","call":{{"id":"call-1","name":"read_file"}},"sequence":3}}"#,
            "\n",
            r#"{{"type":"assistant_message","sessionId":"{sid}","turnId":"turn-1","sequence":4,"message":{{"role":"assistant","content":[{{"type":"tool_call","id":"call-1","name":"read_file","input":{{"file_path":"a.txt"}}}}]}}}}"#,
            "\n",
            r#"{{"type":"tool_result","sessionId":"{sid}","turnId":"turn-1","result":{{"toolCallId":"call-1"}},"outcome":"completed","sequence":5}}"#,
            "\n",
            r#"{{"type":"tool_result_message","sessionId":"{sid}","turnId":"turn-1","sequence":6,"message":{{"role":"user","content":[{{"type":"tool_result","toolCallId":"call-1","content":[{{"type":"text","text":"{big_body}"}}]}}]}}}}"#,
            "\n",
            r#"{{"type":"assistant_message","sessionId":"{sid}","turnId":"turn-1","sequence":7,"message":{{"role":"assistant","content":[{{"type":"text","text":"done reading"}}]}}}}"#,
            "\n",
            r#"{{"type":"turn_result","sessionId":"{sid}","turnId":"turn-1","sequence":8,"result":{{"type":"success","usage":{{"inputTokens":12,"outputTokens":4}}}}}}"#,
            "\n"
        ),
        sid = sid,
        big_body = big_body,
    );

    let audit = audit_jsonl_transcript(jsonl.as_bytes(), &LiveSessionActivity::default()).unwrap();
    assert_eq!(audit.message_count, 4);
    assert_eq!(audit.turn_count, 1);

    let target_sid = format!("kernel-target-{sid}");
    let ledger = AuthoritativeContextLedger::new(target_sid.clone());
    let first = import_stationary_session(
        jsonl.as_bytes(),
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    assert_eq!(first.imported_revision, 1);
    assert_eq!(first.imported_messages, 4);

    let decoded = boundary_codec::decode(&ledger.snapshot().payload.bytes).unwrap();
    assert!(stringify_json(&decoded).unwrap().unwrap().contains("😀"));

    // TokenCounter from pilotdeck-context-iface must match pilotdeck-tokenizer identically.
    let counter = PilotDeckTokenCounter::new();
    let sample = "inspect files 😀";
    let expected_tokens = u64::try_from(count_tokens(&JsString::from(sample)).unwrap()).unwrap();
    assert_eq!(counter.count_text(sample), expected_tokens);

    // Advance revision and confirm re-import is strictly idempotent.
    let adv = ledger
        .commit_transition(LedgerTransition::new(
            "stitched-post-import-tx",
            1,
            ContextOperationKind::CaptureTurn.into(),
        ))
        .unwrap();
    assert_eq!(adv.revision, 2);
    let second = import_stationary_session(
        jsonl.as_bytes(),
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    assert_eq!(second, first);
    assert_eq!(ledger.snapshot().revision, 2);

    // Assemble read-only Micro + Snip projection via StoreContract without mutating authority.
    let mem_store = Arc::new(MemoryStore::default());
    let ctx_sid: SessionId = target_sid.clone();
    block_on(mem_store.create_session(&ctx_sid)).unwrap();
    block_on(mem_store.append(EventRecord::text(
        ctx_sid.clone(),
        KIND_TOOL_REGISTERED,
        "read_file|{}|9",
    )))
    .unwrap();
    let contract = PilotDeckContextContract::new(mem_store.clone())
        .with_ledger(ledger.clone())
        .with_capabilities(vec![("context.core".into(), "0.1.0".into())])
        .with_micro_options(MicroCompactionEngineOptions {
            keep_latest: Some(0.0),
            trim_to_tokens: Some(32.0),
            ..Default::default()
        })
        .with_snip_options(SnipEngineOptions {
            keep_head_turns: Some(1.0),
            keep_tail_turns: Some(1.0),
            enabled: Some(true),
            ..Default::default()
        })
        .with_snip_call_options(SnipCallOptions {
            target_tokens: Some(16.0),
            target_total_tokens: None,
        });
    let snap = block_on(contract.assemble(&ctx_sid, 1)).unwrap();
    assert_eq!(snap.content_type.as_deref(), Some(SNAPSHOT_CONTENT_TYPE));
    let doc: Value = serde_json::from_slice(&snap.bytes).unwrap();
    assert_eq!(doc["revision"], 2);
    assert_eq!(ledger.snapshot().revision, 2);

    let trace_id = block_on(contract.trace_input(
        &ctx_sid,
        1,
        &CallMeta {
            session_id: ctx_sid.clone(),
            call_id: "c-trace".into(),
            turn: 1,
        },
        b"stitched prompt",
    ))
    .unwrap();
    assert!(trace_id.starts_with("trace-"));
    assert_eq!(
        block_on(mem_store.read(&ctx_sid, 1, 10))
            .unwrap()
            .last()
            .unwrap()
            .kind,
        KIND_CONTEXT_INPUT
    );

    let compaction = PilotDeckCompactionPersistence::new(mem_store);
    let committed = block_on(compaction.commit(&ctx_sid, 1, b"{\"k\":1}", b"[1]")).unwrap();
    assert_eq!(committed.turn, 1);

    // P7 failure handler never silently falls back to LegacyRuntime.
    let err = IfaceError::new(
        ErrorCategory::Execution,
        ERR_APP,
        "simulated kernel failure",
    );
    let disp = handle_kernel_turn_failure(
        &store,
        sid,
        "turn-2",
        BackendOwner::KernelScopedResident,
        &err,
        false,
        Some(true),
    )
    .unwrap();
    assert_eq!(disp.disposition, KernelFailureDisposition::FailedNoFallback);
    assert_eq!(
        store.get(sid).unwrap().unwrap().owner,
        BackendOwner::KernelScopedResident
    );
}

#[test]
fn test_03_stitched_host_native_loop_model_and_workspace_builtin_tools_e2e() {
    let root = StitchedRoot::new("e2e");
    root.write_kernel_toml();
    root.copy_stitched_components(&stitched_components_dir());

    let ws_dir = root.path.join("workspace");
    fs::create_dir_all(&ws_dir).unwrap();
    let boundary = WorkspaceBoundary::new(&ws_dir).unwrap();
    let (tool_contract, _todo_store) =
        workspace_tool_contract(WorkspaceToolConfig::new(boundary)).unwrap();
    let tool_contract = Arc::new(tool_contract);

    // Direct stitched builtin workspace tool round-trip & escape rejection + batch partitioning.
    let mk_call = |name: &str, args: &[u8]| {
        ToolCall::new(
            CallMeta {
                session_id: "s-ws".into(),
                call_id: format!("c-{name}"),
                turn: 1,
            },
            name.into(),
            None,
            args.to_vec(),
        )
    };
    let w_res = block_on(tool_contract.invoke(mk_call(
        "write_file",
        br#"{"file_path":"src/hello.txt","content":"alpha\n"}"#,
    )))
    .unwrap();
    assert!(w_res.ok);
    let w_val = parse_json(std::str::from_utf8(&w_res.bytes).unwrap()).unwrap();
    let sha1 = String::from_utf16_lossy(w_val.get("sha256").unwrap().as_string().unwrap().units());
    let edit_args = format!(
        r#"{{"file_path":"src/hello.txt","old_string":"alpha","new_string":"stitched_ok","expected_sha256":"{sha1}"}}"#
    );
    assert!(
        block_on(tool_contract.invoke(mk_call("edit_file", edit_args.as_bytes())))
            .unwrap()
            .ok
    );
    let grep_res =
        block_on(tool_contract.invoke(mk_call("grep", br#"{"pattern":"stitched_ok"}"#))).unwrap();
    assert!(grep_res.ok);
    let read_res =
        block_on(tool_contract.invoke(mk_call("read_file", br#"{"file_path":"src/hello.txt"}"#)))
            .unwrap();
    assert!(read_res.ok);
    assert!(
        std::str::from_utf8(&read_res.bytes)
            .unwrap()
            .contains("stitched_ok")
    );
    let escape_res =
        block_on(tool_contract.invoke(mk_call("read_file", br#"{"file_path":"../outside.txt"}"#)))
            .unwrap();
    assert!(!escape_res.ok);
    assert_eq!(
        parse_json(std::str::from_utf8(&escape_res.bytes).unwrap())
            .unwrap()
            .get("code"),
        Some(&JsValue::from("path_not_allowed"))
    );

    let batch_slices = partition_tool_batch(
        &tool_contract.catalog_entries(),
        &[
            ToolCallCandidate {
                call_id: "1".into(),
                tool_name: "read_file".into(),
                read_only_call: true,
            },
            ToolCallCandidate {
                call_id: "2".into(),
                tool_name: "write_file".into(),
                read_only_call: false,
            },
        ],
    );
    assert_eq!(batch_slices.len(), 2);

    // Build ExternalToolBindings for all 6 workspace builtin tools to register in agentcpm-host.
    let specs: Vec<ToolSpec> = block_on(tool_contract.list());
    assert_eq!(specs.len(), WORKSPACE_TOOL_NAMES.len());
    let tool_bindings: Vec<ExternalToolBinding> = specs
        .into_iter()
        .map(|spec| {
            let impl_id = format!("pilotdeck.workspace.{}", spec.name);
            ExternalToolBinding {
                spec,
                contract: tool_contract.clone() as Arc<dyn ToolContract>,
                impl_id,
                version: "0.1.2".to_owned(),
            }
        })
        .collect();

    // Verify stitched pilotdeck-native-loop rejects unknown/empty args with exit 64.
    let (program, sha256) = root.install_native_loop_copy();
    let bad_arg = Command::new(&program)
        .arg(OsStr::from_bytes(b"--unknown-stitched-arg"))
        .output()
        .unwrap();
    assert_eq!(bad_arg.status.code(), Some(64));

    let http = LocalMockHttp::start();
    let origin = format!("http://{}", http.addr);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let model_setup: Arc<dyn TrustedModelSetup> = Arc::new(StitchedModelSetup::new(
        origin,
        rt.handle().clone(),
        drops.clone(),
    ));
    let loop_binding = root.loop_binding();
    let controller_binding = root.controller_binding();
    let assembly = valid_scheduler_assembly(&program, &sha256, &loop_binding, &controller_binding);
    let guard = HostGuard::spawn(&root, assembly, tool_bindings, Some(model_setup));

    {
        let mut wire = UdsWire::connect(&guard.uds_path);

        // Verify all 6 builtin workspace tools are visible on the stitched live host.
        let listed = wire.call("tool.list", json!({}));
        let tool_names: Vec<&str> = listed["data"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for expected in WORKSPACE_TOOL_NAMES {
            assert!(
                tool_names.contains(&expected),
                "tool.list missing {expected}: {tool_names:?}"
            );
        }

        let created = wire.call("session/new", json!({}));
        let sid = created["session_id"].as_str().unwrap().to_owned();
        let _ = wire.call(
            "session.lease_acquire",
            json!({"session_id": sid, "ttl_ms": 300_000}),
        );
        let opened = wire.call("execution.open", json!({"session_id": sid}));
        assert_eq!(opened["input_controller_ref"], controller_binding);

        let payload = json!({
            "kind": "request",
            "method": "execute",
            "messageId": "stitched-msg-1",
            "runId": "stitched-run-1",
            "operationId": "stitched-turn-1",
            "requestId": "stitched-req-1",
            "idempotencyKey": "stitched-idem-1",
            "sessionId": sid,
            "turnId": "stitched-turn-1",
            "payload": {
                "provider": "p",
                "model": "m",
                "systemPrompt": "Preserve stitched system prompt.",
                "maxOutputTokens": 64,
                "maxTurns": 2,
                "messages": [{
                    "role": "user",
                    "content": [{ "type": "text", "text": "Run stitched MintHub E2E." }]
                }]
            }
        });
        let accepted = wire.call(
            "execution.input",
            json!({
                "session_id": sid,
                "idempotency_key": "stitched-turn-1",
                "turn": 1,
                "input": payload.to_string(),
                "consumer_demand": true,
            }),
        );
        let controller_call = accepted["call_ref"].as_str().unwrap().to_owned();
        let controller_view = wire.wait_terminal(&sid, &controller_call);
        assert_eq!(controller_view["state"], "completed", "{controller_view}");
        let child_ref = controller_view["result"]["loop_ref"]
            .as_str()
            .expect("child loop_ref")
            .to_owned();

        let mut cursor = 0u64;
        let mut observed_events: Vec<Value> = Vec::new();
        let deadline = Instant::now() + WAIT;
        let loop_view = loop {
            let page = wire.call(
                "execution.events",
                json!({"session_id": sid, "after": cursor}),
            );
            for event in page.as_array().unwrap() {
                if let Some(next_cursor) = event["cursor"].as_u64() {
                    cursor = next_cursor;
                }
                observed_events.push(event.clone());
                if event["payload"]["content"]["kind"] == "agent_event" {
                    let _ = wire.call(
                        "execution.output_ack",
                        json!({"session_id": sid, "call_ref": controller_call, "cursor": cursor}),
                    );
                }
            }
            let view = wire.call(
                "execution.get",
                json!({"session_id": sid, "call_ref": child_ref}),
            );
            if matches!(
                view["state"].as_str(),
                Some("completed" | "failed" | "cancelled" | "unknown")
            ) {
                let tail = wire.call(
                    "execution.events",
                    json!({"session_id": sid, "after": cursor}),
                );
                for event in tail.as_array().unwrap() {
                    if let Some(next_cursor) = event["cursor"].as_u64() {
                        cursor = next_cursor;
                    }
                    observed_events.push(event.clone());
                    if event["payload"]["content"]["kind"] == "agent_event" {
                        let _ = wire.call(
                            "execution.output_ack",
                            json!({"session_id": sid, "call_ref": controller_call, "cursor": cursor}),
                        );
                    }
                }
                break view;
            }
            assert!(Instant::now() < deadline, "loop timed out: {view}");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            loop_view["state"],
            "completed",
            "loop_view={loop_view}; observed_events={}; http_requests={:?}",
            serde_json::to_string(&observed_events).unwrap_or_default(),
            http.requests.lock().unwrap()
        );
        let events_text = serde_json::to_string(&observed_events).unwrap();
        assert!(
            events_text.contains("Stitched MintHub ") && events_text.contains("packages OK."),
            "unexpected events: {events_text}"
        );
    }

    assert_eq!(guard.shutdown(), 0);
    let requests = http.shutdown();
    assert_eq!(requests.len(), 1);
    assert!(drops.load(Ordering::SeqCst) >= 1);
    assert_clean_teardown(&root);
}
