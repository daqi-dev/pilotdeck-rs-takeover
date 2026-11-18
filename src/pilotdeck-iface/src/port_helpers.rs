//! Shared helpers for the port/codec modules: structured invalid-response
//! rejections and execution identity construction from a scheduler
//! [`CallContext`].

use agentcpm_scheduler_protocol::{CallContext, CallView};
use pilotdeck_agent::ports::{CallControl, CancelCause, ExecutionIdentity, PortError};
use pilotdeck_context_runtime::operations::ContextInput;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::runtime::Handle;

/// Structured invalid-response rejection shared by every codec module.
pub(crate) fn invalid(message: &str) -> PortError {
    PortError::InvalidResponse {
        message: message.into(),
    }
}

/// Atomically allocate the next strictly increasing sequence number.
///
/// `compare_exchange_weak` preserves the old `fetch_update` semantics on the
/// project's MSRV without depending on the newer `try_update` API.
pub(crate) fn next_sequence(
    counter: &AtomicU64,
    exhausted: &'static str,
) -> Result<u64, PortError> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(1).ok_or_else(|| invalid(exhausted))?;
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(next),
            Err(observed) => current = observed,
        }
    }
}

/// Build the execution identity of the current invocation with explicit key
/// fields.
pub(crate) fn execution_identity(
    context: &CallContext,
    operation_id: &str,
    request_id: &str,
    idempotency_key: &str,
) -> ExecutionIdentity {
    ExecutionIdentity {
        session_id: context.session_ref.as_str().into(),
        turn_id: context.interaction.turn.to_string().into(),
        run_id: context.call_ref.as_str().into(),
        operation_id: operation_id.into(),
        request_id: request_id.into(),
        idempotency_key: Some(idempotency_key.into()),
        module_instance_id: context.binding.id.as_str().into(),
        connection_generation: context.interaction.generation.to_string().into(),
    }
}

/// Identity of the current invocation keyed by its correlation id.
pub(crate) fn invocation_identity(context: &CallContext) -> ExecutionIdentity {
    execution_identity(
        context,
        context.invocation.correlation_id.as_str(),
        context.call_ref.as_str(),
        context.invocation.idempotency_key.as_str(),
    )
}

/// Accepted-unknown rejection carrying the given identity.
pub(crate) fn unknown_with_identity(identity: ExecutionIdentity) -> PortError {
    PortError::AcceptedUnknown {
        identity: Box::new(identity),
        last_applied_sequence: None,
        cause: None,
    }
}

/// Accepted-unknown rejection keyed by `key` for the current invocation.
pub(crate) fn accepted_unknown(context: &CallContext, key: &str) -> PortError {
    unknown_with_identity(ExecutionIdentity {
        idempotency_key: Some(key.into()),
        ..invocation_identity(context)
    })
}

/// Whether two scheduler call contexts describe the exact same call scope.
pub(crate) fn same_call_scope(a: &CallContext, b: &CallContext) -> bool {
    a.call_ref == b.call_ref
        && a.session_ref == b.session_ref
        && a.interaction == b.interaction
        && a.binding == b.binding
        && a.invocation == b.invocation
        && a.state == b.state
        && a.origin == b.origin
        && a.security == b.security
        && a.attempt == b.attempt
}

/// Whether an authoritative scheduler call view matches the retained context,
/// controller binding, and idempotency key (shared event/notice identity core).
pub(crate) fn call_view_matches(
    context: &CallContext,
    view: &CallView,
    controller: &str,
    controller_revision: &str,
    key: &str,
) -> bool {
    view.session_ref == context.session_ref
        && view.parent_session_ref.as_deref() == Some(context.session_ref.as_str())
        && view.binding.id == controller
        && view.binding.revision == controller_revision
        && view.source_binding.as_deref() == Some(context.binding.id.as_str())
        && view.cause_execution_ref.as_deref() == Some(context.call_ref.as_str())
        && view.invocation.parent_execution_ref.as_deref() == Some(context.call_ref.as_str())
        && view.invocation.idempotency_key == key
        && view.invocation.correlation_id == context.invocation.correlation_id
        && view.interaction == context.interaction
        && view.origin == context.interaction
        && view.security == context.security
}

/// Call owners that retain the (context, controller, revision) identity a
/// scheduler call view must match.
pub(crate) trait CallViewScope {
    /// The retained call context, controller, and controller revision.
    fn call_view_scope(&self) -> (&CallContext, &str, &str);

    /// Whether an authoritative call view matches this owner's retained identity.
    fn matches_call_view(&self, view: &CallView, key: &str) -> bool {
        let (context, controller, controller_revision) = self.call_view_scope();
        call_view_matches(context, view, controller, controller_revision, key)
    }
}

/// Soft-cancellation probe: a requested/deadline cancellation without a
/// hard-stop cause.
pub(crate) fn soft_cancel_check(control: &CallControl) -> Option<PortError> {
    match control.check() {
        Err(PortError::Cancelled { cause })
            if control.cancellation.hard_stop_cause().is_none()
                && matches!(cause, CancelCause::Requested(_) | CancelCause::Deadline) =>
        {
            Some(PortError::Cancelled { cause })
        }
        _ => None,
    }
}

/// Probe `check` now and, when `wait_for_interrupt` is set, poll it for a short
/// grace window on the given runtime.
pub(crate) fn wait_for_soft_cancel(
    runtime: &Handle,
    mut check: impl FnMut() -> Option<PortError>,
    wait_for_interrupt: bool,
) -> Option<PortError> {
    if let Some(cancelled) = check() {
        return Some(cancelled);
    }
    if !wait_for_interrupt {
        return None;
    }
    runtime.block_on(async {
        for _ in 0..25 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(cancelled) = check() {
                return Some(cancelled);
            }
        }
        None
    })
}

/// Definitive capability/policy denial codes for Kernel resource failures.
pub(crate) fn is_capability_denial_code(code: &str) -> bool {
    matches!(
        code,
        "CAPABILITY_DENIED"
            | "POLICY_DENIED"
            | "DELEGATION_DENIED"
            | "SESSION_FORBIDDEN"
            | "AUTHORITY_REVOKED"
            | "CAPABILITY_PARAMS"
    )
}

/// Fail closed for an admission attempt whose authority was already retired.
pub(crate) fn authority_revoked(detail: &'static str) -> agentcpm_scheduler_protocol::Failure {
    agentcpm_scheduler_protocol::Failure::external("AUTHORITY_REVOKED", detail)
}

/// Context input carrying the bound fields with no host abort or budget hooks.
pub(crate) fn context_input(fields: pilotdeck_protocol::JsObject) -> ContextInput {
    ContextInput {
        fields,
        abort_signal: None,
        budget_evaluator: None,
    }
}
