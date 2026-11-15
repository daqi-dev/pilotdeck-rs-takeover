//! Model-free context compaction, ported from the fixed TS baseline
//! `context/compaction/SnipEngine.ts` and `MicroCompactionEngine.ts`.
//!
//! Both engines work on the canonical wire `JsValue` messages that the context
//! runtime hands to the model. Snip prunes middle turns at user/assistant
//! boundaries and injects `<snip-boundary .../>`; micro rewrites old
//! compactable tool results in place. Neither calls a model, so both are pure
//! functions of the message list plus their options.
//!
//! Token estimates go through the already-ported `model_cost` block counter,
//! which uses the shared o200k tokenizer; no second estimator lives here.
//! `String(value)`, JSON serialization and trimming keep the JS semantics from
//! `crate::js_value` / `crate::metadata`.

pub mod engine;
mod micro;
pub mod orchestrator;
pub mod policy;
pub mod recovery;
mod snip;
pub mod tool_pair;

pub use micro::{
    COMPACTABLE_TOOL_NAMES, MICROCOMPACT_CLEARED, MICROCOMPACT_FAILURES_FOLDED,
    MICROCOMPACT_RECOVERED_FAILURE_PREFIX, MicroCompactionEngine, MicroCompactionEngineOptions,
    MicroCompactionInput, MicroCompactionResult,
};
pub use snip::{
    SnipCallOptions, SnipEngine, SnipEngineOptions, SnipResult, create_snip_boundary,
    is_snip_boundary_message, project_snipped_view,
};

use crate::model_cost;
use pilotdeck_protocol::{
    JsObject, JsString, JsValue,
    model::{
        ContentBlock, ContentBody, ImageBlock, MediaSource, PdfBlock, TextBlock, ThinkingBlock,
        ToolCall, ToolResult, ToolResultContent, ToolResultItem,
    },
};

pub(crate) use tool_pair::{
    collect_tool_call_ids, collect_tool_result_ids, ensure_trailing_user_message,
    is_direct_tool_result, is_media_reference_with_id, is_real_user_request_message,
    strip_unpaired_tool_calls, strip_unpaired_tool_results,
};

pub(crate) use pilotdeck_context_core::wire::*;

/// TS `flattenToolResultBlockText` (`model/protocol/toolResultContent.ts`).
pub(crate) fn flatten_tool_result_block_text(block: &JsValue) -> JsString {
    let items = match field(block, "content") {
        JsValue::Array(items) => items.as_slice(),
        _ => return JsString::default(),
    };
    let mut units: Vec<u16> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            units.push(u16::from(b'\n'));
        }
        match block_type(item)
            .and_then(|kind| kind.to_utf8().ok())
            .as_deref()
        {
            Some("text") => {
                if let Some(text) = string_field(item, "text") {
                    units.extend_from_slice(text.units());
                }
            }
            Some("image") => {
                let mime = string_field(item, "mimeType").cloned().unwrap_or_default();
                let data = string_field(item, "data").cloned().unwrap_or_default();
                units.extend("[Image: ".encode_utf16());
                units.extend_from_slice(mime.units());
                units.extend(", ".encode_utf16());
                if string_field(item, "source").is_some_and(|source| source.equals_str("url")) {
                    units.extend_from_slice(data.units());
                } else {
                    units.extend(data.len_utf16().to_string().encode_utf16());
                    units.extend(" base64 characters".encode_utf16());
                }
                units.push(u16::from(b']'));
            }
            Some("pdf") => {
                let mime = string_field(item, "mimeType").cloned().unwrap_or_default();
                let data = string_field(item, "data").cloned().unwrap_or_default();
                units.extend("[PDF: ".encode_utf16());
                units.extend_from_slice(mime.units());
                units.extend(", ".encode_utf16());
                units.extend(data.len_utf16().to_string().encode_utf16());
                units.extend(" base64 characters".encode_utf16());
                let pages = field(item, "pages");
                if js_truthy(pages) {
                    units.extend(", ".encode_utf16());
                    units.extend_from_slice(
                        &crate::js_value::to_string_units(pages).unwrap_or_default(),
                    );
                    units.extend(" pages".encode_utf16());
                }
                units.push(u16::from(b']'));
            }
            _ => {}
        }
    }
    JsString::from_utf16(units)
}

pub(crate) fn js_truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
        JsValue::String(value) => !value.is_empty(),
        JsValue::Array(_) | JsValue::Object(_) => true,
    }
}

/// TS `TokenBudgetManager.estimateMessagesTokens` over wire messages. Each
/// block is projected onto the typed canonical model and counted by the
/// shared `model_cost` implementation (same o200k tokenizer, 4-token message
/// overhead, fixed multimedia placeholder).
pub(crate) fn estimate_messages_tokens(
    messages: &[JsValue],
) -> Result<usize, pilotdeck_tokenizer::TokenizeError> {
    let mut total = 0usize;
    for message in messages {
        total += model_cost::PER_MESSAGE_OVERHEAD;
        for block in content(message) {
            if let Some(block) = estimate_block(block) {
                total += model_cost::estimate_block_tokens(&block)?;
            }
        }
    }
    Ok(total)
}

