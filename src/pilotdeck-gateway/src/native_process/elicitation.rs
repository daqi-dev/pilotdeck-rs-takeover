//! The elicitation channel of a native session: `GatewayElicitationChannel`
//! (the `elicitation_request` round-trip over the session's gateway stream)
//! inside `createDurableElicitationChannel` (the `question_started` /
//! `question_completed` / `question_failed` bracket in the transcript).
use super::permission_prompt::GatewayEmitSlot;
use crate::{
    runtime::{
        interaction::{GatewayInteractionCoordinator, PendingEntry},
        tools::{ElicitationChannel, ElicitationRequest, ToolError, ToolFuture, ToolRunContext},
    },
    storage::recorder::Recorder,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsObject {
    let mut object = JsObject::new();
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    object
}

#[derive(Clone)]
pub(super) struct GatewayElicitation {
    pub session: JsString,
    pub coordinator: Arc<GatewayInteractionCoordinator>,
    pub emit: GatewayEmitSlot,
    pub recorder: Arc<Recorder>,
    pub timeout: Duration,
    /// `questionProvider: "deterministic"` (the headless profile): the
    /// request is answered by `createDeterministicElicitationAnswerer`
    /// through `createElicitationChannelFromAnswerer`, inside the same
    /// durable bracket, without a gateway round-trip.
    pub deterministic: Option<super::permission_prompt::SessionInteraction>,
}

/// `createElicitationChannelFromAnswerer(createDeterministicElicitationAnswerer(),
/// { policy, policyMode, canPrompt })`.
fn deterministic_answer(
    request: &ElicitationRequest,
    interaction: &super::permission_prompt::SessionInteraction,
) -> JsValue {
    if let Some(reason) = interaction.refusal(true) {
        return JsValue::Object(object([
            ("type", "cancelled".into()),
            ("reason", reason.into()),
        ]));
    }
    let mut answers = JsObject::new();
    if let JsValue::Array(questions) = &request.questions {
        for question in questions.iter() {
            let text = match question.get("question") {
                Some(JsValue::String(text)) => text.clone(),
                Some(other) => crate::js_value::to_string_units(other)
                    .map(JsString::from_utf16)
                    .unwrap_or_default(),
                None => "undefined".into(),
            };
            let first = match question.get("options") {
                Some(JsValue::Array(options)) if !options.is_empty() => Some(
                    options[0]
                        .get("label")
                        .cloned()
                        .unwrap_or(JsValue::Undefined),
                ),
                _ => None,
            };
            let multi = question
                .get("multiSelect")
                .is_some_and(|value| match value {
                    JsValue::Undefined | JsValue::Null => false,
                    JsValue::Bool(value) => *value,
                    JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
                    JsValue::String(value) => !value.is_empty(),
                    _ => true,
                });
            let answer = match first {
                Some(label) if multi => JsValue::Array(vec![label]),
                Some(label) => label,
                None => "yes".into(),
            };
            answers.insert(text, answer);
        }
    }
    JsValue::Object(object([
        ("type", "answered".into()),
        ("answers", JsValue::Object(answers)),
    ]))
}

impl GatewayElicitation {
    fn deliver(&self, event: JsObject) {
        if let Some(emit) = self.emit.get() {
            emit(&self.session, JsValue::Object(event));
        }
    }

    /// One transcript entry of the question bracket. They belong to the turn,
    /// not to a step.
    async fn record(&self, turn: &JsString, draft: JsObject) -> Result<(), ToolError> {
        let recorder = self.recorder.clone();
        let (session, turn) = (self.session.clone(), turn.clone());
        tokio::task::spawn_blocking(move || {
            recorder
                .writer()
                .record_session_event(&session, &turn, draft)
        })
        .await
        .map_err(|error| ToolError::failed(error.to_string()))?
        .map_err(|error| ToolError::failed(error.to_string()))
    }

    /// `GatewayElicitationChannel.askUser`: `Ok` is the host's answer (or a
    /// synthesized cancellation), `Err` the reason the pending entry was
    /// rejected with.
    async fn round_trip(
        &self,
        request: &ElicitationRequest,
        context: &ToolRunContext,
    ) -> Result<JsValue, JsString> {
        let request_id: JsString = uuid::Uuid::new_v4().to_string().into();
        let (sender, mut receiver) = oneshot::channel::<Result<JsValue, JsString>>();
        let sender = Arc::new(Mutex::new(Some(sender)));
        let (resolve, reject) = (sender.clone(), sender);
        let bus = self.coordinator.elicitation_bus();
        // The reconnect snapshot keeps the serializable request only.
        let mut replay = object([
            ("toolCallId", JsValue::String(request.tool_call_id.clone())),
            ("toolName", JsValue::String(request.tool_name.clone())),
        ]);
        if let Some(format) = &request.preview_format {
            replay.insert("previewFormat".into(), JsValue::String(format.clone()));
        }
        replay.insert("questions".into(), request.questions.clone());
        if let Some(metadata) = &request.metadata {
            replay.insert("metadata".into(), metadata.clone());
        }
        let registration = bus.register(
            &self.session,
            PendingEntry {
                request_id: request_id.clone(),
                tool_call_id: request.tool_call_id.clone(),
                tool_name: request.tool_name.clone(),
                resolve: Arc::new(move |value| {
                    if let Some(sender) = resolve.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        let _ = sender.send(Ok(value));
                    }
                }),
                reject: Arc::new(move |reason| {
                    if let Some(sender) = reject.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        let _ = sender.send(Err(reason));
                    }
                }),
            },
            Some(JsValue::Object(replay)),
        )?;
        let mut event = object([
            ("type", "elicitation_request".into()),
            ("requestId", JsValue::String(request_id.clone())),
            ("toolCallId", JsValue::String(request.tool_call_id.clone())),
            ("toolName", JsValue::String(request.tool_name.clone())),
        ]);
        if let Some(format) = &request.preview_format {
            event.insert("previewFormat".into(), JsValue::String(format.clone()));
        }
        event.insert("questions".into(), request.questions.clone());
        if let Some(metadata) = &request.metadata {
            event.insert("metadata".into(), metadata.clone());
        }
        self.deliver(event);
        // Whoever takes the entry off the bus settles it; a cancellation is
        // also announced so the host can drop its dialog.
        let cancel = |reason: &str| -> Option<Result<JsValue, JsString>> {
            bus.consume(&self.session, &request_id, None)?;
            self.deliver(object([
                ("type", "elicitation_cancelled".into()),
                ("requestId", JsValue::String(request_id.clone())),
                ("reason", reason.into()),
            ]));
            Some(Ok(JsValue::Object(object([
                ("type", "cancelled".into()),
                ("reason", reason.into()),
            ]))))
        };
        let dropped = || Err(JsString::from("elicitation_channel_disposed"));
        let settled = if context.is_aborted() {
            cancel("aborted")
        } else {
            tokio::select! {
                biased;
                answer = &mut receiver => Some(answer.unwrap_or_else(|_| dropped())),
                _ = context.cancelled() => cancel("aborted"),
                _ = tokio::time::sleep(self.timeout) => cancel("timeout"),
            }
        };
        registration.dispose();
        match settled {
            Some(answer) => answer,
            None => receiver.await.unwrap_or_else(|_| dropped()),
        }
    }
}

