//! P2.atomic-install verification suite exercising real `agentcpm-host` startup,
//! rollback, tool staging conflicts/idempotency, and end-to-end execution using
//! the compiled `pilotdeck-native-loop` binary over UDS.
#![cfg(unix)]
#![forbid(unsafe_code)]

#[allow(dead_code)]
#[path = "managed_model/setup.rs"]
mod setup;
#[allow(dead_code)]
#[path = "managed_model/trace.rs"]
mod trace;

use agentcpm_host::{
    EmbedOptions, EmbedTransport, ExternalBackends, ExternalToolBinding, ResidentComposition,
    ResidentShutdown, ResidentShutdownRequest, SchedulerAssembly, SchedulerComponent,
    SchedulerImplementation, StartupOptions, TrustedModelSetup, TrustedToolBatchBackend,
    TrustedToolBatchExecution, TrustedToolBatchInstallation, TrustedToolBatchSetup,
    TrustedToolFinalCandidate, TrustedToolFinalResolution, TrustedToolLifecycle,
    TrustedToolPreparation, TrustedUdsIdentityMapping, serve_resident_composed,
};
use agentcpm_iface::{
    model_session::MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
    tool::{ToolContract, ToolResult, ToolSpec, ToolStaging},
    types::{ERR_APP, Error as IfaceError, ErrorCategory},
};
use agentcpm_scheduler_protocol::{self as p, context_history::OpaquePayload};
use pilotdeck_protocol::parse_json;
use pilotdeck_tool_core::{ToolCatalogEntry, ToolConcurrencyPolicy};
use pilotdeck_tool_iface::{CatalogToolContract, CatalogToolFn, VerifiedToolContext};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::any::Any;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WAIT: Duration = Duration::from_secs(30);
const REAP_WAIT: Duration = Duration::from_secs(3);

static ROOT_SEQ: AtomicU64 = AtomicU64::new(1);

struct TestRoot {
    path: PathBuf,
    owner_uid: u32,
    nonce: String,
}

