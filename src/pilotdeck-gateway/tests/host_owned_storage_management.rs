//! Public LocalGateway storage-management contracts, characterized by the five
//! fixed-cc7 SDK tests in agents-e2e.spec.ts:6582,6864,7154,7391,7447.
//! Host-owned contents are observed independently of the gateway's projections.
use pilotdeck_gateway::{
    extensions::{NativeExtension, NativeExtensions},
    host::{HostError, modules::capability::ToolDefinition},
    native_process::{LocalGateway, LocalOptions},
    runtime::{
        context::tool_budget::{ToolResultBudget, ToolResultBudgetOptions},
        tools::{ExecutableTool, ToolFuture, ToolOutput, ToolRunContext},
    },
    storage::{
        AsyncTranscriptAdapter, JsonlTranscriptStore, PrepareTranscriptReplacement, ProjectKey,
        SessionStorage, SessionStorageInput, StoreResult, StoredSession, TranscriptBlobStore,
        TranscriptKey, TranscriptRead, TranscriptReplacementAction, TranscriptStore,
        sanitize_session_id,
    },
};
use pilotdeck_protocol::{JsString, JsValue, parse_json, stringify_json};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

fn js(value: Value) -> JsValue {
    parse_json(&value.to_string()).unwrap()
}
fn value(input: &JsValue) -> Value {
    serde_json::from_str(&stringify_json(input).unwrap().unwrap()).unwrap()
}
fn text(input: &JsValue, key: &str) -> String {
    input
        .get(key)
        .unwrap()
        .as_string()
        .unwrap()
        .to_utf8()
        .unwrap()
}

type BlobContents = BTreeMap<(PathBuf, String), (TranscriptKey, Vec<u8>)>;

#[derive(Default)]
struct Blobs(Mutex<BlobContents>, Mutex<Vec<(TranscriptKey, String)>>);
impl TranscriptBlobStore for Blobs {
    fn write(&self, key: &TranscriptKey, name: &str, bytes: &[u8]) -> StoreResult<()> {
        let mut blobs = self.0.lock().unwrap();
        let index = (key.transcript_path.clone(), name.into());
        if let Some((owner, _)) = blobs.get(&index) {
            assert_eq!(owner, key, "blob write full owner key");
        }
        blobs.insert(index, (key.clone(), bytes.to_vec()));
        self.1.lock().unwrap().push((key.clone(), name.to_owned()));
        Ok(())
    }
    fn read(&self, key: &TranscriptKey, name: &str) -> StoreResult<Option<Vec<u8>>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(&(key.transcript_path.clone(), name.into()))
            .map(|(owner, bytes)| {
                assert_eq!(owner, key, "blob read full owner key");
                bytes.clone()
            }))
    }
    fn delete(&self, key: &TranscriptKey, name: &str) -> StoreResult<()> {
        let mut blobs = self.0.lock().unwrap();
        let index = (key.transcript_path.clone(), name.into());
        if let Some((owner, _)) = blobs.get(&index) {
            assert_eq!(owner, key, "blob delete full owner key");
        }
        blobs.remove(&index);
        Ok(())
    }
    fn delete_all(&self, key: &TranscriptKey) -> StoreResult<()> {
        self.0.lock().unwrap().retain(|(path, _), (owner, _)| {
            if path == &key.transcript_path {
                assert_eq!(owner, key, "blob deleteAll full owner key");
                false
            } else {
                true
            }
        });
        Ok(())
    }
}

