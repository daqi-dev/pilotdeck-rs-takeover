//! The gateway permission prompt of a native session: what the baseline
//! composes from `SessionInteractionBundle` (the `PermissionRequest` callback
//! hook `pilotdeck.gateway.permission`) and `createGatewayPermissionHook`.
//!
//! The tool runtime asks here when the permission decision is `ask`. The
//! prompt is announced on the session's active gateway stream, parked on the
//! permission bus, and settled by `permission_decide`, by the deadline, by the
//! turn's abort signal, or by the bus being torn down.
use crate::{
    config::permission::{PermissionRule, RuleSetHandle},
    runtime::{
        interaction::{GatewayInteractionCoordinator, PendingEntry},
        tools::ToolRunContext,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::oneshot;

/// `GatewaySessionInteractionFacade.emit`: push a gateway event into the
/// session's active `submit_turn` stream; `false` when there is none.
pub(super) type GatewayEmit = Arc<dyn Fn(&JsString, JsValue) -> bool + Send + Sync>;
/// The session factory is assembled before the gateway host exists, so the
/// emitter is published into this slot once the host's coordinator is built.
pub(super) type GatewayEmitSlot = Arc<OnceLock<GatewayEmit>>;

/// `DEFAULT_PERMISSION_TIMEOUT_MS` / `DEFAULT_ELICITATION_TIMEOUT_MS`
/// (`cli/LocalGatewayBootConfig.ts:7-8`).
const DEFAULT_INTERACTION_TIMEOUT_MS: u64 = 120_000;

/// `readPositiveIntegerEnv` (`cli/LocalGatewayBootConfig.ts:63`):
/// `Number.parseInt(value.trim(), 10)`, kept only when positive.
pub(super) fn interaction_timeout(value: Option<&str>) -> Duration {
    let parsed = value.and_then(|value| {
        let value = value.trim();
        let (negative, digits) = match value.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, value.strip_prefix('+').unwrap_or(value)),
        };
        let end = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        let number = digits[..end].parse::<u64>().ok()?;
        (!negative && number > 0).then_some(number)
    });
    Duration::from_millis(parsed.unwrap_or(DEFAULT_INTERACTION_TIMEOUT_MS))
}

fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsValue {
    let mut object = JsObject::new();
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    JsValue::Object(object)
}

/// `toGatewayHookExecutionStatus` for the callback hook: the hook runtime
/// reports a callback under the name `callback`.
fn hook_status(event: &str, hook_event: &str, completed: bool) -> JsValue {
    let mut detail = JsObject::new();
    detail.insert("hookName".into(), "callback".into());
    detail.insert("hookEvent".into(), hook_event.into());
    if completed {
        detail.insert("outcome".into(), "success".into());
        detail.insert("exitCode".into(), JsValue::Number(0.0));
    }
    object([
        ("type", "agent_status".into()),
        ("event", event.into()),
        ("detail", JsValue::Object(detail)),
    ])
}

/// What the prompt settled to: the hook's `specific.decision`.
pub(super) enum PromptOutcome {
    Allow,
    Deny(JsString),
}

pub(super) struct PermissionPrompt {
    pub session: JsString,
    pub coordinator: Arc<GatewayInteractionCoordinator>,
    pub emit: GatewayEmitSlot,
    /// The session rule set whose `allow` array the permission context reads:
    /// a remembered approval is pushed here.
    pub rules: RuleSetHandle,
    pub timeout: Duration,
    /// The project generation's interaction profile and the session's
    /// prompt admission (`override?.canPrompt ?? profile.canPrompt`,
    /// ProjectSessionFactory.ts:268-271).
    pub interaction: SessionInteraction,
}

/// `SessionInteractionBundleOptions.profile` / `.canPrompt`.
#[derive(Clone, Copy, Debug)]
pub(super) struct SessionInteraction {
    pub profile: crate::runtime::project::InteractionProfile,
    pub can_prompt: bool,
}

impl SessionInteraction {
    /// `hasGatewayPermissionAnswerer` (SessionInteractionBundle.ts:117-121):
    /// the gateway is always present in this host.
    pub(super) fn has_permission_answerer(&self) -> bool {
        self.profile.permission_provider == "gateway" && self.can_prompt
    }

