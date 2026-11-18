//! UTF-16 accumulator limit; a rejected append leaves the buffer unchanged.
use super::error::CodecError;
pub const MAX_STRING_LENGTH: usize = 0x1fff_ffe8;
thread_local! { static STRING_LIMIT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
pub fn string_limit() -> usize {
    STRING_LIMIT
        .with(std::cell::Cell::get)
        .unwrap_or(MAX_STRING_LENGTH)
}
/// Scoped override used by deterministic boundary tests. Restores the prior limit.
#[doc(hidden)]
pub struct LimitGuard(Option<usize>);
impl LimitGuard {
    pub fn set(limit: usize) -> Self {
        Self(STRING_LIMIT.with(|cell| cell.replace(Some(limit))))
    }
}
impl Drop for LimitGuard {
    fn drop(&mut self) {
        STRING_LIMIT.with(|cell| cell.set(self.0));
    }
}
pub fn append_js_string(buffer: &mut Vec<u16>, more: &[u16]) -> Result<(), CodecError> {
    if buffer.len().saturating_add(more.len()) > string_limit() {
        return Err(CodecError::Thrown {
            name: "RangeError".into(),
            message: "Invalid string length".into(),
        });
    }
    buffer.extend_from_slice(more);
    Ok(())
}
