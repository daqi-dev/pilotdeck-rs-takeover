//! Long-lived native Module2 TCP service. Physical connections may come and
//! go while an explicitly resumable execution retains its original Rust owner.
//! Session durability and accepted host effects still belong to the host.
mod io;
mod replay;
mod requests;
#[cfg(test)]
mod tests;

use self::{
    io::{EngineInput, PeerOutput, line, lock, read_frame},
    replay::ReplayStore,
};
use crate::{
    server::{ExecutionClock, ExecutionFactory, ExecutionServer},
    stdio::ConnectionOwner,
};
use pilotdeck_protocol::{JsString, JsValue, admission::SidecarBinding, parse_json};
pub use replay::ReplayLimits;
use std::{
    collections::{HashMap, VecDeque},
    io::{self as stdio, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct TcpServerOptions {
    pub host: String,
    pub port: u16,
    /// Default CLI parity is false. Embedders can opt into same-instance replay.
    pub stream_resume: bool,
    pub replay: ReplayLimits,
    pub max_connections: usize,
    pub max_executions: usize,
    pub queue_capacity: usize,
    pub max_frame_bytes: usize,
    pub max_queued_bytes: usize,
}
impl Default for TcpServerOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 0,
            stream_resume: false,
            replay: ReplayLimits::default(),
            max_connections: 64,
            max_executions: 128,
            queue_capacity: 128,
            max_frame_bytes: 16 * 1024 * 1024,
            max_queued_bytes: 32 * 1024 * 1024,
        }
    }
}
struct Peer {
    binding: SidecarBinding,
    output: Arc<PeerOutput>,
}
struct Engine {
    server: Arc<ExecutionServer>,
    owner: ConnectionOwner,
    input: Mutex<Option<SyncSender<Vec<u8>>>>,
}
impl Engine {
    fn send(&self, value: &JsValue) -> stdio::Result<()> {
        let bytes = line(value)?;
        lock(&self.input)
            .as_ref()
            .ok_or_else(|| stdio::Error::other("Execution input closed."))?
            .try_send(bytes)
            .map_err(stdio::Error::other)
    }
    fn close(&self) {
        lock(&self.input).take();
        self.server.retire();
    }
}
struct Execution {
    engine: Arc<Engine>,
    boundary: JsValue,
    run_id: JsString,
    operation_id: JsString,
    request_id: JsString,
    peer: Option<JsString>,
    stream: Option<JsString>,
    initial_binding: SidecarBinding,
    pending: VecDeque<(JsString, JsValue)>,
    pending_bytes: usize,
    terminal: Option<Instant>,
    final_event: Option<JsValue>,
    retired: bool,
}
struct ControlReply {
    peer: JsString,
    original: JsString,
    engine: JsString,
}
struct State {
    stopped: bool,
    peers: HashMap<JsString, Peer>,
    executions: HashMap<JsString, Execution>,
    operations: HashMap<JsString, JsString>,
    requests: HashMap<JsString, JsString>,
    streams: HashMap<JsString, JsString>,
    callbacks: HashMap<JsString, JsString>,
    controls: HashMap<JsString, ControlReply>,
    replay: ReplayStore,
}
struct Hub {
    state: Mutex<State>,
    options: TcpServerOptions,
    module_instance: JsString,
    factory: Arc<dyn ExecutionFactory>,
    clock: Arc<dyn ExecutionClock>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    incomplete_engines: Arc<AtomicUsize>,
    listener_done: AtomicBool,
    listener_failure: Mutex<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpShutdownReport {
    pub listener_joined: bool,
    pub tasks_joined: usize,
    pub tasks_remaining: usize,
    pub incomplete_engines: usize,
}
impl TcpShutdownReport {
    pub fn complete(self) -> bool {
        self.listener_joined && self.tasks_remaining == 0 && self.incomplete_engines == 0
    }
}
pub struct TcpExecutionServer {
    hub: Arc<Hub>,
    address: SocketAddr,
    listener: Mutex<Option<JoinHandle<()>>>,
}
impl TcpExecutionServer {
    pub fn listen(
        mut options: TcpServerOptions,
        factory: Arc<dyn ExecutionFactory>,
        clock: Arc<dyn ExecutionClock>,
    ) -> stdio::Result<Self> {
        options.host = options.host.trim_matches(js_whitespace).to_owned();
        if options.host.is_empty() {
            return Err(stdio::Error::other("sidecar TCP host must not be empty."));
        }
        if [
            options.max_connections,
            options.max_executions,
            options.queue_capacity,
            options.max_frame_bytes,
            options.max_queued_bytes,
        ]
        .contains(&0)
        {
            return Err(stdio::Error::other("TCP service bounds must be positive."));
        }
        let replay = ReplayStore::new(options.replay.clone()).map_err(stdio::Error::other)?;
        let listener = TcpListener::bind((options.host.as_str(), options.port))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let hub = Arc::new(Hub {
            state: Mutex::new(State {
                stopped: false,
                peers: HashMap::new(),
                executions: HashMap::new(),
                operations: HashMap::new(),
                requests: HashMap::new(),
                streams: HashMap::new(),
                callbacks: HashMap::new(),
                controls: HashMap::new(),
                replay,
            }),
            module_instance: format!("pilotdeck-agent-loop-instance-{}", uuid::Uuid::new_v4())
                .into(),
            options,
            factory,
            clock,
            tasks: Mutex::new(Vec::new()),
            incomplete_engines: Arc::default(),
            listener_done: AtomicBool::new(false),
            listener_failure: Mutex::new(None),
        });
        let accept_hub = hub.clone();
        let accept = thread::spawn(move || {
            let _finished = ListenerFinished(accept_hub.clone());
            while !lock(&accept_hub.state).stopped {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let _ = accept_hub.attach(socket);
                    }
                    Err(error) if error.kind() == stdio::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) if error.kind() == stdio::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        *lock(&accept_hub.listener_failure) = Some(error.to_string());
                        break;
                    }
                }
                accept_hub.reap();
            }
        });
        Ok(Self {
            hub,
            address,
            listener: Mutex::new(Some(accept)),
        })
    }
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }
    pub fn module_instance_id(&self) -> &JsString {
        &self.hub.module_instance
    }
    /// Keep a foreground entry alive until explicit close or listener failure.
    /// This does not hold the join mutex while waiting, so another owner can
    /// close sockets and complete shutdown concurrently.
    pub fn wait(&self) -> stdio::Result<TcpShutdownReport> {
        while !self.hub.listener_done.load(Ordering::Acquire) {
            thread::park_timeout(Duration::from_millis(20));
        }
        let report = self.close(Duration::from_secs(5));
        match lock(&self.hub.listener_failure).as_ref() {
            Some(error) => Err(stdio::Error::other(error.clone())),
            None => Ok(report),
        }
    }
    /// Socket shutdown interrupts owned readers/writers. Engine cancellation
    /// preserves accepted-call uncertainty; a misbehaving factory is reported,
    /// never counted as a graceful release merely because its waiter was dropped.
    pub fn close(&self, grace: Duration) -> TcpShutdownReport {
        let engines = {
            let mut state = lock(&self.hub.state);
            state.stopped = true;
            for peer in state.peers.values() {
                peer.output.close();
            }
            state.peers.clear();
            state
                .executions
                .values()
                .map(|execution| execution.engine.clone())
                .collect::<Vec<_>>()
        };
        for engine in engines {
            engine.close();
        }
        let until = Instant::now()
            .checked_add(grace)
            .unwrap_or_else(Instant::now);
        let mut listener = lock(&self.listener);
        while listener
            .as_ref()
            .is_some_and(|listener| !listener.is_finished())
            && Instant::now() < until
        {
            thread::sleep(Duration::from_millis(5));
        }
        let listener_joined = listener.as_ref().is_none_or(JoinHandle::is_finished);
        if listener_joined
            && let Some(listener) = listener.take()
            && listener.join().is_err()
        {
            *lock(&self.hub.listener_failure) = Some("TCP listener thread panicked.".into());
        }
        let mut tasks = lock(&self.hub.tasks);
        while tasks.iter().any(|task| !task.is_finished()) && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
        let mut tasks_joined = 0;
        let mut pending = Vec::new();
        for task in tasks.drain(..) {
            if task.is_finished() {
                let _ = task.join();
                tasks_joined += 1;
            } else {
                pending.push(task);
            }
        }
        *tasks = pending;
        TcpShutdownReport {
            listener_joined,
            tasks_joined,
            tasks_remaining: tasks.len(),
            incomplete_engines: self.hub.incomplete_engines.load(Ordering::Acquire),
        }
    }
}
struct ListenerFinished(Arc<Hub>);
impl Drop for ListenerFinished {
    fn drop(&mut self) {
        self.0.listener_done.store(true, Ordering::Release);
    }
}
impl Drop for TcpExecutionServer {
    fn drop(&mut self) {
        let _ = self.close(Duration::from_secs(5));
    }
}

