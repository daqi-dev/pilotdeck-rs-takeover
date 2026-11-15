use agentcpm_iface::types::TurnInput as IfaceTurnInput;
use agentcpm_scheduler_protocol::{self as p, CallContext, Client, State};
use pilotdeck_agent::{
    ports::*,
    turn::{TurnClock, TurnEventPort, TurnInput},
};
use pilotdeck_iface::{
    model_session_codec::ModelSessionCodec,
    original::{OriginalBindings, OriginalPayloadAdmission},
    resources::ResourceCodec,
};
use pilotdeck_protocol::{JsString, model::BoundarySnapshot};
use pilotdeck_turn_preparation::{
    PreparationEffects,
    execute::{FactoryPayloadDecoder, OriginalFactoryPayloadDecoder},
};
use serde_json::{Value, json};
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

// 仅试件的受限组装配置：模型资源真实绑定，未安装的扩展在准入处拒绝。
struct ModelFixture;
impl OriginalBindings for ModelFixture {
    fn resources(
        &self,
        context: &CallContext,
        _: &IfaceTurnInput,
    ) -> PortResult<Box<dyn ResourceCodec>> {
        let raw = context
            .invocation
            .payload
            .as_str()
            .ok_or_else(|| invalid("original payload text required"))?;
        let boundary =
            pilotdeck_protocol::parse_json(raw).map_err(|_| invalid("invalid original payload"))?;
        if boundary
            .get("payload")
            .and_then(|payload| payload.get("hostModules"))
            .is_some()
        {
            return Err(invalid("fixture does not install hostModules"));
        }
        let decoded = OriginalFactoryPayloadDecoder
            .decode(&boundary, &"/fixture".into())
            .map_err(|error| invalid(format!("fixture admission: {error:?}")))?;
        if !decoded.tools.is_empty() || decoded.elicitation_available || decoded.input.can_elicit {
            return Err(invalid("fixture does not install tools or elicitation"));
        }
        Ok(Box::new(ModelSessionCodec::new_with_local_resource_facts(
            context.clone(),
        )))
    }
    fn effects(
        &self,
        context: &CallContext,
        _: &TurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Arc<Mutex<dyn PreparationEffects>>> {
        Ok(Arc::new(Mutex::new(PreparationObserver(Publisher::new(
            context,
            client,
            control,
            "preparation",
        )))))
    }
    fn events(
        &self,
        context: &CallContext,
        _: &TurnInput,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<Box<dyn TurnEventPort + Send>> {
        Ok(Box::new(ProductEvents(Publisher::new(
            context, client, control, "events",
        ))))
    }
    fn clock(&self, _: &CallContext) -> PortResult<Box<dyn TurnClock + Send>> {
        Ok(Box::new(FixtureClock(AtomicU64::new(1))))
    }
}

struct Publisher {
    context: CallContext,
    client: Arc<dyn Client>,
    runtime: tokio::runtime::Handle,
    control: CallControl,
    prefix: &'static str,
    sequence: u64,
}
impl Publisher {
    fn new(
        context: &CallContext,
        client: Arc<dyn Client>,
        control: &CallControl,
        prefix: &'static str,
    ) -> Self {
        Self {
            context: context.clone(),
            client,
            runtime: tokio::runtime::Handle::current(),
            control: control.clone(),
            prefix,
            sequence: 0,
        }
    }
    fn publish(&mut self, mut payload: Value, accepted: bool) -> PortResult<()> {
        let control = if accepted {
            CallControl {
                cancellation: self.control.cancellation.cleanup_scope(),
                deadline: None,
            }
        } else {
            self.control.clone()
        };
        control.check()?;
        self.sequence += 1;
        let key = format!(
            "{}:{}:{}",
            self.context.call_ref, self.prefix, self.sequence
        );
        payload["trusted_turn"] = json!(self.context.interaction.turn);
        let mut invocation = p::Invocation::new(
            "controller",
            "fixture-event",
            &key,
            &self.context.invocation.correlation_id,
            payload,
        );
        invocation.parent_execution_ref = Some(self.context.call_ref.clone());
        let result = self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let call = self.client.invoke(invocation).await?;
                let reply = self.client.wait(&call).await?;
                if reply.state != State::Completed {
                    return Err(p::Failure::external(
                        "FIXTURE_PUBLISH",
                        format!("controller event was not confirmed: {:?}", reply.state),
                    ));
                }
                Ok(())
            })
            .await
        });
        if matches!(result, Ok(Ok(()))) {
            return Ok(());
        }
        // ACK 丢失没有证明发布未发生；不能从同一事件生成第二次调用。
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
        })
    }
}

