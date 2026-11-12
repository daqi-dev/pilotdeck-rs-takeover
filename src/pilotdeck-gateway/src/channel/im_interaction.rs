//! The slice of `ImElicitationHelper` / `ImPermissionHelper` the IM channels
//! drive (`WeComCallbackChannel.ts:216-251`, `FeishuChannel.ts:337-429`).
//!
//! Those helpers live outside the individual adapter directories; this unit
//! ports the channel-side orchestration against one narrow port shared by the
//! wecom-callback and feishu lanes. Assembly supplies a shared implementation,
//! and the TS oracles/Rust tests supply the same scripted fake so the
//! comparison covers each channel, not helper internals.

mod helper;
mod native;

pub use super::bluebubbles::HelperInteraction;
pub use helper::{InteractionResponder, JsHelperInteraction, ResponderRef};
pub use native::NativeInteractionResponder;

use std::{future::Future, pin::Pin, sync::Arc};

use pilotdeck_protocol::{JsString, JsValue};

use super::protocol::ChannelError;

pub(crate) type InteractionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A new helper belongs to one adapter instance. Only its responder is shared
/// with other adapters; chat IDs and delivery tokens are adapter-local.
pub fn js_helper_interaction(responder: ResponderRef) -> Arc<dyn JsInteractionPort> {
    Arc::new(JsHelperInteraction::new(responder))
}

/// Compatibility constructor for adapters that already use scalar strings.
pub fn helper_interaction(responder: ResponderRef) -> Arc<dyn InteractionPort> {
    Arc::new(HelperInteraction::new(responder))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JsPermissionAnswer {
    pub text: Option<JsString>,
    pub can_advance: bool,
    pub retry_prompt: bool,
    pub answer_token: Option<u64>,
}

/// The JS-string preserving helper boundary for newly migrated channels.
/// Request IDs, option labels, and prompts may contain lone UTF-16 units.
pub trait JsInteractionPort: Send + Sync {
    /// Release this adapter's local helper state after every owned turn and
    /// reply task has successfully drained. This never cancels a gateway run.
    fn clear_chat_for_shutdown(&self, chat_id: &JsString);
    fn elicitation_pending(&self, chat_id: &JsString) -> bool;
    fn elicitation_capture(
        &self,
        chat_id: &JsString,
        session_key: &JsString,
        event: &JsValue,
    ) -> JsString;
    fn elicitation_answer<'a>(
        &'a self,
        chat_id: &'a JsString,
        text: &'a JsString,
    ) -> InteractionFuture<'a, Result<Option<JsString>, ChannelError>>;
    fn elicitation_clear(&self, chat_id: &JsString);
    fn permission_pending(&self, chat_id: &JsString) -> bool;
    fn permission_capture(
        &self,
        chat_id: &JsString,
        session_key: &JsString,
        event: &JsValue,
    ) -> Option<JsString>;
    fn permission_confirm_initial(
        &self,
        chat_id: &JsString,
        delivered: bool,
        request_id: Option<&JsString>,
    );
    fn permission_answer<'a>(
        &'a self,
        chat_id: &'a JsString,
        text: &'a JsString,
    ) -> InteractionFuture<'a, Result<Option<JsPermissionAnswer>, ChannelError>>;
    fn permission_release_answer(&self, chat_id: &JsString, answer_token: Option<u64>);
    fn permission_take_next_prompt(
        &self,
        chat_id: &JsString,
        answer_token: Option<u64>,
    ) -> Option<JsString>;
    fn permission_prompt_request_id(
        &self,
        chat_id: &JsString,
        answer_token: Option<u64>,
    ) -> Option<JsString>;
    fn permission_confirm_next(
        &self,
        chat_id: &JsString,
        delivered: bool,
        request_id: Option<&JsString>,
        answer_token: Option<u64>,
    );
    fn permission_clear_after_turn(&self, chat_id: &JsString);
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionAnswer {
    pub text: Option<String>,
    pub can_advance: bool,
    pub retry_prompt: bool,
    pub answer_token: Option<u64>,
}

pub trait InteractionPort: Send + Sync {
    fn elicitation_pending(&self, chat_id: &str) -> bool;
    fn elicitation_capture(&self, chat_id: &str, session_key: &str, event: &JsValue) -> String;
    fn elicitation_answer<'a>(
        &'a self,
        chat_id: &'a str,
        text: &'a str,
    ) -> InteractionFuture<'a, Result<Option<String>, ChannelError>>;
    fn elicitation_clear(&self, chat_id: &str);

    fn permission_pending(&self, chat_id: &str) -> bool;
    fn permission_capture(
        &self,
        chat_id: &str,
        session_key: &str,
        event: &JsValue,
    ) -> Option<String>;
    fn permission_confirm_initial(&self, chat_id: &str, delivered: bool, request_id: Option<&str>);
    fn permission_answer<'a>(
        &'a self,
        chat_id: &'a str,
        text: &'a str,
    ) -> InteractionFuture<'a, Result<Option<PermissionAnswer>, ChannelError>>;
    fn permission_release_answer(&self, chat_id: &str, answer_token: Option<u64>);
    fn permission_take_next_prompt(
        &self,
        chat_id: &str,
        answer_token: Option<u64>,
    ) -> Option<String>;
    fn permission_prompt_request_id(
        &self,
        chat_id: &str,
        answer_token: Option<u64>,
    ) -> Option<String>;
    fn permission_confirm_next(
        &self,
        chat_id: &str,
        delivered: bool,
        request_id: Option<&str>,
        answer_token: Option<u64>,
    );
    fn permission_clear_after_turn(&self, chat_id: &str);
}

/// The `hasPending`-only default: no interaction state, so inbound text flows
/// to the gateway. An assembly that has not installed the shared helpers yet
/// still gets the TS text path.
#[derive(Default)]
pub(crate) struct NoInteraction;

impl InteractionPort for NoInteraction {
    fn elicitation_pending(&self, _chat_id: &str) -> bool {
        false
    }
    fn elicitation_capture(&self, _: &str, _: &str, _: &JsValue) -> String {
        String::new()
    }
    fn elicitation_answer<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> InteractionFuture<'a, Result<Option<String>, ChannelError>> {
        Box::pin(async { Ok(None) })
    }
    fn elicitation_clear(&self, _: &str) {}

    fn permission_pending(&self, _: &str) -> bool {
        false
    }
    fn permission_capture(&self, _: &str, _: &str, _: &JsValue) -> Option<String> {
        None
    }
    fn permission_confirm_initial(&self, _: &str, _: bool, _: Option<&str>) {}
    fn permission_answer<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> InteractionFuture<'a, Result<Option<PermissionAnswer>, ChannelError>> {
        Box::pin(async { Ok(None) })
    }
    fn permission_release_answer(&self, _: &str, _: Option<u64>) {}
    fn permission_take_next_prompt(&self, _: &str, _: Option<u64>) -> Option<String> {
        None
    }
    fn permission_prompt_request_id(&self, _: &str, _: Option<u64>) -> Option<String> {
        None
    }
    fn permission_confirm_next(&self, _: &str, _: bool, _: Option<&str>, _: Option<u64>) {}
    fn permission_clear_after_turn(&self, _: &str) {}
}

#[cfg(test)]
mod tests;
