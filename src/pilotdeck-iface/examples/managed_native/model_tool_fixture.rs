//! Controlled fixture role exercising the real production Native Loop
//! composition (`ToolBatchCodec` + `InitialReadCodec` + `ModelSessionCodec`
//! under `ResourceMux` + `ProductController` events + `ProductionTurnClock`).
//! Intentionally omits non-Recovery Context writers and advisory lifecycle hooks
//! so unadvertised optional host modules stay fail-closed.
use agentcpm_iface::types::TurnInput as IfaceTurnInput;
use agentcpm_scheduler_protocol::{CallContext, Client};
use pilotdeck_agent::{
    ports::{CallControl, ExecutionIdentity, PortError, PortResult, Route},
    turn::{TurnClock, TurnEventPort, TurnInput},
};
use pilotdeck_iface::{
    context_codec::ContextProofs,
    initial_read_codec::InitialReadCodec,
    model_session_codec::ModelSessionCodec,
    original::{OriginalBindings, OriginalPayloadAdmission},
    product,
    production_clock::ProductionTurnClock,
    resource_mux::ResourceMux,
    resources::ResourceCodec,
    tool_batch_codec::ToolBatchCodec,
    tool_batch_owner::SourceToolOwners,
};
use pilotdeck_protocol::model::BoundarySnapshot;
use pilotdeck_turn_preparation::PreparationEffects;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

fn invalid(message: impl Into<String>) -> PortError {
    PortError::InvalidResponse {
        message: message.into().into(),
    }
}

struct ModelToolBindings {
    controller: String,
}

struct ScopedModelTool {
    context: CallContext,
    input: IfaceTurnInput,
    client: Arc<dyn Client>,
    runtime: tokio::runtime::Handle,
    owners: Arc<SourceToolOwners>,
    controller: String,
    sequence: Arc<AtomicU64>,
}

impl OriginalBindings for ModelToolBindings {
    fn supports_context_call(&self) -> bool {
        true
    }

    fn bind_context_call(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
        _proofs: Arc<ContextProofs>,
    ) -> PortResult<Option<Arc<dyn OriginalBindings>>> {
        control.check()?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| invalid("SDK runtime is unavailable"))?;
        let owners = SourceToolOwners::new(client.clone());
        Ok(Some(Arc::new(ScopedModelTool {
            context: context.clone(),
            input: input.clone(),
            client,
            runtime,
            owners,
            controller: self.controller.clone(),
            sequence: Arc::new(AtomicU64::new(0)),
        })))
    }

    fn resources(&self, _: &CallContext, _: &IfaceTurnInput) -> PortResult<Box<dyn ResourceCodec>> {
        Err(invalid("model tool fixture requires scoped binding"))
    }

    fn effects(
        &self,
        _: &CallContext,
        _: &TurnInput,
        _: Arc<dyn Client>,
        _: &CallControl,
    ) -> PortResult<Arc<Mutex<dyn PreparationEffects>>> {
        Err(invalid("model tool fixture requires scoped binding"))
    }

    fn events(
        &self,
        _: &CallContext,
        _: &TurnInput,
        _: Arc<dyn Client>,
        _: &CallControl,
    ) -> PortResult<Box<dyn TurnEventPort + Send>> {
        Err(invalid("model tool fixture requires scoped binding"))
    }

    fn clock(&self, _: &CallContext) -> PortResult<Box<dyn TurnClock + Send>> {
        Err(invalid("model tool fixture requires scoped binding"))
    }
}

impl OriginalBindings for ScopedModelTool {
    fn resources(
        &self,
        context: &CallContext,
        input: &IfaceTurnInput,
    ) -> PortResult<Box<dyn ResourceCodec>> {
        if context.call_ref != self.context.call_ref
            || context.session_ref != self.context.session_ref
            || context.binding != self.context.binding
            || context.invocation != self.context.invocation
            || context.interaction != self.context.interaction
            || input != &self.input
        {
            return Err(invalid("scoped call identity changed"));
        }
        let (tool, owner) =
            ToolBatchCodec::new(context.clone(), self.client.clone(), self.runtime.clone())?;
        if !self.owners.matches_client(&self.client) {
            return Err(invalid("tool source client mismatch"));
        }
        self.owners
            .register_native(&self.client)
            .map_err(|_| invalid("native source custody registration failed"))?;
        self.owners
            .retain(owner)
            .map_err(|_| invalid("tool source owner retention failed"))?;
        let initial = InitialReadCodec::new(self.client.as_ref());
        let model = ModelSessionCodec::new_with_local_resource_facts(context.clone())
            .with_consumer_demand()
            .with_invocation_context();
        Ok(Box::new(ResourceMux::new(vec![
            Box::new(tool),
            Box::new(initial),
            Box::new(model),
        ])?))
    }

