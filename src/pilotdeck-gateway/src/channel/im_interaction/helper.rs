//! Shared IM helper orchestration. Both JS-string and legacy scalar ports use
//! these same permission and elicitation state machines.
use std::sync::{Arc, Mutex as StdMutex};

use pilotdeck_protocol::{JsString, JsValue};

use crate::channel::im_interaction::{
    InteractionFuture, JsInteractionPort, JsPermissionAnswer as PortAnswer,
};
use crate::channel::protocol::{BoxFuture, ChannelError};
use crate::channel::sms::{
    self, AnswerGuard, BeginAnswer, DecisionReply, ElicitationResponse, ImElicitationHelper,
    ImPermissionHelper, PermissionAnswer, PermissionDecision, PermissionDecisionOutcome,
    PermissionState,
};

/// `gateway.respondElicitation(response)` / `gateway.permissionDecide(decision)`
/// — everything the TS helpers need from the gateway besides `submitTurn`.
pub trait InteractionResponder: Send + Sync {
    fn respond_elicitation(
        &self,
        response: ElicitationResponse,
    ) -> BoxFuture<Result<(), ChannelError>>;
    fn permission_decide(
        &self,
        decision: PermissionDecision,
    ) -> BoxFuture<Result<PermissionDecisionOutcome, ChannelError>>;
}

pub type ResponderRef = Arc<dyn InteractionResponder>;

/// The `InteractionPort` the channel drives, with the real helper state
/// machines (`ImElicitationHelper.ts` / `ImPermissionHelper.ts`) behind it.
pub struct JsHelperInteraction {
    responder: ResponderRef,
    elicitation: StdMutex<ImElicitationHelper>,
    permissions: PermissionState,
}

impl JsHelperInteraction {
    pub fn new(responder: ResponderRef) -> Self {
        Self {
            responder,
            elicitation: StdMutex::new(ImElicitationHelper::default()),
            permissions: Arc::new(StdMutex::new(ImPermissionHelper::default())),
        }
    }
}

/// Preserve JS text while adapting the existing helper token width.
fn port_answer(answer: PermissionAnswer) -> PortAnswer {
    PortAnswer {
        text: answer.text,
        can_advance: answer.can_advance,
        retry_prompt: answer.retry_prompt,
        answer_token: answer.answer_token.map(u64::from),
    }
}

impl JsInteractionPort for JsHelperInteraction {
    fn clear_chat_for_shutdown(&self, chat_id: &JsString) {
        self.elicitation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear(chat_id);
        self.permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear(chat_id);
    }

    fn elicitation_pending(&self, chat_id: &JsString) -> bool {
        let id = chat_id;
        self.elicitation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .has_pending(id)
    }

    /// Capture the prompt without normalizing UTF-16 units.
    fn elicitation_capture(
        &self,
        chat_id: &JsString,
        session_key: &JsString,
        event: &JsValue,
    ) -> JsString {
        let id = chat_id;

        self.elicitation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capture(id, session_key, event)
    }