struct Pending {
    key: TranscriptKey,
    turn: JsString,
    original: Vec<JsValue>,
}
#[derive(Default)]
struct Contents {
    records: BTreeMap<PathBuf, (TranscriptKey, Vec<JsValue>)>,
    pending: BTreeMap<String, Pending>,
    recoveries: Vec<PathBuf>,
    finalizations: Vec<TranscriptReplacementAction>,
    recursive_deletes: Vec<PathBuf>,
}
/// A consumer-supplied persistence implementation, not a gateway management
/// substitute. It stores opaque entries and implements the optional host hooks.
#[derive(Default)]
struct HostStore {
    state: Mutex<Contents>,
    disk: Option<JsonlTranscriptStore>,
    transactions: bool,
    recovery_enabled: AtomicBool,
    reads: AtomicUsize,
    read_counts: Mutex<BTreeMap<PathBuf, usize>>,
    second_scan_failure: Mutex<Option<PathBuf>>,
    replaces: Mutex<Vec<TranscriptKey>>,
    artifacts: Arc<Blobs>,
    backups: Arc<Blobs>,
}
impl HostStore {
    fn with_disk() -> Self {
        Self {
            disk: Some(JsonlTranscriptStore::new()),
            ..Self::default()
        }
    }
    fn entries(&self, path: &Path) -> Vec<JsValue> {
        self.state
            .lock()
            .unwrap()
            .records
            .get(path)
            .map(|(_, entries)| entries.clone())
            .unwrap_or_default()
    }
    fn assert_key(&self, key: &TranscriptKey) {
        if let Some((owner, _)) = self.state.lock().unwrap().records.get(&key.transcript_path) {
            assert_eq!(owner, key, "transcript full host owner key");
        }
    }
    fn replace_contents(&self, key: &TranscriptKey, entries: &[JsValue]) -> StoreResult<()> {
        self.assert_key(key);
        self.replaces.lock().unwrap().push(key.clone());
        if let Some(disk) = &self.disk {
            disk.replace(key, entries).unwrap()?;
        }
        self.state
            .lock()
            .unwrap()
            .records
            .insert(key.transcript_path.clone(), (key.clone(), entries.to_vec()));
        Ok(())
    }
    fn pending_count(&self) -> usize {
        self.state.lock().unwrap().pending.len()
    }
}
impl TranscriptStore for HostStore {
    fn append(&self, key: &TranscriptKey, entry: &JsValue) -> StoreResult<()> {
        self.assert_key(key);
        if let Some(disk) = &self.disk {
            disk.append(key, entry)?;
        }
        self.state
            .lock()
            .unwrap()
            .records
            .entry(key.transcript_path.clone())
            .or_insert_with(|| (key.clone(), Vec::new()))
            .1
            .push(entry.clone());
        Ok(())
    }
    fn read(&self, key: &TranscriptKey, limit: Option<u64>) -> StoreResult<TranscriptRead> {
        self.assert_key(key);
        self.reads.fetch_add(1, Ordering::SeqCst);
        let count = {
            let mut reads = self.read_counts.lock().unwrap();
            let count = reads.entry(key.transcript_path.clone()).or_default();
            *count += 1;
            *count
        };
        if count == 2
            && self.second_scan_failure.lock().unwrap().as_ref() == Some(&key.transcript_path)
        {
            return Ok(TranscriptRead {
                entries: self.entries(&key.transcript_path),
                diagnostics: vec![
                    pilotdeck_gateway::storage::Diagnostic {
                        code: "host-warning",
                        severity: "warning",
                        line: None,
                        message: "ignored warning".into(),
                    },
                    pilotdeck_gateway::storage::Diagnostic {
                        code: "host-error-one",
                        severity: "error",
                        line: None,
                        message: "artifact scan failed first".into(),
                    },
                    pilotdeck_gateway::storage::Diagnostic {
                        code: "host-error-two",
                        severity: "error",
                        line: None,
                        message: "artifact scan failed second".into(),
                    },
                ],
            });
        }
        if let Some(disk) = &self.disk {
            return disk.read(key, limit);
        }
        Ok(TranscriptRead {
            entries: self.entries(&key.transcript_path),
            diagnostics: vec![],
        })
    }
    fn list(&self, project: &ProjectKey) -> Option<StoreResult<Vec<StoredSession>>> {
        Some(Ok(self
            .state
            .lock()
            .unwrap()
            .records
            .values()
            .filter(|(key, _)| {
                key.project_root == project.project_root
                    && key.transcript_path.file_name().is_some_and(|name| {
                        name == std::ffi::OsStr::new(&format!(
                            "{}.jsonl",
                            sanitize_session_id(&key.session_id)
                        ))
                    })
            })
            .map(|(key, entries)| StoredSession {
                session_id: key.session_id.clone(),
                last_modified: 1_750_000_000_000.0,
                file_size: Some(entries.len() as f64),
            })
            .collect()))
    }
    fn has(&self, key: &TranscriptKey) -> Option<StoreResult<bool>> {
        self.assert_key(key);
        Some(Ok(self
            .state
            .lock()
            .unwrap()
            .records
            .contains_key(&key.transcript_path)))
    }
    fn delete(&self, key: &TranscriptKey) -> Option<StoreResult<()>> {
        self.assert_key(key);
        self.state
            .lock()
            .unwrap()
            .records
            .remove(&key.transcript_path);
        if let Some(disk) = &self.disk {
            return disk.delete(key);
        }
        Some(Ok(()))
    }
    fn delete_session(&self, key: &TranscriptKey) -> Option<StoreResult<()>> {
        self.assert_key(key);
        let session_dir = key.transcript_path.with_extension("");
        let removed: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .records
            .keys()
            .filter(|path| *path == &key.transcript_path || path.starts_with(&session_dir))
            .cloned()
            .collect();
        let mut state = self.state.lock().unwrap();
        state.recursive_deletes.push(key.transcript_path.clone());
        for path in &removed {
            state.records.remove(path);
        }
        drop(state);
        if let Some(disk) = &self.disk {
            for path in removed {
                let mut child = key.clone();
                child.transcript_path = path;
                disk.delete(&child).unwrap().unwrap();
            }
        }
        Some(Ok(()))
    }
    fn replace(&self, key: &TranscriptKey, entries: &[JsValue]) -> Option<StoreResult<()>> {
        Some(self.replace_contents(key, entries))
    }
    fn supports_replace(&self) -> bool {
        true
    }
    fn tool_result_artifacts(&self) -> Option<Arc<dyn TranscriptBlobStore>> {
        Some(self.artifacts.clone())
    }
    fn file_history_backups(&self) -> Option<Arc<dyn TranscriptBlobStore>> {
        Some(self.backups.clone())
    }
    fn supports_replacement_transactions(&self) -> bool {
        self.transactions
    }
    fn prepare_replacement(
        &self,
        key: &TranscriptKey,
        input: &PrepareTranscriptReplacement,
    ) -> Option<StoreResult<()>> {
        if !self.transactions {
            return None;
        }
        self.assert_key(key);
        let original = self.entries(&key.transcript_path);
        self.state.lock().unwrap().pending.insert(
            input.transaction_id.to_utf8().unwrap(),
            Pending {
                key: key.clone(),
                turn: input.replacement_turn_id.clone(),
                original,
            },
        );
        Some(self.replace_contents(key, &input.entries))
    }
    fn finalize_replacement(
        &self,
        key: &TranscriptKey,
        id: &JsString,
        action: TranscriptReplacementAction,
    ) -> Option<StoreResult<()>> {
        if !self.transactions {
            return None;
        }
        self.assert_key(key);
        let pending = self
            .state
            .lock()
            .unwrap()
            .pending
            .remove(&id.to_utf8().unwrap())
            .expect("known host transaction");
        assert_eq!(&pending.key, key, "transaction full owner key");
        self.state.lock().unwrap().finalizations.push(action);
        Some(if action == TranscriptReplacementAction::Rollback {
            self.replace_contents(&pending.key, &pending.original)
        } else {
            Ok(())
        })
    }
    fn recover_replacements(&self, key: &TranscriptKey) -> Option<StoreResult<()>> {
        if !self.transactions {
            return None;
        }
        self.assert_key(key);
        self.state
            .lock()
            .unwrap()
            .recoveries
            .push(key.transcript_path.clone());
        if !self.recovery_enabled.load(Ordering::SeqCst) {
            return Some(Ok(()));
        }
        let ids: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .pending
            .iter()
            .filter(|(_, pending)| pending.key.transcript_path == key.transcript_path)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let pending = self.state.lock().unwrap().pending.remove(&id).unwrap();
            let accepted = self.entries(&key.transcript_path).iter().any(|entry| {
                entry.get("type") == Some(&"accepted_input".into())
                    && entry.get("turnId") == Some(&JsValue::String(pending.turn.clone()))
            });
            if !accepted && let Err(error) = self.replace_contents(key, &pending.original) {
                return Some(Err(error));
            }
        }
        Some(Ok(()))
    }
}