struct ProductEvents(Publisher);
impl ProductEvents {
    fn send(&mut self, event: BoundarySnapshot, accepted: bool) -> PortResult<()> {
        let raw = pilotdeck_protocol::stringify_json(event.value())
            .map_err(|_| invalid("event serialization"))?
            .ok_or_else(|| invalid("event is not JSON"))?;
        self.0.publish(
            json!({"fixture_kind":"product_event","event_json":raw}),
            accepted,
        )
    }
}
impl TurnEventPort for ProductEvents {
    fn emit(&mut self, event: BoundarySnapshot) -> PortResult<()> {
        self.send(event, false)
    }
    fn emit_accepted(&mut self, event: BoundarySnapshot) -> PortResult<()> {
        self.send(event, true)
    }
}
// 这些是试件观察记录，明确不是产品 lifecycle.dispatch/event.emit 的替代实现。
// 观察调用真实持久化到 Scheduler，失败不能默默成功。
struct PreparationObserver(Publisher);
impl PreparationEffects for PreparationObserver {
    fn notice(&mut self, _: &TurnInput, has_system_prompt: bool) -> PortResult<()> {
        self.0.publish(
            json!({"fixture_kind":"preparation_notice","has_system_prompt":has_system_prompt}),
            false,
        )
    }
    fn before_model_request(&mut self, route: &Route) -> PortResult<()> {
        self.0.publish(json!({"fixture_kind":"preparation_model","provider":route.provider.to_utf8().map_err(|_| invalid("fixture provider encoding"))?,"model":route.model.to_utf8().map_err(|_| invalid("fixture model encoding"))?}), false)
    }
    fn finish(&mut self) -> PortResult<()> {
        self.0
            .publish(json!({"fixture_kind":"preparation_finished"}), true)
    }
}
struct FixtureClock(AtomicU64);
impl TurnClock for FixtureClock {
    fn now_iso(&self) -> JsString {
        "2026-11-15T00:00:00.000Z".into()
    }
    fn uuid(&self) -> JsString {
        format!("fixture-{}", self.0.fetch_add(1, Ordering::SeqCst)).into()
    }
}
struct FixtureController;
impl p::Controller for FixtureController {
    fn handle(
        &self,
        context: CallContext,
        client: Arc<dyn Client>,
    ) -> agentcpm_iface::BoxFuture<'_, p::Result<p::Transition>> {
        Box::pin(async move {
            if context.invocation.context_ref == "fixture-event" {
                client
                    .publish(
                        &context.invocation.idempotency_key,
                        &context.invocation.correlation_id,
                        context.invocation.payload.clone(),
                        State::Running,
                        None,
                        None,
                    )
                    .await?;
                return Ok(p::Transition::default());
            }
            let event: p::Event = serde_json::from_value(context.invocation.payload.clone())
                .map_err(|error| p::Failure::external("FIXTURE_INPUT", error.to_string()))?;
            if event.kind == "session_open"
                && event.call_ref.is_none()
                && event.session_ref == context.session_ref
            {
                return Ok(p::Transition {
                    response: json!({"ready":true,"session_ref":event.session_ref}),
                    ..Default::default()
                });
            }
            if context.invocation.context_ref == "fixture-result" {
                if event.kind != "execution_result" || event.call_ref.is_none() {
                    return Err(p::Failure::external(
                        "FIXTURE_RESULT",
                        "expected the Kernel-authored execution result",
                    ));
                }
                let state = event.state.ok_or_else(|| {
                    p::Failure::external("FIXTURE_RESULT", "execution outcome is missing")
                })?;
                let error =
                    serde_json::from_value::<Option<p::Failure>>(event.payload["error"].clone())
                        .map_err(|error| {
                            p::Failure::external("FIXTURE_RESULT", error.to_string())
                        })?;
                let waiting =
                    serde_json::from_value::<Option<p::Wait>>(event.payload["waiting"].clone())
                        .map_err(|error| {
                            p::Failure::external("FIXTURE_RESULT", error.to_string())
                        })?;
                client
                    .publish(
                        &context.invocation.idempotency_key,
                        &event.correlation_id,
                        json!({"fixture_kind":"product_result","child_ref":event.call_ref,
                        "result":event.payload["result"],"trusted_turn":context.interaction.turn}),
                        state,
                        error,
                        waiting,
                    )
                    .await?;
                return Ok(p::Transition::default());
            }
            if event.kind != "input" {
                return Err(p::Failure::external(
                    "FIXTURE_INPUT",
                    "only original input is accepted",
                ));
            }
            let mut invocation = p::Invocation::new(
                "pilotdeck",
                "default",
                format!("native-{}", context.call_ref),
                &context.invocation.correlation_id,
                event.payload,
            );
            // Client.invoke 不能直投 Session。Kernel 将结果交回已注册的
            // input Controller，由该 Controller 的授权 output port 发布。
            invocation.return_to = Some(p::Destination::Component {
                binding_ref: "controller".into(),
                context_ref: "fixture-result".into(),
            });
            let child_ref = client.invoke(invocation).await?;
            Ok(p::Transition {
                response: json!({"child_ref":child_ref}),
                ..Default::default()
            })
        })
    }
}
pub fn serve(controller: bool) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            if controller {
                p::provider::serve(p::provider::Export::Controller(Arc::new(FixtureController)))
                    .await
            } else {
                pilotdeck_iface::serve(Arc::new(OriginalPayloadAdmission::new(
                    "/fixture".into(),
                    Arc::new(ModelFixture),
                )))
                .await
            }
        })
        .expect("native provider serving");
}
