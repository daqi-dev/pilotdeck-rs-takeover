//! 产品执行客户端边界。原 HostClient 与显式安装的 Kernel 客户端共用
//! LiveSession 的准入、历史、持久化门和事件消费者，不在这里另建会话运行时。
use crate::host::{
    Cancellation, ExecuteRequest, ExecuteResult, HostClient, HostError, HostErrorKind, HostFuture,
    HostHandler,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, admission::ExecutionIdentity};
use std::sync::Arc;

/// 由真实会话 prepare 返回并直接保留的后端。实现者须持有认证后的会话能力，
/// 不得用 identity.session_id 或其他请求字符串查询/提升执行权限。
pub trait TurnExecutionBackend: Send + Sync + 'static {
    /// LiveSession 的持久输入门完成前，此 future 不会被轮询。
    fn prepare(&self, facts: TurnExecutionFacts) -> HostFuture<Box<dyn TurnExecutionClient>>;
}

/// The exact original per-turn host handles. They stay inside the trusted
/// product host; none is serialized to the Native provider or used as a
/// replacement Kernel permission. Every tool call still requires its own
/// original complete HostCall and the Kernel's current scoped permit.
#[derive(Clone)]
pub struct TurnExecutionFacts {
    pub identity: ExecutionIdentity,
    pub model: crate::host::modules::model_port::RouterModelPortOptions,
    pub tools: TurnToolFacts,
    pub turn_signal: crate::runtime::hooks::Cancellation,
}

#[derive(Clone)]
pub struct TurnToolFacts {
    pub execution: Arc<dyn crate::host::modules::capability::ToolExecution>,
    pub context: Arc<dyn crate::host::modules::capability::ToolContext>,
    pub checkpoint: crate::host::modules::tool_context::HostToolCheckpoint,
    pub observer: Arc<dyn crate::host::modules::capability::CapabilityResultObserver>,
    pub selection_error: Option<HostError>,
}

impl TurnExecutionFacts {
    pub(crate) fn from_session_ports(
        identity: &ExecutionIdentity,
        ports: &super::host_factory::SessionPorts,
        turn_signal: crate::runtime::hooks::Cancellation,
    ) -> Self {
        Self {
            identity: identity.clone(),
            model: ports.model.clone(),
            turn_signal,
            tools: TurnToolFacts {
                execution: ports.tool_execution.clone(),
                context: ports.tool_context.clone(),
                checkpoint: ports.tool_checkpoint.clone(),
                observer: ports.tool_observer.clone(),
                selection_error: ports.tool_selection_error.clone(),
            },
        }
    }
}

/// 一次 turn 的已绑定客户端。Unknown/取消/连接关闭必须保留真实结果，
/// 不得回退旧执行路径或重放已接纳的写操作。
pub trait TurnExecutionClient: Send + 'static {
    fn module_id(&self) -> JsString;
    fn execute(
        self: Box<Self>,
        request: ExecuteRequest,
        handler: Arc<dyn HostHandler>,
        cancellation: Cancellation,
    ) -> HostFuture<ExecuteResult>;
}

pub(super) enum TurnClient {
    Legacy(HostClient),
    Installed(Box<dyn TurnExecutionClient>),
}
impl TurnClient {
    pub(super) fn module_id(&self) -> JsString {
        match self {
            Self::Legacy(client) => client.binding().module_id.clone(),
            Self::Installed(client) => client.module_id(),
        }
    }
    pub(super) async fn execute_cancellable(
        self,
        request: ExecuteRequest,
        handler: Arc<dyn HostHandler>,
        cancellation: Cancellation,
    ) -> Result<ExecuteResult, HostError> {
        match self {
            Self::Legacy(client) => {
                client
                    .execute_cancellable(request, handler, cancellation)
                    .await
            }
            Self::Installed(client) => client.execute(request, handler, cancellation).await,
        }
    }
}

/// 本地产品兼容投影。输入必须已经经过 Kernel 的来源、call、序号与身份核对；
/// 这里不发送或模拟旧 Sidecar 协议，只进入原 LiveSession 的事件检查与回压。
pub async fn deliver_product_event(
    handler: &Arc<dyn HostHandler>,
    identity: &ExecutionIdentity,
    event: JsValue,
) -> Result<(), HostError> {
    check_product_identity(&event, identity)?;
    if event.get("type") == Some(&JsValue::from("turn_completed")) {
        handler.observe_loop_completion(&event);
        return Ok(());
    }
    let guard = handler.validate_event(&event)?;
    let mut local = JsObject::new();
    local.insert("payload".into(), event);
    local.insert("final".into(), JsValue::Bool(false));
    let result = handler.event(JsValue::Object(local)).await;
    drop(guard);
    result
}

