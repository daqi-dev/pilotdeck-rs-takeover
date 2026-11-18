use super::google::throw_if_google_abort;
use super::test_support::{google_response_error, send_google_request};
use super::tests::{
    SequenceBody, TEST_OVERLONG_CHOICES, TestFrame, TimedHeaders, poll_pending, poll_ready,
};
use super::*;
use crate::transport::{ModelHttpTransport, TransportFuture};
use pilotdeck_model_core::codec::projection::text as header_text;

/// Internal cancellation constructs AbortError with a fixed message. Exercise
/// the production catch handler directly to cover a distinct SDK abort reason.
#[test]
fn google_abort_name_preserves_original_message() {
    let error = ModelStreamError::thrown("AbortError", "SDK cancelled this specific request");
    let error = throw_if_google_abort(error, &StreamOptions::default(), false).unwrap_err();
    assert!(
        matches!(error, ModelStreamError::Thrown { name, message }
        if name.equals_str("AbortError")
            && message.equals_str("SDK cancelled this specific request")),
        "the original AbortError name and message must survive"
    );
}

/// Google uses bare SDK fetch but must preserve the same transport event
/// sources as the generic provider, including every redirected hop.
#[tokio::test(start_paused = true)]
async fn google_exchange_orders_response_timeout_and_parent() {
    for (response_ms, parent_ms, sync, expected) in [
        (60, 80, false, "response"),
        (100, 120, false, "timeout"),
        (110, 90, false, "abort"),
        (60, 80, true, "abort"),
    ] {
        let provider = parse_json(r#"{"id":"p","protocol":"google","url":"http://example.test","apiKey":"k","headers":{}}"#).unwrap();
        let abort = AbortFlag::new();
        let options = StreamOptions {
            abort: Some(abort.clone()),
            stream_timeout_ms: Some(100.0),
            ..StreamOptions::default()
        };
        let started = tokio::time::Instant::now();
        let transport = TimedHeaders(Duration::from_millis(response_ms));
        let model = JsValue::from("m");
        let body = JsValue::Null;
        let mut pending = Box::pin(send_google_request(
            &transport, &provider, &model, &body, &options,
        ));
        poll_pending(pending.as_mut()).await;
        tokio::time::advance(Duration::from_millis(130)).await;
        if sync {
            abort.abort();
        } else {
            abort.cancel_at(started + Duration::from_millis(parent_ms));
        }
        let result = poll_ready(pending.as_mut())
            .await
            .map_err(|(error, _)| error);
        match expected {
            "response" => assert!(result.is_ok(), "{result:?}"),
            "abort" => assert!(result.unwrap_err().name().equals_str("AbortError")),
            "timeout" => assert!(
                result
                    .unwrap_err()
                    .name()
                    .equals_str("StreamIdleTimeoutError")
            ),
            _ => unreachable!(),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn google_redirect_retains_absolute_deadline_and_second_hop_source() {
    struct Redirect(std::sync::atomic::AtomicUsize);
    impl ModelHttpTransport for Redirect {
        fn send(&self, _: Request<Full<Bytes>>) -> TransportFuture {
            let first = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            TransportFuture::after(
                Duration::from_millis(if first { 60 } else { 50 }),
                Ok(Response::builder()
                    .status(if first { 307 } else { 200 })
                    .header("location", "/second")
                    .body(
                        Full::new(Bytes::new())
                            .map_err(|never| match never {})
                            .boxed(),
                    )
                    .unwrap()),
            )
        }
    }
    let provider = parse_json(
        r#"{"id":"p","protocol":"google","url":"http://example.test","apiKey":"k","headers":{}}"#,
    )
    .unwrap();
    let options = StreamOptions {
        stream_timeout_ms: Some(100.0),
        ..StreamOptions::default()
    };
    let transport = Redirect(std::sync::atomic::AtomicUsize::new(0));
    let model = JsValue::from("m");
    let body = JsValue::Null;
    let mut pending = Box::pin(send_google_request(
        &transport, &provider, &model, &body, &options,
    ));
    poll_pending(pending.as_mut()).await;
    tokio::time::advance(Duration::from_millis(60)).await;
    poll_pending(pending.as_mut()).await;
    assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_millis(60)).await;
    assert!(
        poll_ready(pending.as_mut())
            .await
            .unwrap_err()
            .0
            .name()
            .equals_str("StreamIdleTimeoutError")
    );
}

/// Reuse the original TS error cases without needing a listening socket.
/// This supplements (and does not replace) the real HTTP oracle replay.
#[tokio::test]
async fn google_sdk_errors_match_the_recorded_status_text_and_content_type() {
    let oracle = crate::http::test_baseline::oracle(include_str!(
        "../../tests/fixtures/model-driver/model-stream-google.json"
    ));
    let mut checked = 0;
    for case in pilotdeck_model_core::codec::projection::array(field(&oracle, "cases")) {
        if ![
            "error-json-401",
            "error-custom-status-text",
            "error-content-type-case",
        ]
        .contains(&header_text(field(case, "name")).as_str())
        {
            continue;
        }
        let reply = &pilotdeck_model_core::codec::projection::array(field(case, "replies"))[0];
        let mut response =
            Response::builder().status(field(reply, "status").as_number().unwrap() as u16);
        for (name, value) in field(reply, "headers").as_object().unwrap().entries() {
            response = response.header(name.to_utf8().unwrap(), header_text(value));
        }
        if let Some(reason) = field(reply, "statusText").as_string() {
            response = response
                .extension(hyper::ext::ReasonPhrase::try_from(reason.to_utf8().unwrap()).unwrap());
        }
        let bytes: Vec<u8> = pilotdeck_model_core::codec::projection::array(field(reply, "chunks"))
            .iter()
            .flat_map(|chunk| header_text(field(chunk, "text")).into_bytes())
            .collect();
        let body = Full::new(Bytes::from(bytes))
            .map_err(|never| match never {})
            .boxed();
        let error = google_response_error(
            response.body((body, false)).unwrap(),
            &StreamOptions::default(),
            field(&oracle, "provider"),
        )
        .await
        .unwrap();
        let ModelStreamError::Provider(actual) = error else {
            panic!("expected canonical SDK error");
        };
        let timeline = pilotdeck_model_core::codec::projection::array(field(case, "timeline"));
        let mut expected = field(field(timeline.last().unwrap(), "event"), "error")
            .as_object()
            .unwrap()
            .clone();
        // The outer retry loop adds this checkpoint after SDK normalization.
        // Its full shape remains covered by the loopback timeline replay.
        expected.insert("streamInterruption".into(), JsValue::Undefined);
        assert_eq!(
            pilotdeck_model_core::codec::js::json_stringify(&JsValue::Object(actual)).unwrap(),
            pilotdeck_model_core::codec::js::json_stringify(&JsValue::Object(expected)).unwrap(),
            "{}: SDK error",
            header_text(field(case, "name"))
        );
        checked += 1;
    }
    assert_eq!(checked, 3);
}

/// The non-2xx Google path shares the `response_text` reader, so an
/// over-limit error body is drained to EOF and then rejects with V8's plain
/// `Error` — the SDK error-body promise never normalizes a string that could
/// not be created.
#[tokio::test(start_paused = true)]
async fn google_non_2xx_overlong_body_throws_the_v8_error() {
    let _limit = LimitGuard::set(32);
    let provider = JsValue::Object(JsObject::new());
    let eof = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let body = SequenceBody {
        frames: vec![
            TestFrame::After(Duration::from_millis(40), TEST_OVERLONG_CHOICES),
            TestFrame::After(Duration::from_millis(40), b" "),
        ]
        .into(),
        timer: None,
        eof: eof.clone(),
    }
    .boxed();
    let response = Response::builder().status(500).body((body, false)).unwrap();
    let error = google_response_error(response, &StreamOptions::default(), &provider)
        .await
        .expect_err("an over-long error body must fail the text conversion");
    assert!(error.name().equals_str("Error"), "{error:?}");
    assert_eq!(error.message_text(), crate::fetch_redirect::STRING_TOO_LONG);
    assert!(
        eof.load(std::sync::atomic::Ordering::Relaxed),
        "the error was raised before the error body reached EOF"
    );
}