    /// `createDefaultInteractionPolicy().decide(...)` (InteractionPolicy.ts:
    /// 33-49): `None` admits the request, `Some` is the refusal reason.
    pub(super) fn refusal(&self, has_answerer: bool) -> Option<&'static str> {
        if self.profile.policy_mode == "disabled" {
            Some("Interaction is disabled for this profile.")
        } else if !self.can_prompt {
            Some("Prompts are disabled for this session.")
        } else if !has_answerer {
            Some("No interaction answerer is available.")
        } else {
            None
        }
    }

    /// `createProfiledPermissionDecisionPort` (ProfiledPermissionDecisionPort
    /// .ts:23-49): an `ask` the profile cannot answer becomes a runtime deny
    /// before any `PermissionRequest` hook or prompt runs.
    pub(super) fn admit_permission_ask(&self) -> Option<JsValue> {
        let reason = self.refusal(self.has_permission_answerer())?;
        Some(object([
            ("type", "deny".into()),
            (
                "reason",
                object([("type", "runtime".into()), ("message", reason.into())]),
            ),
            ("message", reason.into()),
        ]))
    }
}

impl Default for SessionInteraction {
    fn default() -> Self {
        let profile = crate::runtime::project::RuntimeProfile::resolve(
            &JsValue::Undefined,
            &Default::default(),
        )
        .interaction;
        Self {
            profile,
            can_prompt: profile.can_prompt,
        }
    }
}

impl PermissionPrompt {
    fn deliver(&self, event: JsValue) -> bool {
        self.emit
            .get()
            .is_some_and(|emit| emit(&self.session, event))
    }

    /// One `PermissionRequest` dispatch through the gateway callback hook.
    pub(super) async fn request(
        &self,
        tool_name: &JsString,
        tool_call_id: &JsString,
        input: &JsValue,
        suggestions: Option<&JsValue>,
        context: &ToolRunContext,
    ) -> PromptOutcome {
        // The callback hook's execution events carry the dispatching run's
        // session id; the session's projection drops any other session's
        // (GatewayHookExecutionProjection.ts:22). A subagent's tools share
        // this prompt (SubagentRuntimeComposition.ts:57-81) but dispatch
        // under the child's session id, so their hook status stays off the
        // stream while the ask itself is still delivered.
        let own_session = context
            .fields
            .get("sessionId")
            .and_then(JsValue::as_string)
            .is_none_or(|session| session == &self.session);
        if own_session {
            self.deliver(hook_status(
                "hook_execution_started",
                "PermissionRequest",
                false,
            ));
        }
        let outcome = self
            .ask(tool_name, tool_call_id, input, suggestions, context)
            .await;
        if own_session {
            self.deliver(hook_status(
                "hook_execution_completed",
                "PermissionRequest",
                true,
            ));
        }
        outcome
    }