impl ElicitationChannel for GatewayElicitation {
    fn ask_user(
        &self,
        request: ElicitationRequest,
        context: ToolRunContext,
    ) -> ToolFuture<JsValue> {
        let channel = self.clone();
        Box::pin(async move {
            let operation: JsString = uuid::Uuid::new_v4().to_string().into();
            // The bracket is recorded against the request's own `toolCallId`,
            // which `ask_user_question` fills with the turn id.
            let turn = request.tool_call_id.clone();
            let count = match &request.questions {
                JsValue::Array(questions) => questions.len() as f64,
                _ => 0.0,
            };
            channel
                .record(
                    &turn,
                    object([
                        ("type", "question_started".into()),
                        ("operationId", JsValue::String(operation.clone())),
                        ("toolCallId", JsValue::String(request.tool_call_id.clone())),
                        ("toolName", JsValue::String(request.tool_name.clone())),
                        ("questionCount", JsValue::Number(count)),
                    ]),
                )
                .await?;
            let answer = match &channel.deterministic {
                Some(interaction) => Ok(deterministic_answer(&request, interaction)),
                None => channel.round_trip(&request, &context).await,
            };
            match answer {
                Ok(answer) => {
                    let cancelled = answer.get("type") == Some(&JsValue::from("cancelled"));
                    let mut draft = object([
                        ("type", "question_completed".into()),
                        ("operationId", JsValue::String(operation)),
                        (
                            "status",
                            if cancelled { "cancelled" } else { "answered" }.into(),
                        ),
                        ("questionCount", JsValue::Number(count)),
                    ]);
                    if cancelled
                        && let Some(reason) = answer.get("reason").filter(|reason| {
                            !matches!(reason, JsValue::Undefined | JsValue::Null)
                                && !matches!(reason, JsValue::String(text) if text.is_empty())
                        })
                    {
                        draft.insert("reason".into(), reason.clone());
                    }
                    channel.record(&turn, draft).await?;
                    Ok(answer)
                }
                Err(reason) => {
                    // The failure record is best effort; the rejection is
                    // what the tool reports.
                    let _ = channel
                        .record(
                            &turn,
                            object([
                                ("type", "question_failed".into()),
                                ("operationId", JsValue::String(operation)),
                                ("error", JsValue::String(reason.clone())),
                            ]),
                        )
                        .await;
                    Err(ToolError::failed(reason))
                }
            }
        })
    }
}