impl TestRoot {
    fn new(tag: &str) -> Self {
        let seq = ROOT_SEQ.fetch_add(1, Ordering::SeqCst);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000_000;
        // Keep UDS path well under Unix sun_path limit (104/108 bytes).
        let base = if std::env::temp_dir().as_os_str().len() <= 24 {
            std::env::temp_dir()
        } else {
            PathBuf::from("/tmp")
        };
        let nonce = format!("pdai-{}-{seq}-{stamp}", std::process::id());
        let path = base.join(format!("pd_ai_{}_{seq}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("data")).unwrap();
        std::fs::create_dir_all(path.join("components")).unwrap();
        let owner_uid = std::fs::metadata(&path).unwrap().uid();
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
        std::fs::write(
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

    fn write_valid_install_json(&self) {
        std::fs::create_dir_all(self.components_dir().join("fixture-iface")).unwrap();
        let ledger = self.components_dir().join("install.json");
        std::fs::write(
            &ledger,
            json!({
                "schema": "agentcpm.kernel/install@1",
                "packages": [{
                    "name": "fixture-iface",
                    "version": "0.0.0",
                    "kind": "iface",
                    "dir": "fixture-iface",
                    "provides": { "interfaces": ["env"] },
                    "sha256": {}
                }]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn install_native_loop_copy(&self) -> (PathBuf, String) {
        let source = native_loop_bin();
        let program = self.path.join("pilotdeck-native-loop");
        std::fs::copy(&source, &program).expect("copy compiled pilotdeck-native-loop");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
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
            .expect("verify pilotdeck-native-loop --version");
        assert!(
            version.status.success(),
            "pilotdeck-native-loop --version failed: stderr={}",
            String::from_utf8_lossy(&version.stderr)
        );
        let expected_prefix = format!("pilotdeck-native-loop {}", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            String::from_utf8_lossy(&version.stdout).trim(),
            expected_prefix,
            "unexpected pilotdeck-native-loop --version output"
        );
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
            TrustedUdsIdentityMapping::new(self.owner_uid, "pilotdeck-app", "tenant-atomic")
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

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn native_loop_bin() -> PathBuf {
    let bin = std::env::var_os("PILOTDECK_IFACE_NATIVE_LOOP_BIN")
        .map(PathBuf::from)
        .or_else(|| option_env!("CARGO_BIN_EXE_pilotdeck-native-loop").map(PathBuf::from))
        .expect(
            "missing compiled pilotdeck-native-loop path (neither PILOTDECK_IFACE_NATIVE_LOOP_BIN nor CARGO_BIN_EXE_pilotdeck-native-loop is set)",
        );
    assert!(
        bin.is_file(),
        "compiled pilotdeck-native-loop does not exist at {}",
        bin.display()
    );
    bin
}

fn sha256_file(path: &Path) -> String {
    let bytes = std::fs::read(path).expect("read artifact for sha256");
    format!("{:x}", Sha256::digest(&bytes))
}

fn flip_first_hex_char(hex: &str) -> String {
    let mut chars = hex.chars();
    let first = chars.next().expect("non-empty sha256");
    let replacement = if first == '0' { '1' } else { '0' };
    let mut flipped = String::with_capacity(hex.len());
    flipped.push(replacement);
    flipped.extend(chars);
    flipped
}

fn collect_sock_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
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
        .expect("run ps to inspect leaked child processes");
    assert!(output.status.success(), "ps exited unexpectedly");
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

fn assert_clean_teardown(root: &TestRoot) {
    let socks = collect_sock_files(&root.path);
    assert!(
        socks.is_empty(),
        "leaked Unix socket files under {}: {:?}",
        root.path.display(),
        socks
    );
    let deadline = Instant::now() + REAP_WAIT;
    loop {
        let leaked = find_live_processes(&root.nonce);
        if leaked.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "leaked child processes matching {}: {:?}",
            root.nonce,
            leaked
        );
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
                    revision: "atomic-loop-v1".into(),
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
                    revision: "atomic-ctrl-v1".into(),
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
                "features": ["model-session/1.0", MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1],
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
        root: &TestRoot,
        assembly: SchedulerAssembly,
        tools: Vec<ExternalToolBinding>,
        managed_model: Option<Arc<dyn TrustedModelSetup>>,
        contexts: Vec<p::context_operations::ContextBinding>,
    ) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let options = root.embed_options(None, Some(ready_tx));
        let mut composition = ResidentComposition::new(ExternalBackends {
            scheduler_assembly: Some(assembly),
            tools,
            ..Default::default()
        })
        .with_context_bindings(contexts)
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
            ResidentShutdownRequest::Requested,
            "resident host must accept shutdown control request"
        );
        self.thread
            .take()
            .unwrap()
            .join()
            .expect("resident host thread panicked")
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

struct LocalMockHttp {
    addr: SocketAddr,
    requests: Arc<MutexVec>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

type MutexVec = std::sync::Mutex<Vec<Value>>;

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
                    Err(_) => {
                        // WouldBlock (empty accept queue) is the steady state, and
                        // transient errors must be retried too: under parallel load a
                        // queued client connection can be reset before accept
                        // (ECONNABORTED) or fd pressure can surface EMFILE. Panicking
                        // here would drop the listener and stall the scoped native
                        // loop until the test deadline. Same hardening as the default
                        // e2e mock in 3123ee09e.
                        std::thread::sleep(Duration::from_millis(5));
                    }
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

/// I/O failures on an individual connection (`try_clone` / header reads / body
/// read / response write) drop that request instead of panicking:
/// `handle_mock_http` runs inline in the accept thread, so a panic would tear
/// down the whole listener and stall every later model request.
fn handle_mock_http(mut stream: TcpStream, requests: &MutexVec) {
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let Some(length) = length else {
        return;
    };
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    requests.lock().unwrap().push(json!({
        "request_line": first.trim_end(),
        "body": serde_json::from_slice::<Value>(&body).unwrap(),
    }));
    let frames = [
        json!({"id":"atomic-resp","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"Atomic "},"finish_reason":null}]}),
        json!({"id":"atomic-resp","model":"m","choices":[{"index":0,"delta":{"content":"install OK."},"finish_reason":null}]}),
        json!({"id":"atomic-resp","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"id":"atomic-resp","model":"m","choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}}),
    ];
    let sse = format!(
        "{}data: [DONE]\n\n",
        frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>()
    );
    // A failed response write on an already-aborted connection must not panic
    // the inline accept thread; the dropped request surfaces through the
    // test's own assertions on `requests`.
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        sse.len(),
        sse
    )
    .and_then(|()| stream.flush());
}

fn echo_tool_catalog(tool_name: &str, alias: Option<&str>) -> Arc<CatalogToolContract> {
    let catalog = Arc::new(CatalogToolContract::new());
    catalog
        .register_tool(
            ToolCatalogEntry {
                name: tool_name.to_owned(),
                aliases: alias.map(|a| vec![a.to_owned()]).unwrap_or_default(),
                description: format!("Atomic install test tool {tool_name}"),
                kind: "custom".to_owned(),
                input_schema: parse_json(
                    r#"{"type":"object","properties":{"text":{"type":"string"}}}"#,
                )
                .unwrap(),
                concurrency: ToolConcurrencyPolicy::ConcurrentReadOnly,
            },
            Arc::new(CatalogToolFn::new(|_ctx: VerifiedToolContext| {
                Box::pin(async move {
                    Ok(ToolResult {
                        ok: true,
                        bytes: br#"{"echo":"ok"}"#.to_vec(),
                        unknown: false,
                    })
                })
            })),
        )
        .expect("register echo test tool");
    catalog
}

fn external_tool_binding(
    tool_name: &str,
    description: &str,
    contract: Arc<dyn ToolContract>,
) -> ExternalToolBinding {
    ExternalToolBinding {
        spec: ToolSpec {
            name: tool_name.to_owned(),
            description: description.to_owned(),
            parameters: br#"{"type":"object","properties":{"text":{"type":"string"}}}"#.to_vec(),
        },
        contract,
        impl_id: format!("impl.{tool_name}"),
        version: "0.1.1".to_owned(),
    }
}

#[test]
fn test_a_sha256_mismatch_fails_closed_before_ready_and_recovers_on_fresh_root() {
    // Subcase A1: Mismatched SHA-256 digest in components/install.json fails before ready.
    let root_a1 = TestRoot::new("a1");
    root_a1.write_kernel_toml();
    let (program_a1, actual_sha_a1) = root_a1.install_native_loop_copy();
    let pkg_dir = root_a1.components_dir().join("bad-native").join("bin");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    let pkg_bin = pkg_dir.join("loop-server");
    std::fs::copy(&program_a1, &pkg_bin).unwrap();
    let bad_ledger = root_a1.components_dir().join("install.json");
    let mismatched_pkg_sha = flip_first_hex_char(&actual_sha_a1);
    std::fs::write(
        &bad_ledger,
        json!({
            "schema": "agentcpm.kernel/install@1",
            "packages": [{
                "name": "bad-native",
                "version": "0.1.0",
                "kind": "plugin",
                "subkind": "native",
                "dir": "bad-native",
                "provides": { "loops": [root_a1.loop_binding()] },
                "artifacts": { "bin": "bin/loop-server" },
                "sha256": { "bin/loop-server": mismatched_pkg_sha },
                "child_kind": "loop"
            }]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&bad_ledger, std::fs::Permissions::from_mode(0o644)).unwrap();

    let ready_a1 = Arc::new(AtomicBool::new(false));
    let assembly_a1 = valid_scheduler_assembly(
        &program_a1,
        &actual_sha_a1,
        &root_a1.loop_binding(),
        &root_a1.controller_binding(),
    );
    let code_a1 = serve_resident_composed(
        root_a1.embed_options(Some(ready_a1.clone()), None),
        ResidentComposition::new(ExternalBackends {
            scheduler_assembly: Some(assembly_a1),
            ..Default::default()
        }),
    );
    assert_ne!(
        code_a1, 0,
        "install.json sha256 mismatch must exit non-zero"
    );
    assert!(
        !ready_a1.load(Ordering::SeqCst),
        "host must fail closed before calling on_ready when install.json sha256 mismatches"
    );
    assert!(UnixStream::connect(root_a1.uds_path()).is_err());
    assert_clean_teardown(&root_a1);

    // Subcase A2: Mismatched SHA-256 digest in SchedulerComponent fails before ready.
    let root_a2 = TestRoot::new("a2");
    root_a2.write_kernel_toml();
    root_a2.write_valid_install_json();
    let (program_a2, actual_sha_a2) = root_a2.install_native_loop_copy();
    let mismatched_component_sha = flip_first_hex_char(&actual_sha_a2);
    let ready_a2 = Arc::new(AtomicBool::new(false));
    let assembly_a2 = valid_scheduler_assembly(
        &program_a2,
        &mismatched_component_sha,
        &root_a2.loop_binding(),
        &root_a2.controller_binding(),
    );
    let code_a2 = serve_resident_composed(
        root_a2.embed_options(Some(ready_a2.clone()), None),
        ResidentComposition::new(ExternalBackends {
            scheduler_assembly: Some(assembly_a2),
            ..Default::default()
        }),
    );
    assert_ne!(
        code_a2, 0,
        "SchedulerComponent sha256 mismatch must fail closed with non-zero exit"
    );
    assert!(
        !ready_a2.load(Ordering::SeqCst),
        "host must fail closed before accepting connections on SchedulerComponent sha256 mismatch"
    );
    assert!(UnixStream::connect(root_a2.uds_path()).is_err());
    assert_clean_teardown(&root_a2);

    // Subcase A3: Subsequent valid startup on a fresh temp root succeeds cleanly.
    let root_a3 = TestRoot::new("a3");
    root_a3.write_kernel_toml();
    root_a3.write_valid_install_json();
    let (program_a3, actual_sha_a3) = root_a3.install_native_loop_copy();
    let assembly_a3 = valid_scheduler_assembly(
        &program_a3,
        &actual_sha_a3,
        &root_a3.loop_binding(),
        &root_a3.controller_binding(),
    );
    let guard = HostGuard::spawn(&root_a3, assembly_a3, Vec::new(), None, Vec::new());
    {
        let mut wire = UdsWire::connect(&guard.uds_path);
        let init = wire.call(
            "initialize",
            json!({
                "abi_version": 2,
                "api_version": agentcpm_iface::API_VERSION,
                "features": [],
            }),
        );
        assert_eq!(
            init["server_capabilities"]["scheduler"]["default_loop_ref"],
            root_a3.loop_binding()
        );
        assert_eq!(
            init["server_capabilities"]["scheduler"]["input_controller_ref"],
            root_a3.controller_binding()
        );
        let denied_shutdown = wire.expect_error("shutdown", json!({}));
        assert_eq!(
            denied_shutdown["code"], -32013,
            "non-admin principal calling UDS shutdown must be denied: {denied_shutdown}"
        );
    }
    let exit_a3 = guard.shutdown();
    assert_eq!(exit_a3, 0, "valid recovery startup must shut down cleanly");
    assert_clean_teardown(&root_a3);
}

#[test]
fn test_b_invalid_or_unmounted_binding_fails_closed_atomically() {
    // Subcase B1: Duplicate SchedulerComponent binding fails atomically before ready.
    let root_b1 = TestRoot::new("b1");
    root_b1.write_kernel_toml();
    root_b1.write_valid_install_json();
    let (program_b1, sha_b1) = root_b1.install_native_loop_copy();
    let mut assembly_b1 = valid_scheduler_assembly(
        &program_b1,
        &sha_b1,
        &root_b1.loop_binding(),
        &root_b1.controller_binding(),
    );
    assembly_b1.bindings.push(assembly_b1.bindings[0].clone());
    let ready_b1 = Arc::new(AtomicBool::new(false));
    let code_b1 = serve_resident_composed(
        root_b1.embed_options(Some(ready_b1.clone()), None),
        ResidentComposition::new(ExternalBackends {
            scheduler_assembly: Some(assembly_b1),
            ..Default::default()
        }),
    );
    assert_ne!(code_b1, 0, "duplicate scheduler binding must fail closed");
    assert!(!ready_b1.load(Ordering::SeqCst));
    assert_clean_teardown(&root_b1);

    // Subcase B2: Unmounted ContextBinding reference fails atomically before ready.
    let root_b2 = TestRoot::new("b2");
    root_b2.write_kernel_toml();
    root_b2.write_valid_install_json();
    let (program_b2, sha_b2) = root_b2.install_native_loop_copy();
    let assembly_b2 = valid_scheduler_assembly(
        &program_b2,
        &sha_b2,
        &root_b2.loop_binding(),
        &root_b2.controller_binding(),
    );
    let ready_b2 = Arc::new(AtomicBool::new(false));
    let unmounted_context = p::context_operations::ContextBinding {
        binding_ref: format!("unmounted-ctx-{}", root_b2.nonce),
        operation: p::context_operations::Operation::RecoverFromModelError,
    };
    let code_b2 = serve_resident_composed(
        root_b2.embed_options(Some(ready_b2.clone()), None),
        ResidentComposition::new(ExternalBackends {
            scheduler_assembly: Some(assembly_b2),
            ..Default::default()
        })
        .with_context_bindings(vec![unmounted_context]),
    );
    assert_ne!(
        code_b2, 0,
        "unmounted ContextBinding reference must fail startup atomically"
    );
    assert!(!ready_b2.load(Ordering::SeqCst));
    assert_clean_teardown(&root_b2);

    // Subcase B3: SessionConfig profile pointing to unmounted controller/loop refusions fail closed.
    let root_b3 = TestRoot::new("b3");
    root_b3.write_kernel_toml();
    root_b3.write_valid_install_json();
    let (program_b3, sha_b3) = root_b3.install_native_loop_copy();
    let mut assembly_b3 = valid_scheduler_assembly(
        &program_b3,
        &sha_b3,
        &root_b3.loop_binding(),
        &root_b3.controller_binding(),
    );
    let unmounted_ctrl = format!("unmounted-ctrl-{}", root_b3.nonce);
    let unmounted_loop = format!("unmounted-loop-{}", root_b3.nonce);
    assembly_b3.profile = p::SessionConfig {
        input_controller_ref: Some(unmounted_ctrl.clone()),
        default_loop_ref: unmounted_loop.clone(),
        allowed_bindings: [unmounted_ctrl, unmounted_loop].into(),
    };
    let guard_b3 = HostGuard::spawn(&root_b3, assembly_b3, Vec::new(), None, Vec::new());
    {
        let mut wire = UdsWire::connect(&guard_b3.uds_path);
        let created = wire.call("session/new", json!({}));
        let sid = created["session_id"]
            .as_str()
            .expect("session_id")
            .to_owned();
        let _ = wire.call(
            "session.lease_acquire",
            json!({"session_id": sid, "ttl_ms": 300_000}),
        );
        let err = wire.expect_error("execution.open", json!({"session_id": sid}));
        assert!(
            err.get("code").is_some(),
            "execution.open with unmounted profile bindings must return JSON-RPC error: {err}"
        );
    }
    let exit_b3 = guard_b3.shutdown();
    assert_eq!(exit_b3, 0);
    assert_clean_teardown(&root_b3);
}

#[test]
fn test_c_conflicting_tool_specs_and_staging_idempotency_with_native_loop() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Part C1: CatalogToolContract + ToolStaging duplicate/conflict/idempotency guarantees.
    let catalog = echo_tool_catalog("atomic.echo", Some("atomic.echo.alias"));
    assert!(catalog.contains("atomic.echo"));
    assert!(catalog.contains("atomic.echo.alias"));

    let dup_err = catalog
        .register_tool(
            ToolCatalogEntry {
                name: "atomic.echo".to_owned(),
                aliases: vec![],
                description: "Conflicting duplicate tool".to_owned(),
                kind: "custom".to_owned(),
                input_schema: parse_json(r#"{"type":"object"}"#).unwrap(),
                concurrency: ToolConcurrencyPolicy::SerialSideEffect,
            },
            Arc::new(CatalogToolFn::new(|_: VerifiedToolContext| {
                Box::pin(async move {
                    Ok(ToolResult {
                        ok: true,
                        bytes: Vec::new(),
                        unknown: false,
                    })
                })
            })),
        )
        .unwrap_err();
    assert_eq!(dup_err.category, ErrorCategory::Execution);
    assert_eq!(dup_err.code, ERR_APP);

    let alias_conflict = catalog
        .register_tool(
            ToolCatalogEntry {
                name: "atomic.echo.alias".to_owned(),
                aliases: vec![],
                description: "Collides with existing alias".to_owned(),
                kind: "custom".to_owned(),
                input_schema: parse_json(r#"{"type":"object"}"#).unwrap(),
                concurrency: ToolConcurrencyPolicy::ConcurrentReadOnly,
            },
            Arc::new(CatalogToolFn::new(|_: VerifiedToolContext| {
                Box::pin(async move {
                    Ok(ToolResult {
                        ok: true,
                        bytes: Vec::new(),
                        unknown: false,
                    })
                })
            })),
        )
        .unwrap_err();
    assert_eq!(alias_conflict.category, ErrorCategory::Execution);

    // Same spec on ToolContract::register is idempotent; conflicting spec is rejected.
    let original_spec = rt.block_on(catalog.list()).pop().unwrap();
    rt.block_on(catalog.register(original_spec.clone()))
        .expect("identical ToolSpec registration must be idempotent");
    let conflicting_spec = ToolSpec {
        name: original_spec.name.clone(),
        description: "Conflicting description override".to_owned(),
        parameters: br#"{"type":"object","properties":{"other":{"type":"number"}}}"#.to_vec(),
    };
    assert!(
        rt.block_on(catalog.register(conflicting_spec)).is_err(),
        "conflicting ToolSpec registration must fail closed"
    );

    // ToolStaging stub lifecycle & conflict checks.
    assert!(catalog.register_stub("atomic.echo").is_err());
    assert!(catalog.register_stub("atomic.echo.alias").is_err());
    catalog
        .register_stub("atomic.staged")
        .expect("stage new stub");
    assert!(catalog.contains("atomic.staged"));
    assert!(catalog.register_stub("atomic.staged").is_err());
    assert!(
        catalog.remove_stub("atomic.echo").is_err(),
        "must refuse removing executable tool as stub"
    );
    catalog
        .remove_stub("atomic.staged")
        .expect("remove staged stub");
    assert!(!catalog.contains("atomic.staged"));
    catalog
        .remove_stub("atomic.never_existed")
        .expect("removing absent stub must be idempotent");

    // Part C2: Host composition rejects duplicate ExternalToolBinding atomically.
    let root_c1 = TestRoot::new("c1");
    root_c1.write_kernel_toml();
    root_c1.write_valid_install_json();
    let (program_c1, sha_c1) = root_c1.install_native_loop_copy();
    let assembly_c1 = valid_scheduler_assembly(
        &program_c1,
        &sha_c1,
        &root_c1.loop_binding(),
        &root_c1.controller_binding(),
    );
    let ready_c1 = Arc::new(AtomicBool::new(false));
    let binding_1 = external_tool_binding("atomic.echo", "First atomic tool", catalog.clone());
    let binding_2 = external_tool_binding("atomic.echo", "Conflicting duplicate", catalog.clone());
    let code_c1 = serve_resident_composed(
        root_c1.embed_options(Some(ready_c1.clone()), None),
        ResidentComposition::new(ExternalBackends {
            scheduler_assembly: Some(assembly_c1),
            tools: vec![binding_1, binding_2],
            ..Default::default()
        }),
    );
    assert_ne!(
        code_c1, 0,
        "conflicting duplicate ExternalToolBinding must fail startup atomically"
    );
    assert!(!ready_c1.load(Ordering::SeqCst));
    assert_clean_teardown(&root_c1);

    // Part C3: Clean startup with unique ExternalToolBinding + compiled pilotdeck-native-loop.
    let root_c2 = TestRoot::new("c2");
    root_c2.write_kernel_toml();
    root_c2.write_valid_install_json();
    let (program_c2, sha_c2) = root_c2.install_native_loop_copy();
    let assembly_c2 = valid_scheduler_assembly(
        &program_c2,
        &sha_c2,
        &root_c2.loop_binding(),
        &root_c2.controller_binding(),
    );
    let valid_binding = external_tool_binding("atomic.echo", "First atomic tool", catalog);
    let guard_c2 = HostGuard::spawn(&root_c2, assembly_c2, vec![valid_binding], None, Vec::new());
    {
        let mut wire = UdsWire::connect(&guard_c2.uds_path);
        let listed = wire.call("tool.list", json!({}));
        let tools = listed["data"]["tools"]
            .as_array()
            .expect("tool.list data.tools array");
        assert!(
            tools.iter().any(|entry| entry["name"] == "atomic.echo"),
            "registered ExternalToolBinding must appear in tool.list: {listed}"
        );
    }
    let exit_c2 = guard_c2.shutdown();
    assert_eq!(exit_c2, 0);
    assert_clean_teardown(&root_c2);
}

#[test]
fn test_d_compiled_native_loop_binary_end_to_end_startup_and_shutdown() {
    let bin = native_loop_bin();

    // Part D1: Fail-closed CLI exit 64 checks on compiled pilotdeck-native-loop binary.
    let no_args = Command::new(&bin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(no_args.status.code(), Some(64));

    let unknown_field = Command::new(&bin)
        .args([
            "--pilotdeck-scoped-native-loop-v1",
            r#"{"profile":"pilotdeck.scoped-native-loop.v1","loop_binding":"l","controller_binding":"c","unexpected":1}"#,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(unknown_field.status.code(), Some(64));

    let bad_profile = Command::new(&bin)
        .args([
            "--pilotdeck-product-controller-v1",
            r#"{"profile":"wrong.profile","loop_binding":"l","controller_binding":"c"}"#,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(bad_profile.status.code(), Some(64));

    let non_utf8 = Command::new(&bin)
        .arg(OsStr::from_bytes(b"--\xff\xfe"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(non_utf8.status.code(), Some(64));

    // Part D2: Full end-to-end startup, turn execution, and clean shutdown with compiled binary.
    let http = LocalMockHttp::start();
    let root = TestRoot::new("d");
    root.write_kernel_toml();
    root.write_valid_install_json();
    let (program, sha256) = root.install_native_loop_copy();
    let loop_binding = root.loop_binding();
    let controller_binding = root.controller_binding();
    let assembly = valid_scheduler_assembly(&program, &sha256, &loop_binding, &controller_binding);

    let origin = format!("http://{}", http.addr);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let managed_setup: Arc<dyn TrustedModelSetup> = Arc::new(setup::TestSetup::new(
        origin,
        rt.handle().clone(),
        Arc::new(AtomicUsize::new(0)),
        trace::Trace::new(root.path.join("pull.jsonl")),
        false,
    ));
    let catalog = echo_tool_catalog("atomic.e2e_probe", None);
    let tool_binding = external_tool_binding("atomic.e2e_probe", "Atomic E2E probe", catalog);

    let guard = HostGuard::spawn(
        &root,
        assembly,
        vec![tool_binding],
        Some(managed_setup),
        Vec::new(),
    );
    {
        let mut wire = UdsWire::connect(&guard.uds_path);
        let created = wire.call("session/new", json!({}));
        let sid = created["session_id"]
            .as_str()
            .expect("session_id")
            .to_owned();
        let lease = wire.call(
            "session.lease_acquire",
            json!({"session_id": sid, "ttl_ms": 300_000}),
        );
        assert!(lease["data"]["epoch"].as_u64().is_some());

        let opened = wire.call("execution.open", json!({"session_id": sid}));
        assert_eq!(opened["protocol"], p::PROTOCOL);
        assert_eq!(opened["input_controller_ref"], controller_binding);

        let payload = json!({
            "kind": "request",
            "method": "execute",
            "messageId": "atomic-msg-1",
            "runId": "atomic-run-1",
            "operationId": "atomic-turn-1",
            "requestId": "atomic-req-1",
            "idempotencyKey": "atomic-idem-1",
            "sessionId": sid,
            "turnId": "atomic-turn-1",
            "payload": {
                "provider": "p",
                "model": "m",
                "systemPrompt": "Preserve atomic install test instruction.",
                "maxOutputTokens": 64,
                "maxTurns": 2,
                "messages": [{
                    "role": "user",
                    "content": [{ "type": "text", "text": "Run atomic install E2E." }]
                }]
            }
        });
        let accepted = wire.call(
            "execution.input",
            json!({
                "session_id": sid,
                "idempotency_key": "atomic-turn-1",
                "turn": 1,
                "input": payload.to_string(),
                "consumer_demand": true,
            }),
        );
        let controller_call = accepted["call_ref"].as_str().expect("controller call_ref");
        let controller_view = wire.wait_terminal(&sid, controller_call);
        assert_eq!(
            controller_view["state"], "completed",
            "controller call failed: {controller_view}"
        );
        let child_ref = controller_view["result"]["loop_ref"]
            .as_str()
            .unwrap_or_else(|| {
                panic!("controller spawned child loop ref missing: {controller_view}")
            });

        let mut cursor = 0u64;
        let mut observed_events: Vec<Value> = Vec::new();
        let deadline = Instant::now() + WAIT;
        let loop_view = loop {
            let page = wire.call(
                "execution.events",
                json!({"session_id": sid, "after": cursor}),
            );
            for event in page.as_array().expect("execution.events array") {
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
                for event in tail.as_array().expect("execution.events array") {
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
            assert!(
                Instant::now() < deadline,
                "scoped native loop timed out: {view}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            loop_view["state"], "completed",
            "scoped native loop call failed: {loop_view}"
        );
        let events_text = serde_json::to_string(&observed_events).unwrap();
        assert!(
            events_text.contains("Atomic ") && events_text.contains("install OK."),
            "expected streamed model response in execution.events: {events_text}"
        );
    }

    let exit_code = guard.shutdown();
    assert_eq!(exit_code, 0, "resident host must exit 0 on clean shutdown");
    let observed_requests = http.shutdown();
    assert_eq!(
        observed_requests.len(),
        1,
        "expected exactly one mock HTTP model request, got {observed_requests:?}"
    );
    assert_clean_teardown(&root);
}
