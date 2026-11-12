//! Compatibility facade for the original scalar-string IM port. The shared
//! helper owns all state transitions; this facade retains the existing lossy
//! output boundary for adapters that already used `InteractionPort`.
use pilotdeck_protocol::{JsString, JsValue};

use crate::channel::im_interaction::{
    InteractionFuture, InteractionPort, JsHelperInteraction, JsInteractionPort, JsPermissionAnswer,
    PermissionAnswer,
};
use crate::channel::protocol::ChannelError;

pub use crate::channel::im_interaction::{InteractionResponder, ResponderRef};

pub struct HelperInteraction(JsHelperInteraction);

impl HelperInteraction {
    pub fn new(responder: ResponderRef) -> Self {
        Self(JsHelperInteraction::new(responder))
    }
}

fn scalar(text: JsString) -> String {
    String::from_utf16_lossy(text.units())
}

fn scalar_answer(answer: JsPermissionAnswer) -> PermissionAnswer {
    PermissionAnswer {
        text: answer.text.map(scalar),
        can_advance: answer.can_advance,
        retry_prompt: answer.retry_prompt,
        answer_token: answer.answer_token,
    }
}

impl InteractionPort for HelperInteraction {
    fn elicitation_pending(&self, chat_id: &str) -> bool {
        self.0.elicitation_pending(&chat_id.into())
    }

    fn elicitation_capture(&self, chat_id: &str, session_key: &str, event: &JsValue) -> String {
        scalar(
            self.0
                .elicitation_capture(&chat_id.into(), &session_key.into(), event),
        )
    }

    fn elicitation_answer<'a>(
        &'a self,
        chat_id: &'a str,
        text: &'a str,
    ) -> InteractionFuture<'a, Result<Option<String>, ChannelError>> {
        Box::pin(async move {
            self.0
                .elicitation_answer(&chat_id.into(), &text.into())
                .await
                .map(|value| value.map(scalar))
        })
    }

    fn elicitation_clear(&self, chat_id: &str) {
        self.0.elicitation_clear(&chat_id.into());
    }

    fn permission_pending(&self, chat_id: &str) -> bool {
        self.0.permission_pending(&chat_id.into())
    }

    fn permission_capture(
        &self,
        chat_id: &str,
        session_key: &str,
        event: &JsValue,
    ) -> Option<String> {
        self.0
            .permission_capture(&chat_id.into(), &session_key.into(), event)
            .map(scalar)
    }

    fn permission_confirm_initial(&self, chat_id: &str, delivered: bool, request_id: Option<&str>) {
        self.0.permission_confirm_initial(
            &chat_id.into(),
            delivered,
            request_id.map(JsString::from).as_ref(),
        );
    }

    fn permission_answer<'a>(
        &'a self,
        chat_id: &'a str,
        text: &'a str,
    ) -> InteractionFuture<'a, Result<Option<PermissionAnswer>, ChannelError>> {
        Box::pin(async move {
            self.0
                .permission_answer(&chat_id.into(), &text.into())
                .await
                .map(|value| value.map(scalar_answer))
        })
    }

    fn permission_release_answer(&self, chat_id: &str, answer_token: Option<u64>) {
        self.0
            .permission_release_answer(&chat_id.into(), answer_token);
    }

    fn permission_take_next_prompt(
        &self,
        chat_id: &str,
        answer_token: Option<u64>,
    ) -> Option<String> {
        self.0
            .permission_take_next_prompt(&chat_id.into(), answer_token)
            .map(scalar)
    }

    fn permission_prompt_request_id(
        &self,
        chat_id: &str,
        answer_token: Option<u64>,
    ) -> Option<String> {
        self.0
            .permission_prompt_request_id(&chat_id.into(), answer_token)
            .map(scalar)
    }

    fn permission_confirm_next(
        &self,
        chat_id: &str,
        delivered: bool,
        request_id: Option<&str>,
        answer_token: Option<u64>,
    ) {
        self.0.permission_confirm_next(
            &chat_id.into(),
            delivered,
            request_id.map(JsString::from).as_ref(),
            answer_token,
        );
    }

    fn permission_clear_after_turn(&self, chat_id: &str) {
        self.0.permission_clear_after_turn(&chat_id.into());
    }
}
