//! SOP nodes own ordinary fenced session writers. Only already committed
//! child events enter the parent's host event buffer, so the parent recorder
//! never adopts node history as its own assistant messages.
use crate::{
    host::{HostError, HostFuture, ModuleError},
    module_host::staffdeck::sop::nodes::{
        NodeSessionProvider, NodeSessionSpec, NodeTurnInput, NodeTurnResult,
    },
    runtime::{
        network::Cancellation,
        project::{RuntimeError, interaction_events::InteractionEvents},
        session::{
            LiveSessionHandle, SessionContext, SessionFactory,
            factory::{
                FactoryOptions, FactoryStorage, Prepare, ProjectSessionFactory, SessionBinding,
                SessionStorage,
            },
        },
        turn::{host_factory::HostPortDecorators, live::DurableMessageObserver},
    },
    storage::{JsonlTranscriptStore, TranscriptKey},
};
use pilotdeck_agent::ports::{PortError, PortResult};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use pilotdeck_transport::server::{
    ExecuteRequest, ExecutionContext, ExecutionFactory, ExecutionTerminal,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

struct RunSettings {
    factory: Arc<dyn ExecutionFactory>,
    decorators: HostPortDecorators,
}
type Settings = Arc<Mutex<Option<RunSettings>>>;
struct SettingsGuard(Settings);
impl Drop for SettingsGuard {
    fn drop(&mut self) {
        lock(&self.0).take();
    }
}
struct Node {
    session: LiveSessionHandle,
    owner: ProjectSessionFactory,
    settings: Settings,
}
struct Inner {
    parent: SessionContext,
    key: TranscriptKey,
    prepare: Prepare,
    events: InteractionEvents,
    wait_timeout: Duration,
    stop: Cancellation,
    // One parent SOP loop advances one node at a time. The gate also covers
    // creation and cancellation drains, so close cannot retire its writer early.
    nodes: tokio::sync::Mutex<HashMap<JsString, Node>>,
    executor: tokio::runtime::Handle,
}
pub(super) struct NodeSessions(Arc<Inner>);
impl NodeSessions {
    pub(super) fn new(
        parent: SessionContext,
        key: TranscriptKey,
        events: InteractionEvents,
        prepare: Prepare,
        wait_timeout: Duration,
    ) -> Self {
        Self(Arc::new(Inner {
            parent,
            key,
            prepare,
            events,
            wait_timeout,
            stop: Cancellation::new(),
            nodes: Default::default(),
            executor: tokio::runtime::Handle::current(),
        }))
    }
}
impl NodeSessionProvider for NodeSessions {
    fn run(&self, input: NodeTurnInput) -> HostFuture<NodeTurnResult> {
        let inner = self.0.clone();
        Box::pin(async move {
            let cancelled = Cancellation::new();
            let _guard = CancelOnDrop(cancelled.clone());
            // The task owns the submission even when its caller disappears.
            // Dropping this future cancels it, then the task drains through None.
            let task = inner.executor.spawn(run(inner.clone(), input, cancelled));
            task.await.map_err(host_error)?
        })
    }
    fn close(&self) -> HostFuture<()> {
        start_close(self.0.clone())
    }
}
impl Drop for NodeSessions {
    fn drop(&mut self) {
        // Cleanup starts synchronously; abandoning a close waiter cannot cancel it.
        drop(start_close(self.0.clone()));
    }
}
struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn start_close(inner: Arc<Inner>) -> HostFuture<()> {
    inner.stop.abort();
    let task = inner.executor.spawn({
        let inner = inner.clone();
        async move {
            let mut nodes = inner.nodes.lock().await;
            let mut failure = None;
            for (_, node) in nodes.drain() {
                if let Err(error) = node.session.dispose("sop_parent_closed".into()).await {
                    failure.get_or_insert_with(|| host_error(error));
                }
                lock(&node.settings).take();
                if let Err(error) = node.owner.dispose().await {
                    failure.get_or_insert_with(|| host_error(error));
                }
            }
            failure.map_or(Ok(()), Err)
        }
    });
    Box::pin(async move { task.await.map_err(host_error)? })
}

async fn run(
    inner: Arc<Inner>,
    input: NodeTurnInput,
    cancelled: Cancellation,
) -> Result<NodeTurnResult, HostError> {
    validate_spec(&inner.parent.session_key, &input.spec)?;
    let mut nodes = inner.nodes.lock().await;
    if inner.stop.is_aborted() || input.signal.is_aborted() || cancelled.is_aborted() {
        return Err(host_error("SOP node turn was cancelled."));
    }
    if let std::collections::hash_map::Entry::Vacant(entry) =
        nodes.entry(input.spec.session_id.clone())
    {
        let settings: Settings = Arc::default();
        let owner = node_factory(&inner, &input.spec, settings.clone());
        let mut context = inner.parent.clone();
        context.session_key = input.spec.session_id.clone();
        let session = match owner.create(context, None).await {
            Ok(session) => session,
            Err(error) => {
                let _ = owner.dispose().await;
                return Err(host_error(error));
            }
        };
        entry.insert(Node {
            session,
            owner,
            settings,
        });
    }
    let node = nodes.get(&input.spec.session_id).expect("published node");
    if inner.stop.is_aborted() || input.signal.is_aborted() || cancelled.is_aborted() {
        return Err(host_error("SOP node turn was cancelled."));
    }
    *lock(&node.settings) = Some(RunSettings {
        factory: input.factory,
        decorators: input.decorators,
    });
    let _settings = SettingsGuard(node.settings.clone());
    let mut agent_input = JsObject::new();
    agent_input.insert("type".into(), "blocks".into());
    agent_input.insert("content".into(), input.content);
    let mut stream = node
        .session
        .submit(JsValue::Object(agent_input), input.options)
        .map_err(host_error)?;
    let mut result = None;
    let mut aborted = false;
    let mut failure = None;
    loop {
        let next = if aborted {
            stream.next().await
        } else {
            tokio::select! {
                next = stream.next() => next,
                _ = inner.stop.cancelled() => { aborted = true; if let Err(error) = node.session.abort(&"sop_parent_closed".into()) { failure.get_or_insert_with(|| host_error(error)); } continue; }
                _ = input.signal.cancelled() => { aborted = true; if let Err(error) = node.session.abort(&"aborted".into()) { failure.get_or_insert_with(|| host_error(error)); } continue; }
                _ = cancelled.cancelled() => { aborted = true; if let Err(error) = node.session.abort(&"sop_node_waiter_dropped".into()) { failure.get_or_insert_with(|| host_error(error)); } continue; }
            }
        };
        let event = match next {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(error) => {
                failure.get_or_insert_with(|| host_error(error));
                break;
            }
        };
        let Some(event) = event.as_object() else {
            continue;
        };
        let kind = event.get("type").and_then(JsValue::as_string);
        if kind.is_some_and(|kind| kind.equals_str("turn_completed")) {
            result = event.get("result").cloned();
        } else if !kind.is_some_and(|kind| {
            kind.equals_str("turn_started") || kind.equals_str("input_accepted")
        }) && !aborted
        {
            let mut event = event.clone();
            event.insert(
                "sessionId".into(),
                JsValue::String(inner.parent.session_key.clone()),
            );
            if let Err(error) = inner.events.emit_nested_session(event) {
                failure.get_or_insert(error);
                aborted = true;
                let _ = node.session.abort(&"sop_parent_event_buffer_closed".into());
            }
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(NodeTurnResult {
        result: result.ok_or_else(|| host_error("SOP node turn completed without a result."))?,
        messages: node
            .session
            .snapshot()
            .get("messages")
            .cloned()
            .unwrap_or_else(|| JsValue::Array(Vec::new())),
    })
}

fn node_factory(
    inner: &Inner,
    spec: &NodeSessionSpec,
    settings: Settings,
) -> ProjectSessionFactory {
    let prepare = inner.prepare.clone();
    let parent = spec.parent_session_id.clone();
    let mut key = inner.key.clone();
    key.session_id = spec.session_id.clone();
    let stem = key
        .transcript_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let stem = if stem.to_ascii_lowercase().ends_with(".jsonl") {
        &stem[..stem.len() - 6]
    } else {
        &stem
    };
    key.transcript_path = key
        .transcript_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(stem)
        .join("subagents")
        .join(format!(
            "{}.jsonl",
            crate::storage::sanitize_session_id(&spec.sidechain_id)
        ));
    let project = key.project_root.clone();
    ProjectSessionFactory::new(FactoryOptions {
        writer_busy_wait_timeout: inner.wait_timeout,
        prepare: Arc::new(move |context, mcp| {
            let prepared = prepare(context, mcp);
            let parent = parent.clone();
            let settings = settings.clone();
            Box::pin(async move {
                let mut prepared = prepared.await?;
                prepared.tail.collect_file_artifacts = false;
                prepared.tail.auto_generate_title = false;
                prepared.tail.suggestion_generator = None;
                let build = prepared.host;
                prepared.host = Box::new(move |binding| {
                    validate_parent(&binding, &parent)?;
                    let factory = build(binding)?;
                    Ok(factory
                        .with_native_factory(Arc::new(CurrentFactory(settings.clone())))
                        .with_port_decorators(current_decorators(settings))
                        .with_durable_message_observer(DurableMessageObserver {
                            matches: Arc::new(|_, _| true),
                            on_persisted: Arc::new(|_, _| Box::pin(async { Ok(()) })),
                        }))
                });
                Ok(prepared)
            })
        }),
        create_storage: Arc::new(move |_| {
            let key = key.clone();
            Box::pin(async move {
                Ok(SessionStorage::Unlocked(FactoryStorage::new(
                    Arc::new(JsonlTranscriptStore::new()),
                    key,
                )))
            })
        }),
        admission: None,
        diagnostic: Arc::new(|_| {}),
        tool_results_dir: Some(Arc::new(move |context| {
            super::tools::session_tool_results_dir(&project, &context.session_key)
        })),
    })
}

fn validate_parent(binding: &SessionBinding, parent: &JsString) -> Result<(), RuntimeError> {
    let current = binding
        .projection
        .snapshot()?
        .metadata
        .get("parentSessionId")
        .cloned();
    if current.as_ref().is_some_and(|value| match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
        JsValue::String(value) => !value.is_empty(),
        _ => true,
    }) {
        if current.as_ref() != Some(&JsValue::String(parent.clone())) {
            return Err(RuntimeError::new("SOP_NODE_PARENT_MISMATCH"));
        }
    } else {
        let mut metadata = JsObject::new();
        metadata.insert("parentSessionId".into(), JsValue::String(parent.clone()));
        binding
            .recorder
            .writer()
            .record_session_metadata_checked(
                &binding.context.session_key,
                &"sop-node-created".into(),
                JsValue::Object(metadata),
            )
            .map_err(|error| RuntimeError::new(error.to_string()))?;
    }
    Ok(())
}
fn validate_spec(parent: &JsString, spec: &NodeSessionSpec) -> Result<(), HostError> {
    let encode = crate::channel::http_client::encode_uri_component;
    let skill = spec.skill_id.to_utf8().map_err(host_error)?;
    let node = spec.node_id.to_utf8().map_err(host_error)?;
    let suffix = JsString::from(format!(":sop-node:{}:{}", encode(&skill), encode(&node)));
    let expected_session = JsString::from_utf16(
        parent
            .units()
            .iter()
            .chain(suffix.units())
            .copied()
            .collect(),
    );
    let pair = JsValue::Array(vec![
        JsValue::String(spec.skill_id.clone()),
        JsValue::String(spec.node_id.clone()),
    ]);
    let encoded_pair = pilotdeck_protocol::stringify_json(&pair)
        .map_err(host_error)?
        .expect("array is serializable");
    if &spec.parent_session_id != parent
        || spec.session_id != expected_session
        || !spec
            .sidechain_id
            .equals_str(&format!("sop-node-{}", encode(&encoded_pair)))
    {
        return Err(host_error("SOP_NODE_PARENT_MISMATCH"));
    }
    Ok(())
}
struct CurrentFactory(Settings);
impl ExecutionFactory for CurrentFactory {
    fn execute(
        &self,
        request: &ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> PortResult<ExecutionTerminal> {
        let factory = lock(&self.0)
            .as_ref()
            .map(|settings| settings.factory.clone())
            .ok_or_else(|| PortError::InvalidResponse {
                message: "SOP node has no admitted turn.".into(),
            })?;
        factory.execute(request, context)
    }
}
fn current_decorators(settings: Settings) -> HostPortDecorators {
    let model = settings.clone();
    let context = settings.clone();
    HostPortDecorators {
        model: Some(Arc::new(move |identity, port| {
            let decorator = lock(&model)
                .as_ref()
                .and_then(|settings| settings.decorators.model.clone());
            decorator.map_or_else(|| port.clone(), |decorate| decorate(identity, port.clone()))
        })),
        context: Some(Arc::new(move |identity, port| {
            let decorator = lock(&context)
                .as_ref()
                .and_then(|settings| settings.decorators.context.clone());
            decorator.map_or_else(|| port.clone(), |decorate| decorate(identity, port.clone()))
        })),
        tool_execution: Some(Arc::new(move |identity, port| {
            let decorator = lock(&settings)
                .as_ref()
                .and_then(|settings| settings.decorators.tool_execution.clone());
            decorator.map_or_else(|| port.clone(), |decorate| decorate(identity, port.clone()))
        })),
    }
}
fn host_error(error: impl std::fmt::Display) -> HostError {
    ModuleError::new(error.to_string()).into()
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        project::{DisposeFuture, ProjectRuntimeRegistry, ResourceBundle, RuntimeResult},
        session::{
            LiveSession, LiveTurn, SessionNext,
            factory::{PreparedResources, PreparedSession},
        },
        turn::{
            host_factory::{Deployment, TurnHostFactory},
            runner::tail::TailConfig,
        },
    };
    use std::{
        collections::VecDeque,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("pdrs-sop-nodes-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn spec(node: &str) -> NodeSessionSpec {
        let pair = pilotdeck_protocol::stringify_json(&JsValue::Array(vec![
            "skill/a".into(),
            node.into(),
        ]))
        .unwrap()
        .unwrap();
        NodeSessionSpec {
            parent_session_id: "parent".into(),
            session_id: format!(
                "parent:sop-node:skill%2Fa:{}",
                crate::channel::http_client::encode_uri_component(node)
            )
            .into(),
            sidechain_id: format!(
                "sop-node-{}",
                crate::channel::http_client::encode_uri_component(&pair)
            )
            .into(),
            skill_id: "skill/a".into(),
            node_id: node.into(),
        }
    }
    fn fixture(root: &Path) -> (NodeSessions, Arc<Mutex<Vec<SessionBinding>>>) {
        let registry = Arc::new(ProjectRuntimeRegistry::new(
            root,
            Arc::new(|_: &Path, _: &mut ResourceBundle| Ok(())),
        ));
        let bindings: Arc<Mutex<Vec<SessionBinding>>> = Arc::default();
        let captured = bindings.clone();
        let prepare: Prepare = Arc::new(move |_, _| {
            let registry = registry.clone();
            let captured = captured.clone();
            Box::pin(async move {
                Ok(PreparedSession {
                    resources: PreparedResources::default(),
                    host: Box::new(move |binding| {
                        lock(&captured).push(binding);
                        Ok(TurnHostFactory::new(
                            registry,
                            Deployment::Native,
                            crate::host::Limits::default(),
                            None,
                            |_, _| panic!("restore does not execute a turn"),
                        ))
                    }),
                    tail: TailConfig::default(),
                })
            })
        });
        (
            NodeSessions::new(
                SessionContext {
                    session_key: "parent".into(),
                    project_key: Some(root.to_string_lossy().as_ref().into()),
                    channel_key: "web".into(),
                    allowed_tools: None,
                    disallowed_tools: None,
                },
                TranscriptKey {
                    project_root: root.to_owned(),
                    pilot_home: root.join("home"),
                    session_id: "parent".into(),
                    transcript_path: root.join("chats/parent.jsonl"),
                },
                InteractionEvents::default(),
                prepare,
                Duration::from_secs(2),
            ),
            bindings,
        )
    }

    #[tokio::test]
    async fn node_writer_restores_exact_sidechain_and_rejects_another_parent() {
        let root = Scratch::new();
        let (provider, bindings) = fixture(&root.0);
        let spec = spec("first node");
        validate_spec(&"parent".into(), &spec).unwrap();
        let owner = node_factory(&provider.0, &spec, Arc::default());
        let mut context = provider.0.parent.clone();
        context.session_key = spec.session_id.clone();
        let first = owner.create(context.clone(), None).await.unwrap();
        let binding = lock(&bindings).last().unwrap().clone();
        let path = binding.recorder.writer().path().to_owned();
        assert_eq!(
            path,
            root.0.join("chats/parent/subagents").join(format!(
                "{}.jsonl",
                crate::storage::sanitize_session_id(&spec.sidechain_id)
            ))
        );
        assert_eq!(
            binding
                .projection
                .snapshot()
                .unwrap()
                .metadata
                .get("parentSessionId"),
            Some(&JsValue::String("parent".into()))
        );
        let before = std::fs::read(&path).unwrap();
        first.dispose("test".into()).await.unwrap();
        let second = owner.create(context.clone(), None).await.unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "restore never duplicates sop-node-created"
        );
        let binding = lock(&bindings).last().unwrap().clone();
        let mut metadata = JsObject::new();
        metadata.insert("parentSessionId".into(), "other".into());
        binding
            .recorder
            .writer()
            .record_session_metadata_checked(
                &spec.session_id,
                &"test-corruption".into(),
                JsValue::Object(metadata),
            )
            .unwrap();
        second.dispose("test".into()).await.unwrap();
        assert!(
            owner.create(context, None).await.err().unwrap().message == "SOP_NODE_PARENT_MISMATCH"
        );
        owner.dispose().await.unwrap();
        provider.close().await.unwrap();
        assert!(
            !root.0.join("chats/parent.jsonl").exists(),
            "node writes never create the parent transcript"
        );
        let mut forged = spec.clone();
        forged.sidechain_id = "../../outside".into();
        assert!(validate_spec(&"parent".into(), &forged).is_err());
    }

    struct MockSession {
        stopped: Cancellation,
        entered: Arc<tokio::sync::Notify>,
        drained: Arc<AtomicUsize>,
        disposed: Arc<AtomicUsize>,
    }
    struct MockTurn {
        events: VecDeque<JsValue>,
        stopped: Cancellation,
        entered: Arc<tokio::sync::Notify>,
        drained: Arc<AtomicUsize>,
        done: bool,
    }
    impl LiveTurn for MockTurn {
        fn next(&mut self) -> SessionNext<'_> {
            Box::pin(async move {
                if let Some(event) = self.events.pop_front() {
                    return Ok(Some(event));
                }
                if !self.done {
                    self.entered.notify_one();
                    self.stopped.cancelled().await;
                    self.done = true;
                    self.drained.fetch_add(1, Ordering::SeqCst);
                    return Ok(Some(pilotdeck_protocol::parse_json(r#"{"type":"turn_completed","sessionId":"node","turnId":"turn","result":{"type":"aborted"}}"#).unwrap()));
                }
                Ok(None)
            })
        }
    }
    impl LiveSession for MockSession {
        fn submit(
            &self,
            input: JsValue,
            options: JsValue,
        ) -> Result<Box<dyn LiveTurn>, RuntimeError> {
            let accepted = crate::runtime::turn::runner::TurnInput::from_agent_input(
                &input,
                &options,
                Vec::new(),
                Vec::new(),
            )
            .expect("node submissions must use the formal AgentInput envelope");
            assert_eq!(
                accepted.accepted_messages()[0].get("content"),
                Some(
                    &pilotdeck_protocol::parse_json(
                        r#"[{"type":"text","text":"latest parent input"}]"#
                    )
                    .unwrap()
                ),
                "the node recorder receives the original content blocks"
            );
            assert_eq!(options.get("turnId"), Some(&JsValue::String("turn".into())));
            Ok(Box::new(MockTurn {
                events: [
                    r#"{"type":"turn_started","sessionId":"node","turnId":"turn"}"#,
                    r#"{"type":"input_accepted","sessionId":"node","turnId":"turn"}"#,
                    r#"{"type":"user_prompt_submitted","sessionId":"node","turnId":"turn","prompt":"latest parent input"}"#,
                    r#"{"type":"assistant_message","sessionId":"node","turnId":"turn","message":{"role":"assistant","content":[]}}"#,
                ].into_iter().map(|text| pilotdeck_protocol::parse_json(text).unwrap()).collect(),
                stopped: self.stopped.clone(), entered: self.entered.clone(), drained: self.drained.clone(), done: false,
            }))
        }
        fn snapshot(&self) -> JsValue {
            pilotdeck_protocol::parse_json(r#"{"messages":[]}"#).unwrap()
        }
        fn abort(&self, _: &JsString) -> RuntimeResult {
            self.stopped.abort();
            Ok(())
        }
        fn dispose(&self) -> DisposeFuture<'_> {
            self.disposed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }
    struct UnusedFactory;
    impl ExecutionFactory for UnusedFactory {
        fn execute(
            &self,
            _: &ExecuteRequest,
            _: &mut ExecutionContext,
        ) -> PortResult<ExecutionTerminal> {
            panic!("mock session owns execution")
        }
    }
    #[tokio::test]
    async fn dropped_node_waiter_aborts_and_drains_then_releases_turn_settings() {
        let root = Scratch::new();
        let (provider, _) = fixture(&root.0);
        let mut router = crate::host::modules::ModuleRouter::default();
        let buffer = provider
            .0
            .events
            .bind_turn(
                crate::host::modules::event::TurnIdentity::new("parent", "turn"),
                "run".into(),
                &mut router,
            )
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let drained = Arc::new(AtomicUsize::new(0));
        let disposed = Arc::new(AtomicUsize::new(0));
        let spec = spec("first");
        let settings: Settings = Arc::default();
        let owner = node_factory(&provider.0, &spec, settings.clone());
        let session = LiveSessionHandle::new(
            Arc::new(MockSession {
                stopped: Cancellation::new(),
                entered: entered.clone(),
                drained: drained.clone(),
                disposed: disposed.clone(),
            }),
            None,
        );
        provider.0.nodes.lock().await.insert(
            spec.session_id.clone(),
            Node {
                session,
                owner,
                settings: settings.clone(),
            },
        );
        let factory = Arc::new(UnusedFactory);
        let weak = Arc::downgrade(&factory);
        let turn = provider.run(NodeTurnInput {
            spec,
            content: pilotdeck_protocol::parse_json(
                r#"[{"type":"text","text":"latest parent input"}]"#,
            )
            .unwrap(),
            options: pilotdeck_protocol::parse_json(r#"{"turnId":"turn"}"#).unwrap(),
            decorators: HostPortDecorators::default(),
            factory,
            signal: Cancellation::new(),
        });
        let waiter = tokio::spawn(turn);
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        let events = buffer.drain_sourced().unwrap();
        assert_eq!(
            events.len(),
            2,
            "node admission and final terminal are not forwarded"
        );
        assert_eq!(
            events[0].event.get("sessionId"),
            Some(&JsValue::String("parent".into()))
        );
        assert!(events[0].nested_prompt);
        assert!(!events[1].nested_prompt);
        waiter.abort();
        let _ = waiter.await;
        tokio::time::timeout(Duration::from_secs(3), provider.close())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(drained.load(Ordering::SeqCst), 1);
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
        assert!(lock(&settings).is_none());
        assert!(
            weak.upgrade().is_none(),
            "cached node must not retain the outer SOP runtime through its per-turn factory"
        );
    }
}