struct Provider {
    origin: String,
    requests: Arc<Mutex<Vec<Value>>>,
    read_alias: Arc<Mutex<Option<String>>>,
    child_spill: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Provider {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let read_alias = Arc::new(Mutex::new(None::<String>));
        let child_spill = Arc::new(AtomicBool::new(false));
        let child_scenario = child_spill.clone();
        let (stopping, seen, alias) = (stop.clone(), requests.clone(), read_alias.clone());
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("owned provider accept: {error}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 8192];
                let (start, length) = loop {
                    let count = socket.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(at) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..at]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                    .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (at + 4, length);
                    }
                };
                while bytes.len() < start + length {
                    let count = socket.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let body: Value = serde_json::from_slice(&bytes[start..start + length]).unwrap();
                seen.lock().unwrap().push(body.clone());
                let title = body.to_string().contains("Generate a concise title");
                let after_tool = body["messages"]
                    .as_array()
                    .and_then(|m| m.last())
                    .is_some_and(|m| m["role"] == "tool");
                let requested = if !title && !after_tool {
                    alias.lock().unwrap().clone()
                } else {
                    None
                };
                let delta = match requested {
                    Some(path) => {
                        json!({"tool_calls":[{"index":0,"id":"read-host-fork","type":"function","function":{"name":"read_file","arguments":json!({"file_path":path}).to_string()}}]})
                    }
                    None => {
                        json!({"content":if title { "{\"title\":\"Host storage\"}" } else { "host storage answer" }})
                    }
                };
                let delta = if child_scenario.load(Ordering::SeqCst) && !title {
                    real_child_spill_delta(&body)
                } else {
                    delta
                };
                let finish = if delta.get("tool_calls").is_some() {
                    "tool_calls"
                } else {
                    "stop"
                };
                let (mime, response) = if body["stream"] == true {
                    let first = json!({"id":"owned","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                    let last = json!({"id":"owned","choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
                    (
                        "text/event-stream",
                        format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),
                    )
                } else {
                    ("application/json", json!({"id":"owned","choices":[{"index":0,"message":{"role":"assistant","content":"Host storage"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}).to_string())
                };
                let wire = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                let _ = socket.write_all(wire.as_bytes());
            }
        });
        Self {
            origin,
            requests,
            read_alias,
            child_spill,
            stop,
            thread: Some(worker),
        }
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}
struct Fixture {
    root: PathBuf,
    project: PathBuf,
    pilot: PathBuf,
    provider: Provider,
    store: Arc<HostStore>,
    adapter: Arc<AsyncTranscriptAdapter>,
    resolutions: Arc<AtomicUsize>,
}
impl Fixture {
    fn new(disk: bool) -> Self {
        Self::with_transactions(disk, true)
    }
    fn with_transactions(disk: bool, transactions: bool) -> Self {
        let root =
            std::env::temp_dir().join(format!("pdrs-host-management-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        let pilot = root.join("home/.pilotdeck");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&pilot).unwrap();
        register_project(&pilot, &project);
        let provider = Provider::start();
        let config = format!(
            "schemaVersion: 1\nagent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: {}/v1\n      apiKey: mock-only\n      models: {{mock: {{}}}}\n",
            provider.origin
        );
        std::fs::write(pilot.join("pilotdeck.yaml"), &config).unwrap();
        let mut owned = if disk {
            HostStore::with_disk()
        } else {
            HostStore::default()
        };
        owned.transactions = transactions;
        let store = Arc::new(owned);
        let host_root = root.join("host-owned");
        let resolutions = Arc::new(AtomicUsize::new(0));
        let count = resolutions.clone();
        let adapter = Arc::new(AsyncTranscriptAdapter::new(
            store.clone(),
            Arc::new(move |key| {
                count.fetch_add(1, Ordering::SeqCst);
                host_root
                    .join(key.project_root.file_name().unwrap())
                    .join("chats")
            }),
        ));
        Self {
            root,
            project,
            pilot,
            provider,
            store,
            adapter,
            resolutions,
        }
    }
    fn storage(&self, project: &Path, session: &str) -> SessionStorage {
        self.adapter
            .create_session_storage(SessionStorageInput {
                project_root: project.into(),
                pilot_home: self.pilot.clone(),
                session_id: session.into(),
                now: None,
            })
            .unwrap()
    }
    fn start(&self) -> LocalGateway {
        self.start_with_extensions(NativeExtensions::default())
    }
    fn start_with_extensions(&self, extensions: NativeExtensions) -> LocalGateway {
        let env = [
            ("HOME", self.root.join("home")),
            ("PILOT_HOME", self.pilot.clone()),
            ("PILOTDECK_CONFIG_PATH", self.pilot.join("pilotdeck.yaml")),
        ]
        .into_iter()
        .map(|(key, path)| (key.into(), path.to_string_lossy().into_owned()))
        .collect();
        LocalGateway::start_with_session_storage(
            LocalOptions {
                telemetry_context: None,
                pilot_home: self.pilot.clone(),
                project: self.project.clone(),
                os_home: Some(self.root.join("home")),
                sidecar: self.root.join("never-a-sidecar"),
                env,
                builtin_skills_root: None,
                builtin_plugins_root: None,
            },
            extensions,
            self.adapter.clone(),
        )
        .unwrap()
    }
    fn params(&self, project: &Path, session: &str) -> Value {
        json!({"sessionKey":session,"projectKey":project})
    }
    fn request(&self, gateway: &LocalGateway, method: &str, params: Value) -> Value {
        value(
            &gateway
                .request(method, js(params))
                .unwrap_or_else(|error| panic!("{method}: {error:?}")),
        )
    }
    fn messages(&self, gateway: &LocalGateway, project: &Path, session: &str) -> Value {
        self.request(
            gateway,
            "read_session_messages",
            self.params(project, session),
        )
    }
    /// The SDK supplies the omitted fork point using the public message view
    /// (fixed client.ts:3360-3372); raw fork_session itself requires that id.
    fn fork_like_sdk(&self, gateway: &LocalGateway, project: &Path, session: &str) -> Value {
        let projected = self.messages(gateway, project, session);
        let point = projected["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find_map(|message| {
                let id = message
                    .get("entryId")
                    .filter(|value| !value.is_null())
                    .or_else(|| message.get("id").filter(|value| !value.is_null()));
                // Projected entryId/id are strings. Preserve nullish fallback and
                // skip an empty entryId instead of falling through to its id.
                id.map(|id| {
                    id.as_str()
                        .expect("public projected message id is a string")
                })
                .filter(|id| !id.is_empty())
            })
            .expect("public messages contain a forkable entry");
        self.request(
            gateway,
            "fork_session",
            json!({"sessionKey":session,
            "projectKey":project,"fromEntryId":point}),
        )
    }
    fn turn(
        &self,
        gateway: &LocalGateway,
        session: &str,
        input: &str,
        run: Option<&str>,
    ) -> Vec<Value> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let output = events.clone();
        let mut params = json!({"sessionKey":session,"projectKey":self.project,"channelKey":"host-storage","message":input,"mode":"bypassPermissions"});
        if let Some(run) = run {
            params["runId"] = run.into();
        }
        gateway
            .submit_turn(
                js(params),
                Arc::new(move |event| output.lock().unwrap().push(value(&event))),
            )
            .unwrap();
        let events = events.lock().unwrap().clone();
        assert!(
            events.iter().any(|event| event["type"] == "turn_completed"),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| event["type"] == "error" || event["type"] == "turn_failed"),
            "{events:?}"
        );
        events
    }
    fn stop(&self, gateway: LocalGateway) {
        gateway.shutdown(Duration::from_secs(5)).unwrap();
        drop(gateway);
    }
    fn no_default_transcript(&self, project: &Path, session: &str) {
        let default =
            pilotdeck_gateway::session_chat_dir(&self.pilot, project, &project.to_string_lossy())
                .join(format!("{}.jsonl", sanitize_session_id(&session.into())));
        assert!(
            !default.exists(),
            "host choice fell back to {}",
            default.display()
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn register_project(pilot: &Path, project: &Path) {
    let chat = pilotdeck_gateway::session_chat_dir(pilot, project, &project.to_string_lossy());
    let registered = chat.parent().unwrap();
    std::fs::create_dir_all(registered).unwrap();
    std::fs::write(
        registered.join(".cwd"),
        project.to_string_lossy().as_bytes(),
    )
    .unwrap();
}
fn listed(f: &Fixture, gateway: &LocalGateway, project: &Path, session: &str) -> bool {
    f.request(gateway, "list_sessions", json!({"projectKey":project}))["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["sessionId"] == session)
}

fn seed(storage: &SessionStorage, session: &str, turn: &str, input: &str, answer: &str) {
    storage
        .transcript
        .record_accepted_input(
            &session.into(),
            &turn.into(),
            vec![js(
                json!({"role":"user","content":[{"type":"text","text":input}]}),
            )],
            None,
        )
        .unwrap();
    event(
        &storage.transcript,
        session,
        turn,
        json!({"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":answer}]}}),
    );
}
fn event(
    writer: &pilotdeck_gateway::storage::TranscriptWriter,
    session: &str,
    turn: &str,
    draft: Value,
) {
    writer
        .record_session_event(
            &session.into(),
            &turn.into(),
            js(draft).as_object().unwrap().clone(),
        )
        .unwrap();
}
fn latest_turn(store: &HostStore, path: &Path) -> String {
    store
        .entries(path)
        .iter()
        .rev()
        .find(|entry| entry.get("type") == Some(&"accepted_input".into()))
        .map(|entry| text(entry, "turnId"))
        .unwrap()
}
fn contains_text(messages: &Value, expected: &str) -> bool {
    match messages {
        Value::Object(object) => {
            object.get("text").is_some_and(|text| text == expected)
                || object.values().any(|child| contains_text(child, expected))
        }
        Value::Array(items) => items.iter().any(|child| contains_text(child, expected)),
        _ => false,
    }
}

#[test]
fn host_owned_layout_routes_turn_list_fork_export_restore_and_delete() {
    let f = Fixture::new(true);
    let gateway = f.start();
    let session = "host-layout";
    f.turn(&gateway, session, "host controlled native layout", None);
    let storage = f.storage(&f.project, session);
    assert!(storage.transcript_path.is_file());
    f.no_default_transcript(&f.project, session);
    assert!(listed(&f, &gateway, &f.project, session));
    assert!(contains_text(
        &f.messages(&gateway, &f.project, session),
        "host controlled native layout"
    ));
    let fork = f.fork_like_sdk(&gateway, &f.project, session);
    let fork_id = fork["newSessionKey"].as_str().unwrap();
    assert!(f.storage(&f.project, fork_id).transcript_path.is_file());
    let archive = f.request(
        &gateway,
        "export_session_transcript",
        f.params(&f.project, session),
    );
    f.request(
        &gateway,
        "restore_session_transcript",
        json!({"projectKey":f.project,"sessionKey":"host-restored","archive":archive}),
    );
    let restored = f.storage(&f.project, "host-restored");
    assert!(restored.transcript_path.is_file());
    f.request(
        &gateway,
        "delete_session",
        f.params(&f.project, "host-restored"),
    );
    assert!(!restored.transcript_path.exists());
    assert!(f.resolutions.load(Ordering::SeqCst) > 0);
    f.no_default_transcript(&f.project, fork_id);
    f.stop(gateway);
}

#[test]
fn host_store_reopens_replaces_recovers_and_recursively_deletes_then_restores() {
    let f = Fixture::new(false);
    let session = "host-async";
    let gateway = f.start();
    f.turn(&gateway, session, "first host-store turn", None);
    f.stop(gateway);
    let gateway = f.start();
    f.turn(&gateway, session, "second host-store turn", None);
    let storage = f.storage(&f.project, session);
    let messages = f.messages(&gateway, &f.project, session);
    assert!(
        contains_text(&messages, "first host-store turn")
            && contains_text(&messages, "second host-store turn")
    );
    assert!(f.store.reads.load(Ordering::SeqCst) >= 3);
    assert!(!storage.transcript_path.exists());
    let turn = latest_turn(&f.store, &storage.transcript_path);
    let prepared = f.request(&gateway, "replace_last_turn", json!({"projectKey":f.project,"sessionKey":session,"expectedTurnId":turn,"replacementTurnId":"abandoned-host-turn"}));
    f.request(&gateway, "finalize_last_turn_replacement", json!({"projectKey":f.project,"sessionKey":session,"transactionId":prepared["transactionId"],"action":"rollback"}));
    assert!(contains_text(
        &f.messages(&gateway, &f.project, session),
        "second host-store turn"
    ));
    f.request(&gateway, "replace_last_turn", json!({"projectKey":f.project,"sessionKey":session,"expectedTurnId":turn,"replacementTurnId":"replacement-host-turn"}));
    f.turn(
        &gateway,
        session,
        "replacement host-store turn",
        Some("replacement-host-turn"),
    );
    let messages = f.messages(&gateway, &f.project, session);
    assert!(contains_text(&messages, "replacement host-store turn"));
    assert!(!contains_text(&messages, "second host-store turn"));
    assert_eq!(f.store.pending_count(), 0);
    assert!(
        f.store
            .state
            .lock()
            .unwrap()
            .finalizations
            .contains(&TranscriptReplacementAction::Rollback)
    );
    assert!(
        f.store
            .state
            .lock()
            .unwrap()
            .finalizations
            .contains(&TranscriptReplacementAction::Commit)
    );
    let fork = f.fork_like_sdk(&gateway, &f.project, session);
    let fork_id = fork["newSessionKey"].as_str().unwrap();
    assert!(contains_text(
        &f.messages(&gateway, &f.project, fork_id),
        "first host-store turn"
    ));
    assert!(!f.storage(&f.project, fork_id).transcript_path.exists());
    let turn = latest_turn(&f.store, &storage.transcript_path);
    f.request(&gateway, "replace_last_turn", json!({"projectKey":f.project,"sessionKey":session,"expectedTurnId":turn,"replacementTurnId":"crashed-host-turn"}));
    assert_eq!(f.store.pending_count(), 1);
    f.stop(gateway);
    f.store.recovery_enabled.store(true, Ordering::SeqCst);
    let gateway = f.start();
    assert!(contains_text(
        &f.messages(&gateway, &f.project, session),
        "replacement host-store turn"
    ));
    assert_eq!(f.store.pending_count(), 0);
    let archive = f.request(
        &gateway,
        "export_session_transcript",
        f.params(&f.project, session),
    );
    let sentinel = storage.subagent_transcript_path(&"delete-sentinel".into());
    let mut child = storage.key();
    child.transcript_path = sentinel.clone();
    f.store.replace(&child, &[]).unwrap().unwrap();
    storage
        .tool_result_artifact_storage()
        .unwrap()
        .write("delete-artifact", b"owned payload")
        .unwrap();
    storage
        .file_history_backup_storage()
        .unwrap()
        .write("delete-backup", b"owned checkpoint")
        .unwrap();
    f.request(&gateway, "delete_session", f.params(&f.project, session));
    assert_eq!(
        storage
            .tool_result_artifact_storage()
            .unwrap()
            .read("delete-artifact")
            .unwrap(),
        None
    );
    assert_eq!(
        storage
            .file_history_backup_storage()
            .unwrap()
            .read("delete-backup")
            .unwrap(),
        None
    );
    assert!(
        !f.store
            .state
            .lock()
            .unwrap()
            .records
            .contains_key(&storage.transcript_path)
    );
    assert!(
        !f.store
            .state
            .lock()
            .unwrap()
            .records
            .contains_key(&sentinel)
    );
    assert!(!listed(&f, &gateway, &f.project, session));
    f.request(
        &gateway,
        "restore_session_transcript",
        json!({"projectKey":f.project,"sessionKey":"restored-async","archive":archive}),
    );
    assert!(contains_text(
        &f.messages(&gateway, &f.project, "restored-async"),
        "first host-store turn"
    ));
    assert!(
        !f.storage(&f.project, "restored-async")
            .transcript_path
            .exists()
    );
    f.no_default_transcript(&f.project, session);
    f.stop(gateway);
}

#[test]
fn host_fork_copies_nested_sidechains_checkpoint_and_relocated_tool_payload() {
    let f = Fixture::new(false);
    let source = f.storage(&f.project, "fork-source");
    seed(
        &source,
        "fork-source",
        "parent-turn",
        "retain parent history",
        "parent answer",
    );
    let child = source.transcript.for_subagent(&"child-agent".into());
    event(
        &source.transcript,
        "fork-source",
        "parent-turn",
        json!({"type":"subagent_started","subagentId":"child-agent","subagentType":"general-purpose","prompt":"child","transcriptRelativePath":source.transcript.relative_subagent_path(&"child-agent".into())}),
    );
    child
        .writer
        .record_accepted_input(
            &"child-session".into(),
            &"child-turn".into(),
            vec![js(
                json!({"role":"user","content":[{"type":"text","text":"child input"}]}),
            )],
            None,
        )
        .unwrap();
    let grandchild = child.writer.for_subagent(&"grandchild-agent".into());
    event(
        &child.writer,
        "child-session",
        "child-turn",
        json!({"type":"subagent_started","subagentId":"grandchild-agent","subagentType":"general-purpose","prompt":"nested","transcriptRelativePath":child.writer.relative_subagent_path(&"grandchild-agent".into())}),
    );
    event(
        &child.writer,
        "child-session",
        "child-turn",
        json!({"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":"child host answer"}]}}),
    );
    grandchild
        .writer
        .record_accepted_input(
            &"grandchild-session".into(),
            &"grandchild-turn".into(),
            vec![js(
                json!({"role":"user","content":[{"type":"text","text":"nested input"}]}),
            )],
            None,
        )
        .unwrap();
    event(
        &grandchild.writer,
        "grandchild-session",
        "grandchild-turn",
        json!({"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":"nested host answer"}]}}),
    );
    source
        .file_history_backup_storage()
        .unwrap()
        .write("checkpoint@v1", b"before fork\n")
        .unwrap();
    event(
        &source.transcript,
        "fork-source",
        "parent-turn",
        json!({"type":"file_snapshot_recorded","messageId":"parent-turn","trackedFileBackups":{(f.project.join("checkpoint.txt").to_string_lossy().as_ref()):{"backupFileName":"checkpoint@v1","version":1,"backupTime":"2026-09-10T00:00:00.000Z"}},"snapshotTimestamp":"2026-09-10T00:00:00.000Z"}),
    );
    let payload = format!("forked host artifact\n{}", "payload ".repeat(80));
    let budget = ToolResultBudget::new(ToolResultBudgetOptions {
        tool_results_dir: source.tool_results_dir.clone(),
        max_result_size_chars: Some(64),
        max_result_size_tokens: Some(20),
        preview_bytes: Some(32),
    })
    .unwrap()
    .with_artifact_storage(source.tool_result_artifact_storage());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let message = runtime.block_on(budget.apply_to_message(&js(json!({"role":"user","content":[{"type":"tool_result","toolCallId":"fork-payload","content":[{"type":"text","text":payload}]}]})), Some(&"parent-turn".into()))).unwrap();
    let source_reference = value(&message)["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_result_reference")
        .unwrap()
        .clone();
    event(
        &source.transcript,
        "fork-source",
        "parent-turn",
        json!({"type":"durable_message","message":value(&message)}),
    );
    event(
        &source.transcript,
        "fork-source",
        "parent-turn",
        json!({"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":"fork point answer"}]}}),
    );
    let point = text(
        f.store.entries(&source.transcript_path).last().unwrap(),
        "entryId",
    );
    let gateway = f.start();
    let fork = f.request(&gateway, "fork_session", json!({"projectKey":f.project,"sessionKey":"fork-source","fromEntryId":point,"resumeAt":true}));
    let target_id = fork["newSessionKey"].as_str().unwrap();
    let target = f.storage(&f.project, target_id);
    assert!(contains_text(
        &f.request(
            &gateway,
            "read_subagent_messages",
            json!({"projectKey":f.project,"sessionKey":target_id,"subagentId":"child-agent"})
        ),
        "child host answer"
    ));
    let target_child = target.transcript.for_subagent(&"child-agent".into());
    let target_grandchild = target_child.writer.for_subagent(&"grandchild-agent".into());
    assert!(
        target
            .read_transcript_at_path(&target_grandchild.transcript_path, None)
            .unwrap()
            .entries
            .iter()
            .any(|entry| value(entry).to_string().contains("nested host answer"))
    );
    assert_eq!(
        target
            .file_history_backup_storage()
            .unwrap()
            .read("checkpoint@v1")
            .unwrap(),
        Some(b"before fork\n".to_vec())
    );
    let target_reference = target
        .read_transcript(None)
        .unwrap()
        .entries
        .iter()
        .flat_map(|entry| {
            value(entry)["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .find(|block| block["type"] == "tool_result_reference")
        .unwrap();
    assert_ne!(target_reference["path"], source_reference["path"]);
    let artifact = Path::new(target_reference["path"].as_str().unwrap())
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        target
            .tool_result_artifact_storage()
            .unwrap()
            .read(artifact)
            .unwrap(),
        Some(payload.as_bytes().to_vec())
    );
    if target.tool_results_dir.exists() {
        std::fs::remove_dir_all(&target.tool_results_dir).unwrap();
    }
    *f.provider.read_alias.lock().unwrap() =
        Some(target_reference["readFilePath"].as_str().unwrap().into());
    let events = f.turn(&gateway, target_id, "read forked host payload", None);
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "tool_call_finished"
                && event["toolCallId"] == "read-host-fork"
                && event["ok"] == true),
        "{events:?}"
    );
    assert_eq!(
        std::fs::read_to_string(
            f.project
                .join(target_reference["readFilePath"].as_str().unwrap())
        )
        .unwrap(),
        payload
    );
    assert!(f.provider.requests.lock().unwrap().iter().any(|request| {
        request["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["role"] == "tool" && message.to_string().contains("forked host artifact")
            })
        })
    }));
    assert!(!target.transcript_path.exists());
    f.no_default_transcript(&f.project, target_id);
    f.stop(gateway);
}

