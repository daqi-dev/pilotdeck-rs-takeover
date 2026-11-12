use super::*;
use crate::{
    metadata::object,
    runtime::interaction::{GatewayInteractionCoordinator, InteractionOptions, PendingEntry},
};
use pilotdeck_protocol::{JsString, JsValue, parse_json, stringify_json};
use std::sync::{Arc, Mutex};

static UNDEFINED: JsValue = JsValue::Undefined;

fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}
fn string(value: &JsValue, key: &str) -> JsString {
    field(value, key).as_string().cloned().unwrap_or_default()
}
fn label(value: &JsValue, key: &str, expected: &str) -> bool {
    field(value, key)
        .as_string()
        .is_some_and(|value| value.equals_str(expected))
}
fn array(value: &JsValue) -> &[JsValue] {
    let JsValue::Array(values) = value else {
        panic!("expected array")
    };
    values
}
fn optional(value: Option<JsString>) -> JsValue {
    value.map(JsValue::String).unwrap_or(JsValue::Undefined)
}
fn answer_value(value: Option<JsPermissionAnswer>) -> JsValue {
    value
        .map(|answer| {
            object([
                ("text", optional(answer.text)),
                ("canAdvance", JsValue::Bool(answer.can_advance)),
                (
                    "retryPrompt",
                    if answer.retry_prompt {
                        JsValue::Bool(true)
                    } else {
                        JsValue::Undefined
                    },
                ),
                (
                    "answerToken",
                    answer
                        .answer_token
                        .map(|value| JsValue::Number(value as f64))
                        .unwrap_or(JsValue::Undefined),
                ),
            ])
        })
        .unwrap_or(JsValue::Undefined)
}

async fn replay(case: &JsValue, mutate_deny: bool) -> JsValue {
    let observed: Arc<Mutex<Vec<JsValue>>> = Arc::default();
    let delivered = observed.clone();
    let coordinator = Arc::new(GatewayInteractionCoordinator::new(InteractionOptions {
        on_elicitation_delivered: Some(Arc::new(move |session, id| {
            delivered.lock().unwrap().push(object([
                ("event", "delivered".into()),
                ("session", JsValue::String(session.clone())),
                ("id", JsValue::String(id.clone())),
            ]))
        })),
        ..Default::default()
    }));
    let mut responder: ResponderRef =
        Arc::new(NativeInteractionResponder::new(coordinator.clone()));
    if mutate_deny {
        responder = Arc::new(MutateDeny(responder));
    }
    let helpers = [
        js_helper_interaction(responder.clone()),
        js_helper_interaction(responder),
    ];
    let mut tokens = [None, None];
    let chat = JsString::from_utf16("chat".encode_utf16().chain([0xd800]).collect());
    let mut trace = Vec::new();
    for step in array(field(case, "steps")) {
        let session = field(step, "session")
            .as_string()
            .cloned()
            .unwrap_or_else(|| "session".into());
        let index = field(step, "helper").as_number().unwrap_or(0.0) as usize;
        let helper = &helpers[index];
        let mut result = JsValue::Undefined;
        if label(step, "op", "register") {
            let id = string(step, "id");
            let (resolve, reject) = (observed.clone(), observed.clone());
            let (resolved_session, rejected_session) = (session.clone(), session.clone());
            let (resolved_id, rejected_id) = (id.clone(), id.clone());
            let entry = PendingEntry {
                request_id: id,
                tool_call_id: "tc".into(),
                tool_name: "fixture".into(),
                resolve: Arc::new(move |value| {
                    resolve.lock().unwrap().push(object([
                        ("event", "resolve".into()),
                        ("session", JsValue::String(resolved_session.clone())),
                        ("id", JsValue::String(resolved_id.clone())),
                        ("value", value),
                    ]))
                }),
                reject: Arc::new(move |message| {
                    reject.lock().unwrap().push(object([
                        ("event", "reject".into()),
                        ("session", JsValue::String(rejected_session.clone())),
                        ("id", JsValue::String(rejected_id.clone())),
                        ("message", JsValue::String(message)),
                    ]))
                }),
            };
            if label(step, "kind", "permission") {
                coordinator
                    .permission_bus()
                    .register(&session, entry, None)
                    .unwrap();
            } else {
                coordinator
                    .elicitation_bus()
                    .register(&session, entry, None)
                    .unwrap();
            }
        } else if label(step, "op", "capture") {
            result = if label(step, "kind", "permission") {
                optional(helper.permission_capture(&chat, &session, field(step, "event")))
            } else {
                JsValue::String(helper.elicitation_capture(&chat, &session, field(step, "event")))
            };
        } else if label(step, "op", "confirm") {
            helper.permission_confirm_initial(
                &chat,
                field(step, "delivered").as_bool().unwrap(),
                field(step, "id").as_string(),
            );
        } else if label(step, "op", "answer") {
            let text = string(step, "text");
            if label(step, "kind", "question") {
                result = optional(helper.elicitation_answer(&chat, &text).await.unwrap());
            } else {
                let answer = helper.permission_answer(&chat, &text).await.unwrap();
                if let Some(token) = answer.as_ref().and_then(|answer| answer.answer_token) {
                    tokens[index] = Some(token);
                }
                result = answer_value(answer);
            }
        } else if label(step, "op", "cancel") {
            coordinator.reject_pending_turn(&session, &string(step, "reason"));
        } else if label(step, "op", "next") {
            result = object([
                (
                    "text",
                    optional(helper.permission_take_next_prompt(&chat, tokens[index])),
                ),
                (
                    "id",
                    optional(helper.permission_prompt_request_id(&chat, tokens[index])),
                ),
            ]);
        } else if label(step, "op", "confirm_next") {
            helper.permission_confirm_next(
                &chat,
                field(step, "delivered").as_bool().unwrap(),
                helper
                    .permission_prompt_request_id(&chat, tokens[index])
                    .as_ref(),
                tokens[index],
            );
        } else if label(step, "op", "release") {
            helper.permission_release_answer(&chat, tokens[index]);
        } else {
            panic!("unknown fixture operation");
        }
        trace.push(object([
            ("op", field(step, "op").clone()),
            ("result", result),
            (
                "helperQuestion",
                JsValue::Bool(helper.elicitation_pending(&chat)),
            ),
            (
                "helperPermission",
                JsValue::Bool(helper.permission_pending(&chat)),
            ),
            (
                "pendingQuestion",
                JsValue::Number(coordinator.elicitation_bus().pending_count(&session) as f64),
            ),
            (
                "pendingPermission",
                JsValue::Number(coordinator.permission_bus().pending_count(&session) as f64),
            ),
            ("observed", JsValue::Array(observed.lock().unwrap().clone())),
        ]));
    }
    // Like JSON.stringify in the generator, omit real undefined fields.
    parse_json(&stringify_json(&JsValue::Array(trace)).unwrap().unwrap()).unwrap()
}