impl Hub {
    fn attach(self: &Arc<Self>, socket: TcpStream) -> stdio::Result<()> {
        // BSD/macOS may inherit O_NONBLOCK from the listener. Each owned
        // reader/writer below is blocking and interrupted by socket shutdown.
        socket.set_nonblocking(false)?;
        socket.set_nodelay(true)?;
        socket.set_write_timeout(Some(Duration::from_secs(5)))?;
        let reader = socket.try_clone()?;
        let writer = socket.try_clone()?;
        let (sender, receiver) = mpsc::sync_channel(self.options.queue_capacity);
        let output = Arc::new(PeerOutput::new(
            socket,
            sender,
            self.options.max_queued_bytes,
        ));
        let id: JsString = uuid::Uuid::new_v4().to_string().into();
        let mut state = lock(&self.state);
        if state.stopped || state.peers.len() >= self.options.max_connections {
            output.close();
            return Ok(());
        }
        state.peers.insert(
            id.clone(),
            Peer {
                binding: SidecarBinding {
                    module_id: "pilotdeck-agent-loop".into(),
                    module_instance_id: self.module_instance.clone(),
                    connection_generation: format!(
                        "{}-{}",
                        String::from_utf16_lossy(self.module_instance.units()),
                        uuid::Uuid::new_v4()
                    )
                    .into(),
                    capabilities_version: "1".into(),
                },
                output: output.clone(),
            },
        );
        let writer_hub = self.clone();
        let writer_id = id.clone();
        lock(&self.tasks).push(thread::spawn(move || {
            output.drive(receiver, writer);
            writer_hub.detach(&writer_id);
        }));
        let reader_hub = self.clone();
        lock(&self.tasks).push(thread::spawn(move || {
            let mut input = BufReader::new(reader);
            while let Ok(Some(bytes)) = read_frame(&mut input, reader_hub.options.max_frame_bytes) {
                let text = String::from_utf8_lossy(&bytes);
                if text.trim_matches(js_whitespace).is_empty() {
                    continue;
                }
                let result = match parse_json(&text) {
                    Ok(value) => reader_hub.receive(&id, &value),
                    Err(_) => {
                        reader_hub.error(&id, "INVALID_JSON", "Module message is not valid JSON.")
                    }
                };
                if result.is_err() {
                    break;
                }
            }
            reader_hub.detach(&id);
        }));
        Ok(())
    }
    fn detach(&self, id: &JsString) {
        let engines = {
            let mut state = lock(&self.state);
            if let Some(peer) = state.peers.remove(id) {
                peer.output.close();
            }
            state.controls.retain(|_, control| &control.peer != id);
            let mut engines = Vec::new();
            for execution in state.executions.values_mut() {
                if execution.peer.as_ref() == Some(id) {
                    execution.peer = None;
                    if !self.options.stream_resume && execution.terminal.is_none() {
                        execution.terminal = Some(Instant::now());
                        engines.push(execution.engine.clone());
                    }
                }
            }
            engines
        };
        for engine in engines {
            engine.close();
        }
    }
    fn reap(&self) {
        let now = Instant::now();
        let engines = {
            let mut state = lock(&self.state);
            state.replay.prune(now);
            let expired = state
                .executions
                .iter()
                .filter(|(_, execution)| {
                    execution.terminal.is_some_and(|terminal| {
                        now.saturating_duration_since(terminal) >= self.options.replay.terminal_ttl
                    })
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            let mut engines = Vec::new();
            for id in expired {
                if let Some(execution) = state.executions.remove(&id) {
                    state.operations.remove(&execution.operation_id);
                    state.requests.remove(&execution.request_id);
                    for (callback, _) in &execution.pending {
                        state.callbacks.remove(callback);
                    }
                    state.controls.retain(|_, control| control.engine != id);
                    if let Some(stream) = execution.stream {
                        state.streams.remove(&stream);
                        state.replay.remove(&stream);
                    }
                    engines.push(execution.engine);
                }
            }
            engines
        };
        for engine in engines {
            engine.close();
        }
        let mut tasks = lock(&self.tasks);
        let mut index = 0;
        while index < tasks.len() {
            if tasks[index].is_finished() {
                let _ = tasks.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
    }
    fn start_engine(
        self: &Arc<Self>,
        state: &mut State,
        peer: &JsString,
        value: &JsValue,
    ) -> stdio::Result<()> {
        let id: JsString = uuid::Uuid::new_v4().to_string().into();
        let binding = state
            .peers
            .get(peer)
            .ok_or_else(|| stdio::Error::other("TCP peer closed."))?
            .binding
            .clone();
        let owner = ConnectionOwner::new_namespaced(
            binding.clone(),
            self.options.queue_capacity,
            id.clone(),
        );
        let server =
            ExecutionServer::new(owner.clone(), self.factory.clone(), self.clock.clone(), 1);
        let (sender, receiver) = mpsc::sync_channel(self.options.queue_capacity);
        let engine = Arc::new(Engine {
            server: server.clone(),
            owner: owner.clone(),
            input: Mutex::new(Some(sender)),
        });
        let string = |key| {
            value
                .get(key)
                .and_then(JsValue::as_string)
                .expect("validated execute")
                .clone()
        };
        let operation_id = string("operationId");
        let request_id = string("requestId");
        state.operations.insert(operation_id.clone(), id.clone());
        state.requests.insert(request_id.clone(), id.clone());
        state.executions.insert(
            id.clone(),
            Execution {
                engine: engine.clone(),
                boundary: value.clone(),
                run_id: string("runId"),
                operation_id,
                request_id,
                peer: Some(peer.clone()),
                stream: None,
                initial_binding: binding,
                pending: VecDeque::new(),
                pending_bytes: 0,
                terminal: None,
                final_event: None,
                retired: false,
            },
        );
        let output = EngineOutput {
            hub: Arc::downgrade(self),
            id: id.clone(),
            buffer: Vec::new(),
        };
        let hub = Arc::downgrade(self);
        let incomplete = self.incomplete_engines.clone();
        lock(&self.tasks).push(thread::spawn(move || {
            // An owned channel, unlike a physical socket, stays open over detach.
            let report = server.serve(
                EngineInput::new(receiver, owner),
                output,
                Duration::from_secs(5),
            );
            if !report.complete() {
                incomplete.fetch_add(1, Ordering::AcqRel);
            }
            if let Some(hub) = hub.upgrade() {
                hub.engine_finished(&id);
            }
        }));
        engine.send(value)
    }

    fn engine_finished(&self, id: &JsString) {
        let mut state = lock(&self.state);
        let Some(execution) = state.executions.get_mut(id) else {
            return;
        };
        execution.retired = true;
        execution.terminal.get_or_insert_with(Instant::now);
        let callbacks = execution
            .pending
            .drain(..)
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        execution.pending_bytes = 0;
        let stream = execution.stream.clone();
        for callback in callbacks {
            state.callbacks.remove(&callback);
        }
        // A failed engine cannot be resumed merely because its previous prefix
        // is still cached. A fresh socket must reconcile this unknown result.
        if let Some(stream) = stream {
            state.replay.remove(&stream);
        }
    }
}

struct EngineOutput {
    hub: Weak<Hub>,
    id: JsString,
    buffer: Vec<u8>,
}
impl Write for EngineOutput {
    fn write(&mut self, bytes: &[u8]) -> stdio::Result<usize> {
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| stdio::Error::other("TCP owner closed."))?;
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > hub.options.max_frame_bytes {
            return Err(stdio::Error::other("Engine output frame limit exceeded."));
        }
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let frame = self.buffer.drain(..=end).collect::<Vec<_>>();
            let value =
                parse_json(&String::from_utf8_lossy(&frame)).map_err(stdio::Error::other)?;
            hub.output(&self.id, value)?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> stdio::Result<()> {
        Ok(())
    }
}
fn js_whitespace(value: char) -> bool {
    matches!(value, '\u{0009}'..='\u{000D}' | '\u{0020}' | '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}
