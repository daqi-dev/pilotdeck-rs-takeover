//! Continuable background subagents: the native host's port of
//! `SubagentContinuationManager` + `NativeSubagentContinuationHost` +
//! `AgentTurnInbox` (agent/sub/SubagentContinuationManager.ts,
//! agent/sub/NativeSubagentContinuationHost.ts, agent/session/AgentTurnInbox.ts,
//! agent/scope/AgentHandle.ts `followup` / `runQueuedPump`).
//!
//! A continuable child is a durable session of its own under the parent's
//! `subagents/<id>.jsonl`, with a FIFO inbox: every message (the first one
//! included) is admitted as an `agent_turn_enqueued` entry, and the child's
//! pump starts the next turn with `turn_started.inboxItemId`. Once the child
//! is idle with nothing queued and no live child of its own, it settles: it
//! is disposed and its parent receives `Continuable subagent <id> settled:
//! <state>.` as a follow-up of its own.
//!
//! A parent is either a Gateway session ([`SessionParent`]: its follow-ups
//! become host-started turns of the live session, run when the session is
//! idle, with no client stream) or another continuable child ([`Activation`]:
//! its follow-ups join that child's inbox). A child the process no longer
//! holds is resumed from its transcript when a message is sent to it.
use super::{
    subagents::{SubagentRuntime, continuable_child_config},
    tools::ForkSeed,
};
use crate::{
    runtime::{
        session::{LiveSession, SessionOwner, factory::FactoryStorage},
        tools::builtin::{
            agent::{SubagentDefinition, subagent_definition},
            subagent_continuation::{AdmissionFuture, ContinuationAdmission, ContinuationPort},
        },
        turn::live::LiveSessionAdapter,
    },
    storage::{JsonlTranscriptStore, TranscriptKey, TranscriptStore, recorder::Recorder},
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Notify;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// `pilotdeck-native`: the only provider family this host composes.
const PROVIDER: &str = "pilotdeck-native";
/// `SUBAGENT_CONTINUABLE_DESCRIPTOR_VERSION`.
const DESCRIPTOR_VERSION: f64 = 2.0;
const ONE_SHOT_DESCRIPTOR_VERSION: f64 = 1.0;

fn text(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}

fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in entries {
        result.insert(key.into(), value);
    }
    result
}

fn new_id() -> JsString {
    uuid::Uuid::new_v4().to_string().into()
}

fn is_blank(value: &JsString) -> bool {
    text(value).trim().is_empty()
}

/// One admitted, not yet started turn (`AgentQueuedTurn`).
#[derive(Clone)]
struct Queued {
    item: JsString,
    turn: JsString,
    input: JsValue,
}

fn text_input(message: JsString) -> JsValue {
    JsValue::Object(object([
        ("type", "text".into()),
        ("text", JsValue::String(message)),
    ]))
}

/// `recordTurnEnqueued`: the durable admission of one queued turn.
async fn record_enqueued(
    recorder: Arc<Recorder>,
    session: JsString,
    queued: &Queued,
) -> Result<(), JsString> {
    let draft = object([
        ("type", "agent_turn_enqueued".into()),
        ("itemId", JsValue::String(queued.item.clone())),
        ("input", queued.input.clone()),
        ("submitOptions", JsValue::Object(JsObject::new())),
    ]);
    let turn = queued.turn.clone();
    record(recorder, session, turn, draft).await
}

/// `recordTurnDiscarded(..., { itemId, reason: "agent_disposed" })`.
async fn record_discarded(
    recorder: Arc<Recorder>,
    session: JsString,
    queued: &Queued,
) -> Result<(), JsString> {
    let draft = object([
        ("type", "agent_turn_discarded".into()),
        ("itemId", JsValue::String(queued.item.clone())),
        ("reason", "agent_disposed".into()),
    ]);
    record(recorder, session, queued.turn.clone(), draft).await
}

async fn record(
    recorder: Arc<Recorder>,
    session: JsString,
    turn: JsString,
    draft: JsObject,
) -> Result<(), JsString> {
    tokio::task::spawn_blocking(move || {
        recorder
            .writer()
            .record_session_event(&session, &turn, draft)
            .map_err(|error| JsString::from(error.to_string()))
    })
    .await
    .map_err(|error| JsString::from(error.to_string()))?
}