struct MutateDeny(ResponderRef);
impl InteractionResponder for MutateDeny {
    fn respond_elicitation(
        &self,
        response: crate::channel::sms::ElicitationResponse,
    ) -> crate::channel::protocol::BoxFuture<Result<(), ChannelError>> {
        self.0.respond_elicitation(response)
    }
    fn permission_decide(
        &self,
        mut decision: crate::channel::sms::PermissionDecision,
    ) -> crate::channel::protocol::BoxFuture<
        Result<crate::channel::sms::PermissionDecisionOutcome, ChannelError>,
    > {
        decision.deny = false;
        self.0.permission_decide(decision)
    }
}

#[tokio::test]
async fn h1_native_im_answers_match_fixed_ts_pending_buses() {
    let oracle = parse_json(include_str!(
        "../../../tests/fixtures/h1-im-interaction/oracle.json"
    ))
    .unwrap();
    for case in array(field(&oracle, "cases")) {
        assert_eq!(
            &replay(case, false).await,
            field(case, "trace"),
            "{}",
            string(case, "id").to_utf8().unwrap()
        );
    }
}

#[tokio::test]
async fn h1_native_im_oracle_detects_wrong_permission_decision() {
    let oracle = parse_json(include_str!(
        "../../../tests/fixtures/h1-im-interaction/oracle.json"
    ))
    .unwrap();
    let case = array(field(&oracle, "cases"))
        .iter()
        .find(|case| label(case, "id", "permission_0"))
        .unwrap();
    assert_ne!(&replay(case, true).await, field(case, "trace"));
}

