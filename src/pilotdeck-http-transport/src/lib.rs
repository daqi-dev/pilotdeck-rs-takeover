//! Shared product HTTP socket/proxy/TLS/decoding implementation.
//! Dial authority is injected; this package depends on no Kernel service.
#![forbid(unsafe_code)]
pub mod executor;
mod proxy_socks;
#[cfg(test)]
mod test_negative_receipt;
#[cfg(test)]
mod test_scratch;
pub mod transport;
fn js_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