/// The agent a continuable child reports to.
pub(super) trait ContinuationParent: Send + Sync {
    fn session_id(&self) -> JsString;
    /// `isLiveParent`: the exact parent is still admitting work.
    fn is_live(&self) -> bool;
    /// The directory holding every session's `<id>/subagents/` tree.
    fn chat_dir(&self) -> PathBuf;
    /// `parent.followup({ type: "text", text })`: durable admission only.
    fn followup(self: Arc<Self>, message: JsString) -> BoxFuture<Result<(), JsString>>;
    /// The parent's own activation when it is itself a continuable child.
    fn activation(self: Arc<Self>) -> Option<Arc<Activation>> {
        None
    }
}

fn same_parent(a: &Arc<dyn ContinuationParent>, b: &Arc<dyn ContinuationParent>) -> bool {
    std::ptr::eq(Arc::as_ptr(a) as *const (), Arc::as_ptr(b) as *const ())
}

/// Where a session's tools find the agent they act for. Set once the agent
/// exists; held weakly so the agent's own tools do not keep it alive.
#[derive(Clone, Default)]
pub(super) struct ParentRef(Arc<OnceLock<Weak<dyn ContinuationParent>>>);
impl ParentRef {
    fn bind(&self, parent: Weak<dyn ContinuationParent>) {
        let _ = self.0.set(parent);
    }
    fn get(&self) -> Option<Arc<dyn ContinuationParent>> {
        self.0.get().and_then(Weak::upgrade)
    }
}

/// The process-wide continuation runtime (`GatewaySubagentRuntimeBundle`'s
/// manager): every live continuable child, by id.
#[derive(Default)]
pub(super) struct Continuations {
    activations: Mutex<HashMap<JsString, Arc<Activation>>>,
    /// `ChildOperationLock`: one operation per child id at a time.
    locks: Mutex<HashMap<JsString, Arc<tokio::sync::Mutex<()>>>>,
    owner: OnceLock<SessionOwner>,
}

impl Continuations {
    /// The Gateway's session owner, for the parents' live handles.
    pub(super) fn bind_owner(&self, owner: SessionOwner) {
        let _ = self.owner.set(owner);
    }

    fn lock_for(&self, child: &JsString) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(child.clone())
            .or_default()
            .clone()
    }

    fn activation(&self, child: &JsString) -> Option<Arc<Activation>> {
        self.activations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(child)
            .cloned()
    }

    /// A Gateway session as a continuation parent (`bindParent` plus the
    /// session's dispose-start `drainDescendants`). The caller sets the
    /// recorder once the session's storage is bound and calls
    /// [`SessionParent::release`] when the session's resources are released.
    pub(super) fn bind_session(self: &Arc<Self>, session: JsString) -> Arc<SessionParent> {
        Arc::new(SessionParent {
            session,
            manager: Arc::downgrade(self),
            recorder: OnceLock::new(),
            alive: AtomicBool::new(true),
            released: Notify::new(),
            inbox: Mutex::new(SessionInbox::default()),
        })
    }

    /// `drainDescendants(parent)`: dispose every child the parent owns,
    /// directly or through its own children.
    async fn drain_descendants(&self, parent: &Arc<dyn ContinuationParent>) {
        let targets: Vec<Arc<Activation>> = self
            .activations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|activation| same_parent(&activation.parent, parent))
            .cloned()
            .collect();
        for activation in targets {
            activation.force_dispose().await;
        }
    }
}

#[derive(Default)]
struct SessionInbox {
    queue: VecDeque<Queued>,
    pumping: bool,
}

/// A Gateway session as the parent of continuable children. Its follow-ups
/// are host-started turns: admitted durably to the session's transcript and
/// started on the live session as soon as it is idle.
pub(super) struct SessionParent {
    session: JsString,
    manager: Weak<Continuations>,
    recorder: OnceLock<Arc<Recorder>>,
    alive: AtomicBool,
    released: Notify,
    inbox: Mutex<SessionInbox>,
}