struct InterruptOnce {
    inner: ResponderRef,
    first: std::sync::atomic::AtomicBool,
    entered: Arc<tokio::sync::Notify>,
    fail: bool,
}
impl InteractionResponder for InterruptOnce {
    fn respond_elicitation(
        &self,
        response: crate::channel::sms::ElicitationResponse,
    ) -> crate::channel::protocol::BoxFuture<Result<(), ChannelError>> {
        self.inner.respond_elicitation(response)
    }
    fn permission_decide(
        &self,
        decision: crate::channel::sms::PermissionDecision,
    ) -> crate::channel::protocol::BoxFuture<
        Result<crate::channel::sms::PermissionDecisionOutcome, ChannelError>,
    > {
        if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let entered = self.entered.clone();
            let fail = self.fail;
            Box::pin(async move {
                entered.notify_one();
                if fail {
                    Err(ChannelError::error("fixture responder failed"))
                } else {
                    std::future::pending().await
                }
            })
        } else {
            self.inner.permission_decide(decision)
        }
    }
}

async fn interrupted_answer_retries(fail: bool) {
    let coordinator = Arc::new(GatewayInteractionCoordinator::default());
    let resolved = Arc::new(Mutex::new(Vec::new()));
    let receipts = resolved.clone();
    coordinator
        .permission_bus()
        .register(
            &"s".into(),
            PendingEntry {
                request_id: "p".into(),
                tool_call_id: "tc".into(),
                tool_name: "fixture".into(),
                resolve: Arc::new(move |value| receipts.lock().unwrap().push(value)),
                reject: Arc::new(|_| panic!("permission should resolve")),
            },
            None,
        )
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let helper = js_helper_interaction(Arc::new(InterruptOnce {
        inner: Arc::new(NativeInteractionResponder::new(coordinator.clone())),
        first: std::sync::atomic::AtomicBool::new(true),
        entered: entered.clone(),
        fail,
    }));
    let chat = "c".into();
    let text = "1".into();
    helper.permission_capture(
        &chat,
        &"s".into(),
        &object([("requestId", "p".into()), ("toolName", "fixture".into())]),
    );
    helper.permission_confirm_initial(&chat, true, Some(&"p".into()));
    let mut answer = helper.permission_answer(&chat, &text);
    if fail {
        assert!(answer.as_mut().await.is_err());
    } else {
        tokio::time::timeout(std::time::Duration::from_secs(2),async {
            tokio::select! { biased; _=entered.notified()=>{}, result=&mut answer=>panic!("unexpected response {result:?}") }
        }).await.unwrap();
    }
    drop(answer);
    assert!(helper.permission_pending(&chat));
    assert_eq!(coordinator.permission_bus().pending_count(&"s".into()), 1);
    let retry = helper
        .permission_answer(&chat, &text)
        .await
        .unwrap()
        .unwrap();
    assert!(retry.can_advance);
    assert_eq!(resolved.lock().unwrap().len(), 1);
    assert_eq!(coordinator.permission_bus().pending_count(&"s".into()), 0);
}

#[tokio::test]
async fn h1_permission_responder_failure_restores_retry() {
    interrupted_answer_retries(true).await;
}

#[tokio::test]
async fn h1_cancelled_permission_answer_restores_retry() {
    interrupted_answer_retries(false).await;
}

#[tokio::test]
async fn h1_native_response_settles_bus_before_helper_continuation() {
    let coordinator = Arc::new(GatewayInteractionCoordinator::default());
    coordinator
        .permission_bus()
        .register(
            &"s".into(),
            PendingEntry {
                request_id: "p".into(),
                tool_call_id: "tc".into(),
                tool_name: "fixture".into(),
                resolve: Arc::new(|_| {}),
                reject: Arc::new(|_| panic!("unexpected reject")),
            },
            None,
        )
        .unwrap();
    let helper = js_helper_interaction(Arc::new(NativeInteractionResponder::new(
        coordinator.clone(),
    )));
    let chat = "c".into();
    let text = "1".into();
    helper.permission_capture(
        &chat,
        &"s".into(),
        &object([("requestId", "p".into()), ("toolName", "fixture".into())]),
    );
    helper.permission_confirm_initial(&chat, true, Some(&"p".into()));
    let mut answer = helper.permission_answer(&chat, &text);
    std::future::poll_fn(|cx| {
        assert!(
            answer.as_mut().poll(cx).is_pending(),
            "TS helper continuation must yield after the gateway call"
        );
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(coordinator.permission_bus().pending_count(&"s".into()), 0);
    let status = helper
        .permission_answer(&chat, &text)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.text, Some("权限决定处理中，请稍候。".into()));
    assert!(!status.can_advance);
    assert!(answer.await.unwrap().unwrap().can_advance);
}