/// 将已确认的 Kernel Loop 结果交给与旧后端相同的完整性检查和落盘流程。
/// 不能借 RecoveredTerminal 跳过本次流的完整性检查。
pub fn product_terminal(
    identity: &ExecutionIdentity,
    result: JsValue,
) -> Result<ExecuteResult, HostError> {
    let inner = result.get("result").ok_or_else(|| {
        HostError::new(
            HostErrorKind::Protocol,
            "Kernel Loop result lacks the product result.",
        )
    })?;
    check_product_identity(inner, identity)?;
    if inner.get("sessionId").and_then(JsValue::as_string) != Some(&identity.session_id)
        || inner.get("turnId").and_then(JsValue::as_string) != Some(&identity.turn_id)
    {
        return Err(HostError::new(
            HostErrorKind::Protocol,
            "Kernel Loop result does not match the admitted product session and turn.",
        ));
    }
    let mut local = JsObject::new();
    local.insert("payload".into(), result);
    Ok(ExecuteResult::Terminal(JsValue::Object(local)))
}

fn check_product_identity(value: &JsValue, identity: &ExecutionIdentity) -> Result<(), HostError> {
    if value.as_object().is_none() {
        return Err(HostError::new(
            HostErrorKind::Protocol,
            "Product event is not an object.",
        ));
    }
    for (key, expected) in [
        ("sessionId", &identity.session_id),
        ("turnId", &identity.turn_id),
    ] {
        if let Some(actual) = value.get(key)
            && actual.as_string() != Some(expected)
        {
            return Err(HostError::new(
                HostErrorKind::Protocol,
                format!("Kernel product event changed {key}."),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::EventGuard;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    fn identity() -> ExecutionIdentity {
        ExecutionIdentity {
            run_id: "run".into(),
            operation_id: "operation".into(),
            request_id: "request".into(),
            session_id: "session".into(),
            turn_id: "turn".into(),
        }
    }
    #[derive(Default)]
    struct Observer {
        delivered: Arc<Mutex<Vec<JsValue>>>,
        completed: AtomicUsize,
        released: Arc<AtomicUsize>,
    }
    impl HostHandler for Observer {
        fn module_call(&self, _: JsValue) -> HostFuture<JsObject> {
            Box::pin(async {
                Err(protocol_for_test(
                    "Kernel event projection must never execute a legacy module.",
                ))
            })
        }
        fn validate_event(&self, _: &JsValue) -> Result<EventGuard, HostError> {
            let released = self.released.clone();
            Ok(EventGuard::new(move || {
                released.fetch_add(1, Ordering::SeqCst);
            }))
        }
        fn event(&self, event: JsValue) -> HostFuture<()> {
            let delivered = self.delivered.clone();
            Box::pin(async move {
                delivered.lock().unwrap().push(event);
                Ok(())
            })
        }
        fn observe_loop_completion(&self, _: &JsValue) {
            self.completed.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn protocol_for_test(message: &str) -> HostError {
        HostError::new(HostErrorKind::Protocol, message)
    }
    #[tokio::test]
    async fn cross_turn_event_is_rejected_before_callback_or_guard() {
        let concrete = Arc::new(Observer::default());
        let handler: Arc<dyn HostHandler> = concrete.clone();
        let raw = pilotdeck_protocol::parse_json(
            r#"{"type":"model_event","sessionId":"session","turnId":"other"}"#,
        )
        .unwrap();
        assert!(
            deliver_product_event(&handler, &identity(), raw)
                .await
                .is_err()
        );
        assert!(concrete.delivered.lock().unwrap().is_empty());
        assert_eq!(concrete.released.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn original_utf16_event_is_preserved_through_the_product_callback() {
        let concrete = Arc::new(Observer::default());
        let handler: Arc<dyn HostHandler> = concrete.clone();
        let raw = pilotdeck_protocol::parse_json(r#"{"type":"model_event","sessionId":"session","turnId":"turn","event":{"text":"\ud800"}}"#).unwrap();
        deliver_product_event(&handler, &identity(), raw.clone())
            .await
            .unwrap();
        let delivered = concrete.delivered.lock().unwrap();
        assert_eq!(delivered[0].get("payload"), Some(&raw));
        assert_eq!(concrete.released.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn loop_completion_is_a_witness_and_not_a_second_product_terminal() {
        let concrete = Arc::new(Observer::default());
        let handler: Arc<dyn HostHandler> = concrete.clone();
        let raw = pilotdeck_protocol::parse_json(r#"{"type":"turn_completed","sessionId":"session","turnId":"turn","result":{"type":"success"}}"#).unwrap();
        deliver_product_event(&handler, &identity(), raw)
            .await
            .unwrap();
        assert_eq!(concrete.completed.load(Ordering::SeqCst), 1);
        assert!(concrete.delivered.lock().unwrap().is_empty());
    }
    #[test]
    fn terminal_identity_is_mandatory_and_does_not_use_recovery_bypass() {
        let wrong = pilotdeck_protocol::parse_json(
            r#"{"result":{"type":"success","sessionId":"other","turnId":"turn"}}"#,
        )
        .unwrap();
        assert!(product_terminal(&identity(), wrong).is_err());
        let correct = pilotdeck_protocol::parse_json(
            r#"{"result":{"type":"success","sessionId":"session","turnId":"turn"},"messages":[]}"#,
        )
        .unwrap();
        let ExecuteResult::Terminal(value) =
            product_terminal(&identity(), correct.clone()).unwrap()
        else {
            panic!("must retain current-stream integrity checks")
        };
        assert_eq!(value.get("payload"), Some(&correct));
    }
}