fn pending_recovery_case(new_project: bool) {
    let f = Fixture::with_transactions(true, false);
    let project = if new_project {
        let path = f.root.join("new-project");
        std::fs::create_dir_all(&path).unwrap();
        register_project(&f.pilot, &path);
        std::fs::copy(f.pilot.join("pilotdeck.yaml"), path.join("pilotdeck.yaml")).unwrap();
        path
    } else {
        f.project.clone()
    };
    let storage = f.storage(&project, "recover-owned");
    seed(
        &storage,
        "recover-owned",
        "turn-1",
        "original host request",
        "original host answer",
    );
    let original = std::fs::read(&storage.transcript_path).unwrap();
    // Original cc7 tests prepare without an owner. Seed that documented v1
    // crash-state format, not a live v2 owner in this still-running test PID.
    // Only recovery is exercised via the public gateway; this is fixture data,
    // not a claim to have called a public prepare API in a prior process.
    let transaction = uuid::Uuid::new_v4().to_string();
    let safe = sanitize_session_id(&"recover-owned".into());
    let backup = storage
        .chat_dir
        .join(format!(".{safe}.{transaction}.replace.bak"));
    let journal = storage
        .chat_dir
        .join(format!(".{safe}.{transaction}.replace.json"));
    std::fs::write(&backup, &original).unwrap();
    std::fs::write(
        &journal,
        serde_json::to_vec(&json!({"version":1,"transactionId":transaction,
        "sessionKey":"recover-owned","replacementTurnId":"never-accepted",
        "preparedAt":"2026-09-10T00:00:00.000Z"}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(&storage.transcript_path, b"").unwrap();
    let gateway = f.start();
    if new_project {
        assert!(
            backup.is_file() && journal.is_file(),
            "inactive project's filesystem transaction remains pending"
        );
        assert!(listed(&f, &gateway, &project, "recover-owned"));
    }
    assert_eq!(
        std::fs::read(&storage.transcript_path).unwrap(),
        original,
        "selected chatDir filesystem recovery precedes startup publication/project projection"
    );
    assert!(!backup.exists() && !journal.exists());
    assert!(!std::fs::read_dir(&storage.chat_dir).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".replace.")
    }));
    assert!(
        f.store.state.lock().unwrap().recoveries.is_empty(),
        "startup is not host callback recovery"
    );
    assert!(contains_text(
        &f.messages(&gateway, &project, "recover-owned"),
        "original host answer"
    ));
    f.no_default_transcript(&project, "recover-owned");
    f.stop(gateway);
}

