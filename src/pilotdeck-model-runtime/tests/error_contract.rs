use pilotdeck_model_core::codec::normalize_error::{self as errors, is_error_instance_raw};
use pilotdeck_model_runtime::{BodyFailure, ModelStreamError, TransportFailure};
use pilotdeck_protocol::{JsString, JsValue, parse_json};

// The original Gateway http_tests.rs test is retained here unchanged except imports.
/// Every Error projection a `JsError` carries is recognized as an Error
/// instance (durable records store `{}` for it); a provider's parsed JSON
/// error body is not, even when it has a `code` or a `name`.
#[test]
fn error_instance_raws_cover_every_js_error_shape_and_no_provider_body() {
    for error in [
        ModelStreamError::thrown("TypeError", "terminated").as_js_error(),
        ModelStreamError::thrown("StreamIdleTimeoutError", "idle").as_js_error(),
        errors::NetworkFetchError::new("network_timeout", "t", JsValue::Undefined).as_js_error(),
        errors::NetworkFetchError::classify("fetch failed", None).as_js_error(),
    ] {
        assert!(is_error_instance_raw(&error.raw), "{:?}", error.raw);
    }
    for raw in [
        r#"{"name":"ApiError","status":500}"#,
        r#"{"errno":-2,"code":"ENOENT","syscall":"open","path":"/x"}"#,
    ] {
        assert!(is_error_instance_raw(&parse_json(raw).unwrap()), "{raw}");
    }
    for raw in [
        r#"{"error":{"message":"bad","type":"invalid_request_error","code":"bbx_bad"}}"#,
        r#"{"code":"network_timeout"}"#,
        r#"{"name":"NetworkFetchError","code":"network_timeout","message":"x"}"#,
        r#"{"name":"ApiError","status":500,"error":{}}"#,
        r#"{"name":"SomethingElse"}"#,
        r#"[]"#,
    ] {
        assert!(!is_error_instance_raw(&parse_json(raw).unwrap()), "{raw}");
    }
}

#[test]
fn error_types_preserve_utf16_raw_and_abort_precedence() {
    let text = JsString::from_utf16(vec![0xd800, 0xdfff, 0xd800]);
    let thrown = ModelStreamError::thrown("TypeError", text.clone());
    assert_eq!(thrown.message(), text);
    assert_eq!(thrown.as_js_error().message, text);
    let original = parse_json(r#"{"message":"request aborted","retryable":true,"raw":{"text":"\ud800"},"code":"server_error","retryAfterMs":2}"#).unwrap();
    let error = ModelStreamError::Provider(original.as_object().unwrap().clone());
    assert!(error.is_abort());
    let ModelStreamError::Provider(retained) = error else {
        panic!("provider error")
    };
    assert_eq!(JsValue::Object(retained), original);
    assert!(ModelStreamError::abort().name().equals_str("AbortError"));
}

#[test]
fn typed_transport_failures_preserve_original_external_messages() {
    assert_eq!(BodyFailure::Terminated.to_string(), "terminated");
    assert_eq!(
        BodyFailure::Decode("incorrect header check").to_string(),
        "terminated"
    );
    assert_eq!(
        BodyFailure::StringTooLong.to_string(),
        "Cannot create a string longer than 0x1fffffe8 characters"
    );
    let failure = TransportFailure::fetch_failed(Some("ECONNREFUSED"));
    assert_eq!(failure.message, "fetch failed");
    assert_eq!(failure.cause_code, Some("ECONNREFUSED"));
}

#[test]
fn request_errors_keep_code_details_and_utf16_without_projection() {
    let details = parse_json(r#"{"input":"\ud800","missing":null}"#).unwrap();
    let message = JsString::from_utf16(vec![0xd800]);
    let error = ModelStreamError::Request {
        code: "unsupported_tool_use".into(),
        message: message.clone(),
        details: Some(Box::new(details.clone())),
    };
    assert!(error.name().equals_str("ModelRequestError"));
    assert_eq!(error.message(), message);
    let ModelStreamError::Request {
        code,
        message: retained,
        details: retained_details,
    } = error
    else {
        panic!("request variant changed");
    };
    assert!(code.equals_str("unsupported_tool_use"));
    assert_eq!(retained, message);
    assert_eq!(retained_details.as_deref(), Some(&details));
}
