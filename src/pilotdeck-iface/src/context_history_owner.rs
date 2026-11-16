//! Parent-loop access to Kernel-owned context.history.v2.
//! No product journal, independent CAS or replay-on-missing-receipt exists here.
//! Requires the matching public Kernel SDK profile (`context_history` + `context_operations`).
use agentcpm_scheduler_protocol::{
    CallContext, Client, Failure, Result, context_history as history, context_operations as ops,
};
use pilotdeck_context_runtime::operations::Operation;
use std::sync::Arc;

pub enum ReservationOutcome {
    /// Only this first admission can dispatch the corresponding Context child.
    Dispatch(DispatchPermit),
    /// No new dispatch permitted; Unknown includes no-receipt and is not a
    /// proof that the previous work did not happen.
    AlreadyReserved(history::ReceiptResult),
}
pub struct ContextHistoryOwner {
    client: Arc<dyn Client>,
    scope: CallContext,
}
impl ContextHistoryOwner {
    pub fn new(client: Arc<dyn Client>, scope: CallContext) -> Self {
        Self { client, scope }
    }
    pub async fn read(&self) -> Result<history::HistoryView> {
        history::read(self.client.as_ref()).await
    }
    /// Read one stable, bounded prefix of this history scope. Pages are not
    /// permission to emit new effects and never clear a transaction fence.
    /// A kind with no completion proof is data, not evidence of model or turn
    /// admission. Even a Context proof only establishes its own fixed operation.
    pub async fn read_fact_snapshot(&self) -> Result<(u64, Vec<history::FactEntry>)> {
        let mut snapshot = None;
        let mut offset = 0u64;
        let mut expected_count = None;
        let mut facts = Vec::new();
        let mut last_cursor = 0u64;
        let mut last_index = None;
        let mut total_bytes = 0usize;
        loop {
            let page = history::facts(
                self.client.as_ref(),
                &history::FactsQuery {
                    snapshot_cursor: snapshot,
                    offset,
                    limit: 64,
                },
            )
            .await?;
            if snapshot.is_some_and(|original| original != page.snapshot_cursor)
                || page.facts.len() > 64
            {
                return Err(Failure::external(
                    "CONTEXT_PROTOCOL",
                    "history facts snapshot changed",
                ));
            }
            if expected_count.is_some_and(|total| total != page.total_facts)
                || page.total_facts > 100_000
            {
                return Err(Failure::external(
                    "CONTEXT_PROTOCOL",
                    "history facts total changed or exceeds limit",
                ));
            }
            snapshot = Some(page.snapshot_cursor);
            expected_count = Some(page.total_facts);
            for fact in &page.facts {
                let entry_bytes = serde_json::to_vec(fact)
                    .map_err(|_| {
                        Failure::external("CONTEXT_PROTOCOL", "history fact size encoding failed")
                    })?
                    .len();
                total_bytes = total_bytes.checked_add(entry_bytes).ok_or_else(|| {
                    Failure::external("CONTEXT_PROTOCOL", "history facts byte limit")
                })?;
                if total_bytes > history::MAX_RECORD_BYTES {
                    return Err(Failure::external(
                        "CONTEXT_PROTOCOL",
                        "history facts exceed snapshot byte limit",
                    ));
                }
                if fact.event_cursor < last_cursor
                    || fact.event_cursor > page.snapshot_cursor
                    || (fact.event_cursor == last_cursor
                        && last_index.and_then(|index: u32| index.checked_add(1))
                            != Some(fact.fact_index))
                    || (fact.event_cursor != last_cursor && fact.fact_index != 0)
                    || fact.owner.session != self.scope.session_ref
                    || fact.owner.binding != self.scope.binding.id
                    || fact.owner.context != self.scope.invocation.context_ref
                    || fact
                        .completion
                        .as_ref()
                        .is_some_and(|proof| proof.origin != fact.owner.origin)
                {
                    return Err(Failure::external(
                        "CONTEXT_HISTORY_IDENTITY_MISMATCH",
                        "history fact is outside authenticated scope",
                    ));
                }
                last_cursor = fact.event_cursor;
                last_index = Some(fact.fact_index);
            }
            let next = offset.checked_add(page.facts.len() as u64).ok_or_else(|| {
                Failure::external("CONTEXT_PROTOCOL", "history facts offset overflow")
            })?;
            if next > 100_000 {
                return Err(Failure::external(
                    "CONTEXT_PROTOCOL",
                    "history facts exceed snapshot limit",
                ));
            }
            facts.extend(page.facts);
            match page.next_offset {
                None if next == page.total_facts => return Ok((page.snapshot_cursor, facts)),
                Some(actual) if actual == next && next > offset && next < page.total_facts => {
                    offset = next
                }
                _ => {
                    return Err(Failure::external(
                        "CONTEXT_PROTOCOL",
                        "history facts page skipped or repeated entries",
                    ));
                }
            }
        }
    }
    /// Only reads the authority. No pending reservation or receipt is not a
    /// final rejection and must never be used to mint a new dispatch permit.
    pub async fn observe_unknown(&self, transaction_id: &str) -> Result<history::HistoryView> {
        let view = self.read().await?;
        if let Some(reservation) = &view.pending
            && reservation.transaction_id == transaction_id
            && (reservation.owner.session != self.scope.session_ref
                || reservation.owner.binding != self.scope.binding.id
                || reservation.owner.context != self.scope.invocation.context_ref
                || reservation.owner.origin != self.scope.interaction)
        {
            return Err(Failure::external(
                "CONTEXT_HISTORY_IDENTITY_MISMATCH",
                "pending reservation does not belong to the authenticated owner",
            ));
        }
        Ok(view)
    }
    pub async fn prepare(
        &self,
        transaction_id: String,
        expected_revision: u64,
        operation: Operation,
        request: history::OpaquePayload,
    ) -> std::result::Result<ReservationOutcome, TransactionError> {
        if request.profile != pilotdeck_context_runtime::boundary_codec::PROFILE
            || request.bytes.len() > pilotdeck_context_runtime::boundary_codec::MAX_BYTES
        {
            return Err(TransactionError::Rejected(Failure::external(
                "CONTEXT_PROTOCOL",
                "invalid context input profile or size",
            )));
        }
        let decoded =
            pilotdeck_context_runtime::boundary_codec::decode(&request.bytes).map_err(|_| {
                TransactionError::Rejected(Failure::external(
                    "CONTEXT_PROTOCOL",
                    "invalid context input encoding",
                ))
            })?;
        if decoded.as_object().is_none() {
            return Err(TransactionError::Rejected(Failure::external(
                "CONTEXT_PROTOCOL",
                "context input must be an object",
            )));
        }
        let prepare_request = history::PrepareRequest {
            transaction_id: transaction_id.clone(),
            expected_revision,
            operation: operation.name().into(),
            request,
        };
        let prepared = match history::prepare(self.client.as_ref(), &prepare_request).await {
            Ok(value) => value,
            Err(cause) => {
                return Err(TransactionError::Unknown {
                    transaction_id,
                    request_digest: None,
                    prepare_evidence: Some(Box::new(prepare_request)),
                    cause: Box::new(cause),
                });
            }
        };
        let reservation = prepared.reservation;
        let expected_owner = history::Owner {
            session: self.scope.session_ref.clone(),
            binding: self.scope.binding.id.clone(),
            context: self.scope.invocation.context_ref.clone(),
            call: self.scope.call_ref.clone(),
            origin: self.scope.interaction.clone(),
        };
        if reservation
            .verify(&expected_owner, &prepare_request)
            .is_err()
        {
            return Err(TransactionError::Unknown {
                transaction_id,
                request_digest: Some(reservation.request_digest),
                prepare_evidence: Some(Box::new(prepare_request)),
                cause: Box::new(Failure::external(
                    "CONTEXT_HISTORY_IDENTITY_MISMATCH",
                    "reservation does not match the authenticated history owner",
                )),
            });
        }
        if prepared.may_dispatch {
            return Ok(ReservationOutcome::Dispatch(DispatchPermit {
                reservation,
                operation: map_operation(operation),
                client: self.client.clone(),
            }));
        }
        let query = history::ReceiptQuery {
            transaction_id,
            request_digest: reservation.request_digest,
        };
        let result = history::receipt(self.client.as_ref(), &query)
            .await
            .map_err(|cause| TransactionError::Unknown {
                transaction_id: query.transaction_id.clone(),
                request_digest: Some(query.request_digest.clone()),
                prepare_evidence: None,
                cause: Box::new(cause),
            })?;
        if let history::ReceiptResult::Confirmed { receipt } = &result
            && (receipt.transaction_id != query.transaction_id
                || receipt.request_digest != query.request_digest)
        {
            return Err(TransactionError::Unknown {
                transaction_id: query.transaction_id,
                request_digest: Some(query.request_digest),
                prepare_evidence: None,
                cause: Box::new(Failure::external(
                    "CONTEXT_HISTORY_IDENTITY_MISMATCH",
                    "receipt does not match the retained transaction",
                )),
            });
        }
        Ok(ReservationOutcome::AlreadyReserved(result))
    }
}