#[test]
fn host_owned_pending_replacement_recovers_before_startup_publication() {
    pending_recovery_case(false);
}
#[test]
fn host_owned_new_project_activation_recovers_before_first_list() {
    pending_recovery_case(true);
}

fn artifact_reference(storage: &SessionStorage, name: &str) -> Value {
    json!({"type":"tool_result_reference","toolCallId":format!("call-{name}"),"isError":false,
        "path":storage.tool_results_dir.join(name),"originalBytes":10,"preview":"owned",
        "hasMore":true,"mimeType":"text/plain","reason":"tool_result_too_large"})
}

fn seeded_fork_child(
    source: &SessionStorage,
    session: &str,
) -> pilotdeck_gateway::storage::SubagentHandle {
    let child = source.transcript.for_subagent(&"ordered-child".into());
    event(
        &source.transcript,
        session,
        "ordered-turn",
        json!({"type":"subagent_started",
        "subagentId":"ordered-child","subagentType":"general-purpose","prompt":"preserve child",
        "transcriptRelativePath":source.transcript.relative_subagent_path(&"ordered-child".into())}),
    );
    child
        .writer
        .record_accepted_input(
            &"child-logical-session".into(),
            &"child-turn".into(),
            vec![js(
                json!({"role":"user","content":[{"type":"text","text":"child input"}]}),
            )],
            None,
        )
        .unwrap();
    child
}

