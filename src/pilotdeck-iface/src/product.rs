//! PilotDeck 产品数据在公开 Scheduler 调用、Controller 输出口中的格式。
//! 只承载原始输入、事件和结果；不承载模型/工具/Context 的替代 RPC。
use agentcpm_scheduler_protocol::{self as p, CallContext, Client};
use serde_json::{Value, json};
use std::sync::Arc;

pub const PROFILE: &str = "pilotdeck.turn/1";
pub const EVENT_CONTEXT: &str = "pilotdeck.event";
pub const RESULT_CONTEXT: &str = "pilotdeck.result";
pub const TURN_CONTEXT: &str = "pilotdeck.turn";

pub struct ProductController {
    pub loop_binding: String,
    pub controller_binding: String,
}
fn rejected(reason: &str) -> p::Failure {
    p::Failure::external("PILOTDECK_OUTPUT_CONTRACT", reason)
}
impl p::Controller for ProductController {
    fn handle(
        &self,
        context: CallContext,
        client: Arc<dyn Client>,
    ) -> agentcpm_iface::BoxFuture<'_, p::Result<p::Transition>> {
        Box::pin(async move {
            let current = client.get(&context.call_ref).await?;
            if context.binding.id != self.controller_binding
                || current.binding != context.binding
                || current.session_ref != context.session_ref
                || current.invocation != context.invocation
            {
                return Err(rejected("controller binding or invocation changed"));
            }
            if context.invocation.context_ref == EVENT_CONTEXT {
                let parent = current
                    .cause_execution_ref
                    .as_deref()
                    .ok_or_else(|| rejected("product event has no authoritative source call"))?;
                let source = client.get(parent).await?;
                if current.source_binding.as_deref() != Some(self.loop_binding.as_str())
                    || source.binding.id != self.loop_binding
                    || source.session_ref != context.session_ref
                    || !(source.interaction == context.interaction
                        || source.interactions.contains(&context.interaction))
                {
                    return Err(rejected(
                        "product event source does not match the installed Loop",
                    ));
                }
                let raw = context
                    .invocation
                    .payload
                    .as_str()
                    .ok_or_else(|| rejected("product event must retain original JSON text"))?;
                client.publish_with_demand(
                    &context.invocation.idempotency_key,
                    &context.invocation.correlation_id,
                    json!({"profile":PROFILE,"kind":"agent_event","loop_ref":parent,"event_json":raw}),
                    p::State::Running, None,
                ).await?;
                return Ok(p::Transition::default());
            }
            let event: p::Event = serde_json::from_value(context.invocation.payload.clone())
                .map_err(|_| rejected("input is not a public Scheduler event"))?;
            if event.session_ref != context.session_ref {
                return Err(rejected("event belongs to a different session"));
            }
            if context.invocation.context_ref == RESULT_CONTEXT {
                let call = event
                    .call_ref
                    .as_deref()
                    .ok_or_else(|| rejected("result lacks its originating Loop"))?;
                let result = client.get(call).await?;
                if event.kind != "execution_result"
                    || result.binding.id != self.loop_binding
                    || result.session_ref != context.session_ref
                    || result.interaction != context.interaction
                    || event.state != Some(result.state)
                    || event.payload["result"] != json!(result.result)
                    || event.payload["error"] != json!(result.error)
                    || event.payload["waiting"] != json!(result.waiting)
                    || event.payload.get("settlement_data") != result.settlement_data.as_ref()
                    || event.payload.get("settlement_error").cloned()
                        != result.settlement_error.as_ref().map(|error| json!(error))
                {
                    return Err(rejected("result contradicts the authoritative Loop view"));
                }
                let mut content = json!({"profile":PROFILE,"kind":"turn_result","loop_ref":call,"result":result.result});
                if let Some(data) = &result.settlement_data {
                    // Do not infer a state/permission from any product field.
                    // Kernel's private settlement receipt gates this publish.
                    crate::settlement::decode(data).map_err(rejected)?;
                    content["settlement_data"] = data.clone();
                }
                if let Some(error) = &result.settlement_error {
                    content["settlement_error"] = json!(error);
                }
                client
                    .publish(
                        &context.invocation.idempotency_key,
                        &event.correlation_id,
                        content,
                        result.state,
                        result.error,
                        result.waiting,
                    )
                    .await?;
                return Ok(p::Transition::default());
            }
            if current.source_binding.is_some() {
                return Err(rejected(
                    "only the authenticated input entry may start a product turn",
                ));
            }
            if event.kind == "session_open" && event.call_ref.is_none() {
                return Ok(p::Transition {
                    response: json!({"profile":PROFILE}),
                    ..Default::default()
                });
            }
            if event.kind != "input" || !event.payload.is_string() {
                return Err(rejected("original product input must remain JSON text"));
            }
            let mut invocation = p::Invocation::new(
                &self.loop_binding,
                TURN_CONTEXT,
                format!("pilotdeck.loop:{}", context.call_ref),
                &context.invocation.correlation_id,
                event.payload,
            );
            invocation.limits = context.invocation.limits.clone();
            invocation.return_to = Some(p::Destination::Component {
                binding_ref: self.controller_binding.clone(),
                context_ref: RESULT_CONTEXT.into(),
            });
            let child = client.invoke(invocation).await?;
            Ok(p::Transition {
                response: json!({"profile":PROFILE,"loop_ref":child}),
                ..Default::default()
            })
        })
    }
}