fn estimate_block(block: &JsValue) -> Option<ContentBlock> {
    let body = match block_type(block)
        .and_then(|kind| kind.to_utf8().ok())
        .as_deref()
    {
        Some("text") => ContentBody::Text(TextBlock {
            text: string_field(block, "text").cloned().unwrap_or_default(),
            block_id: None,
        }),
        Some("thinking") => ContentBody::Thinking(ThinkingBlock {
            text: string_field(block, "text").cloned().unwrap_or_default(),
            block_id: None,
            signature: None,
            reasoning_content: string_field(block, "reasoningContent").cloned(),
        }),
        Some("image" | "pdf" | "audio" | "media_reference") => ContentBody::Image(ImageBlock {
            source: MediaSource::Base64,
            data: JsString::default(),
            mime_type: JsString::default(),
            bytes: None,
            detail: None,
        }),
        Some("tool_call") => ContentBody::ToolCall(ToolCall {
            timeline: None,
            id: JsString::default(),
            name: string_field(block, "name").cloned().unwrap_or_default(),
            input: field(block, "input").clone(),
            raw: None,
        }),
        Some("tool_result") => ContentBody::ToolResult(ToolResult {
            tool_call_id: string_field(block, "toolCallId")
                .cloned()
                .unwrap_or_default(),
            content: tool_result_items(block),
            is_error: None,
            raw: None,
        }),
        Some("tool_result_reference") => ContentBody::Text(TextBlock {
            text: string_field(block, "preview").cloned().unwrap_or_default(),
            block_id: None,
        }),
        _ => return None,
    };
    Some(ContentBlock {
        timeline: None,
        body,
    })
}

fn tool_result_items(block: &JsValue) -> Vec<ToolResultItem> {
    let items = match field(block, "content") {
        JsValue::Array(items) => items.as_slice(),
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|item| {
            let body = match block_type(item)
                .and_then(|kind| kind.to_utf8().ok())
                .as_deref()
            {
                Some("text") => ToolResultContent::Text(TextBlock {
                    text: string_field(item, "text").cloned().unwrap_or_default(),
                    block_id: None,
                }),
                Some("image") => ToolResultContent::Image(ImageBlock {
                    source: if string_field(item, "source")
                        .is_some_and(|source| source.equals_str("url"))
                    {
                        MediaSource::Url
                    } else {
                        MediaSource::Base64
                    },
                    data: string_field(item, "data").cloned().unwrap_or_default(),
                    mime_type: string_field(item, "mimeType").cloned().unwrap_or_default(),
                    bytes: None,
                    detail: None,
                }),
                Some("pdf") => ToolResultContent::Pdf(PdfBlock {
                    data: string_field(item, "data").cloned().unwrap_or_default(),
                    bytes: 0.0,
                    pages: field(item, "pages").as_number(),
                }),
                _ => return None,
            };
            Some(ToolResultItem {
                timeline: None,
                body,
            })
        })
        .collect()
}

pub(crate) fn count_tokens(value: &JsString) -> Result<usize, pilotdeck_tokenizer::TokenizeError> {
    pilotdeck_tokenizer::count_tokens(value)
}

#[cfg(test)]
mod tests {
    use super::{js_max, js_min};

    const PLUS_ZERO: u64 = 0.0f64.to_bits();
    const MINUS_ZERO: u64 = (-0.0f64).to_bits();

    /// Node v22.22.0: `Math.max(+0,-0)` / `Math.max(-0,+0)` are `+0`;
    /// `Math.min(+0,-0)` / `Math.min(-0,+0)` are `-0`.
    #[test]
    fn js_math_signed_zero_ties_match_ts() {
        for (index, (left, right)) in [(0.0, -0.0), (-0.0, 0.0)].into_iter().enumerate() {
            assert_eq!(
                js_max(left, right).to_bits(),
                PLUS_ZERO,
                "#947 compaction js_max tie {index} ({left}, {right}) must be +0"
            );
            assert_eq!(
                js_min(left, right).to_bits(),
                MINUS_ZERO,
                "#947 compaction js_min tie {index} ({left}, {right}) must be -0"
            );
        }
        assert_eq!(
            js_max(-0.0, -0.0).to_bits(),
            MINUS_ZERO,
            "#947 compaction js_max(-0,-0) must stay -0"
        );
        assert_eq!(
            js_min(0.0, 0.0).to_bits(),
            PLUS_ZERO,
            "#947 compaction js_min(+0,+0) must stay +0"
        );
    }
}
