use crate::channel::sms;
use pilotdeck_protocol::{JsString, JsValue};

pub(super) fn fragment(event: &JsValue) -> Option<JsString> {
    if event
        .get("type")
        .and_then(JsValue::as_string)
        .is_some_and(|kind| kind.equals_str("error"))
    {
        // A template interpolation stringifies null. The shared SMS helper
        // treats this field as optional; preserve this adapter's actual value.
        let message = sms::js_string_of(event.get("message").unwrap_or(&JsValue::Undefined));
        return Some(sms::concat_units(&[
            &"\n❌ ".encode_utf16().collect::<Vec<_>>(),
            message.units(),
            &[10],
        ]));
    }
    sms::render::render_sms_event(event)
}