/// 由当前调用实际提交到Kernel的事件发布；构造器不接受任意call_ref。
pub struct PendingProductEvent {
    call: String,
}
pub enum ProductEventSettlement {
    Completed,
    Failed(Option<p::Failure>),
    Cancelled,
    Unknown,
}
impl PendingProductEvent {
    pub fn call_ref(&self) -> &str {
        &self.call
    }
    /// 与观察I/O错误分开返回已确认失败，通知owner可保留原advisory策略。
    pub async fn settlement(&self, client: &dyn Client) -> p::Result<ProductEventSettlement> {
        let result = client.wait(&self.call).await?;
        if result.id != self.call {
            return Err(p::Failure::external(
                "OUTCOME_UNKNOWN",
                "product publication returned another call identity",
            ));
        }
        match result.state {
            p::State::Completed => Ok(ProductEventSettlement::Completed),
            p::State::Failed => Ok(ProductEventSettlement::Failed(result.error)),
            p::State::Cancelled => Ok(ProductEventSettlement::Cancelled),
            p::State::Unknown => Ok(ProductEventSettlement::Unknown),
            _ => Err(p::Failure::external(
                "OUTCOME_UNKNOWN",
                "product publication is not settled",
            )),
        }
    }
    /// 已提交不代表已发布。超时取消此future不消费身份，可由同owner继续回读。
    pub async fn wait(&self, client: &dyn Client) -> p::Result<()> {
        match self.settlement(client).await? {
            ProductEventSettlement::Completed => Ok(()),
            ProductEventSettlement::Failed(error) => Err(error.unwrap_or_else(|| {
                p::Failure::external("PILOTDECK_EVENT_FAILED", "product event publication failed")
            })),
            ProductEventSettlement::Cancelled => Err(p::Failure::external(
                "CANCELLED",
                "product event publication cancelled",
            )),
            ProductEventSettlement::Unknown => Err(p::Failure::external(
                "OUTCOME_UNKNOWN",
                "product event publication is not confirmed",
            )),
        }
    }
}

/// 返回时仅确认真实调用已接纳，供原有有界通知owner跟踪；不得当成发布完成。
pub async fn admit_event(
    client: &dyn Client,
    context: &CallContext,
    controller: &str,
    key: &str,
    original_json: String,
) -> p::Result<PendingProductEvent> {
    if !key
        .strip_prefix(&context.call_ref)
        .is_some_and(|suffix| suffix.starts_with(':') && suffix.len() > 1)
    {
        return Err(rejected(
            "product event key must be scoped to its actual Loop call",
        ));
    }
    let mut invocation = p::Invocation::new(
        controller,
        EVENT_CONTEXT,
        key,
        &context.invocation.correlation_id,
        Value::String(original_json),
    );
    invocation.parent_execution_ref = Some(context.call_ref.clone());
    let call = client.invoke(invocation).await?;
    Ok(PendingProductEvent { call })
}
/// 从 Native Loop 的已绑定 SDK 发出真实产品事件；调用完成才确认发布。
pub async fn publish_event(
    client: &dyn Client,
    context: &CallContext,
    controller: &str,
    key: &str,
    original_json: String,
) -> p::Result<()> {
    admit_event(client, context, controller, key, original_json)
        .await?
        .wait(client)
        .await
}

/// 产品Native helper的Controller角色，只使用正式SDK标准输入/输出协议。
pub async fn serve_controller(controller: ProductController) -> p::Result<()> {
    p::provider::serve(p::provider::Export::Controller(Arc::new(controller))).await
}

#[cfg(test)]
mod tests;
