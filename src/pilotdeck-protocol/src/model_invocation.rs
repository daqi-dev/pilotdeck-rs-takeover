//! Lossless detached model invocation data; this is data, never authority.
use crate::{JsObject, JsValue, boundary_codec, parse_json, stringify_json};
pub const CONTEXT_PROFILE: &str = "pilotdeck.model-invocation-context/1";
pub const MAX_CONTEXT_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum Error {
    InvalidEnvelope,
    InvalidRequest,
    InvalidContext,
}

pub struct InvocationInput {
    pub request: JsValue,
    pub context: JsValue,
    pub context_bytes: Vec<u8>,
}
struct Reader<'a> {
    raw: &'a str,
    at: usize,
}
impl<'a> Reader<'a> {
    fn space(&mut self) {
        while self
            .raw
            .as_bytes()
            .get(self.at)
            .is_some_and(|c| matches!(c, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.at += 1;
        }
    }
    fn byte(&mut self, byte: u8) -> Result<(), Error> {
        self.space();
        if self.raw.as_bytes().get(self.at) != Some(&byte) {
            return Err(Error::InvalidEnvelope);
        }
        self.at += 1;
        Ok(())
    }
    fn string(&mut self) -> Result<crate::JsString, Error> {
        self.space();
        let start = self.at;
        self.byte(b'"')?;
        let mut escaped = false;
        while let Some(&byte) = self.raw.as_bytes().get(self.at) {
            self.at += 1;
            if escaped {
                escaped = false;
                continue;
            }
            if byte == b'\\' {
                escaped = true;
                continue;
            }
            if byte == b'"' {
                return match parse_json(&self.raw[start..self.at]) {
                    Ok(JsValue::String(value)) => Ok(value),
                    _ => Err(Error::InvalidEnvelope),
                };
            }
        }
        Err(Error::InvalidEnvelope)
    }
    fn pair(&mut self) -> Result<crate::JsString, Error> {
        let key = self.string()?;
        self.byte(b':')?;
        Ok(key)
    }
    fn finish(&mut self) -> Result<(), Error> {
        self.space();
        if self.at == self.raw.len() {
            Ok(())
        } else {
            Err(Error::InvalidEnvelope)
        }
    }
}
fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(H[(byte >> 4) as usize] as char);
        out.push(H[(byte & 15) as usize] as char);
    }
    out
}
fn unhex(value: &str) -> Result<Vec<u8>, Error> {
    if !value.len().is_multiple_of(2) || value.len() / 2 > MAX_CONTEXT_BYTES {
        return Err(Error::InvalidContext);
    }
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|p| {
            Ok(nibble(p[0]).ok_or(Error::InvalidContext)? * 16
                + nibble(p[1]).ok_or(Error::InvalidContext)?)
        })
        .collect()
}
pub fn encode(request: &JsValue, context: &JsValue) -> Result<Vec<u8>, Error> {
    if context.as_object().is_none() {
        return Err(Error::InvalidContext);
    }
    let request_json = stringify_json(request)
        .map_err(|_| Error::InvalidRequest)?
        .ok_or(Error::InvalidRequest)?;
    let context_bytes = boundary_codec::encode(context).map_err(|_| Error::InvalidContext)?;
    if context_bytes.len() > MAX_CONTEXT_BYTES {
        return Err(Error::InvalidContext);
    }
    let mut nested = JsObject::new();
    nested.insert("profile".into(), CONTEXT_PROFILE.into());
    nested.insert("content_hex".into(), hex(&context_bytes).into());
    let mut envelope = JsObject::new();
    envelope.insert("request_json".into(), request_json.into());
    envelope.insert("context".into(), JsValue::Object(nested));
    let result = stringify_json(&JsValue::Object(envelope))
        .map_err(|_| Error::InvalidEnvelope)?
        .ok_or(Error::InvalidEnvelope)?;
    Ok(result.into_bytes())
}
pub fn decode(bytes: &[u8]) -> Result<InvocationInput, Error> {
    let raw = std::str::from_utf8(bytes).map_err(|_| Error::InvalidEnvelope)?;
    let mut reader = Reader { raw, at: 0 };
    reader.byte(b'{')?;
    let mut request_json = None;
    let mut context_profile = None;
    let mut encoded = None;
    for index in 0..2 {
        if index != 0 {
            reader.byte(b',')?;
        }
        let key = reader.pair()?;
        if key.equals_str("request_json") && request_json.is_none() {
            request_json = Some(reader.string()?);
        } else if key.equals_str("context") && context_profile.is_none() {
            reader.byte(b'{')?;
            for nested_index in 0..2 {
                if nested_index != 0 {
                    reader.byte(b',')?;
                }
                let key = reader.pair()?;
                if key.equals_str("profile") && context_profile.is_none() {
                    context_profile = Some(reader.string()?);
                } else if key.equals_str("content_hex") && encoded.is_none() {
                    encoded = Some(reader.string()?);
                } else {
                    return Err(Error::InvalidEnvelope);
                }
            }
            reader.byte(b'}')?;
        } else {
            return Err(Error::InvalidEnvelope);
        }
    }
    reader.byte(b'}')?;
    reader.finish()?;
    if !context_profile
        .as_ref()
        .is_some_and(|profile| profile.equals_str(CONTEXT_PROFILE))
    {
        return Err(Error::InvalidContext);
    }
    let request_json = request_json
        .ok_or(Error::InvalidEnvelope)?
        .to_utf8()
        .map_err(|_| Error::InvalidRequest)?;
    let request = parse_json(&request_json).map_err(|_| Error::InvalidRequest)?;
    let encoded = encoded
        .ok_or(Error::InvalidEnvelope)?
        .to_utf8()
        .map_err(|_| Error::InvalidContext)?;
    let context_bytes = unhex(&encoded)?;
    if context_bytes.is_empty() {
        return Err(Error::InvalidContext);
    }
    let context = boundary_codec::decode(&context_bytes).map_err(|_| Error::InvalidContext)?;
    if context.as_object().is_none() {
        return Err(Error::InvalidContext);
    }
    Ok(InvocationInput {
        request,
        context,
        context_bytes,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_request_and_context_values() {
        let request = parse_json(r#"{"messages":[{"role":"user","content":"A\ud800"}]}"#).unwrap();
        let mut context = JsObject::new();
        context.insert("absent".into(), JsValue::Undefined);
        context.insert(
            "unpaired".into(),
            JsValue::String(crate::JsString::from_utf16(vec![0xd800])),
        );
        context.insert("minus_zero".into(), JsValue::Number(-0.0));
        let raw = encode(&request, &JsValue::Object(context)).unwrap();
        let decoded = decode(&raw).unwrap();
        assert_eq!(decoded.request, request);
        assert_eq!(decoded.context.get("absent"), Some(&JsValue::Undefined));
        assert_eq!(
            decoded
                .context
                .get("unpaired")
                .unwrap()
                .as_string()
                .unwrap()
                .units(),
            &[0xd800]
        );
        let JsValue::Number(n) = decoded.context.get("minus_zero").unwrap() else {
            panic!()
        };
        assert_eq!(n.to_bits(), (-0.0f64).to_bits());
    }
    #[test]
    fn rejects_duplicate_and_unknown_envelope_members() {
        for bytes in [
            br#"{"request_json":"{}","request_json":"{}","context":{"profile":"pilotdeck.model-invocation-context/1","content_hex":"0700000000"}}"#.as_slice(),
            br#"{"request_json":"{}","context":{"profile":"pilotdeck.model-invocation-context/1","profile":"pilotdeck.model-invocation-context/1","content_hex":"0700000000"}}"#.as_slice(),
            br#"{"request_json":"{}","context":{"profile":"pilotdeck.model-invocation-context/1","content_hex":"0700000000"},"extra":0}"#.as_slice(),
        ] { assert!(decode(bytes).is_err()); }
    }
    #[test]
    fn rejects_unknown_profile_and_invalid_hex() {
        let bytes = br#"{"request_json":"{}","context":{"profile":"other","content_hex":""}}"#;
        assert!(decode(bytes).is_err());
        let bytes = br#"{"request_json":"{}","context":{"profile":"pilotdeck.model-invocation-context/1","content_hex":"GG"}}"#;
        assert!(decode(bytes).is_err());
    }
}