    fn effects(
        &self,
        _: &CallContext,
        _: &TurnInput,
        _: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Arc<Mutex<dyn PreparationEffects>>> {
        Ok(Arc::new(Mutex::new(FixturePreparationEffects {
            control: control.clone(),
        })))
    }

    fn events(
        &self,
        context: &CallContext,
        _: &TurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Box<dyn TurnEventPort + Send>> {
        Ok(Box::new(ProductEventPort {
            context: context.clone(),
            client,
            runtime: self.runtime.clone(),
            control: control.clone(),
            controller: self.controller.clone(),
            sequence: self.sequence.clone(),
        }))
    }

    fn clock(&self, _: &CallContext) -> PortResult<Box<dyn TurnClock + Send>> {
        Ok(Box::new(ProductionTurnClock))
    }
}

struct FixturePreparationEffects {
    control: CallControl,
}

impl PreparationEffects for FixturePreparationEffects {
    fn notice(&mut self, _: &TurnInput, _: bool) -> PortResult<()> {
        self.control.check()
    }

    fn before_model_request(&mut self, _: &Route) -> PortResult<()> {
        self.control.check()
    }

    fn finish(&mut self) -> PortResult<()> {
        Ok(())
    }
}

struct ProductEventPort {
    context: CallContext,
    client: Arc<dyn Client>,
    runtime: tokio::runtime::Handle,
    control: CallControl,
    controller: String,
    sequence: Arc<AtomicU64>,
}

impl ProductEventPort {
    fn publish(&mut self, event: BoundarySnapshot, accepted: bool) -> PortResult<()> {
        let control = if accepted {
            CallControl {
                cancellation: self.control.cancellation.cleanup_scope(),
                deadline: None,
            }
        } else {
            self.control.clone()
        };
        control.check()?;
        let raw = pilotdeck_protocol::stringify_json(event.value())
            .map_err(|_| invalid("event serialization"))?
            .ok_or_else(|| invalid("event is not JSON"))?;
        let seq = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let key = format!("{}:event:{seq}", self.context.call_ref);
        let client = self.client.clone();
        let context = self.context.clone();
        let controller = self.controller.clone();
        let result = self.runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(10),
                product::publish_event(&*client, &context, &controller, &key, raw),
            )
            .await
        });
        if matches!(result, Ok(Ok(()))) {
            return Ok(());
        }
        Err(PortError::AcceptedUnknown {
            identity: Box::new(ExecutionIdentity {
                session_id: self.context.session_ref.as_str().into(),
                turn_id: self.context.interaction.turn.to_string().into(),
                run_id: self.context.call_ref.as_str().into(),
                operation_id: self.context.invocation.correlation_id.as_str().into(),
                request_id: key.clone().into(),
                idempotency_key: Some(key.into()),
                module_instance_id: self.context.binding.id.as_str().into(),
                connection_generation: self.context.interaction.generation.to_string().into(),
            }),
            last_applied_sequence: None,
            cause: None,
        })
    }
}

impl TurnEventPort for ProductEventPort {
    fn emit(&mut self, event: BoundarySnapshot) -> PortResult<()> {
        self.publish(event, false)
    }

    fn emit_accepted(&mut self, event: BoundarySnapshot) -> PortResult<()> {
        self.publish(event, true)
    }
}

pub fn serve() {
    let process_cwd = std::env::args()
        .nth(2)
        .filter(|arg| std::path::Path::new(arg).is_absolute())
        .expect("--native-model-tool-loop requires an absolute project path");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            agentcpm_scheduler_protocol::provider::serve_with_required_host_features(
                agentcpm_scheduler_protocol::provider::Export::Loop(Arc::new(
                    pilotdeck_iface::NativeFactory::new(Arc::new(OriginalPayloadAdmission::new(
                        process_cwd.as_str().into(),
                        Arc::new(ModelToolBindings {
                            controller: "controller".into(),
                        }),
                    )))
                    .with_scoped_steps(),
                )),
                &[agentcpm_scheduler_protocol::step_scope::FEATURE],
            )
            .await
        })
        .expect("native model tool loop serving");
}