#[test]
fn host_fork_preserves_first_seen_backup_and_artifact_write_order() {
    let f = Fixture::new(false);
    let session = "fork-first-seen";
    let source = f.storage(&f.project, session);
    source
        .transcript
        .record_accepted_input(
            &session.into(),
            &"ordered-turn".into(),
            vec![js(
                json!({"role":"user","content":[{"type":"text","text":"preserve payload order"}]}),
            )],
            None,
        )
        .unwrap();
    // JS own-property order matters here. Parsing literal JSON preserves z->a;
    // routing these keys through serde_json::Map would sort this fixture itself.
    let checkpoint = parse_json(r#"{"type":"file_snapshot_recorded","messageId":"ordered-turn","trackedFileBackups":{"z-file.txt":{"backupFileName":"z-first@v1","version":1,"backupTime":"2026-09-10T00:00:00.000Z"},"a-file.txt":{"backupFileName":"a-second@v1","version":1,"backupTime":"2026-09-10T00:00:00.000Z"}},"snapshotTimestamp":"2026-09-10T00:00:00.000Z"}"#).unwrap();
    source
        .transcript
        .record_session_event(
            &session.into(),
            &"ordered-turn".into(),
            checkpoint.as_object().unwrap().clone(),
        )
        .unwrap();
    source
        .file_history_backup_storage()
        .unwrap()
        .write("z-first@v1", b"z-backup")
        .unwrap();
    source
        .file_history_backup_storage()
        .unwrap()
        .write("a-second@v1", b"a-backup")
        .unwrap();
    source
        .tool_result_artifact_storage()
        .unwrap()
        .write("z-first.txt", b"z-artifact")
        .unwrap();
    source
        .tool_result_artifact_storage()
        .unwrap()
        .write("a-second.txt", b"a-artifact")
        .unwrap();
    event(
        &source.transcript,
        session,
        "ordered-turn",
        json!({"type":"durable_message","message":{"role":"user","content":[artifact_reference(&source,"z-first.txt")]}}),
    );
    let child = seeded_fork_child(&source, session);
    event(
        &child.writer,
        "child-logical-session",
        "child-turn",
        json!({"type":"durable_message","message":{"role":"user","content":[artifact_reference(&source,"a-second.txt"),artifact_reference(&source,"z-first.txt")]}}),
    );
    event(
        &child.writer,
        "child-logical-session",
        "child-turn",
        json!({"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":"ordered child answer"}]}}),
    );
    event(
        &source.transcript,
        session,
        "ordered-turn",
        json!({"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":"ordered fork point"}]}}),
    );
    let point = text(
        f.store.entries(&source.transcript_path).last().unwrap(),
        "entryId",
    );
    let gateway = f.start();
    let fork = f.request(
        &gateway,
        "fork_session",
        json!({"projectKey":f.project,"sessionKey":session,"fromEntryId":point,"resumeAt":true}),
    );
    let target = f.storage(&f.project, fork["newSessionKey"].as_str().unwrap());
    let expected_key = target.key();
    let target_writes = |blobs: &Blobs| -> Vec<String> {
        blobs
            .1
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.transcript_path != source.transcript_path)
            .map(|(key, name)| {
                assert_eq!(
                    key, &expected_key,
                    "fork blobs use the target primary owner key"
                );
                name.clone()
            })
            .collect()
    };
    assert_eq!(
        target_writes(&f.store.backups),
        vec!["z-first@v1", "a-second@v1"]
    );
    assert_eq!(
        target_writes(&f.store.artifacts),
        vec!["z-first.txt", "a-second.txt"],
        "first occurrence order with duplicate z deduplicated, not BTreeSet order"
    );
    assert_eq!(
        target
            .file_history_backup_storage()
            .unwrap()
            .read("z-first@v1")
            .unwrap(),
        Some(b"z-backup".to_vec())
    );
    assert_eq!(
        target
            .file_history_backup_storage()
            .unwrap()
            .read("a-second@v1")
            .unwrap(),
        Some(b"a-backup".to_vec())
    );
    assert_eq!(
        target
            .tool_result_artifact_storage()
            .unwrap()
            .read("z-first.txt")
            .unwrap(),
        Some(b"z-artifact".to_vec())
    );
    assert_eq!(
        target
            .tool_result_artifact_storage()
            .unwrap()
            .read("a-second.txt")
            .unwrap(),
        Some(b"a-artifact".to_vec())
    );
    f.stop(gateway);
}

