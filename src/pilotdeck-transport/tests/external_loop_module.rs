//! The host-port contract of a sidecar running as the baseline's external
//! loop module (`serve_as_external_loop`, set process-wide; this test binary
//! is its own process). Evidence: bbx `sidecar-raw-stdio-model` against the
//! baseline sidecar, and hostModelInvokerPort.ts:171-174, 208-212, 221-223.
use pilotdeck_agent::ports::*;
use pilotdeck_protocol::admission::CallEffect;
use pilotdeck_protocol::model::BoundarySnapshot;
use pilotdeck_protocol::{JsObject, JsValue, parse_json};
use pilotdeck_transport::*;

fn json(s: &str) -> JsValue {
    parse_json(s).unwrap()
}
fn owner() -> ExecutionIdentity {
    ExecutionIdentity {
        session_id: "s".into(),
        turn_id: "t".into(),
        run_id: "r".into(),
        operation_id: "o".into(),
        request_id: "model-abc".into(),
        idempotency_key: None,
        module_instance_id: "instance".into(),
        connection_generation: "generation".into(),
    }
}
struct Exchange(Vec<JsValue>);
impl HostExchange for Exchange {
    fn exchange(
        &mut self,
        _: &CallControl,
        _: &'static str,
        _: &'static str,
        _: CallEffect,
        _: JsObject,
    ) -> PortResult<HostExchangeResponse> {
        Ok(HostExchangeResponse {
            identity: owner(),
            cancelled_before_response: false,
            boundary: BoundarySnapshot::capture(&self.0.remove(0)),
        })
    }
}
fn host(payload: &str) -> RemoteHost<Exchange> {
    let response = format!(
        r#"{{"kind":"response","messageId":"m","inReplyTo":"reply","ok":true,"payload":{payload}}}"#
    );
    RemoteHost::new(
        Exchange(vec![json(&response)]),
        AdvertisedMethods {
            modules: vec![(
                "model".into(),
                vec!["stream".into(), "stream_next".into(), "prepare".into()],
            )],
        },
    )
}
fn control() -> CallControl {
    CallControl {
        cancellation: Cancellation::default(),
        deadline: None,
    }
}
fn hints() -> ModelHints {
    ModelHints {
        session_id: "s".into(),
        turn_id: "t".into(),
        execution: ExecutionHints {
            run_id: "r".into(),
            operation_id: "o".into(),
            idempotency_key: None,
            operation_deadline: None,
        },
        workspace_id: None,
        storage_config_version: None,
        metadata: None,
        model_override: None,
    }
}
fn request() -> CapturedRequest {
    admit_model_request(&json(r#"{"provider":"p","model":"m","messages":[]}"#)).unwrap()
}

#[test]
fn successful_model_reply_without_events_is_the_ports_invalid_model_response() {
    serve_as_external_loop();
    let eager = || HostCall::ModelEagerStream {
        handle: PreparedRequestHandle("prepared-x-1".into()),
        request: Some(Box::new(request())),
        hints: hints(),
    };
    let pull = || HostCall::ModelPull {
        handle: PreparedRequestHandle("prepared-x-1".into()),
        request: Box::new(request()),
        hints: hints(),
    };
    for (call, payload, message) in [
        (
            eager(),
            "{}",
            "Model module response must contain canonical events.",
        ),
        (
            eager(),
            r#"{"events":{}}"#,
            "Model module response must contain canonical events.",
        ),
        (
            pull(),
            r#"{"events":{},"done":true}"#,
            "Model stream_next response must contain events and done.",
        ),
        (
            pull(),
            r#"{"events":[],"done":0}"#,
            "Model stream_next response must contain events and done.",
        ),
        (
            pull(),
            r#"{"events":[]}"#,
            "Model stream_next response must contain events and done.",
        ),
    ] {
        let error = host(payload).call(&control(), call).unwrap_err();
        assert!(
            matches!(&error, PortError::HostFailure { code, message: m, .. }
                if code.equals_str("INVALID_MODEL_RESPONSE") && m.equals_str(message)),
            "{payload}: {error:?}"
        );
    }
    // A well-formed reply is unaffected.
    assert!(matches!(
        host(r#"{"events":[]}"#).call(&control(), eager()),
        Ok(HostReply::ModelEagerEvents(events)) if events.is_empty()
    ));
}
