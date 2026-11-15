//! Original compaction input and result wire projection.
use crate::host::Cancellation;
use pilotdeck_context_core::wire::object;
use pilotdeck_protocol::{JsString, JsValue};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub enum ProtectedTools {
    #[default]
    Default,
    Names(Vec<String>),
    Disabled,
}

#[derive(Clone)]
pub struct CompactionInput {
    pub trigger: JsString,
    pub messages: Vec<JsValue>,
    pub keep_tail_ratio: Option<f64>,
    pub effective_context_tokens: Option<f64>,
    pub target_post_tokens: Option<f64>,
    pub protected_tool_names: ProtectedTools,
    pub user_instruction: Option<JsString>,
    pub max_output_tokens: Option<f64>,
    pub cache_reset: Option<bool>,
    pub attachments: Vec<JsValue>,
    pub hook_results: Vec<JsValue>,
    pub signal: Option<Arc<dyn Cancellation>>,
    pub session_id: Option<JsString>,
    pub turn_id: Option<JsString>,
}
impl CompactionInput {
    pub fn new(trigger: impl Into<JsString>, messages: Vec<JsValue>) -> Self {
        Self {
            trigger: trigger.into(),
            messages,
            keep_tail_ratio: None,
            effective_context_tokens: None,
            target_post_tokens: None,
            protected_tool_names: ProtectedTools::Default,
            user_instruction: None,
            max_output_tokens: None,
            cache_reset: None,
            attachments: Vec::new(),
            hook_results: Vec::new(),
            signal: None,
            session_id: None,
            turn_id: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompactionResult {
    pub compaction_id: JsString,
    pub trigger: JsString,
    pub pre_tokens: f64,
    pub post_tokens: Option<f64>,
    pub messages_summarized: usize,
    pub summary_message: Option<JsValue>,
    pub boundary_marker: JsValue,
    pub messages_to_keep: Vec<JsValue>,
    pub stable_prefix: Vec<JsValue>,
    pub cache_reset: bool,
    pub target_post_tokens: Option<f64>,
    pub summary_generated: bool,
    pub checkpoint_merged: bool,
    pub summary_usage: Option<JsValue>,
    pub attachments: Vec<JsValue>,
    pub hook_results: Vec<JsValue>,
    pub diagnostics: Vec<JsValue>,
    pub error: Option<JsString>,
}
impl CompactionResult {
    /// Insertion order is the observable TS result-object order.
    pub fn to_value(&self) -> JsValue {
        let mut value = object([
            ("compactionId", JsValue::String(self.compaction_id.clone())),
            ("trigger", JsValue::String(self.trigger.clone())),
            ("preTokens", JsValue::Number(self.pre_tokens)),
            ("messagesSummarized", num(self.messages_summarized)),
            (
                "summaryMessage",
                self.summary_message.clone().unwrap_or(JsValue::Undefined),
            ),
            ("boundaryMarker", self.boundary_marker.clone()),
            (
                "messagesToKeep",
                JsValue::Array(self.messages_to_keep.clone()),
            ),
            ("stablePrefix", JsValue::Array(self.stable_prefix.clone())),
            ("cacheReset", JsValue::Bool(self.cache_reset)),
            (
                "targetPostTokens",
                self.target_post_tokens
                    .map_or(JsValue::Undefined, JsValue::Number),
            ),
            ("summaryGenerated", JsValue::Bool(self.summary_generated)),
            ("checkpointMerged", JsValue::Bool(self.checkpoint_merged)),
            (
                "summaryUsage",
                self.summary_usage.clone().unwrap_or(JsValue::Undefined),
            ),
            ("attachments", JsValue::Array(self.attachments.clone())),
            ("hookResults", JsValue::Array(self.hook_results.clone())),
            ("diagnostics", JsValue::Array(self.diagnostics.clone())),
            (
                "error",
                self.error
                    .clone()
                    .map_or(JsValue::Undefined, JsValue::String),
            ),
        ]);
        if let Some(tokens) = self.post_tokens {
            set(&mut value, "postTokens", JsValue::Number(tokens));
        }
        value
    }
}

fn num(value: usize) -> JsValue {
    JsValue::Number(value as f64)
}
fn set(value: &mut JsValue, key: &str, child: JsValue) {
    if let JsValue::Object(value) = value {
        value.insert(key.into(), child);
    }
}