impl SessionParent {
    pub(super) fn bind_recorder(&self, recorder: Arc<Recorder>) {
        let _ = self.recorder.set(recorder);
    }

    /// The session's tools act for this parent.
    pub(super) fn parent_ref(self: &Arc<Self>) -> ParentRef {
        let parent = ParentRef::default();
        let this: Arc<dyn ContinuationParent> = self.clone();
        parent.bind(Arc::downgrade(&this));
        parent
    }

    /// The session stops admitting work: its children are disposed first
    /// (no settlement notice reaches a parent that is going away), then its
    /// queued follow-ups are discarded (`AgentHandle.finishDispose`).
    pub(super) async fn release(self: Arc<Self>) {
        if !self.alive.swap(false, Ordering::SeqCst) {
            return;
        }
        self.released.notify_waiters();
        if let Some(manager) = self.manager.upgrade() {
            let this: Arc<dyn ContinuationParent> = self.clone();
            manager.drain_descendants(&this).await;
        }
        let pending: Vec<Queued> = {
            let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
            inbox.queue.drain(..).collect()
        };
        if let Some(recorder) = self.recorder.get() {
            for queued in &pending {
                let _ = record_discarded(recorder.clone(), self.session.clone(), queued).await;
            }
        }
    }

    /// `runQueuedPump`: start each queued follow-up once the session is idle.
    async fn pump(self: Arc<Self>) {
        loop {
            let next = {
                let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
                match inbox.queue.front() {
                    Some(next) if self.alive.load(Ordering::SeqCst) => next.clone(),
                    _ => {
                        inbox.pumping = false;
                        return;
                    }
                }
            };
            let Some(handle) = self
                .manager
                .upgrade()
                .and_then(|manager| manager.owner.get().cloned())
                .and_then(|owner| owner.current_handle(&self.session))
            else {
                self.inbox.lock().unwrap_or_else(|e| e.into_inner()).pumping = false;
                return;
            };
            let released = self.released.notified();
            tokio::pin!(released);
            let idle = tokio::select! {
                _ = &mut released => false,
                idle = handle.wait_idle() => idle,
            };
            if !idle || !self.alive.load(Ordering::SeqCst) {
                self.inbox.lock().unwrap_or_else(|e| e.into_inner()).pumping = false;
                return;
            }
            let options = JsValue::Object(object([
                ("turnId", JsValue::String(next.turn.clone())),
                ("inboxItemId", JsValue::String(next.item.clone())),
            ]));
            let mut submission = match handle.submit_queued(next.input.clone(), options) {
                Ok(submission) => submission,
                // `runQueuedPump`'s catch: the pump stops (`queuedPumpBlocked`)
                // and the item stays pending, so ordinary submits stay refused.
                Err(_) => {
                    self.inbox.lock().unwrap_or_else(|e| e.into_inner()).pumping = false;
                    return;
                }
            };
            self.inbox
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .queue
                .pop_front();
            // Follow-up has admission semantics, not a second result stream.
            while let Ok(Some(_)) = submission.next().await {}
        }
    }
}

