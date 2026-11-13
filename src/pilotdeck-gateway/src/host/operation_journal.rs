//! Optional durable transport journal. Native loops do not install this port.
use super::HostFuture;
use crate::storage::{OperationIdentity, OperationResolution};
use pilotdeck_protocol::{JsString, JsValue, module::ModuleOutcome};

pub trait OperationJournal: Send + Sync + 'static {
    /// Recover a prior durable result, or durably start before sending execute.
    fn begin(&self, identity: OperationIdentity) -> HostFuture<Option<OperationResolution>>;
    /// Called only after the peer's real acceptance supplied its stream ID.
    fn accepted(&self, stream_id: JsString) -> HostFuture<()>;
    /// The host has already projected the actual terminal's messages here.
    fn terminal(
        &self,
        outcome: ModuleOutcome,
        last_applied_sequence: f64,
        projected: JsValue,
    ) -> HostFuture<()>;
    /// Execute ownership was dropped; consult only the real accepted state.
    fn abandoned(&self, last_applied_sequence: f64, error: JsValue) -> HostFuture<()>;
    /// Close admission and await all already-owned blocking writes.
    fn drain(&self) -> HostFuture<()>;
    /// Persist uncertainty before returning an accepted-but-unknown failure.
    fn result_unknown(
        &self,
        stream_id: JsString,
        last_applied_sequence: f64,
        code: Option<JsString>,
        error: Option<JsValue>,
    ) -> HostFuture<()>;
}

use super::{HostClient, HostError, HostErrorKind, budget, execution::TurnScope};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

struct PendingFinalizer {
    client: HostClient,
    scope: TurnScope,
    outcome: Option<ModuleOutcome>,
    stream_id: Option<JsString>,
    last_applied_sequence: f64,
    finish_terminal: bool,
    _terminal_lease: Option<budget::Lease>,
    applied_cursor: Arc<AtomicU64>,
}
/// Owns the real external connection until the consumer's integrity fence and
/// exact terminal projection have passed. No successful settlement is reported
/// by Drop: it transfers cleanup, retaining the handler's generation lease.
pub struct OperationFinalizer {
    pending: Option<Box<PendingFinalizer>>,
    runtime: tokio::runtime::Handle,
}
impl std::fmt::Debug for OperationFinalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationFinalizer")
            .field("pending", &self.pending.is_some())
            .finish()
    }
}
impl OperationFinalizer {
    pub(super) fn executing(
        client: HostClient,
        scope: TurnScope,
        applied_cursor: Arc<AtomicU64>,
    ) -> Self {
        Self {
            pending: Some(Box::new(PendingFinalizer {
                client,
                scope,
                outcome: None,
                stream_id: None,
                last_applied_sequence: -1.0,
                finish_terminal: false,
                _terminal_lease: None,
                applied_cursor,
            })),
            runtime: tokio::runtime::Handle::current(),
        }
    }
    pub(super) fn parts_mut(&mut self) -> (&mut HostClient, &mut TurnScope) {
        let pending = self.pending.as_mut().expect("active execution owner");
        (&mut pending.client, &mut pending.scope)
    }
    pub(super) fn prepare_terminal(
        &mut self,
        outcome: Option<ModuleOutcome>,
        stream_id: Option<JsString>,
        sequence: f64,
        finish_terminal: bool,
        terminal_lease: Option<budget::Lease>,
    ) {
        let pending = self.pending.as_mut().expect("active execution owner");
        pending.outcome = outcome;
        pending.stream_id = stream_id;
        pending.last_applied_sequence = sequence;
        pending.finish_terminal = finish_terminal;
        pending._terminal_lease = terminal_lease;
    }
    pub(super) fn disarm(mut self) {
        // Called only after explicit close and dispose have really completed.
        self.pending.take();
    }
    /// Submission is synchronous, so an unpolled/dropped waiter cannot strand
    /// the owned connection or a blocking journal write. Success requires the
    /// owned task to finish journal, actual transport close and handler drain.
    pub(crate) fn settle(mut self, projected: Result<JsValue, HostError>) -> HostFuture<()> {
        let pending = self.pending.take().expect("single finalizer owner");
        let task = self.runtime.spawn(settle(*pending, projected));
        Box::pin(async move {
            task.await
                .map_err(|error| HostError::protocol(error.to_string()))?
        })
    }
}
impl Drop for OperationFinalizer {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            if pending.client.journal.is_none() {
                // Preserve the pre-existing native Drop behavior exactly.
                drop(pending);
                return;
            }
            let error = HostError::new(
                HostErrorKind::ResultUnknown,
                "External operation terminal delivery was dropped before durable settlement.",
            );
            // The task owns the same scope/handler lease until close+dispose;
            // dropping its JoinHandle does not claim a completed cleanup.
            drop(self.runtime.spawn(settle(*pending, Err(error))));
        }
    }
}
async fn settle(
    mut pending: PendingFinalizer,
    projected: Result<JsValue, HostError>,
) -> Result<(), HostError> {
    pending.scope.stop_callbacks().await;
    let journal: Arc<dyn OperationJournal> = pending
        .client
        .journal
        .as_ref()
        .expect("external journal finalizer")
        .clone();
    let result = match (projected, pending.outcome) {
        (Ok(projected), Some(outcome)) => {
            journal
                .terminal(outcome, pending.last_applied_sequence, projected)
                .await
        }
        // A recovered terminal is already projected and durably known. It is
        // never re-recorded or given a new stream/binding descriptor.
        (Ok(_), None) => Ok(()),
        (Err(error), _) => Err(error),
    };
    if let (Err(error), Some(stream)) = (&result, &pending.stream_id) {
        let mut serialized = pilotdeck_protocol::JsObject::new();
        serialized.insert("name".into(), "Error".into());
        serialized.insert("message".into(), error.message.as_str().into());
        // A failed journal write may have uncertain disk outcome: its health
        // gate rejects this follow-up rather than pretending recovery is safe.
        let _ = journal
            .result_unknown(
                stream.clone(),
                pending.last_applied_sequence,
                None,
                Some(JsValue::Object(serialized)),
            )
            .await;
    }
    if let (Err(error), None) = (&result, &pending.stream_id) {
        let mut serialized = pilotdeck_protocol::JsObject::new();
        serialized.insert("name".into(), "Error".into());
        serialized.insert("message".into(), error.message.as_str().into());
        let _ = journal
            .abandoned(
                f64::from_bits(pending.applied_cursor.load(Ordering::SeqCst)),
                JsValue::Object(serialized),
            )
            .await;
    }
    let drain = journal.drain().await;
    let terminal_close = if result.is_ok() && drain.is_ok() && pending.finish_terminal {
        pending.client.connection.finish_terminal().await
    } else {
        Ok(())
    };
    let close = pending.client.connection.close().await;
    pending.scope.dispose().await;
    result
        .and(drain)
        .and(terminal_close)
        .and(close)
        .map_err(|mut error| {
            error.accepted = pending.stream_id.is_some();
            error.stream_id = pending.stream_id.clone();
            error.last_applied_sequence = error.accepted.then_some(pending.last_applied_sequence);
            error
        })
}