    async fn ask(
        &self,
        tool_name: &JsString,
        tool_call_id: &JsString,
        input: &JsValue,
        suggestions: Option<&JsValue>,
        context: &ToolRunContext,
    ) -> PromptOutcome {
        let request_id: JsString = uuid::Uuid::new_v4().to_string().into();
        let (sender, receiver) = oneshot::channel::<JsValue>();
        let sender = Arc::new(Mutex::new(Some(sender)));
        let (resolve, reject) = (sender.clone(), sender);
        let bus = self.coordinator.permission_bus();
        let registration = bus.register(
            &self.session,
            PendingEntry {
                request_id: request_id.clone(),
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                resolve: Arc::new(move |value| {
                    if let Some(sender) = resolve.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        let _ = sender.send(value);
                    }
                }),
                // The permission bus never rejects (teardown denies); dropping
                // the sender reads as a plain denial below.
                reject: Arc::new(move |_| {
                    reject.lock().unwrap_or_else(|e| e.into_inner()).take();
                }),
            },
            Some(object([
                ("toolName", JsValue::String(tool_name.clone())),
                ("toolCallId", JsValue::String(tool_call_id.clone())),
                ("payload", input.clone()),
            ])),
        );
        let Ok(registration) = registration else {
            return PromptOutcome::Deny("Permission prompt denied.".into());
        };
        // `settle`: only the party that takes the entry off the bus decides.
        let settle = |reason: &str| -> Option<PromptOutcome> {
            bus.consume(&self.session, &request_id, None)
                .map(|_| PromptOutcome::Deny(reason.into()))
        };
        let mut receiver = receiver;
        let answer = if context.is_aborted() {
            settle("Permission prompt cancelled.")
        } else if !self.deliver(object([
            ("type", "permission_request".into()),
            ("requestId", JsValue::String(request_id.clone())),
            ("toolName", JsValue::String(tool_name.clone())),
            ("payload", input.clone()),
        ])) {
            settle("Permission prompt could not be delivered to the Web UI.")
        } else {
            tokio::select! {
                biased;
                answer = &mut receiver => Some(self.settled(answer.ok(), tool_name, suggestions)),
                _ = context.cancelled() => settle("Permission prompt cancelled."),
                _ = tokio::time::sleep(self.timeout) => settle("Permission prompt timed out."),
            }
        };
        registration.dispose();
        match answer {
            Some(outcome) => outcome,
            // Someone else consumed the entry between the trigger and our
            // settle: their answer is the decision.
            None => self.settled(receiver.await.ok(), tool_name, suggestions),
        }
    }

    fn settled(
        &self,
        answer: Option<JsValue>,
        tool_name: &JsString,
        suggestions: Option<&JsValue>,
    ) -> PromptOutcome {
        let Some(answer) = answer else {
            return PromptOutcome::Deny("Permission prompt denied.".into());
        };
        let allowed = answer
            .get("decision")
            .and_then(JsValue::as_string)
            .is_some_and(|decision| decision.equals_str("allow"));
        if !allowed {
            return PromptOutcome::Deny(
                answer
                    .get("reason")
                    .and_then(JsValue::as_string)
                    .cloned()
                    .unwrap_or_else(|| "Permission prompt denied.".into()),
            );
        }
        if answer.get("remember").is_some_and(truthy) {
            let suggested = session_allow_rules(suggestions, tool_name);
            let remembered = if suggested.is_empty() {
                vec![PermissionRule::new("session", "allow", tool_name.clone())]
            } else {
                suggested
            };
            self.rules.update_member("allow", |existing| {
                for rule in remembered {
                    let known = existing
                        .iter()
                        .filter_map(|value| PermissionRule::from_value(&value.to_value()))
                        .any(|value| {
                            value.behavior.equals_str("allow")
                                && value.tool_name == rule.tool_name
                                && value.pattern == rule.pattern
                        });
                    if !known {
                        existing.push(rule.into());
                    }
                }
            });
        }
        PromptOutcome::Allow
    }
}

fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
        JsValue::String(value) => !value.is_empty(),
        JsValue::Array(_) | JsValue::Object(_) => true,
    }
}

/// `extractSessionAllowRules`: the rules of the request's `allow_session`
/// option, as session allow rules.
fn session_allow_rules(options: Option<&JsValue>, fallback: &JsString) -> Vec<PermissionRule> {
    let Some(JsValue::Array(options)) = options else {
        return vec![];
    };
    let Some(JsValue::Array(rules)) = options
        .iter()
        .filter_map(JsValue::as_object)
        .find(|option| {
            option
                .get("id")
                .and_then(JsValue::as_string)
                .is_some_and(|id| id.equals_str("allow_session"))
                && matches!(option.get("rules"), Some(JsValue::Array(_)))
        })
        .and_then(|option| option.get("rules"))
    else {
        return vec![];
    };
    rules
        .iter()
        .filter_map(JsValue::as_object)
        .filter_map(|rule| {
            let tool = rule
                .get("toolName")
                .and_then(JsValue::as_string)
                .filter(|name| !name.is_empty())
                .unwrap_or(fallback);
            if tool.is_empty() {
                return None;
            }
            let mut result = PermissionRule::new("session", "allow", tool.clone());
            result.pattern = rule
                .get("pattern")
                .and_then(JsValue::as_string)
                .filter(|pattern| !pattern.is_empty())
                .cloned();
            Some(result)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::interaction_timeout;
    use std::time::Duration;

    #[test]
    fn timeout_env_follows_parse_int_and_falls_back_to_the_default() {
        let default = Duration::from_millis(120_000);
        assert_eq!(interaction_timeout(None), default);
        assert_eq!(
            interaction_timeout(Some(" 400 ")),
            Duration::from_millis(400)
        );
        assert_eq!(
            interaction_timeout(Some("250ms")),
            Duration::from_millis(250)
        );
        assert_eq!(interaction_timeout(Some("+7")), Duration::from_millis(7));
        for rejected in ["", "abc", "0", "-5", "x9"] {
            assert_eq!(interaction_timeout(Some(rejected)), default, "{rejected:?}");
        }
    }
}