impl ContinuationParent for SessionParent {
    fn session_id(&self) -> JsString {
        self.session.clone()
    }
    fn is_live(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    fn chat_dir(&self) -> PathBuf {
        self.recorder
            .get()
            .and_then(|recorder| recorder.writer().path().parent().map(PathBuf::from))
            .unwrap_or_default()
    }
    fn followup(self: Arc<Self>, message: JsString) -> BoxFuture<Result<(), JsString>> {
        Box::pin(async move {
            let (Some(recorder), true) = (self.recorder.get().cloned(), self.is_live()) else {
                return Err("Cannot enqueue a follow-up turn: agent handle is not active.".into());
            };
            let queued = Queued {
                item: new_id(),
                turn: new_id(),
                input: text_input(message),
            };
            record_enqueued(recorder, self.session.clone(), &queued).await?;
            if let Some(handle) = self
                .manager
                .upgrade()
                .and_then(|manager| manager.owner.get().cloned())
                .and_then(|owner| owner.current_handle(&self.session))
            {
                handle.admit_queued();
            }
            let start = {
                let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
                inbox.queue.push_back(queued);
                !std::mem::replace(&mut inbox.pumping, true)
            };
            if start {
                tokio::spawn(self.clone().pump());
            }
            Ok(())
        })
    }
}

#[derive(Default)]
struct ActivationState {
    queue: VecDeque<Queued>,
    /// Live continuable children this child owns.
    owned: usize,
    /// Disposal started: no more turns, and the queue is discarded.
    disposing: bool,
    /// The caller was handed an accepted message id for this activation.
    announced: bool,
}

/// One live continuable child (`Activation` of the manager plus its
/// `AgentHandle`).
pub(super) struct Activation {
    id: JsString,
    parent: Arc<dyn ContinuationParent>,
    manager: Weak<Continuations>,
    session: Arc<LiveSessionAdapter>,
    storage: Arc<FactoryStorage>,
    chat_dir: PathBuf,
    state: Mutex<ActivationState>,
    wake: Notify,
    settled: Notify,
    done: AtomicBool,
    /// The pump runs (set when the first message is announced).
    pumping: AtomicBool,
}

impl Activation {
    fn recorder(&self) -> Arc<Recorder> {
        self.storage.recorder.clone()
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ActivationState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `handle.followup(input, { itemId, turnId })` on the child: durable
    /// admission, then the pump picks it up.
    async fn admit(self: &Arc<Self>, queued: Queued) -> Result<ContinuationAdmission, JsString> {
        record_enqueued(self.recorder(), self.id.clone(), &queued).await?;
        self.lock_state().queue.push_back(queued.clone());
        self.wake.notify_one();
        Ok(ContinuationAdmission {
            child_session_id: self.id.clone(),
            item_id: queued.item,
            turn_id: queued.turn,
        })
    }

    /// `runQueuedPump` + `watchSettlement`: run queued turns in order; settle
    /// once idle with nothing queued and no owned child left.
    async fn pump(self: Arc<Self>) {
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        let lock = manager.lock_for(&self.id);
        enum Step {
            Run(Queued),
            Wait,
            Settle(&'static str),
        }
        loop {
            let guard = lock.lock().await;
            let wake = self.wake.notified();
            let step = {
                let mut state = self.lock_state();
                if state.disposing {
                    Step::Settle("parent_agent_disposed")
                } else if let Some(next) = state.queue.pop_front() {
                    Step::Run(next)
                } else if state.owned > 0 {
                    Step::Wait
                } else {
                    Step::Settle("continuable_subagent_settled")
                }
            };
            match step {
                Step::Run(next) => {
                    drop(guard);
                    self.run_turn(next).await;
                }
                // Waiting on an owned child: a release or new work wakes it.
                Step::Wait => {
                    drop(guard);
                    wake.await;
                }
                Step::Settle(reason) => {
                    self.settle(&manager, reason).await;
                    drop(guard);
                    return;
                }
            }
        }
    }

    /// `submitQueuedTurn`: drain one turn of the child.
    async fn run_turn(&self, next: Queued) {
        let options = JsValue::Object(object([
            ("sessionId", JsValue::String(self.id.clone())),
            ("turnId", JsValue::String(next.turn.clone())),
            ("inboxItemId", JsValue::String(next.item.clone())),
        ]));
        if let Ok(mut turn) = self.session.submit(next.input.clone(), options) {
            while let Ok(Some(_)) = turn.next().await {}
        }
    }

    /// `finishActivationDisposal`.
    async fn settle(self: &Arc<Self>, manager: &Arc<Continuations>, reason: &str) {
        let _ = reason;
        // Owned children go first (`finishActivationDisposal`).
        let this: Arc<dyn ContinuationParent> = self.clone();
        Box::pin(manager.drain_descendants(&this)).await;
        // `terminalFor`: the session's state before it is torn down.
        let status = self.session.snapshot();
        let state = match status.get("status").and_then(JsValue::as_string).map(text) {
            Some(status) if status == "aborted" => "aborted",
            Some(status) if status == "failed" => "failed",
            Some(status) if status == "idle" => "completed",
            _ => "unknown",
        };
        // Disposal discards what is still queued.
        let pending: Vec<Queued> = self.lock_state().queue.drain(..).collect();
        for queued in &pending {
            let _ = record_discarded(self.recorder(), self.id.clone(), queued).await;
        }
        let _ = self.session.dispose().await;
        let _ = self.storage.close_sidechain().await;
        {
            let mut activations = manager
                .activations
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if activations
                .get(&self.id)
                .is_some_and(|current| Arc::ptr_eq(current, self))
            {
                activations.remove(&self.id);
            }
        }
        // The notice is admitted while the ownership edge still exists, so an
        // idle continuable parent cannot settle before it arrives.
        let announced = self.lock_state().announced;
        if announced && self.parent.is_live() {
            let message = format!("Continuable subagent {} settled: {state}.", text(&self.id));
            let _ = self.parent.clone().followup(message.as_str().into()).await;
        }
        if let Some(parent) = self.parent.clone().activation() {
            parent.lock_state().owned -= 1;
            parent.wake.notify_one();
        }
        self.done.store(true, Ordering::SeqCst);
        self.settled.notify_waiters();
    }

    /// Dispose on the parent's teardown: abort the running turn, discard
    /// the queue, settle without a notice (the parent is not live).
    async fn force_dispose(self: &Arc<Self>) {
        let settled = self.settled.notified();
        tokio::pin!(settled);
        settled.as_mut().enable();
        if self.done.load(Ordering::SeqCst) {
            return;
        }
        self.lock_state().disposing = true;
        let _ = self.session.abort(&"parent_agent_disposed".into());
        self.wake.notify_one();
        // Before its first admission the child has no pump: whoever is
        // starting it settles it once it sees the disposal.
        if !self.pumping.load(Ordering::SeqCst) {
            return;
        }
        settled.await;
    }
}

impl ContinuationParent for Activation {
    fn session_id(&self) -> JsString {
        self.id.clone()
    }
    fn is_live(&self) -> bool {
        !self.done.load(Ordering::SeqCst) && !self.lock_state().disposing
    }
    fn chat_dir(&self) -> PathBuf {
        self.chat_dir.clone()
    }
    fn followup(self: Arc<Self>, message: JsString) -> BoxFuture<Result<(), JsString>> {
        Box::pin(async move {
            if !self.is_live() {
                return Err("Cannot enqueue a follow-up turn: agent handle is not active.".into());
            }
            self.admit(Queued {
                item: new_id(),
                turn: new_id(),
                input: text_input(message),
            })
            .await
            .map(|_| ())
        })
    }
    fn activation(self: Arc<Self>) -> Option<Arc<Activation>> {
        Some(self)
    }
}

/// `parseSubagentDescriptor` for the fields a resume needs; `Ok(None)` for
/// an unknown version.
fn parse_descriptor(value: &JsValue) -> Result<Option<(bool, JsObject)>, String> {
    let Some(fields) = value.as_object() else {
        return Err("Persisted subagent descriptor must be an object.".into());
    };
    let Some(version) = fields.get("version").and_then(JsValue::as_number) else {
        return Err("Persisted subagent descriptor version must be a number.".into());
    };
    let continuable = if version == ONE_SHOT_DESCRIPTOR_VERSION {
        false
    } else if version == DESCRIPTOR_VERSION {
        true
    } else {
        return Ok(None);
    };
    let keys: &[&str] = if continuable {
        &[
            "version",
            "mode",
            "provider",
            "definitionId",
            "parentSessionId",
            "label",
            "agentProvider",
            "agentModel",
        ]
    } else {
        &["version", "mode", "provider", "definitionId"]
    };
    if let Some((key, _)) = fields
        .entries()
        .iter()
        .find(|(key, _)| !keys.iter().any(|known| key.equals_str(known)))
    {
        return Err(format!(
            "Persisted subagent descriptor has unknown field \"{}\".",
            text(key)
        ));
    }
    let mode = if continuable {
        "continuable"
    } else {
        "one-shot"
    };
    if fields.get("mode") != Some(&JsValue::from(mode)) {
        return Err(format!(
            "Persisted subagent descriptor mode must be \"{mode}\"."
        ));
    }
    for key in &keys[2..] {
        if !fields
            .get(key)
            .and_then(JsValue::as_string)
            .is_some_and(|value| !is_blank(value))
        {
            return Err(format!(
                "Subagent descriptor {key} must be a non-empty string."
            ));
        }
    }
    Ok(Some((continuable, fields.clone())))
}

fn descriptor_string(descriptor: &JsObject, key: &str) -> JsString {
    descriptor
        .get(key)
        .and_then(JsValue::as_string)
        .cloned()
        .unwrap_or_default()
}

/// `bindSubagentContinuationPort`: the continuation tools of one turn of the
/// parent agent.
pub(super) struct SessionContinuationPort {
    pub(super) manager: Arc<Continuations>,
    pub(super) parent: ParentRef,
    pub(super) runtime: SubagentRuntime,
    pub(super) seed: ForkSeed,
    /// The parent's runtime config for this turn.
    pub(super) config: JsObject,
}

impl SessionContinuationPort {
    fn live_parent(&self) -> Result<Arc<dyn ContinuationParent>, JsString> {
        match self.parent.get() {
            Some(parent) if parent.is_live() => Ok(parent),
            Some(parent) => Err(format!(
                "Continuable subagent operation requires the exact live parent agent: {}",
                text(&parent.session_id())
            )
            .into()),
            None => Err("Continuable subagent parent is not bound.".into()),
        }
    }

    /// `NativeSubagentContinuationHost.createChild`: the child session over
    /// its durable storage, with its own tools and its own inbox.
    async fn materialize(
        &self,
        parent: &Arc<dyn ContinuationParent>,
        child: &JsString,
        definition: Arc<SubagentDefinition>,
        model: (JsString, JsString),
        resume: bool,
    ) -> Result<Arc<Activation>, JsString> {
        let chat_dir = parent.chat_dir();
        let config = continuable_child_config(&self.config, &definition, child, model);
        let slot = ParentRef::default();
        let (session, storage) = self
            .runtime
            .materialize_continuable(
                self.seed.clone(),
                &self.config,
                config,
                definition,
                child,
                transcript_path(&chat_dir, &parent.session_id(), child),
                resume,
                slot.clone(),
            )
            .await?;
        let activation = Arc::new(Activation {
            id: child.clone(),
            parent: parent.clone(),
            manager: Arc::downgrade(&self.manager),
            session,
            storage,
            chat_dir,
            state: Mutex::new(ActivationState::default()),
            wake: Notify::new(),
            settled: Notify::new(),
            done: AtomicBool::new(false),
            pumping: AtomicBool::new(false),
        });
        let as_parent: Arc<dyn ContinuationParent> = activation.clone();
        slot.bind(Arc::downgrade(&as_parent));
        Ok(activation)
    }

    /// Publish a materialized child (`activations.set` + `acquireOwnership`)
    /// and start its pump.
    fn publish(&self, activation: &Arc<Activation>) -> Result<(), JsString> {
        if let Some(parent) = activation.parent.clone().activation() {
            let mut state = parent.lock_state();
            if state.disposing || parent.done.load(Ordering::SeqCst) {
                return Err(format!(
                    "Continuable parent is already disposing: {}",
                    text(&parent.id)
                )
                .into());
            }
            state.owned += 1;
        }
        self.manager
            .activations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(activation.id.clone(), activation.clone());
        Ok(())
    }

    /// Tear down a child whose pump never started (pre-admission rollback):
    /// no notice, it was never announced.
    async fn rollback(&self, activation: &Arc<Activation>) {
        let lock = self.manager.lock_for(&activation.id);
        let _guard = lock.try_lock();
        activation.lock_state().disposing = true;
        activation
            .settle(&self.manager, "continuable_subagent_admission_rollback")
            .await;
    }

    /// Admit the first message of a just-published child; on failure the
    /// child is torn down without a notice (it was never announced).
    async fn admit_first(
        &self,
        activation: &Arc<Activation>,
        queued: Queued,
    ) -> Result<ContinuationAdmission, JsString> {
        match activation.admit(queued).await {
            Ok(admission) => {
                activation.lock_state().announced = true;
                // `watchSettlement` starts once the caller holds an id.
                activation.pumping.store(true, Ordering::SeqCst);
                tokio::spawn(activation.clone().pump());
                Ok(admission)
            }
            Err(error) => {
                self.rollback(activation).await;
                Err(error)
            }
        }
    }

    async fn start(
        &self,
        label: JsString,
        definition_id: JsString,
        prompt: JsString,
    ) -> Result<ContinuationAdmission, JsString> {
        let Some(definition) = subagent_definition(&text(&definition_id)) else {
            return Err(format!("Unknown subagent type: {}", text(&definition_id)).into());
        };
        let child = new_id();
        let parent = self.live_parent()?;
        let parent_id = parent.session_id();
        if child == parent_id {
            return Err("A continuable subagent cannot use its parent session id.".into());
        }
        // `snapshotSubagentDescriptor`.
        let selected = self.config.get("subagentModel");
        let pick = |key: &str| {
            selected
                .and_then(|selected| selected.get(key))
                .or_else(|| self.config.get(key))
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default()
        };
        let (agent_provider, agent_model) = (pick("provider"), pick("model"));
        for (key, value) in [
            ("parentSessionId", &parent_id),
            ("label", &label),
            ("agentProvider", &agent_provider),
            ("agentModel", &agent_model),
        ] {
            if is_blank(value) {
                return Err(
                    format!("Subagent descriptor {key} must be a non-empty string.").into(),
                );
            }
        }
        let descriptor = object([
            ("version", JsValue::Number(DESCRIPTOR_VERSION)),
            ("mode", "continuable".into()),
            ("provider", PROVIDER.into()),
            ("definitionId", definition.id.as_str().into()),
            ("parentSessionId", JsValue::String(parent_id)),
            ("label", JsValue::String(label)),
            ("agentProvider", JsValue::String(agent_provider.clone())),
            ("agentModel", JsValue::String(agent_model.clone())),
        ]);
        let queued = Queued {
            item: new_id(),
            turn: new_id(),
            input: text_input(prompt),
        };
        let lock = self.manager.lock_for(&child);
        let _guard = lock.lock().await;
        let parent = self.live_parent()?;
        if self.manager.activation(&child).is_some() {
            return Err(format!("Continuable subagent already exists: {}", text(&child)).into());
        }
        let activation = self
            .materialize(
                &parent,
                &child,
                definition,
                (agent_provider, agent_model),
                false,
            )
            .await?;
        if let Err(error) = self.publish(&activation) {
            self.rollback(&activation).await;
            return Err(error);
        }
        // `recordSubagentDescriptor(turnId, descriptor)` before the first input.
        let draft = object([
            ("type", "subagent_descriptor".into()),
            ("descriptor", JsValue::Object(descriptor)),
        ]);
        if let Err(error) = record(
            activation.recorder(),
            child.clone(),
            queued.turn.clone(),
            draft,
        )
        .await
        {
            self.rollback(&activation).await;
            return Err(error);
        }
        self.admit_first(&activation, queued).await
    }

    async fn followup(
        &self,
        child: JsString,
        message: JsString,
    ) -> Result<ContinuationAdmission, JsString> {
        if is_blank(&child) {
            return Err("childSessionId must not be empty.".into());
        }
        self.live_parent()?;
        let lock = self.manager.lock_for(&child);
        let queued = Queued {
            item: new_id(),
            turn: new_id(),
            input: text_input(message),
        };
        loop {
            let guard = lock.lock().await;
            let parent = self.live_parent()?;
            let Some(activation) = self.manager.activation(&child) else {
                let admitted = self.cold_resume(&parent, child, queued).await;
                drop(guard);
                return admitted;
            };
            if !same_parent(&activation.parent, &parent) {
                return Err(format!(
                    "Continuable subagent {} belongs to another live parent agent.",
                    text(&child)
                )
                .into());
            }
            if activation.is_live() {
                return activation.admit(queued).await;
            }
            // A releasing child is resumed from its transcript once its
            // disposal is done; its pump settles under this same lock.
            drop(guard);
            activation.force_dispose().await;
        }
    }

    /// `coldResume`: rebuild a child the process no longer holds from its
    /// transcript.
    async fn cold_resume(
        &self,
        parent: &Arc<dyn ContinuationParent>,
        child: JsString,
        queued: Queued,
    ) -> Result<ContinuationAdmission, JsString> {
        let not_resumable = || {
            JsString::from(format!(
                "Continuable subagent is not resumable: {}",
                text(&child)
            ))
        };
        let path = transcript_path(&parent.chat_dir(), &parent.session_id(), &child);
        let key = TranscriptKey {
            project_root: PathBuf::new(),
            pilot_home: PathBuf::new(),
            session_id: child.clone(),
            transcript_path: path,
        };
        let read =
            tokio::task::spawn_blocking(move || JsonlTranscriptStore::new().read(&key, None))
                .await
                .map_err(|_| not_resumable())?
                .map_err(|_| not_resumable())?;
        let entries: Vec<&JsValue> = read
            .entries
            .iter()
            .filter(|entry| entry.get("sessionId") == Some(&JsValue::String(child.clone())))
            .collect();
        let descriptor = match entries
            .iter()
            .find(|entry| entry.get("type") == Some(&"subagent_descriptor".into()))
        {
            None => None,
            Some(entry) => parse_descriptor(entry.get("descriptor").unwrap_or(&JsValue::Undefined))
                .map_err(|_| {
                    JsString::from(format!(
                        "Continuable subagent has invalid durable state: {}",
                        text(&child)
                    ))
                })?,
        };
        let Some((true, descriptor)) = descriptor else {
            return Err(not_resumable());
        };
        let parent = self.live_parent()?;
        if descriptor_string(&descriptor, "parentSessionId") != parent.session_id() {
            return Err(format!(
                "Continuable subagent {} belongs to another parent session.",
                text(&child)
            )
            .into());
        }
        let definition_id = descriptor_string(&descriptor, "definitionId");
        let Some(definition) = subagent_definition(&text(&definition_id)) else {
            return Err(
                format!("Unknown persisted subagent type: {}", text(&definition_id)).into(),
            );
        };
        if self.manager.activation(&child).is_some() {
            return Err(format!("Continuable subagent already exists: {}", text(&child)).into());
        }
        let model = (
            descriptor_string(&descriptor, "agentProvider"),
            descriptor_string(&descriptor, "agentModel"),
        );
        let activation = self
            .materialize(&parent, &child, definition, model, true)
            .await?;
        if let Err(error) = self.publish(&activation) {
            self.rollback(&activation).await;
            return Err(error);
        }
        self.admit_first(&activation, queued).await
    }
}

/// `subagentSessionStoragePaths`: `<chats>/<parent>/subagents/<child>.jsonl`.
fn transcript_path(chat_dir: &std::path::Path, parent: &JsString, child: &JsString) -> PathBuf {
    chat_dir
        .join(crate::storage::sanitize_session_id(parent))
        .join("subagents")
        .join(format!(
            "{}.jsonl",
            crate::storage::sanitize_session_id(child)
        ))
}

impl ContinuationPort for SessionContinuationPort {
    fn start(&self, label: JsString, definition_id: JsString, prompt: JsString) -> AdmissionFuture {
        let port = self.clone_port();
        Box::pin(async move { port.start(label, definition_id, prompt).await })
    }
    fn followup(&self, child_session_id: JsString, message: JsString) -> AdmissionFuture {
        let port = self.clone_port();
        Box::pin(async move { port.followup(child_session_id, message).await })
    }
}

impl SessionContinuationPort {
    fn clone_port(&self) -> Arc<Self> {
        Arc::new(Self {
            manager: self.manager.clone(),
            parent: self.parent.clone(),
            runtime: self.runtime.clone(),
            seed: self.seed.clone(),
            config: self.config.clone(),
        })
    }
}

#[cfg(test)]
#[path = "continuations_tests.rs"]
mod tests;
