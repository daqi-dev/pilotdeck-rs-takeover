//! Exact `o200k_base` implementation of [`agentcpm_iface::context::TokenCounter`].
//!
//! Bridges `pilotdeck_tokenizer::count_tokens` for text, arbitrary bytes
//! (lossy UTF-8 decoding so invalid UTF-8 never fails), and `(role, content)`
//! message lists with PilotDeck's fixed per-message turn overhead (`4` tokens).

use agentcpm_iface::context::TokenCounter;
use pilotdeck_protocol::JsString;

/// Fixed per-message token overhead (`TokenBudgetManager.DEFAULT_PER_MESSAGE_OVERHEAD`).
pub const MESSAGES_PER_TURN_OVERHEAD: u64 = 4;

/// PilotDeck exact `o200k_base` token counter implementing `iface/context::TokenCounter`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PilotDeckTokenCounter;

impl PilotDeckTokenCounter {
    /// Construct a stateless `o200k_base` token counter.
    pub fn new() -> Self {
        Self
    }

    /// Count tokens in a UTF-16 [`JsString`] using `pilotdeck_tokenizer::count_tokens`.
    pub fn count_js_string(&self, text: &JsString) -> u64 {
        if text.is_empty() {
            return 0;
        }
        match pilotdeck_tokenizer::count_tokens(text) {
            Ok(tokens) => u64::try_from(tokens).unwrap_or(u64::MAX),
            Err(_) => {
                // Non-panicking defensive bound: non-empty input must never silently report 0.
                u64::try_from(text.len_utf16())
                    .unwrap_or(u64::MAX)
                    .div_ceil(4)
                    .max(1)
            }
        }
    }
}

impl TokenCounter for PilotDeckTokenCounter {
    fn count_text(&self, text: &str) -> u64 {
        if text.is_empty() {
            return 0;
        }
        self.count_js_string(&JsString::from(text))
    }

    fn count_bytes(&self, bytes: &[u8]) -> u64 {
        if bytes.is_empty() {
            return 0;
        }
        let decoded = String::from_utf8_lossy(bytes);
        self.count_text(decoded.as_ref())
    }

    fn count_messages(&self, messages: &[(String, String)]) -> u64 {
        let mut total = 0u64;
        for (role, content) in messages {
            total = total
                .saturating_add(MESSAGES_PER_TURN_OVERHEAD)
                .saturating_add(self.count_text(role))
                .saturating_add(self.count_text(content));
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn empty_text_and_bytes_return_zero() {
        let counter = PilotDeckTokenCounter::new();
        assert_eq!(counter.count_text(""), 0);
        assert_eq!(counter.count_bytes(&[]), 0);
        assert_eq!(counter.count_messages(&[]), 0);
    }

    #[test]
    fn count_text_uses_exact_o200k_base_and_is_deterministic() {
        let counter: Arc<dyn TokenCounter> = Arc::new(PilotDeckTokenCounter::new());
        assert_eq!(counter.count_text("hello"), 1);
        assert_eq!(counter.count_text("hello world"), 2);
        assert_eq!(
            counter.count_text("hello 世界"),
            counter.count_text("hello 世界")
        );
        assert!(counter.count_text("你好世界") > counter.count_text("abcd"));
        assert!(counter.count_text("hello world") >= counter.count_text("hello"));
    }

    #[test]
    fn count_bytes_handles_invalid_utf8_without_failing() {
        let counter = PilotDeckTokenCounter::new();
        let invalid = [0xff_u8, 0xfe, 0xfd, 0xfc, 0xfb];
        let count = counter.count_bytes(&invalid);
        assert!(count > 0);
        assert_eq!(counter.count_bytes(b"hello world"), 2);
    }

    #[test]
    fn count_messages_applies_fixed_per_turn_overhead() {
        let counter = PilotDeckTokenCounter::new();
        let messages = vec![("user".to_string(), "hi".to_string())];
        assert_eq!(
            counter.count_messages(&messages),
            MESSAGES_PER_TURN_OVERHEAD + counter.count_text("user") + counter.count_text("hi")
        );
        assert!(counter.count_messages(&messages) > counter.count_text("hi"));

        let two = vec![
            ("user".to_string(), "hello".to_string()),
            ("assistant".to_string(), "world".to_string()),
        ];
        assert_eq!(
            counter.count_messages(&two),
            MESSAGES_PER_TURN_OVERHEAD * 2
                + counter.count_text("user")
                + counter.count_text("hello")
                + counter.count_text("assistant")
                + counter.count_text("world")
        );
    }
}