/// A first-admission dispatch permit is consumed on use; retries never mint it.
pub struct DispatchPermit {
    reservation: history::Reservation,
    operation: ops::Operation,
    client: Arc<dyn Client>,
}
/// Every ambiguous error retains the original transaction identity. An unknown
/// result requires read-only receipt/get reconciliation, never redispatch.
pub enum TransactionError {
    Rejected(Failure),
    Unknown {
        transaction_id: String,
        request_digest: Option<String>,
        /// Present when admission acknowledgement was lost before the
        /// Kernel-provided digest was observed. Evidence is not authority.
        prepare_evidence: Option<Box<history::PrepareRequest>>,
        cause: Box<Failure>,
    },
}
impl DispatchPermit {
    pub async fn dispatch(self) -> std::result::Result<ops::Dispatched, TransactionError> {
        let request = ops::DispatchRequest {
            transaction_id: self.reservation.transaction_id,
            request_digest: self.reservation.request_digest,
            operation: self.operation,
        };
        match ops::dispatch(self.client.as_ref(), &request).await {
            Ok(value)
                if value.transaction_id == request.transaction_id
                    && value.request_digest == request.request_digest
                    && !value.call_ref.is_empty() =>
            {
                Ok(value)
            }
            Ok(_) => Err(TransactionError::Unknown {
                transaction_id: request.transaction_id,
                request_digest: Some(request.request_digest),
                prepare_evidence: None,
                cause: Box::new(Failure::external(
                    "CONTEXT_PROTOCOL",
                    "dispatch acknowledgement mismatch",
                )),
            }),
            Err(cause) => Err(TransactionError::Unknown {
                transaction_id: request.transaction_id,
                request_digest: Some(request.request_digest),
                prepare_evidence: None,
                cause: Box::new(cause),
            }),
        }
    }
}
impl ContextHistoryOwner {
    /// Read-only lookup after a dispatch/commit acknowledgement loss. Unknown
    /// remains unknown; this never calls dispatch or commit.
    pub async fn observe_operation(
        &self,
        transaction_id: &str,
        request_digest: &str,
        operation: ops::Operation,
        binding: &str,
        binding_revision: &str,
    ) -> Result<ops::OperationStatus> {
        let query = history::ReceiptQuery {
            transaction_id: transaction_id.into(),
            request_digest: request_digest.into(),
        };
        let status = ops::status(self.client.as_ref(), &query).await?;
        match &status {
            ops::OperationStatus::Unknown => {}
            ops::OperationStatus::Dispatched { child } => {
                let request: ops::InvocationRequest =
                    serde_json::from_value(child.invocation.payload.clone()).map_err(|_| {
                        Failure::external("CONTEXT_PROTOCOL", "invalid retained child")
                    })?;
                if child.parent_session_ref.as_deref() != Some(self.scope.session_ref.as_str())
                    || child.origin != self.scope.interaction
                    || child.security != self.scope.security
                    || child.session_ref != self.scope.session_ref
                    || child.binding.id != binding
                    || child.binding.revision != binding_revision
                    || child.interaction != self.scope.interaction
                    || request.operation != operation
                    || request.reservation.transaction_id != transaction_id
                    || request.reservation.request_digest != request_digest
                    || request.reservation.owner.session != self.scope.session_ref
                    || request.reservation.owner.context != self.scope.invocation.context_ref
                    || request.reservation.owner.binding != self.scope.binding.id
                    || child.source_binding.as_deref()
                        != Some(request.reservation.owner.binding.as_str())
                    || child.cause_execution_ref.as_deref()
                        != Some(request.reservation.owner.call.as_str())
                    || child.invocation.parent_execution_ref.as_deref()
                        != Some(request.reservation.owner.call.as_str())
                    || request.reservation.owner.origin != self.scope.interaction
                {
                    return Err(Failure::external(
                        "CONTEXT_HISTORY_IDENTITY_MISMATCH",
                        "retained child does not match transaction",
                    ));
                }
            }
            ops::OperationStatus::Failed { receipt, child } => {
                let request: ops::InvocationRequest =
                    serde_json::from_value(child.invocation.payload.clone()).map_err(|_| {
                        Failure::external("CONTEXT_PROTOCOL", "invalid retained child")
                    })?;
                if receipt.verify_child(child).is_err()
                    || receipt.transaction_id != transaction_id
                    || receipt.request_digest != request_digest
                    || child.security != self.scope.security
                    || child.binding.id != binding
                    || child.binding.revision != binding_revision
                    || request.operation != operation
                    || request.reservation.owner.session != self.scope.session_ref
                    || request.reservation.owner.binding != self.scope.binding.id
                    || request.reservation.owner.context != self.scope.invocation.context_ref
                    || request.reservation.owner.call != self.scope.call_ref
                    || request.reservation.owner.origin != self.scope.interaction
                {
                    return Err(Failure::external(
                        "CONTEXT_HISTORY_IDENTITY_MISMATCH",
                        "failure proof does not match transaction",
                    ));
                }
            }
            ops::OperationStatus::Committed { result, child } => {
                let request: ops::InvocationRequest =
                    serde_json::from_value(child.invocation.payload.clone()).map_err(|_| {
                        Failure::external("CONTEXT_PROTOCOL", "invalid retained child")
                    })?;
                if child.parent_session_ref.as_deref() != Some(self.scope.session_ref.as_str())
                    || child.origin != self.scope.interaction
                    || child.security != self.scope.security
                    || request.operation != operation
                    || request.reservation.transaction_id != transaction_id
                    || request.reservation.request_digest != request_digest
                    || request.reservation.owner.session != self.scope.session_ref
                    || request.reservation.owner.binding != self.scope.binding.id
                    || request.reservation.owner.context != self.scope.invocation.context_ref
                    || request.reservation.owner.origin != self.scope.interaction
                    || child.source_binding.as_deref()
                        != Some(request.reservation.owner.binding.as_str())
                    || child.cause_execution_ref.as_deref()
                        != Some(request.reservation.owner.call.as_str())
                    || child.invocation.parent_execution_ref.as_deref()
                        != Some(request.reservation.owner.call.as_str())
                    || child.id != result.proof.call_ref
                    || child.session_ref != self.scope.session_ref
                    || child.binding.id != binding
                    || child.binding.revision != binding_revision
                    || child.interaction != self.scope.interaction
                    || child.state != agentcpm_scheduler_protocol::State::Completed
                    || result.receipt.transaction_id != transaction_id
                    || result.receipt.request_digest != request_digest
                    || result.proof.binding_ref != binding
                    || result.proof.binding_revision != binding_revision
                    || result.proof.operation != operation
                    || result.proof.origin != self.scope.interaction
                    || child
                        .result
                        .as_ref()
                        .and_then(|raw| ops::result_digest(raw).ok())
                        .as_deref()
                        != Some(result.proof.result_digest.as_str())
                    || (!result.receipt.consumed_inputs.is_empty()
                        && result.receipt.consumed_inputs.as_slice()
                            != [self.scope.interaction.clone()])
                {
                    return Err(Failure::external(
                        "CONTEXT_HISTORY_IDENTITY_MISMATCH",
                        "committed proof does not match transaction",
                    ));
                }
            }
        }
        Ok(status)
    }
    /// Kernel reads the real child result itself; the caller cannot substitute
    /// replacement, input consumption, facts, notices or compaction boundary.
    pub async fn commit_child(
        &self,
        dispatched: &ops::Dispatched,
        operation: ops::Operation,
        child_binding: &str,
        child_binding_revision: &str,
        child_result: &serde_json::Value,
    ) -> std::result::Result<ops::Committed, TransactionError> {
        let request = ops::CommitRequest {
            transaction_id: dispatched.transaction_id.clone(),
            request_digest: dispatched.request_digest.clone(),
            call_ref: dispatched.call_ref.clone(),
        };
        match ops::commit(self.client.as_ref(), &request).await {
            Ok(value)
                if value.receipt.transaction_id == request.transaction_id
                    && value.receipt.request_digest == request.request_digest
                    && value.proof.call_ref == request.call_ref
                    && value.proof.binding_ref == child_binding
                    && value.proof.binding_revision == child_binding_revision
                    && value.proof.operation == operation
                    && value.proof.origin == self.scope.interaction
                    && ops::result_digest(child_result).ok().as_deref()
                        == Some(value.proof.result_digest.as_str())
                    && (value.receipt.consumed_inputs.is_empty()
                        || value.receipt.consumed_inputs.as_slice()
                            == [self.scope.interaction.clone()]) =>
            {
                Ok(value)
            }
            Ok(_) => Err(TransactionError::Unknown {
                transaction_id: request.transaction_id,
                request_digest: Some(request.request_digest),
                prepare_evidence: None,
                cause: Box::new(Failure::external(
                    "CONTEXT_PROTOCOL",
                    "commit acknowledgement mismatch",
                )),
            }),
            Err(cause) => Err(TransactionError::Unknown {
                transaction_id: request.transaction_id,
                request_digest: Some(request.request_digest),
                prepare_evidence: None,
                cause: Box::new(cause),
            }),
        }
    }
}
fn map_operation(op: Operation) -> ops::Operation {
    match op {
        Operation::PrepareForModel => ops::Operation::PrepareForModel,
        Operation::ApplyToolResults => ops::Operation::ApplyToolResults,
        Operation::RecoverFromModelError => ops::Operation::RecoverFromModelError,
        Operation::CaptureTurn => ops::Operation::CaptureTurn,
        Operation::TryAutoCompact => ops::Operation::TryAutoCompact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_iface::BoxFuture;
    use agentcpm_scheduler_protocol::{
        CallView, ControlKind, ControlOutcome, Destination, Event, Invocation, State, StateChange,
        VersionedState, Wait,
    };
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct ReplayClient {
        context: CallContext,
        calls: AtomicUsize,
        facts_mode: Option<(bool, usize)>,
    }
    fn unavailable<T>() -> Result<T> {
        Err(Failure::external("TEST_UNAVAILABLE", "not used"))
    }
    impl Client for ReplayClient {
        fn resource(&self, method: &str, input: Value) -> BoxFuture<'_, Result<Value>> {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            let method = method.to_owned();
            let context = self.context.clone();
            let facts_mode = self.facts_mode;
            Box::pin(async move {
                if let Some((forge, pages)) = facts_mode {
                    assert_eq!(method, history::FACTS);
                    let query: history::FactsQuery = serde_json::from_value(input).unwrap();
                    assert_eq!(query.offset, index as u64);
                    assert_eq!(query.snapshot_cursor, (index != 0).then_some(100));
                    let fact = history::FactEntry {
                        revision: 1,
                        event_cursor: 2 + index as u64,
                        fact_index: 0,
                        transaction_id: format!("tx-{index}"),
                        request_digest: "digest".into(),
                        owner: history::Owner {
                            session: context.session_ref,
                            binding: if forge {
                                "foreign".into()
                            } else {
                                context.binding.id
                            },
                            context: context.invocation.context_ref,
                            call: "prior-call".into(),
                            origin: context.interaction,
                        },
                        completion: None,
                        fact: history::Fact {
                            kind: "test".into(),
                            payload: history::OpaquePayload {
                                profile: "test/1".into(),
                                bytes: vec![index as u8],
                            },
                        },
                    };
                    return Ok(serde_json::to_value(history::FactsPage {
                        snapshot_cursor: 100,
                        total_facts: pages as u64,
                        next_offset: (index + 1 < pages).then_some((index + 1) as u64),
                        facts: vec![fact],
                    })
                    .unwrap());
                }
                if index == 0 && method == history::PREPARE {
                    let owner = history::Owner {
                        session: context.session_ref,
                        binding: context.binding.id,
                        context: context.invocation.context_ref,
                        call: context.call_ref,
                        origin: context.interaction,
                    };
                    let request: history::PrepareRequest = serde_json::from_value(input).unwrap();
                    let reservation = history::Reservation {
                        transaction_id: request.transaction_id.clone(),
                        request_digest: history::request_digest(&owner, &request).unwrap(),
                        expected_revision: request.expected_revision,
                        prepare_cursor: 1,
                        owner,
                    };
                    Ok(serde_json::to_value(history::Prepared {
                        reservation,
                        may_dispatch: false,
                    })
                    .unwrap())
                } else if index == 1 && method == history::RECEIPT {
                    Ok(
                        serde_json::to_value(history::ReceiptResult::Unknown { reservation: None })
                            .unwrap(),
                    )
                } else {
                    panic!("unexpected resource call {method}")
                }
            })
        }
        fn publish(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: State,
            _: Option<Failure>,
            _: Option<Wait>,
        ) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { unavailable() })
        }
        fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { unavailable() })
        }
        fn invoke(&self, _: Invocation) -> BoxFuture<'_, Result<String>> {
            Box::pin(async { unavailable() })
        }
        fn find(&self, _: &str, _: &str) -> BoxFuture<'_, Result<Option<CallView>>> {
            Box::pin(async { unavailable() })
        }
        fn children(&self, _: &str) -> BoxFuture<'_, Result<Vec<CallView>>> {
            Box::pin(async { unavailable() })
        }
        fn get(&self, _: &str) -> BoxFuture<'_, Result<CallView>> {
            Box::pin(async { unavailable() })
        }
        fn wait(&self, _: &str) -> BoxFuture<'_, Result<CallView>> {
            Box::pin(async { unavailable() })
        }
        fn control(
            &self,
            _: &str,
            _: ControlKind,
            _: String,
        ) -> BoxFuture<'_, Result<ControlOutcome>> {
            Box::pin(async { unavailable() })
        }
        fn respond(
            &self,
            _: &str,
            _: &str,
            _: Vec<agentcpm_scheduler_protocol::Message>,
            _: &str,
        ) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { unavailable() })
        }
        fn read_state(&self, _: &str) -> BoxFuture<'_, Result<VersionedState>> {
            Box::pin(async { unavailable() })
        }
        fn write_state(&self, _: StateChange) -> BoxFuture<'_, Result<u64>> {
            Box::pin(async { unavailable() })
        }
        fn events(&self, _: u64) -> BoxFuture<'_, Result<Vec<Event>>> {
            Box::pin(async { unavailable() })
        }
        fn wait_events(&self, _: u64) -> BoxFuture<'_, Result<Vec<Event>>> {
            Box::pin(async { unavailable() })
        }
    }
    #[tokio::test]
    async fn replayed_reservation_only_queries_receipt_and_never_dispatches() {
        let context = crate::tests::context();
        let client = Arc::new(ReplayClient {
            context: context.clone(),
            calls: AtomicUsize::new(0),
            facts_mode: None,
        });
        let owner = ContextHistoryOwner::new(client.clone(), context);
        let payload = history::OpaquePayload {
            profile: pilotdeck_context_runtime::boundary_codec::PROFILE.into(),
            bytes: pilotdeck_context_runtime::boundary_codec::encode(
                &pilotdeck_protocol::JsValue::Object(pilotdeck_protocol::JsObject::new()),
            )
            .unwrap(),
        };
        let result = owner
            .prepare("tx".into(), 0, Operation::PrepareForModel, payload)
            .await;
        assert!(matches!(
            result,
            Ok(ReservationOutcome::AlreadyReserved(
                history::ReceiptResult::Unknown { .. }
            ))
        ));
        assert_eq!(client.calls.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn facts_reader_keeps_one_prefix_and_rejects_foreign_scope() {
        for forge in [false, true] {
            let context = crate::tests::context();
            let client = Arc::new(ReplayClient {
                context: context.clone(),
                calls: AtomicUsize::new(0),
                facts_mode: Some((forge, 2)),
            });
            let owner = ContextHistoryOwner::new(client.clone(), context);
            let result = owner.read_fact_snapshot().await;
            if forge {
                assert_eq!(
                    result.unwrap_err().code,
                    "CONTEXT_HISTORY_IDENTITY_MISMATCH"
                );
                assert_eq!(client.calls.load(Ordering::SeqCst), 1);
            } else {
                let (cursor, facts) = result.unwrap();
                assert_eq!(cursor, 100);
                assert_eq!(facts.len(), 2);
                assert_eq!(client.calls.load(Ordering::SeqCst), 2);
            }
        }
    }
    #[tokio::test]
    async fn facts_reader_counts_repeated_owner_bytes_not_only_payload() {
        let mut context = crate::tests::context();
        context.invocation.context_ref = "c".repeat(1024 * 1024);
        let client = Arc::new(ReplayClient {
            context: context.clone(),
            calls: AtomicUsize::new(0),
            facts_mode: Some((false, 40)),
        });
        let owner = ContextHistoryOwner::new(client.clone(), context);
        assert_eq!(
            owner.read_fact_snapshot().await.unwrap_err().code,
            "CONTEXT_PROTOCOL"
        );
        assert!(client.calls.load(Ordering::SeqCst) < 40);
    }
}
