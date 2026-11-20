//! `resources.watch` — the resource-version observation stream (t023 E, the
//! last unimplemented operation of the 285).
//!
//! Design note (contract `notes.workspace`, operations.json:100444): no new
//! global event bus is required — event notification plus conditional
//! reads/polling is the sanctioned shape, with the authoritative result
//! always coming from the resource snapshot tables. This implementation is
//! the pure conditional-polling form: every [`POLL_INTERVAL`] the source
//! re-resolves each watched resource through the SAME
//! [`resolve_resource_version`] that backs `resources.snapshot`
//! (`operations_ops.rs` — no side cache, no derived log), compares the
//! revision, and emits a frame when it moved. That interval is the
//! implementation's published check interval (acceptance #4: "公布实际可
//! 用检查间隔"): a change becomes visible to a subscriber within at most
//! [`POLL_INTERVAL`] after it lands in the authoritative tables. `wake()`
//! hands out the store's state notifier so run-lifecycle writes also nudge
//! the pump between intervals; the interval remains the guarantee.
//!
//! **Snapshot handover** (acceptance #1): `sub.open` admits the
//! subscription and starts the pump BEFORE any read happens — the first
//! `fetch_next` resolves the initial state after the subscription is
//! buffered, and every later poll diffs against what was actually delivered,
//! so a change during initialization is either already in the snapshot or
//! caught by the next poll. No terminal state is silently dropped.
//!
//! **Cursor semantics** (acceptance #2/#3): each subscription mints a fresh
//! opaque `subscriptionEpoch` (`rwatch-<uuidv7>` from the store's id
//! generator) and numbers its own frames from `sequence: 0`. The order is
//! per-subscription only — it is NOT a cross-project persistent log, and a
//! run-events cursor does not even parse against this input schema. Because
//! a subscription's order dies with it, a cursor carried into a new
//! `sub.open` (or the unary call) always names an order this connection
//! cannot resume: it is rejected at open with `-32000 CURSOR_EXPIRED` and
//! a `details.gap` whose recovery is `fetch_snapshot_then_resume` — the
//! consumer re-takes the state with `resources.snapshot` and re-opens
//! without a cursor. `CURSOR_INVALID` has no producer here: a malformed
//! cursor fails the input schema first (`INVALID_ARGUMENT`).
//!
//! **Frame types** (result enum → when this build emits them):
//! * `snapshot` — once per watched resource at handover time
//!   (`includeInitialSnapshot=true`, the default): the full
//!   `ResourceVersion` (resource/revision/state/snapshotSchemaId).
//! * `changed` — a revision move on a still-resolvable resource (the
//!   everyday case; carries the new revision, so the consumer may re-read
//!   by revision or just track it), or a transition out of `absent` /
//!   `unavailable` into a resolvable state.
//! * `authorizationChanged` — a `project`/`session` kind going `present →
//!   absent`: the row is no longer visible to the resolver, which in this
//!   build is the authorization face of those kinds (there is no finer
//!   per-resource permission model; a revoked access key instead closes the
//!   whole connection at its next admission boundary). The consumer must
//!   drop cached state for the resource (the leak-stop reading of
//!   acceptance #5) and re-read.
//! * `invalidated` — a resolvable resource whose revision source went
//!   `unavailable`: no revision can be named, so the consumer must re-read.
//!   (Defensive: no current kind can transition here — the kinds without a
//!   revision source are `unavailable` from the start.)
//! * `resyncRequired` — no producer: the conditions that would call for it
//!   (cursor expiry, service reload) are answered with the identifiable
//!   `CURSOR_EXPIRED` error / the pump's `shutdown` terminal instead, per
//!   acceptance #3's "或可识别错误" arm.
//! * `heartbeat` — no producer: the transport-level `conn.heartbeat`
//!   (5 s) already covers liveness; watch-level heartbeat frames would only
//!   burn subscription credit.
//!
//! **Unary page** (007 §6.4, `follow=false`): the result schema is one
//! frame object, so the page is the HEAD of the frame sequence — the first
//! watched resource's current version as a `snapshot` frame with a fresh
//! epoch cursor at `sequence: 0`. The batch current-versions read stays
//! `resources.snapshot`'s job (that is its exact contract);
//! `includeInitialSnapshot=false` on a unary call has no page to return
//! and answers `INVALID_ARGUMENT` (observation without a snapshot is what
//! `--follow` is for).

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentcpm_pd_protocol::contract::{EventEnvelope, ResponseOutcome};
use agentcpm_pd_protocol::errors::{
    CODE_CURSOR_EXPIRED, CODE_INVALID_ARGUMENT, CODE_RESOURCE_LIMIT, ContractError, EffectStatus,
    RetrySpec,
};
use agentcpm_pd_protocol::wire::{
    JSONRPC_VERSION, JsonRpcNotification, SubEventNotification, WIRE_SUB_EVENT,
};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::public_v1::PublicV1State;
use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::operations_ops::{
    MAX_SNAPSHOT_RESOURCES, ResourceVersionOut, resolve_resource_version, resource_version_json,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::V1Store;
use crate::public_v1::ws::{StreamFetch, StreamFrame, StreamSource};

/// The published conditional-polling interval (acceptance #4): each watched
/// resource's revision is re-resolved at most once per interval, so a
/// change is observable by a subscriber within this bound. Keep in sync
/// with the module doc — it is the contract's "实际可用检查间隔".
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The wire event type of every delivered frame (the operation declares no
/// dedicated event ids, like `run.events`/`terminals.output`; the frame's
/// own `type` discriminator rides in `data`).
const EVENT_TYPE: &str = "resources.watch";

pub(super) fn register(reg: &mut DomainRegistry) {
    // The dispatch handler serves the unary (non-follow) page and marks
    // the operation implemented (the -32004 gate); the follow stream is
    // the StreamSource registered right after.
    reg.handler("resources.watch", |rt, input, _key, _rev| {
        Box::pin(resources_watch_unary(rt, input))
    });
    crate::public_v1::ws::register_stream_source("resources.watch", resources_watch_source);
}

// ---------------------------------------------------------------------------
// Input, cursors
// ---------------------------------------------------------------------------

fn invalid_arg(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// Extracts and validates the shared watch input (the dispatch layer
/// already enforced the JSON schema; these are the semantic bounds the
/// schema cannot express). Returns the resource refs in input order and
/// whether the initial snapshot was requested (default `true`).
fn parse_watch_input(input: &Value) -> Result<(Vec<Value>, bool), ContractError> {
    let resources = input
        .get("resources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if resources.is_empty() {
        return Err(invalid_arg("`resources` must list at least one resource"));
    }
    if resources.len() > MAX_SNAPSHOT_RESOURCES {
        return Err(ContractError::new(
            CODE_RESOURCE_LIMIT,
            format!(
                "one watch may reference at most {MAX_SNAPSHOT_RESOURCES} resources (got {})",
                resources.len()
            ),
            RetrySpec::backoff_read(Some(100)),
            EffectStatus::NotApplied,
        ));
    }
    let include_initial_snapshot = input
        .get("includeInitialSnapshot")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Ok((resources, include_initial_snapshot))
}

/// The contract's `ResourceCursor` object. The wire envelope carries the
/// same position as its compact JSON string form (the envelope's `cursor`
/// is a string; a consumer can round-trip it through the CLI's `--cursor`
/// as JSON).
fn cursor_value(epoch: &str, sequence: u64) -> Value {
    json!({"subscriptionEpoch": epoch, "sequence": sequence})
}

/// The open-time rejection for a carried cursor (acceptance #2/#3): the
/// epoch it names is not this subscription's — a watch order dies with its
/// subscription, so the position is unrecoverable and the consumer must
/// re-take the state. Mirrors `run.events`' open-time `-32000` shape with
/// the snapshot recovery policy.
fn cursor_expired_error(cursor: &Value) -> ContractError {
    let requested = serde_json::to_string(cursor).unwrap_or_default();
    let mut err = ContractError::new(
        CODE_CURSOR_EXPIRED,
        format!(
            "cursor {requested} names a subscription epoch this connection cannot resume; \
             resource watch cursors order one subscription only (there is no persistent log to \
             replay); re-take the current state with `resources snapshot` and re-open without a \
             cursor"
        ),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    );
    err.details = json!({
        "gap": {
            "kind": "gap",
            "requestedCursor": requested,
            "earliestCursor": null,
            "snapshotRevision": "resources.snapshot",
            "recovery": "fetch_snapshot_then_resume",
        }
    })
    .as_object()
    .cloned();
    err
}

// ---------------------------------------------------------------------------
// Unary page (follow=false)
// ---------------------------------------------------------------------------

async fn resources_watch_unary(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    if let Some(cursor) = input.get("cursor").filter(|value| !value.is_null()) {
        return Err(cursor_expired_error(cursor).into());
    }
    let (resources, include_initial_snapshot) = parse_watch_input(&input)?;
    if !include_initial_snapshot {
        return Err(invalid_arg(
            "a watch without the initial snapshot only observes changes; pass --follow to \
             observe, or use `resources snapshot` for the current-state read",
        )
        .into());
    }
    // The page is the head of the frame sequence: the first resource's
    // current version, in a fresh epoch's cursor (see the module doc).
    let store = rt.store();
    let epoch = format!("rwatch-{}", store.next_id());
    let first = {
        let conn = store.conn.lock().expect("sqlite mutex");
        resolve_resource_version(&conn, &resources[0])?
    };
    Ok((
        ResponseOutcome::Succeeded,
        frame_data(&epoch, 0, "snapshot", &first, None),
    ))
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// One watched resource's last delivered version — the diff base for the
/// conditional polls.
struct WatchedResource {
    resource: Value,
    revision: String,
    state: &'static str,
}

/// The contract result object of one frame. `snapshot` frames additionally
/// carry the full `ResourceVersion`; `changed` frames carry the new
/// revision; the notification-only types carry a `reason` instead.
fn frame_data(
    epoch: &str,
    sequence: u64,
    frame_type: &str,
    out: &ResourceVersionOut,
    reason: Option<&str>,
) -> Value {
    let mut data = json!({
        "type": frame_type,
        "cursor": cursor_value(epoch, sequence),
        "resource": out.resource,
    });
    if frame_type == "snapshot" || frame_type == "changed" {
        data["revision"] = json!(out.revision);
    }
    if frame_type == "snapshot" {
        data["snapshot"] = resource_version_json(out);
    }
    if let Some(reason) = reason {
        data["reason"] = json!(reason);
    }
    data
}

/// Decides the frame type for one observed transition (see the module doc's
/// frame-type table). `None` means "no change".
fn transition_frame(
    watched: &WatchedResource,
    out: &ResourceVersionOut,
) -> Option<(&'static str, Option<String>)> {
    if watched.state == out.state && watched.revision == out.revision {
        return None;
    }
    match (watched.state, out.state) {
        // present -> absent: for the identity-scoped kinds the resolver's
        // visibility IS the authorization face of this build (acceptance
        // #5's leak-stop reading); every other resolvable kind reports the
        // absent state as a revision move.
        ("present", "absent") => {
            let kind = watched
                .resource
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(kind, "project" | "session") {
                Some((
                    "authorizationChanged",
                    Some(format!(
                        "the {kind} is no longer visible to the resolver; drop cached state \
                         and re-read it"
                    )),
                ))
            } else {
                Some(("changed", None))
            }
        }
        // The revision source itself went away: no revision can be named,
        // so the consumer must re-read rather than diff.
        (_, "unavailable") => Some((
            "invalidated",
            Some("the resource's revision source is unavailable; re-read the resource".to_owned()),
        )),
        // Any other move (revision bump, reappearance, or a kind gaining a
        // revision source) is a resolvable change.
        _ => Some(("changed", None)),
    }
}

// ---------------------------------------------------------------------------
// The follow stream source
// ---------------------------------------------------------------------------

/// The `resources.watch` data plane for the subscription pump.
struct ResourcesWatchSource {
    store: Arc<V1Store>,
    instance_id: String,
    sub_id: String,
    epoch: String,
    include_initial_snapshot: bool,
    /// The validated refs, in input order.
    resources: Vec<Value>,
    /// How far the initial handover (or, with `includeInitialSnapshot:
    /// false`, the silent seed) has progressed.
    initial_index: usize,
    /// The per-subscription frame counter (`sequence`).
    next_seq: u64,
    /// The diff base, filled by the handover/seed.
    watched: Vec<WatchedResource>,
    /// When the last conditional poll ran; the poll clock starts when the
    /// handover completes.
    last_poll: Option<Instant>,
}

/// The `sub.open` admission: validates the input bounds and rejects any
/// carried cursor before the subscription id becomes visible.
fn resources_watch_source(
    state: &Arc<PublicV1State>,
    input: &Value,
    sub_id: &str,
) -> Result<Box<dyn StreamSource>, ContractError> {
    if let Some(cursor) = input.get("cursor").filter(|value| !value.is_null()) {
        return Err(cursor_expired_error(cursor));
    }
    let (resources, include_initial_snapshot) = parse_watch_input(input)?;
    let store = state.runtime.store().clone();
    let epoch = format!("rwatch-{}", store.next_id());
    Ok(Box::new(ResourcesWatchSource {
        instance_id: store.meta().instance_id.clone(),
        store,
        sub_id: sub_id.to_owned(),
        epoch,
        include_initial_snapshot,
        resources,
        initial_index: 0,
        next_seq: 0,
        watched: Vec::new(),
        last_poll: None,
    }))
}

impl ResourcesWatchSource {
    /// Wraps one result frame into a serialized `sub.event` notification
    /// (the terminal-output shape: envelope cursor = the frame cursor's
    /// string form, event type = the operation id, `data` = the result).
    fn wrap(&self, data: Value) -> StreamFrame {
        let cursor = serde_json::to_string(&data["cursor"]).unwrap_or_default();
        let notif = JsonRpcNotification {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: WIRE_SUB_EVENT.to_owned(),
            params: serde_json::to_value(SubEventNotification {
                subscription_id: self.sub_id.clone(),
                event: EventEnvelope {
                    version: "agentcpm-pd.event/1".to_owned(),
                    instance_id: self.instance_id.clone(),
                    run_id: None,
                    operation_id: None,
                    subscription_id: Some(self.sub_id.clone()),
                    cursor: cursor.clone(),
                    event_type: EVENT_TYPE.to_owned(),
                    data: data.as_object().cloned().unwrap_or_default(),
                },
            })
            .unwrap_or(Value::Null),
        };
        let text = serde_json::to_string(&notif).unwrap_or_default();
        StreamFrame {
            estimated_bytes: text.len() + 128,
            text,
            cursor,
        }
    }
}

impl StreamSource for ResourcesWatchSource {
    fn fetch_next(&mut self, batch: usize) -> Result<StreamFetch, ContractError> {
        let mut frames: Vec<StreamFrame> = Vec::new();

        // Phase A — the handover: resolve each resource's current version
        // AFTER the subscription is established (the pump called us), emit
        // a snapshot frame per resource (or just record the diff base when
        // the consumer opted out of the initial snapshot). The connection
        // lock is held only for the resolutions of this fetch.
        if self.initial_index < self.resources.len() {
            let conn = self.store.conn.lock().expect("sqlite mutex");
            while self.initial_index < self.resources.len() && frames.len() < batch {
                let resource = self.resources[self.initial_index].clone();
                let out = resolve_resource_version(&conn, &resource)?;
                if self.include_initial_snapshot {
                    let sequence = self.next_seq;
                    self.next_seq += 1;
                    frames.push(self.wrap(frame_data(
                        &self.epoch,
                        sequence,
                        "snapshot",
                        &out,
                        None,
                    )));
                }
                self.watched.push(WatchedResource {
                    resource,
                    revision: out.revision,
                    state: out.state,
                });
                self.initial_index += 1;
            }
            drop(conn);
            if self.initial_index == self.resources.len() {
                // The handover (or silent seed) is complete: the poll
                // clock starts from here.
                self.last_poll = Some(Instant::now());
            }
            return Ok(StreamFetch {
                frames,
                stream_ended: false,
            });
        }

        // Phase B — conditional polling: at most one poll per published
        // interval (the pump's 50 ms tick drives the gate check; the store
        // state notifier may nudge it awake between intervals).
        if self
            .last_poll
            .is_some_and(|last| last.elapsed() < POLL_INTERVAL)
        {
            return Ok(StreamFetch {
                frames,
                stream_ended: false,
            });
        }
        self.last_poll = Some(Instant::now());
        let conn = self.store.conn.lock().expect("sqlite mutex");
        for watched in &mut self.watched {
            let out = resolve_resource_version(&conn, &watched.resource)?;
            if let Some((frame_type, reason)) = transition_frame(watched, &out) {
                let sequence = self.next_seq;
                self.next_seq += 1;
                frames.push(self.wrap(frame_data(
                    &self.epoch,
                    sequence,
                    frame_type,
                    &out,
                    reason.as_deref(),
                )));
            }
            watched.revision = out.revision;
            watched.state = out.state;
        }
        drop(conn);

        // A watch has no terminal state of its own: the subscription ends
        // only when the consumer closes it, the connection drops, or the
        // pump's slow-consumer/error paths fire.
        Ok(StreamFetch {
            frames,
            stream_ended: false,
        })
    }

    fn wake(&self) -> Arc<Notify> {
        self.store.state_notifier()
    }

    fn oversized_recovery_hint(&self) -> &'static str {
        "re-take the current state with `resources snapshot` and re-open the watch without a cursor"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use std::path::PathBuf;

    struct Fixture {
        _tmp: TempDir,
        state: Arc<PublicV1State>,
        rt: Arc<PublicV1Runtime>,
        _home: PathBuf,
        instance_id: String,
    }

    fn fixture(tag: &str) -> Fixture {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let (file, _token) = load_local_instance_and_token(&home, None).unwrap();
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
        let store = V1Store::open_and_recover(&home, meta).unwrap();
        let rt = Arc::new(PublicV1Runtime::new(store));
        let instance_id = rt.store().meta().instance_id.clone();
        Fixture {
            state: Arc::new(PublicV1State::new(rt.clone(), "test-token".to_owned())),
            rt,
            _home: home,
            _tmp: tmp,
            instance_id,
        }
    }

    fn seed_project_session(fx: &Fixture, tag: &str) -> (String, String) {
        let (project_id, session_id) = (format!("proj-{tag}"), format!("sess-{tag}"));
        fx.rt
            .store()
            .register_project(&json!({
                "instanceId": fx.instance_id,
                "projectId": project_id,
                "name": "Watch tests",
                "environmentId": "local",
                "root": {"instanceId": fx.instance_id, "rootId": "root", "relativePath": "ws"}
            }))
            .unwrap();
        fx.rt
            .store()
            .create_session(&json!({
                "instanceId": fx.instance_id,
                "sessionId": session_id,
                "projectId": project_id
            }))
            .unwrap();
        (project_id, session_id)
    }

    /// Builds the stream source the way `sub.open` does.
    fn source(fx: &Fixture, input: Value) -> Box<dyn StreamSource> {
        resources_watch_source(&fx.state, &input, "sub-test").expect("watch source starts")
    }

    /// Decodes one frame's notification into (envelope, result-data).
    fn frame_parts(frame: &StreamFrame) -> (Value, Value) {
        let envelope: Value = serde_json::from_str(&frame.text).expect("sub.event JSON");
        let data = envelope["params"]["event"]["data"].clone();
        (envelope, data)
    }

    #[test]
    fn initial_snapshot_handover_numbers_frames_within_one_epoch() {
        let fx = fixture("rwatch-snap");
        let (_project_id, session_id) = seed_project_session(&fx, "snap");
        let mut src = source(
            &fx,
            json!({"resources": [{"kind": "projects"}, {"kind": "session", "resourceId": session_id}]}),
        );

        let fetch = src.fetch_next(256).unwrap();
        assert!(!fetch.stream_ended, "a watch never ends on its own");
        assert_eq!(fetch.frames.len(), 2, "one snapshot frame per resource");

        let (envelope, projects) = frame_parts(&fetch.frames[0]);
        assert_eq!(envelope["method"], "sub.event");
        assert_eq!(envelope["params"]["subscriptionId"], "sub-test");
        assert_eq!(envelope["params"]["event"]["type"], EVENT_TYPE);
        assert_eq!(projects["type"], "snapshot");
        assert_eq!(projects["resource"]["kind"], "projects");
        assert_eq!(projects["snapshot"]["state"], "present");
        assert_eq!(
            projects["snapshot"]["snapshotSchemaId"],
            "resources.projects.v1"
        );
        assert_eq!(projects["revision"], projects["snapshot"]["revision"]);
        let epoch = projects["cursor"]["subscriptionEpoch"]
            .as_str()
            .expect("epoch")
            .to_owned();
        assert!(
            epoch.starts_with("rwatch-"),
            "the epoch is an opaque rwatch id: {epoch}"
        );
        assert_eq!(projects["cursor"]["sequence"], 0);
        // The envelope cursor is the same position in string form.
        let wire_cursor: Value = serde_json::from_str(
            envelope["params"]["event"]["cursor"]
                .as_str()
                .expect("cursor"),
        )
        .unwrap();
        assert_eq!(wire_cursor["subscriptionEpoch"], epoch.as_str());
        assert_eq!(wire_cursor["sequence"], 0);

        let (_, session) = frame_parts(&fetch.frames[1]);
        assert_eq!(session["type"], "snapshot");
        assert_eq!(session["resource"]["kind"], "session");
        assert_eq!(session["snapshot"]["state"], "present");
        assert_eq!(session["cursor"]["sequence"], 1, "frames number from 0");
        assert_eq!(
            session["cursor"]["subscriptionEpoch"],
            epoch.as_str(),
            "one epoch per subscription"
        );

        // The frame data is the contract result object.
        for frame in &fetch.frames {
            let (_, data) = frame_parts(frame);
            OperationRegistry::global()
                .validate_result("resources.watch", &data)
                .unwrap();
        }

        // No change: the interval gate keeps the next fetch empty.
        let again = src.fetch_next(256).unwrap();
        assert!(again.frames.is_empty());
    }

    #[test]
    fn revision_change_emits_a_changed_frame_within_the_interval() {
        let fx = fixture("rwatch-changed");
        seed_project_session(&fx, "changed");
        let mut src = source(&fx, json!({"resources": [{"kind": "projects"}]}));
        let initial = src.fetch_next(256).unwrap();
        let (_, data) = frame_parts(&initial.frames[0]);
        let initial_revision = data["revision"].as_str().unwrap().to_owned();

        // Another client registers a project: the `projects` revision moves.
        fx.rt
            .store()
            .register_project(&json!({
                "instanceId": fx.instance_id,
                "projectId": "proj-changed-2",
                "name": "Second client",
                "environmentId": "local",
                "root": {"instanceId": fx.instance_id, "rootId": "root", "relativePath": "ws"}
            }))
            .unwrap();

        // Within one published interval + slack the change surfaces. The
        // first fetch below may or may not be interval-gated depending on
        // machine load; whichever poll runs first after the registration
        // delivers the frame (the revision moved exactly once), so folding
        // the two fetches is deterministic either way.
        let mut candidates = src.fetch_next(256).unwrap().frames;
        if candidates.is_empty() {
            std::thread::sleep(POLL_INTERVAL + Duration::from_millis(100));
            candidates = src.fetch_next(256).unwrap().frames;
        }
        assert_eq!(candidates.len(), 1, "exactly one changed frame");
        let (_, changed) = frame_parts(&candidates[0]);
        assert_eq!(changed["type"], "changed");
        assert_eq!(changed["resource"]["kind"], "projects");
        assert_ne!(changed["revision"], initial_revision.as_str());
        assert_eq!(changed["cursor"]["sequence"], 1);
        assert!(
            changed.get("snapshot").is_none(),
            "only the initial handover carries a snapshot object"
        );
        OperationRegistry::global()
            .validate_result("resources.watch", &changed)
            .unwrap();

        // The new revision is the diff base: no repeat frame.
        std::thread::sleep(POLL_INTERVAL + Duration::from_millis(100));
        let settled = src.fetch_next(256).unwrap();
        assert!(settled.frames.is_empty(), "no change, no frame");
    }

    #[test]
    fn session_invisibility_emits_authorization_changed() {
        let fx = fixture("rwatch-authz");
        let (_project_id, session_id) = seed_project_session(&fx, "authz");
        let mut src = source(
            &fx,
            json!({"resources": [{"kind": "session", "resourceId": session_id}]}),
        );
        let initial = src.fetch_next(256).unwrap();
        let (_, data) = frame_parts(&initial.frames[0]);
        assert_eq!(data["snapshot"]["state"], "present");

        // The session becomes invisible to the resolver (deleted rows are
        // filtered out by the session revision query).
        {
            let conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE sessions SET session_state = 'deleted' WHERE session_id = ?1",
                [&session_id],
            )
            .unwrap();
        }
        std::thread::sleep(POLL_INTERVAL + Duration::from_millis(100));
        let fetch = src.fetch_next(256).unwrap();
        assert_eq!(fetch.frames.len(), 1);
        let (_, changed) = frame_parts(&fetch.frames[0]);
        assert_eq!(changed["type"], "authorizationChanged");
        assert_eq!(changed["resource"]["kind"], "session");
        assert!(
            changed["reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty()),
            "the notification-only frame explains itself: {changed}"
        );
        assert!(
            changed.get("revision").is_none(),
            "an absent resource has no revision to name"
        );
        OperationRegistry::global()
            .validate_result("resources.watch", &changed)
            .unwrap();
    }

    #[test]
    fn without_initial_snapshot_the_watch_waits_for_changes() {
        let fx = fixture("rwatch-no-snap");
        seed_project_session(&fx, "nosnap");
        let mut src = source(
            &fx,
            json!({"resources": [{"kind": "projects"}], "includeInitialSnapshot": false}),
        );

        // The seed is silent: no snapshot frame, ever.
        let seeded = src.fetch_next(256).unwrap();
        assert!(seeded.frames.is_empty());

        fx.rt
            .store()
            .register_project(&json!({
                "instanceId": fx.instance_id,
                "projectId": "proj-nosnap-2",
                "name": "Second client",
                "environmentId": "local",
                "root": {"instanceId": fx.instance_id, "rootId": "root", "relativePath": "ws"}
            }))
            .unwrap();
        std::thread::sleep(POLL_INTERVAL + Duration::from_millis(100));
        let fetch = src.fetch_next(256).unwrap();
        assert_eq!(fetch.frames.len(), 1);
        let (_, changed) = frame_parts(&fetch.frames[0]);
        assert_eq!(changed["type"], "changed");
        assert_eq!(
            changed["cursor"]["sequence"], 0,
            "the counter numbers EMITTED frames, so it starts at 0 without the handover"
        );
    }

    #[test]
    fn carried_cursors_reject_at_open_with_snapshot_recovery() {
        let fx = fixture("rwatch-cursor");
        let input = json!({
            "resources": [{"kind": "projects"}],
            "cursor": {"subscriptionEpoch": "rwatch-gone", "sequence": 3},
        });
        let err = resources_watch_source(&fx.state, &input, "sub-test").unwrap_err();
        assert_eq!(err.code, CODE_CURSOR_EXPIRED);
        let gap = err.details.as_ref().expect("gap details");
        assert_eq!(gap["gap"]["kind"], "gap");
        assert_eq!(gap["gap"]["recovery"], "fetch_snapshot_then_resume");

        // The gap names the requested cursor in serialized form (the
        // shared $defs.gap field is a string).
        assert_eq!(
            gap["gap"]["requestedCursor"],
            json!(serde_json::to_string(&input["cursor"]).unwrap()),
            "gap: {gap}"
        );

        // A run-events cursor does not even parse against this schema, and
        // a plain string cursor fails input validation the same way.
        let bad = json!({
            "resources": [{"kind": "projects"}],
            "cursor": "cur-000000000001",
        });
        assert!(
            OperationRegistry::global()
                .validate_input("resources.watch", &bad)
                .is_err(),
            "a run cursor is rejected by the input schema, not the watch"
        );
    }

    #[tokio::test]
    async fn unary_page_returns_the_head_snapshot_frame() {
        let fx = fixture("rwatch-unary");
        let (_project_id, session_id) = seed_project_session(&fx, "unary");

        let (_, page) = fx
            .rt
            .dispatch(
                "resources.watch",
                json!({
                    "resources": [
                        {"kind": "projects"},
                        {"kind": "session", "resourceId": session_id},
                    ]
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page["type"], "snapshot");
        assert_eq!(page["resource"]["kind"], "projects");
        assert_eq!(page["snapshot"]["state"], "present");
        assert_eq!(page["cursor"]["sequence"], 0);
        assert!(
            page["cursor"]["subscriptionEpoch"]
                .as_str()
                .is_some_and(|epoch| !epoch.is_empty()),
            "the unary page carries a fresh epoch cursor: {page}"
        );
        OperationRegistry::global()
            .validate_result("resources.watch", &page)
            .unwrap();

        // An absent single resource reports its state honestly.
        let (_, absent) = fx
            .rt
            .dispatch(
                "resources.watch",
                json!({"resources": [{"kind": "project", "resourceId": "proj-gone"}]}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(absent["snapshot"]["state"], "absent");

        // A carried cursor rejects on the unary path too.
        let err = fx
            .rt
            .dispatch(
                "resources.watch",
                json!({
                    "resources": [{"kind": "projects"}],
                    "cursor": {"subscriptionEpoch": "rwatch-x", "sequence": 0},
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_CURSOR_EXPIRED),
            other => panic!("expected CURSOR_EXPIRED, got {other:?}"),
        }

        // Without the initial snapshot a unary watch has no page.
        let err = fx
            .rt
            .dispatch(
                "resources.watch",
                json!({
                    "resources": [{"kind": "projects"}],
                    "includeInitialSnapshot": false,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_INVALID_ARGUMENT),
            other => panic!("expected INVALID_ARGUMENT, got {other:?}"),
        }

        // The resource cap matches resources.snapshot's.
        let too_many: Vec<Value> = (0..65)
            .map(|i| json!({"kind": "project", "resourceId": format!("p-{i}")}))
            .collect();
        let err = fx
            .rt
            .dispatch(
                "resources.watch",
                json!({"resources": too_many}),
                None,
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(err) => assert_eq!(err.code, CODE_RESOURCE_LIMIT),
            other => panic!("expected RESOURCE_LIMIT, got {other:?}"),
        }
    }
}
