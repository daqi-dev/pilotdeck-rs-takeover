use super::*;
use crate::{
    host::modules::{
        model::{
            self, ModelExecution, ModelFuture, ModelStream, PrepareInput, PreparedInvocation,
            StreamInput, TurnContext,
        },
        tool_context::{HostToolCheckpoint, ToolContextPorts, TurnBinding, TurnEnvironment},
    },
    native_process::external_modules::{
        context::context_runtime,
        http::{ExternalHttpBinding, HttpModuleClient},
    },
    skills::{
        ModuleEnvironment, ModuleHttpFuture, ModuleHttpRequest, ModuleHttpResponse,
        ModuleHttpTransport,
    },
};
use pilotdeck_protocol::JsString;
use std::sync::Mutex;

struct Environment;
impl TurnEnvironment for Environment {
    fn build(
        &self,
        _: Option<&JsValue>,
        _: &JsString,
        _: &JsString,
        _: &JsString,
    ) -> Result<JsObject, HostError> {
        Ok(JsObject::new())
    }
}
impl ModuleEnvironment for Environment {
    fn var(&self, _: &str) -> Option<String> {
        None
    }
}
struct Execution;
impl ModelExecution for Execution {
    fn prepare(&self, input: PrepareInput) -> ModelFuture<PreparedInvocation> {
        Box::pin(async move {
            Ok(PreparedInvocation::new(
                input.request,
                "p".into(),
                "m".into(),
            ))
        })
    }
    fn stream(&self, _: StreamInput) -> ModelFuture<Box<dyn ModelStream>> {
        Box::pin(async { unreachable!() })
    }
}
struct Transport(Mutex<Vec<JsValue>>);
impl ModuleHttpTransport for Transport {
    fn send<'a>(&'a self, request: ModuleHttpRequest) -> ModuleHttpFuture<'a> {
        Box::pin(async move {
            let body = if request.method == "GET" {
                json(
                    r#"{"protocolVersion":"2.0","implementationId":"context","contract":"pilotdeck.context/v1","transport":"module-http-v2","methods":["prepare_for_model","try_auto_compact"]}"#,
                )
            } else {
                let call =
                    pilotdeck_protocol::parse_json(std::str::from_utf8(&request.body).unwrap())
                        .unwrap();
                self.0.lock().unwrap().push(call.clone());
                let mut result = JsObject::new();
                result.insert("kind".into(), "response".into());
                result.insert("messageId".into(), "response".into());
                result.insert("inReplyTo".into(), call.get("messageId").unwrap().clone());
                result.insert("requestId".into(), call.get("requestId").unwrap().clone());
                result.insert("ok".into(), JsValue::Bool(true));
                result.insert("payload".into(), json(r#"{"result":{"type":"skipped"}}"#));
                JsValue::Object(result)
            };
            Ok(ModuleHttpResponse {
                status: 200,
                body: pilotdeck_protocol::stringify_json(&body)
                    .unwrap()
                    .unwrap()
                    .into_bytes(),
            })
        })
    }
}
fn json(value: &str) -> JsValue {
    pilotdeck_protocol::parse_json(value).unwrap()
}
fn identity() -> ContextRequestIdentity {
    let mut input = JsObject::new();
    input.insert("sessionId".into(), "session".into());
    input.insert("turnId".into(), "turn".into());
    ContextRequestIdentity::new(
        TurnBinding {
            config: JsObject::new(),
            input,
            checkpoint: HostToolCheckpoint::default(),
            abort_signal: None,
            environment: Arc::new(Environment),
        },
        ToolContextPorts::default(),
    )
}

#[tokio::test]
async fn native_external_budget_projection_retains_request_after_real_preparation_validation() {
    let model = model::Handler::with_ports(Arc::new(Execution), None, TurnContext::default());
    model.call(json(r#"{"payload":{"operation":"prepare","preparationId":"known","request":{"provider":"p","model":"m","messages":[]}}}"#)).await.unwrap();
    model.call(json(r#"{"payload":{"operation":"materialize_prepared_request","preparationId":"known","request":{"provider":"p","model":"m","messages":[]}}}"#)).await.unwrap();
    let budget = budget::Handler {
        ports: Some(budget::Ports::local(None)),
        signal: None,
    };
    let transport = Arc::new(Transport(Mutex::new(Vec::new())));
    let binding = ExternalHttpBinding {
        implementation_id: "context".into(),
        contract: "pilotdeck.context/v1".into(),
        transport: "module-http-v2".into(),
        endpoint: "http://context.invalid".into(),
        manifest_path: "/manifest".into(),
        call_path: "/call".into(),
        methods: vec!["prepare_for_model".into(), "try_auto_compact".into()],
        timeout_ms: 10000.0,
        credential_env: None,
    };
    let client = Arc::new(HttpModuleClient::new(
        binding,
        transport.clone(),
        Arc::new(Environment),
    ));
    let runtime = context_runtime(client, "session".into(), None).unwrap();
    let mut native = Handler::new(runtime.clone(), identity(), JsObject::new(), None);
    native.set_native_external_budget_projection(true);
    let sidecar = Handler::new(runtime, identity(), JsObject::new(), None);
    let input = json(
        r#"{"payload":{"operation":"try_auto_compact","input":{"budgetRequest":{"provider":"p","model":"m","messages":[]},"budgetPreparation":{"provider":"p","model":"m","tools":[]},"budgetPreparationId":"known","maxContextTokens":100,"nativeExternalBudgetProjection":true}}}"#,
    );
    native.call(input.clone(), &budget, &model).await.unwrap();
    sidecar.call(input, &budget, &model).await.unwrap();
    {
        let posts = transport.0.lock().unwrap();
        let native_input = posts[0].get("payload").unwrap().get("input").unwrap();
        let sidecar_input = posts[1].get("payload").unwrap().get("input").unwrap();
        assert_eq!(
            native_input.get("budgetRequest"),
            Some(&json(r#"{"provider":"p","model":"m","messages":[]}"#))
        );
        assert!(native_input.get("budgetPreparationId").is_none());
        assert!(sidecar_input.get("budgetRequest").is_none());
        assert_eq!(
            sidecar_input.get("budgetPreparationId"),
            Some(&"known".into())
        );
    }
    let bad = json(
        r#"{"payload":{"operation":"try_auto_compact","input":{"budgetRequest":{"provider":"p","model":"m","messages":[]},"budgetPreparation":{"provider":"p","model":"m","tools":[]},"budgetPreparationId":"missing","maxContextTokens":100}}}"#,
    );
    assert!(native.call(bad, &budget, &model).await.is_err());
    assert_eq!(transport.0.lock().unwrap().len(), 2);
}
