//! Product context boundary profile v1 for the supported detached JsValue
//! subset. Unsupported native/prototype objects are rejected. Explicit byte,
//! node and depth caps are checked before a request can cause host effects.
//! This product payload is not an authority or a Kernel protocol.
use crate::{JsObject, JsString, JsValue};

pub const PROFILE: &str = "pilotdeck.context.boundary.v1";
// A profile limit, leaving room for the 32 MiB journal envelope; larger values
// require a future staged-artifact profile rather than automatic fallback.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_NODES: usize = 100_000;
pub const MAX_DEPTH: usize = 2_048;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    TooLarge,
    Truncated,
    InvalidTag,
    DuplicateKey,
    TrailingBytes,
    UnsupportedNative,
}
enum Task<'a> {
    Value(&'a JsValue, usize),
    Key(&'a JsString),
}
fn count(out: &mut Vec<u8>, size: usize) -> Result<(), CodecError> {
    let n = u32::try_from(size).map_err(|_| CodecError::TooLarge)?;
    out.extend_from_slice(&n.to_le_bytes());
    Ok(())
}
fn units(out: &mut Vec<u8>, value: &JsString) -> Result<(), CodecError> {
    count(out, value.len_utf16())?;
    let need = value
        .len_utf16()
        .checked_mul(2)
        .and_then(|n| out.len().checked_add(n))
        .ok_or(CodecError::TooLarge)?;
    if need > MAX_BYTES {
        return Err(CodecError::TooLarge);
    }
    for unit in value.units() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(())
}
pub fn encode(value: &JsValue) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    let mut stack = vec![Task::Value(value, 0)];
    let mut scheduled_nodes = 1usize;
    while let Some(task) = stack.pop() {
        match task {
            Task::Key(key) => units(&mut out, key)?,
            Task::Value(value, depth) => match value {
                JsValue::Undefined => out.push(0),
                JsValue::Null => out.push(1),
                JsValue::Bool(false) => out.push(2),
                JsValue::Bool(true) => out.push(3),
                JsValue::Number(n) => {
                    out.push(4);
                    out.extend_from_slice(&n.to_bits().to_le_bytes());
                }
                JsValue::String(s) => {
                    out.push(5);
                    units(&mut out, s)?;
                }
                JsValue::Array(items) => {
                    if items.len() > MAX_BYTES.saturating_sub(out.len()) {
                        return Err(CodecError::TooLarge);
                    }
                    if !items.is_empty() && depth >= MAX_DEPTH {
                        return Err(CodecError::TooLarge);
                    }
                    scheduled_nodes = scheduled_nodes
                        .checked_add(items.len())
                        .ok_or(CodecError::TooLarge)?;
                    if scheduled_nodes > MAX_NODES {
                        return Err(CodecError::TooLarge);
                    }
                    out.push(6);
                    count(&mut out, items.len())?;
                    for item in items.iter().rev() {
                        stack.push(Task::Value(item, depth + 1));
                    }
                }
                JsValue::Object(object) => {
                    if object.prototype().is_some() || object.native_tag().is_some() {
                        return Err(CodecError::UnsupportedNative);
                    }
                    if object.entries().len() > MAX_BYTES.saturating_sub(out.len()) / 5 {
                        return Err(CodecError::TooLarge);
                    }
                    if !object.entries().is_empty() && depth >= MAX_DEPTH {
                        return Err(CodecError::TooLarge);
                    }
                    scheduled_nodes = scheduled_nodes
                        .checked_add(object.entries().len())
                        .ok_or(CodecError::TooLarge)?;
                    if scheduled_nodes > MAX_NODES {
                        return Err(CodecError::TooLarge);
                    }
                    out.push(7);
                    count(&mut out, object.entries().len())?;
                    for (key, value) in object.entries().iter().rev() {
                        stack.push(Task::Value(value, depth + 1));
                        stack.push(Task::Key(key));
                    }
                }
            },
        }
        if out.len() > MAX_BYTES {
            return Err(CodecError::TooLarge);
        }
    }
    Ok(out)
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let end = self.at.checked_add(n).ok_or(CodecError::TooLarge)?;
        let bytes = self.bytes.get(self.at..end).ok_or(CodecError::Truncated)?;
        self.at = end;
        Ok(bytes)
    }
    fn u32(&mut self) -> Result<usize, CodecError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize)
    }
    fn string(&mut self) -> Result<JsString, CodecError> {
        let length = self.u32()?;
        let bytes = self.take(length.checked_mul(2).ok_or(CodecError::TooLarge)?)?;
        Ok(JsString::from_utf16(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&x| u16::from_le_bytes(x))
                .collect(),
        ))
    }
}
enum Frame {
    Array {
        remaining: usize,
        values: Vec<JsValue>,
    },
    Object {
        remaining: usize,
        value: JsObject,
        key: JsString,
    },
}
pub fn decode(bytes: &[u8]) -> Result<JsValue, CodecError> {
    if bytes.len() > MAX_BYTES {
        return Err(CodecError::TooLarge);
    }
    let mut reader = Reader { bytes, at: 0 };
    let mut stack = Vec::new();
    let mut made: Option<JsValue> = None;
    let mut nodes = 0usize;
    loop {
        if let Some(value) = made.take() {
            match stack.pop() {
                None => {
                    return if reader.at == bytes.len() {
                        Ok(value)
                    } else {
                        Err(CodecError::TrailingBytes)
                    };
                }
                Some(Frame::Array {
                    remaining,
                    mut values,
                }) => {
                    values.push(value);
                    if remaining == 1 {
                        made = Some(JsValue::Array(values));
                    } else {
                        stack.push(Frame::Array {
                            remaining: remaining - 1,
                            values,
                        });
                    }
                }
                Some(Frame::Object {
                    remaining,
                    value: mut object,
                    key,
                }) => {
                    if object.get_key(&key).is_some() {
                        return Err(CodecError::DuplicateKey);
                    }
                    object.insert(key, value);
                    if remaining == 1 {
                        made = Some(JsValue::Object(object));
                    } else {
                        let key = reader.string()?;
                        stack.push(Frame::Object {
                            remaining: remaining - 1,
                            value: object,
                            key,
                        });
                    }
                }
            }
            continue;
        }
        if stack.len() > MAX_DEPTH {
            return Err(CodecError::TooLarge);
        }
        nodes = nodes.checked_add(1).ok_or(CodecError::TooLarge)?;
        if nodes > MAX_NODES {
            return Err(CodecError::TooLarge);
        }
        let tag = reader.take(1)?[0];
        made = match tag {
            0 => Some(JsValue::Undefined),
            1 => Some(JsValue::Null),
            2 => Some(JsValue::Bool(false)),
            3 => Some(JsValue::Bool(true)),
            4 => Some(JsValue::Number(f64::from_bits(u64::from_le_bytes(
                reader.take(8)?.try_into().unwrap(),
            )))),
            5 => Some(JsValue::String(reader.string()?)),
            6 => {
                let n = reader.u32()?;
                if n > MAX_NODES.saturating_sub(nodes) {
                    return Err(CodecError::TooLarge);
                }
                if n > reader.bytes.len() - reader.at {
                    return Err(CodecError::Truncated);
                }
                if n == 0 {
                    Some(JsValue::Array(Vec::new()))
                } else {
                    stack.push(Frame::Array {
                        remaining: n,
                        values: Vec::new(),
                    });
                    None
                }
            }
            7 => {
                let n = reader.u32()?;
                if n > MAX_NODES.saturating_sub(nodes) {
                    return Err(CodecError::TooLarge);
                }
                if n > (reader.bytes.len() - reader.at) / 5 {
                    return Err(CodecError::Truncated);
                }
                if n == 0 {
                    Some(JsValue::Object(JsObject::new()))
                } else {
                    let key = reader.string()?;
                    stack.push(Frame::Object {
                        remaining: n,
                        value: JsObject::new(),
                        key,
                    });
                    None
                }
            }
            _ => return Err(CodecError::InvalidTag),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_undefined_lone_surrogate_number_bits_and_order() {
        let key = JsString::from_utf16(vec![0xd800, 0x61]);
        let mut object = JsObject::new();
        object.insert("u".into(), JsValue::Undefined);
        object.insert(
            key.clone(),
            JsValue::String(JsString::from_utf16(vec![0xdc00])),
        );
        object.insert(
            "values".into(),
            JsValue::Array(vec![
                JsValue::Number(-0.0),
                JsValue::Number(f64::from_bits(0x7ff8_0000_0000_0042)),
                JsValue::Number(f64::INFINITY),
            ]),
        );
        let bytes = encode(&JsValue::Object(object)).unwrap();
        let decoded = decode(&bytes).unwrap();
        let object = decoded.as_object().unwrap();
        assert!(object.get("u").unwrap().is_undefined());
        assert_eq!(
            object.get_key(&key).unwrap().as_string().unwrap().units(),
            &[0xdc00]
        );
        assert_eq!(object.entries()[1].0, key);
        let Some(JsValue::Array(values)) = object.get("values") else {
            panic!("array")
        };
        let expected = [
            (-0.0_f64).to_bits(),
            0x7ff8_0000_0000_0042,
            f64::INFINITY.to_bits(),
        ];
        for (value, expected) in values.iter().zip(expected) {
            assert_eq!(value.as_number().unwrap().to_bits(), expected);
        }
    }
    #[test]
    fn rejects_truncated_duplicate_unknown_and_trailing_data() {
        assert_eq!(decode(&[4, 0]), Err(CodecError::Truncated));
        assert_eq!(decode(&[255]), Err(CodecError::InvalidTag));
        assert_eq!(decode(&[1, 1]), Err(CodecError::TrailingBytes));
        // object with two zero-unit keys, each followed by null
        let duplicate = [7, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1];
        assert_eq!(decode(&duplicate), Err(CodecError::DuplicateKey));
    }
    #[test]
    fn rejects_node_and_depth_exhaustion_before_large_allocation() {
        let mut many = vec![6];
        many.extend_from_slice(&((MAX_NODES as u32) + 1).to_le_bytes());
        assert_eq!(decode(&many), Err(CodecError::TooLarge));
        let mut nested = [6, 1, 0, 0, 0].repeat(MAX_DEPTH + 1);
        nested.push(1);
        assert_eq!(decode(&nested), Err(CodecError::TooLarge));
        let value = JsValue::Array(vec![JsValue::Null; MAX_NODES]);
        assert_eq!(encode(&value), Err(CodecError::TooLarge));
    }
    #[test]
    fn deep_values_use_iterative_traversal() {
        let mut value = JsValue::Null;
        for _ in 0..2_000 {
            value = JsValue::Array(vec![value]);
        }
        let bytes = encode(&value).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(encode(&decoded).unwrap(), bytes);
    }
}
