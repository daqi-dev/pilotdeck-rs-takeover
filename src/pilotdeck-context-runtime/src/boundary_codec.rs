//! Compatibility path for the shared detached-value boundary profile.
//! The profile identifier, bytes, limits and errors are owned by protocol.
pub use pilotdeck_protocol::boundary_codec::*;

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::{JsObject, JsValue};

    #[test]
    fn compatibility_path_uses_identical_profile_and_wire_bytes() {
        let mut object = JsObject::new();
        object.insert("x".into(), JsValue::Undefined);
        let value = JsValue::Object(object);
        let bytes = encode(&value).unwrap();
        assert_eq!(PROFILE, pilotdeck_protocol::boundary_codec::PROFILE);
        assert_eq!(
            bytes,
            pilotdeck_protocol::boundary_codec::encode(&value).unwrap()
        );
        assert_eq!(bytes, [7, 1, 0, 0, 0, 1, 0, 0, 0, 120, 0, 0]);
        assert_eq!(decode(&bytes).unwrap(), value);
    }
}
