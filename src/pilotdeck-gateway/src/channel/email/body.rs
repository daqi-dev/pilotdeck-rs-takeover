//! Fixed-baseline email body extraction deliberately retains its small MIME heuristic.
use crate::channel::sms;
use pilotdeck_protocol::JsString;

pub fn extract_plain_text(raw: &str) -> JsString {
    if !raw.contains("Content-Type:") {
        return trimmed(raw);
    }
    let plain =
        regex::Regex::new(r"(?i)Content-Type:\s*text/plain[^\r\n]*[\r\n]+").expect("static regex");
    if let Some(header) = plain.find(raw) {
        let rest = &raw[header.end()..];
        let end = regex::Regex::new(r"--[a-f0-9]{8,}|Content-Type:")
            .expect("static regex")
            .find(rest)
            .map_or(rest.len(), |m| m.start());
        let mut body = &rest[..end];
        if !body.is_empty() {
            body = body
                .strip_prefix("\r\n")
                .or_else(|| body.strip_prefix('\n'))
                .unwrap_or(body);
            let encoding =
                regex::Regex::new(r"(?i)^Content-Transfer-Encoding:\s*quoted-printable\r?\n")
                    .expect("static regex");
            if let Some(prefix) = encoding.find(body) {
                let body = body[prefix.end()..].replace("=\r\n", "").replace("=\n", "");
                let hex = regex::Regex::new(r"=([0-9A-F]{2})").expect("static regex");
                return trimmed(&hex.replace_all(&body, |c: &regex::Captures<'_>| {
                    char::from(u8::from_str_radix(&c[1], 16).expect("hex regex")).to_string()
                }));
            }
            return trimmed(body);
        }
    }
    // String.slice counts UTF-16 code units, including a possibly split surrogate.
    let units: Vec<_> = raw.encode_utf16().take(8000).collect();
    JsString::from_utf16(sms::trim_units(&units).to_vec())
}
fn trimmed(text: &str) -> JsString {
    let text = JsString::from(text);
    JsString::from_utf16(sms::trim_units(text.units()).to_vec())
}
