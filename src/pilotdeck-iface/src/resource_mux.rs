//! Per-call composition of independently owned, public resource codecs.
use crate::resources::{
    ObservedResourceStep, ResourceCodec, ResourceRequest, ResourceStep, VerifiedResourceResponse,
};
use agentcpm_scheduler_protocol::Failure;
use pilotdeck_agent::ports::{CallControl, HostCall, HostReply, PortError, PortResult};
use pilotdeck_protocol::{JsString, admission::CallEffect};
use serde_json::Value;

/// Every child keeps its own pending state and cleanup ledger. Ambiguous
/// advertisements fail closed before any child receives a call.
pub struct ResourceMux {
    children: Vec<Box<dyn ResourceCodec>>,
    last: Option<usize>,
}
impl ResourceMux {
    pub fn new(children: Vec<Box<dyn ResourceCodec>>) -> PortResult<Self> {
        if children.is_empty() {
            return Err(invalid("resource composition is empty"));
        }
        Ok(Self {
            children,
            last: None,
        })
    }
    fn index(&self, call: &HostCall) -> PortResult<usize> {
        let (module, operation) = call.method();
        let mut indices = self
            .children
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.accepts_call(call).then_some(i));
        let index = indices
            .next()
            .ok_or(PortError::NotAdvertised { module, operation })?;
        if indices.next().is_some() {
            return Err(invalid("ambiguous resource operation binding"));
        }
        Ok(index)
    }
}
fn invalid(message: &str) -> PortError {
    PortError::InvalidResponse {
        message: JsString::from(message),
    }
}
impl ResourceCodec for ResourceMux {
    fn advertised(&self, module: &str, operation: &str) -> bool {
        self.children
            .iter()
            .any(|c| c.advertised(module, operation))
    }
    fn resource_effect(&self, call: &HostCall, method: &str) -> CallEffect {
        self.index(call)
            .map(|i| self.children[i].resource_effect(call, method))
            .unwrap_or_else(|_| call.effect())
    }
    fn unresolved_effect(
        &self,
        call: &HostCall,
        dispatched: Option<&'static str>,
    ) -> Option<&'static str> {
        self.index(call)
            .map(|i| self.children[i].unresolved_effect(call, dispatched))
            .unwrap_or(dispatched)
    }
    fn admitted_observation(
        &mut self,
        call: &HostCall,
        control: &CallControl,
        ticket: agentcpm_scheduler_protocol::provider::ResourceCall,
    ) -> PortResult<()> {
        let i = self.index(call)?;
        if self.last != Some(i) {
            return Err(invalid("resource response owner changed"));
        }
        self.children[i].admitted_observation(call, control, ticket)
    }
    fn observation_acknowledged(
        &mut self,
        call: &HostCall,
        ticket: &agentcpm_scheduler_protocol::provider::ResourceCall,
    ) {
        if let Ok(i) = self.index(call)
            && self.last == Some(i)
        {
            self.children[i].observation_acknowledged(call, ticket);
        }
    }
    fn observation_ack_failed(
        &mut self,
        call: &HostCall,
        ticket: agentcpm_scheduler_protocol::provider::ResourceCall,
    ) {
        if let Ok(i) = self.index(call)
            && self.last == Some(i)
        {
            self.children[i].observation_ack_failed(call, ticket);
        }
    }
    fn observation_unconsumed(
        &mut self,
        call: &HostCall,
        method: &str,
        ticket: agentcpm_scheduler_protocol::provider::ResourceCall,
    ) {
        if let Ok(i) = self.index(call)
            && self.last == Some(i)
        {
            self.children[i].observation_unconsumed(call, method, ticket);
        }
    }
    fn observe_resource(&self, method: &str) -> bool {
        self.last
            .is_some_and(|i| self.children[i].observe_resource(method))
    }
    fn resume_observed(
        &mut self,
        call: &HostCall,
        control: &CallControl,
        response: VerifiedResourceResponse,
    ) -> ObservedResourceStep {
        let i = match self.index(call) {
            Ok(i) if self.last == Some(i) => i,
            _ => {
                return ObservedResourceStep {
                    result: Err(invalid("resource response owner changed")),
                    observation_consumed: false,
                };
            }
        };
        self.children[i].resume_observed(call, control, response)
    }
    fn begin(&mut self, call: &HostCall) -> PortResult<ResourceStep> {
        let i = self.index(call)?;
        self.last = Some(i);
        self.children[i].begin(call)
    }
    fn resume(&mut self, call: &HostCall, response: Value) -> PortResult<ResourceStep> {
        let i = self.index(call)?;
        if self.last != Some(i) {
            return Err(invalid("resource response owner changed"));
        }
        self.children[i].resume(call, response)
    }
    fn cleanup_calls(&self) -> Vec<HostCall> {
        self.children
            .iter()
            .flat_map(|c| c.cleanup_calls())
            .collect()
    }
    fn resource_failed(&mut self, call: &HostCall, error: &PortError) {
        if let Ok(i) = self.index(call) {
            self.children[i].resource_failed(call, error);
        }
    }
    fn retry_rejected(&mut self, method: &str, failure: &Failure) -> Option<ResourceStep> {
        // HostPort is synchronous; `last` is set by the begin that produced
        // this pending request. Never broadcast a retry to unrelated owners.
        let i = self.last?;
        self.children[i].retry_rejected(method, failure)
    }
    fn reconcile_failure(
        &mut self,
        call: &HostCall,
        method: &str,
        failure: &Failure,
        error: &PortError,
        prior_effect: bool,
    ) -> Option<(ResourceRequest, PortError)> {
        let i = self.index(call).ok()?;
        self.children[i].reconcile_failure(call, method, failure, error, prior_effect)
    }
    fn reconciliation_observed(&mut self, call: &HostCall, result: Result<Value, Failure>) {
        if let Ok(i) = self.index(call) {
            self.children[i].reconciliation_observed(call, result);
        }
    }
    fn reconciliation_reply(
        &mut self,
        call: &HostCall,
        result: &Result<Value, Failure>,
    ) -> Option<PortResult<HostReply>> {
        let i = self.index(call).ok()?;
        self.children[i].reconciliation_reply(call, result)
    }
    fn reconciliation_cause(&self) -> Option<&Failure> {
        self.last
            .and_then(|i| self.children[i].reconciliation_cause())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_agent::ports::PreparedRequestHandle;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Codec {
        module: &'static str,
        operation: &'static str,
        calls: Arc<AtomicUsize>,
        owns_close: bool,
    }
    impl ResourceCodec for Codec {
        fn advertised(&self, module: &str, operation: &str) -> bool {
            module == self.module && operation == self.operation
        }
        fn begin(&mut self, _: &HostCall) -> PortResult<ResourceStep> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ResourceStep::Request(ResourceRequest {
                method: "test.pending",
                payload: Value::Null,
            }))
        }
        fn resume(&mut self, _: &HostCall, _: Value) -> PortResult<ResourceStep> {
            Err(invalid("no response expected"))
        }
        fn cleanup_calls(&self) -> Vec<HostCall> {
            if self.owns_close {
                vec![close()]
            } else {
                vec![]
            }
        }
        fn resource_failed(&mut self, _: &HostCall, _: &PortError) {
            self.calls.fetch_add(10, Ordering::SeqCst);
        }
        fn retry_rejected(&mut self, _: &str, _: &Failure) -> Option<ResourceStep> {
            self.calls.fetch_add(100, Ordering::SeqCst);
            None
        }
        fn reconciliation_reply(
            &mut self,
            _: &HostCall,
            _: &Result<Value, Failure>,
        ) -> Option<PortResult<HostReply>> {
            Some(Err(PortError::HostFailure {
                code: "CONTEXT_PRODUCT_ERROR".into(),
                message: "verified child failure".into(),
                boundary: pilotdeck_protocol::model::BoundarySnapshot::capture(
                    &pilotdeck_protocol::JsValue::Null,
                ),
            }))
        }
    }
    fn close() -> HostCall {
        HostCall::ModelClose {
            handle: PreparedRequestHandle("owned".into()),
        }
    }
    fn codec(calls: Arc<AtomicUsize>, owns_close: bool) -> Box<dyn ResourceCodec> {
        Box::new(Codec {
            module: "model",
            operation: "close_stream",
            calls,
            owns_close,
        })
    }
    #[test]
    fn selects_one_owner_and_preserves_cleanup_and_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let other = Arc::new(AtomicUsize::new(0));
        let other_codec: Box<dyn ResourceCodec> = Box::new(Codec {
            module: "context",
            operation: "recover_from_model_error",
            calls: other.clone(),
            owns_close: false,
        });
        let mut mux = ResourceMux::new(vec![codec(calls.clone(), true), other_codec]).unwrap();
        assert!(mux.advertised("model", "close_stream"));
        assert!(matches!(mux.begin(&close()), Ok(ResourceStep::Request(_))));
        assert_eq!(mux.cleanup_calls().len(), 1);
        let rejected = Failure::new("CONTEXT_FAILURE_UNCONFIRMED", "release.pending");
        assert!(
            mux.retry_rejected("context.operations.settle_failed", &rejected)
                .is_none()
        );
        assert!(matches!(
            mux.reconciliation_reply(&close(), &Ok(Value::Null)),
            Some(Err(PortError::HostFailure { .. }))
        ));
        mux.resource_failed(&close(), &invalid("failed"));
        assert_eq!(calls.load(Ordering::SeqCst), 111);
        assert_eq!(other.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn duplicated_owner_is_rejected_before_dispatch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut mux = ResourceMux::new(vec![
            codec(calls.clone(), false),
            codec(calls.clone(), false),
        ])
        .unwrap();
        assert!(mux.advertised("model", "close_stream"));
        assert!(matches!(
            mux.begin(&close()),
            Err(PortError::InvalidResponse { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    struct PlanCodec {
        read: bool,
        calls: Arc<AtomicUsize>,
    }
    impl ResourceCodec for PlanCodec {
        fn advertised(&self, module: &str, operation: &str) -> bool {
            module == "capability" && operation == "plan_todo"
        }
        fn accepts_call(&self, call: &HostCall) -> bool {
            matches!(call, HostCall::PlanTodo(request) if matches!(&request.mutation, pilotdeck_agent::ports::PlanTodoMutation::Read) == self.read)
        }
        fn begin(&mut self, _: &HostCall) -> PortResult<ResourceStep> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ResourceStep::Request(ResourceRequest {
                method: "test.pending",
                payload: Value::Null,
            }))
        }
        fn resume(&mut self, _: &HostCall, _: Value) -> PortResult<ResourceStep> {
            Err(invalid("unexpected"))
        }
        fn cleanup_calls(&self) -> Vec<HostCall> {
            vec![]
        }
        fn resource_failed(&mut self, _: &HostCall, _: &PortError) {}
    }
    #[test]
    fn same_plan_method_routes_read_and_mutation_to_distinct_owners() {
        use pilotdeck_agent::ports::{PlanTodoMutation, PlanTodoRequest};
        let reads = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let mut mux = ResourceMux::new(vec![
            Box::new(PlanCodec {
                read: true,
                calls: reads.clone(),
            }),
            Box::new(PlanCodec {
                read: false,
                calls: writes.clone(),
            }),
        ])
        .unwrap();
        let call = |mutation| {
            HostCall::PlanTodo(PlanTodoRequest {
                session_id: "s".into(),
                turn_id: "t".into(),
                mutation,
            })
        };
        assert!(mux.begin(&call(PlanTodoMutation::Read)).is_ok());
        assert!(
            mux.begin(&call(PlanTodoMutation::MarkPlanApproved {
                plan: "p".into()
            }))
            .is_ok()
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }
}