#[test]
fn host_fork_second_artifact_scan_diagnostic_prevents_primary_publication() {
    let f = Fixture::new(false);
    let session = "fork-independent-scans";
    let source = f.storage(&f.project, session);
    source.transcript.record_accepted_input(&session.into(), &"ordered-turn".into(),
        vec![js(json!({"role":"user","content":[{"type":"text","text":"second scan must be observable"}]}))], None).unwrap();
    let child = seeded_fork_child(&source, session);
    event(
        &child.writer,
        "child-logical-session",
        "child-turn",
        json!({"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":"copy succeeds before artifact scan fails"}]}}),
    );
    event(
        &source.transcript,
        session,
        "ordered-turn",
        json!({"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":"independent scan fork point"}]}}),
    );
    let point = text(
        f.store.entries(&source.transcript_path).last().unwrap(),
        "entryId",
    );
    *f.store.second_scan_failure.lock().unwrap() = Some(child.transcript_path.clone());
    let gateway = f.start();
    let error = gateway.request("fork_session", js(json!({"projectKey":f.project,"sessionKey":session,"fromEntryId":point,"resumeAt":true}))).expect_err("second artifact scan diagnostics must reject the fork");
    assert_eq!(
        error.message.to_utf8().unwrap(),
        format!(
            "Cannot inspect fork sidechain transcript {}: artifact scan failed first; artifact scan failed second",
            child.transcript_path.display()
        )
    );
    assert_eq!(
        f.store
            .read_counts
            .lock()
            .unwrap()
            .get(&child.transcript_path),
        Some(&2),
        "artifact inspection performs an independent second host read"
    );
    let writes = f.store.replaces.lock().unwrap();
    assert_eq!(
        writes
            .iter()
            .filter(|key| key.transcript_path.parent() == Some(source.chat_dir.as_path()))
            .count(),
        0,
        "target primary transcript must not publish after inspection failure"
    );
    assert_eq!(
        writes.len(),
        1,
        "first scan already copied the child before independent inspection failed"
    );
    assert_ne!(writes[0].transcript_path, child.transcript_path);
    assert!(
        writes[0]
            .transcript_path
            .to_string_lossy()
            .contains("/subagents/")
    );
    assert!(
        f.store.artifacts.1.lock().unwrap().is_empty(),
        "collect failure precedes artifact writes"
    );
    drop(writes);
    f.stop(gateway);
}

const REAL_CHILD_DIRECTIVE: &str =
    "HOST_CHILD_SPILL_MARKER call host_child_large_output exactly once, then finish.";

fn real_child_spill_delta(request: &Value) -> Value {
    let messages = request["messages"].as_array().unwrap();
    let child = messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .to_string()
                .contains("HOST_CHILD_SPILL_MARKER")
    });
    let already_called = |name: &str| {
        messages.iter().any(|message| {
            message["tool_calls"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call["function"]["name"] == name))
        })
    };
    if child && !already_called("host_child_large_output") {
        json!({"tool_calls":[{"index":0,"id":"child-big-output","type":"function",
            "function":{"name":"host_child_large_output","arguments":"{}"}}]})
    } else if child {
        json!({"content":"child native spill completed"})
    } else if !already_called("agent") {
        json!({"tool_calls":[{"index":0,"id":"launch-native-child","type":"function",
            "function":{"name":"agent","arguments":json!({"description":"Read hosted large output",
                "prompt":REAL_CHILD_DIRECTIVE,"subagent_type":"general-purpose"}).to_string()}}]})
    } else {
        json!({"content":"parent native child completed"})
    }
}

struct RealChildLargeTool {
    fields: pilotdeck_protocol::JsObject,
    payload: String,
    calls: Arc<AtomicUsize>,
}
impl ToolDefinition for RealChildLargeTool {
    fn fields(&self) -> &pilotdeck_protocol::JsObject {
        &self.fields
    }
    fn is_read_only(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}
impl ExecutableTool for RealChildLargeTool {
    fn execute(&self, _: JsValue, _: ToolRunContext) -> ToolFuture<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let payload = self.payload.clone();
        Box::pin(async move {
            Ok(ToolOutput {
                content: vec![js(json!({"type":"text","text":payload}))],
                supplemental_messages: None,
                data: None,
                metadata: None,
            })
        })
    }
}

