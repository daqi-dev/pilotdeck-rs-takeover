use pilotdeck_protocol::JsString;
pub(super) struct Envelope {
    pub text: JsString,
}
/// `chunkText`: JS length, lastIndexOf and slice all count UTF-16 code units.
pub(super) fn chunk_text(text: &JsString) -> Vec<JsString> {
    const MAX: usize = 4096;
    let mut rest = text.units();
    let mut chunks = Vec::new();
    if rest.len() <= MAX {
        return vec![text.clone()];
    }
    while rest.len() > MAX {
        let mut split = rest[..=MAX].iter().rposition(|v| *v == 10);
        if split.is_none_or(|n| n < MAX / 2) {
            split = rest[..=MAX].iter().rposition(|v| *v == 32);
        }
        let split = split.filter(|n| *n >= MAX / 2).unwrap_or(MAX);
        chunks.push(JsString::from_utf16(rest[..split].to_vec()));
        rest = &rest[split..];
        while rest.first() == Some(&10) {
            rest = &rest[1..];
        }
    }
    if !rest.is_empty() {
        chunks.push(JsString::from_utf16(rest.to_vec()));
    }
    chunks
}