    /// `ImElicitationHelper.answer` including the `respondElicitation` await:
    /// the channel sees the confirmation text or the error, never the helper
    /// internals. `take` runs in the same lock section as the pending check
    /// (#1339 fix-r1's fused shape — TS runs the stretch synchronously).
    fn elicitation_answer<'a>(
        &'a self,
        chat_id: &'a JsString,
        text: &'a JsString,
    ) -> InteractionFuture<'a, Result<Option<JsString>, ChannelError>> {
        Box::pin(async move {
            let id = chat_id;
            let entry = {
                let mut elicitation = self
                    .elicitation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                elicitation.take(id)
            };
            let Some(entry) = entry else {
                return Ok(None);
            };
            let trimmed = JsString::from_utf16(sms::trim_units(text.units()).to_vec());
            if trimmed.equals_str("0") {
                let response = sms::elicitation::cancelled_response(&entry);
                self.responder.respond_elicitation(response).await?;
                Ok(Some("已取消。".into()))
            } else {
                let response = sms::elicitation::answered_response(&entry, &trimmed);
                self.responder.respond_elicitation(response).await?;
                Ok(None)
            }
        })
    }

    fn elicitation_clear(&self, chat_id: &JsString) {
        let id = chat_id;
        self.elicitation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear(id);
    }

    fn permission_pending(&self, chat_id: &JsString) -> bool {
        let id = chat_id;
        self.permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .has_pending(id)
    }

    fn permission_capture(
        &self,
        chat_id: &JsString,
        session_key: &JsString,
        event: &JsValue,
    ) -> Option<JsString> {
        let id = chat_id;

        self.permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capture(id, session_key, event)
    }

    fn permission_confirm_initial(
        &self,
        chat_id: &JsString,
        delivered: bool,
        request_id: Option<&JsString>,
    ) {
        let id = chat_id;
        let expected = request_id;
        self.permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .confirm_initial_prompt(id, delivered, expected);
    }

    /// `ImPermissionHelper.answerWithState`: the state machine's synchronous
    /// prefix (`begin_answer`), the single `permissionDecide` await, then
    /// `finish_answer` / `fail_answer`. The `AnswerGuard` owns the in-flight
    /// release across the await (#1790), exactly like the sms lane.
    fn permission_answer<'a>(
        &'a self,
        chat_id: &'a JsString,
        text: &'a JsString,
    ) -> InteractionFuture<'a, Result<Option<PortAnswer>, ChannelError>> {
        Box::pin(async move {
            let id = chat_id;

            let began = {
                let mut permissions = self
                    .permissions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                permissions.begin_answer(id, text)
            };
            match began {
                BeginAnswer::None => Ok(None),
                BeginAnswer::Status(answer) => Ok(Some(port_answer(answer))),
                BeginAnswer::Decide {
                    entry,
                    deny,
                    remember,
                    answer_token,
                    generation,
                    shifted_rest,
                } => {
                    let mut guard = AnswerGuard::new(
                        self.permissions.clone(),
                        id.clone(),
                        generation,
                        entry.clone(),
                        shifted_rest.clone(),
                    );
                    let decision = PermissionDecision {
                        session_key: entry.session_key.clone(),
                        request_id: entry.request_id.clone(),
                        deny,
                        remember,
                    };
                    let reply = if deny {
                        DecisionReply::Deny
                    } else if remember {
                        DecisionReply::AllowSession
                    } else {
                        DecisionReply::AllowOnce
                    };
                    match self.responder.permission_decide(decision).await {
                        Err(error) => {
                            self.permissions
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .fail_answer(id, generation, entry, shifted_rest);
                            guard.settle();
                            Err(error)
                        }
                        Ok(outcome) => {
                            let answer = self
                                .permissions
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .finish_answer(
                                    id,
                                    answer_token,
                                    generation,
                                    reply,
                                    shifted_rest,
                                    outcome,
                                );
                            guard.settle();
                            Ok(answer.map(port_answer))
                        }
                    }
                }
            }
        })
    }

    /// The TS channel's catch-side `releaseAnswer(chatGuid, answerToken)` only
    /// ever ran with an assigned token when the reply POST failed to deliver;
    /// the decide-failure path leaves the token unset (the helper's own catch
    /// restored the state), so the error arm here releases nothing.
    fn permission_release_answer(&self, chat_id: &JsString, answer_token: Option<u64>) {
        let id = chat_id;
        self.permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release_answer(id, answer_token.map(|token| token as u32));
    }

    fn permission_take_next_prompt(
        &self,
        chat_id: &JsString,
        answer_token: Option<u64>,
    ) -> Option<JsString> {
        let id = chat_id;
        self.permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_next_prompt(id, answer_token.map(|token| token as u32))
    }

    fn permission_prompt_request_id(
        &self,
        chat_id: &JsString,
        answer_token: Option<u64>,
    ) -> Option<JsString> {
        let id = chat_id;
        self.permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_prompt_request_id(id, answer_token.map(|token| token as u32))
    }

    fn permission_confirm_next(
        &self,
        chat_id: &JsString,
        delivered: bool,
        request_id: Option<&JsString>,
        answer_token: Option<u64>,
    ) {
        let id = chat_id;
        let expected = request_id;
        self.permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .confirm_next_prompt(
                id,
                delivered,
                expected,
                answer_token.map(|token| token as u32),
            );
    }

    fn permission_clear_after_turn(&self, chat_id: &JsString) {
        let id = chat_id;
        self.permissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear_after_turn(id);
    }
}