fn real_child_payload_references(entries: &[JsValue]) -> Vec<Value> {
    entries
        .iter()
        .flat_map(|entry| {
            value(entry)["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|block| {
            block["type"] == "tool_result_reference" && block["toolCallId"] == "child-big-output"
        })
        .collect()
}

#[test]
fn real_native_child_spill_keeps_parent_host_scope_and_is_copied_by_fork() {
    let f = Fixture::new(false);
    assert!(
        f.store.artifacts.0.lock().unwrap().is_empty(),
        "no artifact is seeded by this test"
    );
    f.provider.child_spill.store(true, Ordering::SeqCst);
    let payload = format!("REAL_CHILD_SPILL_PAYLOAD\n{}", "payload ".repeat(40_000));
    assert!(
        payload.len() > 200_000,
        "real tool output must exceed the native budget's character limit"
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let extensions = NativeExtensions::default();
    let registration = extensions.register("real-child-spill", Some(&f.project), NativeExtension {
        tools: vec![Arc::new(RealChildLargeTool {
            fields: js(json!({"name":"host_child_large_output","description":"Return a large owned payload",
                "inputSchema":{"type":"object","properties":{},"additionalProperties":false}})).as_object().unwrap().clone(),
            payload: payload.clone(), calls: calls.clone(),
        })], ..Default::default()
    }).unwrap();
    let gateway = Arc::new(f.start_with_extensions(extensions));
    let session = "real-parent-spill";
    thread::scope(|scope| {
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let running = gateway.clone();
        let fixture = &f;
        let worker = scope.spawn(move || {
            let events = fixture.turn(
                &running,
                session,
                "Delegate one native child to produce its large tool output.",
                None,
            );
            let _ = send.send(events);
        });
        if receive.recv_timeout(Duration::from_secs(30)).is_err() {
            gateway.shutdown(Duration::from_secs(5)).unwrap();
            worker.join().unwrap();
            panic!("bounded real child spill turn did not complete within 30 seconds");
        }
        worker.join().unwrap();
    });
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "registered tool runs once inside the actual child"
    );
    let source = f.storage(&f.project, session);
    let parent = source.read_transcript(None).unwrap().entries;
    let started: Vec<_> = parent
        .iter()
        .filter(|entry| entry.get("type") == Some(&"subagent_started".into()))
        .collect();
    assert_eq!(
        started.len(),
        1,
        "one-shot agent creates one real durable sidechain"
    );
    let child_id = text(started[0], "subagentId");
    let child_path = source.subagent_transcript_path(&child_id.as_str().into());
    let child = source
        .read_transcript_at_path(&child_path, None)
        .unwrap()
        .entries;
    let child_session = text(started[0], "subagentSessionId");
    assert!(!child_session.is_empty());
    assert_ne!(child_session, session);
    assert!(
        child
            .iter()
            .any(|entry| value(entry)["sessionId"].as_str() == Some(child_session.as_str())),
        "actual child records identify the nonempty logical child session, independently of the host owner"
    );
    assert_eq!(
        f.store.state.lock().unwrap().records[&child_path]
            .0
            .session_id,
        source.key().session_id,
        "child transcript persistence retains the parent host owner"
    );
    let references = real_child_payload_references(&child);
    assert_eq!(
        references.len(),
        1,
        "actual child tool execution persists its budget-produced reference"
    );
    let reference = &references[0];
    let source_path = Path::new(reference["path"].as_str().unwrap());
    assert_eq!(
        source_path.parent(),
        Some(source.tool_results_dir.as_path()),
        "real child budget must use parent storage.toolResultsDir, not its logical child identity"
    );
    let artifact = source_path.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        source
            .tool_result_artifact_storage()
            .unwrap()
            .read(artifact)
            .unwrap(),
        Some(payload.as_bytes().to_vec())
    );
    let source_writes: Vec<_> = f
        .store
        .artifacts
        .1
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, name)| name == artifact)
        .cloned()
        .collect();
    assert!(!source_writes.is_empty());
    for (key, _) in source_writes {
        assert_eq!(
            key,
            source.key(),
            "actual budget write uses complete parent host key"
        );
    }
    let business: Vec<_> = f
        .provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| !request.to_string().contains("Generate a concise title"))
        .cloned()
        .collect();
    assert_eq!(
        business.len(),
        4,
        "parent agent/child tool/child completion/parent completion only"
    );
    assert!(
        business.iter().any(|request| request["messages"]
            .to_string()
            .contains("HOST_CHILD_SPILL_MARKER")
            && request["tools"].as_array().is_some_and(|tools| tools
                .iter()
                .any(|tool| tool["function"]["name"] == "host_child_large_output"))),
        "the actual child model can see the registered native tool"
    );
    let point = parent
        .iter()
        .rev()
        .find(|entry| entry.get("type") == Some(&"assistant_message".into()))
        .unwrap();
    let fork = f.request(
        &gateway,
        "fork_session",
        json!({"projectKey":f.project,"sessionKey":session,
        "fromEntryId":text(point,"entryId"),"resumeAt":true}),
    );
    let target = f.storage(&f.project, fork["newSessionKey"].as_str().unwrap());
    let target_child_path = target.subagent_transcript_path(&child_id.as_str().into());
    let copied_child = target
        .read_transcript_at_path(&target_child_path, None)
        .unwrap()
        .entries;
    let copied = real_child_payload_references(&copied_child);
    assert_eq!(copied.len(), 1);
    assert_ne!(copied[0]["path"], reference["path"]);
    let copied_path = Path::new(copied[0]["path"].as_str().unwrap());
    assert_eq!(
        copied_path.parent(),
        Some(target.tool_results_dir.as_path())
    );
    let copied_name = copied_path.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        target
            .tool_result_artifact_storage()
            .unwrap()
            .read(copied_name)
            .unwrap(),
        Some(payload.as_bytes().to_vec()),
        "fork owns the real child spill bytes, not a hand-seeded reference fixture"
    );
    assert!(
        f.store
            .artifacts
            .1
            .lock()
            .unwrap()
            .iter()
            .any(|(key, name)| key == &target.key() && name == copied_name)
    );
    assert_eq!(
        f.store.state.lock().unwrap().records[&target_child_path]
            .0
            .session_id,
        target.key().session_id
    );
    assert!(!source.transcript_path.exists() && !target.transcript_path.exists());
    f.no_default_transcript(&f.project, session);
    let gateway = Arc::try_unwrap(gateway)
        .unwrap_or_else(|_| panic!("owned turn worker must release gateway"));
    f.stop(gateway);
    drop(registration);
}

#[test]
fn public_native_archive_encodes_host_bytes_and_preserves_limits_empty_and_owner_scope() {
    use base64::Engine;
    let f = Fixture::new(false);
    let session = "archive-host-bytes";
    let storage = f.storage(&f.project, session);
    let blobs = storage.tool_result_artifact_storage().unwrap();
    let bytes = [0u8, 1, 2, 127, 128, 255];
    blobs.write("body.bin", &bytes).unwrap();
    blobs.write("empty.bin", &[]).unwrap();
    // These disposable files are deliberately different. A selected host blob
    // (including empty/missing) must not fall through to the local cache.
    std::fs::create_dir_all(&storage.tool_results_dir).unwrap();
    std::fs::write(
        storage.tool_results_dir.join("body.bin"),
        b"wrong cached bytes",
    )
    .unwrap();
    std::fs::write(
        storage.tool_results_dir.join("empty.bin"),
        b"not empty on disk",
    )
    .unwrap();
    std::fs::write(
        storage.tool_results_dir.join("missing.bin"),
        b"disk-only payload",
    )
    .unwrap();
    let gateway = f.start();
    assert!(f.provider.requests.lock().unwrap().is_empty());
    let truncated = f.request(
        &gateway,
        "native_archive_artifact",
        json!({"projectKey":f.project,
        "sessionKey":session,"artifactName":"body.bin","maxBytes":3}),
    );
    assert_eq!(
        truncated,
        json!({"artifactName":"body.bin","content":"AAEC","encoding":"base64",
        "bytes":6,"truncated":true})
    );
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(truncated["content"].as_str().unwrap())
            .unwrap(),
        bytes[..3].to_vec()
    );
    let complete = f.request(
        &gateway,
        "native_archive_artifact",
        json!({"projectKey":f.project,
        "sessionKey":session,"artifactName":"body.bin","maxBytes":6}),
    );
    assert_eq!(
        complete,
        json!({"artifactName":"body.bin","content":"AAECf4D/","encoding":"base64",
        "bytes":6,"truncated":false})
    );
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(complete["content"].as_str().unwrap())
            .unwrap(),
        bytes.to_vec()
    );
    let empty = f.request(
        &gateway,
        "native_archive_artifact",
        json!({"projectKey":f.project,
        "sessionKey":session,"artifactName":"empty.bin","maxBytes":6}),
    );
    assert_eq!(
        empty,
        json!({"artifactName":"empty.bin","content":"","encoding":"base64",
        "bytes":0,"truncated":false})
    );
    let missing = gateway
        .request(
            "native_archive_artifact",
            js(json!({"projectKey":f.project,
        "sessionKey":session,"artifactName":"missing.bin","maxBytes":6})),
        )
        .expect_err("missing host payload never reads a disk-only body");
    assert_eq!(missing.code, "ARCHIVE_ARTIFACT_NOT_FOUND");
    assert!(
        missing
            .message
            .equals_str("Archive artifact missing.bin was not found.")
    );
    let other_owner = gateway
        .request(
            "native_archive_artifact",
            js(json!({"projectKey":f.project,
        "sessionKey":"another-archive-owner","artifactName":"body.bin","maxBytes":6})),
        )
        .expect_err("same artifact name cannot expose another session owner's bytes");
    assert_eq!(other_owner.code, "ARCHIVE_ARTIFACT_NOT_FOUND");
    assert!(
        other_owner
            .message
            .equals_str("Archive artifact body.bin was not found.")
    );
    assert_eq!(blobs.read("body.bin").unwrap(), Some(bytes.to_vec()));
    assert_eq!(blobs.read("empty.bin").unwrap(), Some(Vec::new()));
    assert!(
        f.provider.requests.lock().unwrap().is_empty(),
        "archive API never invokes a model"
    );
    assert!(
        !storage.transcript_path.exists(),
        "artifact reads never manufacture a primary transcript"
    );
    f.no_default_transcript(&f.project, session);
    f.stop(gateway);
}
