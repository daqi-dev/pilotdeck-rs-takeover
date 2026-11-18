//! Gateway-owned model HTTP tests: original filesystem, Date.parse, audit and adapter witnesses.
use super::google::dump_failure_error;
use super::*;
use std::{future::Future, pin::Pin, sync::Arc};

pub(super) async fn poll_pending<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}

type Reply = (u16, Vec<Option<&'static [u8]>>);
struct Scripted(std::sync::Mutex<Vec<Reply>>);
impl ModelHttpTransport for Scripted {
    fn send(&self, _request: Request<Full<Bytes>>) -> TransportFuture {
        let (status, frames) = self.0.lock().unwrap().remove(0);
        let stream = futures_body(frames);
        TransportFuture::ready(Ok(Response::builder().status(status).body(stream).unwrap()))
    }
}
fn futures_body(frames: Vec<Option<&'static [u8]>>) -> ModelBody {
    struct Frames(std::collections::VecDeque<Option<&'static [u8]>>);
    impl hyper::body::Body for Frames {
        type Data = Bytes;
        type Error = BodyFailure;
        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, BodyFailure>>> {
            match self.0.pop_front() {
                None => std::task::Poll::Ready(None),
                Some(None) => {
                    self.0.push_front(None);
                    std::task::Poll::Pending
                }
                Some(Some(bytes)) => std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(
                    Bytes::from_static(bytes),
                )))),
            }
        }
    }
    Frames(frames.into_iter().collect()).boxed()
}
pub(super) fn stream_config() -> (JsValue, JsValue) {
    let request = parse_json(r#"{"provider":"p","model":"m","messages":[]}"#).unwrap();
    let config = parse_json(
        r#"{"providers":{"p":{"id":"p","protocol":"anthropic","url":"http://127.0.0.1:8080/v1","apiKey":"k","headers":{},
        "retry":{"baseDelayMs":1,"maxDelayMs":1},
        "models":{"m":{"id":"m","capabilities":{"supportsStreaming":true,"maxOutputTokens":8},"multimodal":{"input":["text"]}}}}}}"#,
    )
    .unwrap();
    (request, config)
}

#[test]
fn retry_after_header_and_message_parsing() {
    // delta-seconds, rounded like Math.round(seconds * 1000)
    assert_eq!(
        errors::retry_after_header(Some(" 0.0125 "), 0.0),
        Some(13.0)
    );
    assert_eq!(errors::retry_after_header(Some(""), 0.0), None);
    assert_eq!(errors::retry_after_header(None, 0.0), None);
    // IMF-fixdate in the future/past
    let now = 1_445_412_470_000.0; // 2015-10-21T07:27:50Z
    assert_eq!(
        errors::retry_after_header(Some("Wed, 21 Oct 2015 07:28:00 GMT"), now),
        Some(10_000.0)
    );
    assert_eq!(
        errors::retry_after_header(Some("Wed, 21 Oct 2015 07:27:00 GMT"), now),
        None
    );
    assert_eq!(
        retry_after_from_message("Please try again in 1.898s"),
        Some(1898.0)
    );
    assert_eq!(
        retry_after_from_message("RETRY IN 2 minutes"),
        Some(120_000.0)
    );
    assert_eq!(retry_after_from_message("try again in 0s"), None);
    // `i` without `u`: U+017F (long s) does not fold onto ASCII `s`.
    assert_eq!(retry_after_from_message("try again in 3\u{17f}"), None);
}

#[test]
fn retry_after_ranges_follow_node_date_parse() {
    // node probe (r1/probe-r1.out.txt): 32 Oct / 00 / 25:00 / 07:60 /
    // 24:00:01 -> NaN; 31 Feb rolls over; 24:00:00 is next midnight; a
    // second >= 60 reads as :00. These now exercise the shared Node22
    // `Date.parse` port (#360-2); the hand-rolled IMF-only parser is gone.
    let at = |text: &str| errors::retry_after_header(Some(text), 0.0);
    assert_eq!(
        at("Wed, 21 Oct 2015 07:28:00 GMT"),
        Some(1_445_412_480_000.0)
    );
    assert_eq!(at("Wed, 32 Oct 2015 07:28:00 GMT"), None);
    assert_eq!(at("Wed, 00 Oct 2015 07:28:00 GMT"), None);
    assert_eq!(at("Wed, 21 Oct 2015 25:00:00 GMT"), None);
    assert_eq!(at("Wed, 21 Oct 2015 07:60:00 GMT"), None);
    assert_eq!(at("Wed, 21 Oct 2015 24:00:01 GMT"), None);
    assert_eq!(at("Wed, 99999999999999999999 Oct 2015 07:28:00 GMT"), None);
    assert_eq!(
        at("Wed, 31 Feb 2015 07:28:00 GMT"),
        Some(1_425_367_680_000.0)
    );
    assert_eq!(
        at("Wed, 21 Oct 2015 24:00:00 GMT"),
        Some(1_445_472_000_000.0)
    );
    assert_eq!(
        at("Wed, 1 Oct 2015 07:28:00 GMT"),
        Some(1_443_684_480_000.0)
    );
    assert_eq!(
        at("Wed, 21 Oct 2015 07:28:61 GMT"),
        Some(1_445_412_480_000.0)
    );
    assert_eq!(at("Wed, 21 Oct 275761 07:28:00 GMT"), None);
}

#[test]
fn retry_after_year_edges_follow_node_date_parse() {
    let at = |text: &str| errors::retry_after_header(Some(text), 0.0);
    assert_eq!(at("Fri, 01 Jan 999999999 00:00:00 GMT"), None);
    assert_eq!(at("Fri, 01 Jan 292278995 00:00:00 GMT"), None);
    assert_eq!(at("Fri, 01 Jan 1000000 00:00:00 GMT"), None);
    assert_eq!(at("Fri, 31 Dec 999999999 24:00:00 GMT"), None);
    assert_eq!(
        at("Fri, 01 Jan 275760 00:00:00 GMT"),
        Some(8_639_977_881_600_000.0)
    );
    assert_eq!(
        at("Fri, 13 Sep 275760 00:00:00 GMT"),
        Some(8_640_000_000_000_000.0)
    );
    assert_eq!(at("Fri, 14 Sep 275760 00:00:00 GMT"), None);
    assert_eq!(at("Fri, 01 Jan 0 00:00:00 GMT"), Some(946_684_800_000.0));
    assert_eq!(at("Fri, 01 Jan 0000 00:00:00 GMT"), Some(946_684_800_000.0));
    assert_eq!(at("Wed, 21 Oct 0099 07:28:00 GMT"), Some(940_490_880_000.0));
    assert_eq!(at("Fri, 01 Jan 2015 999999999:00:00 GMT"), None);
    assert_eq!(
        at("Fri, 01 Jan 2015 00:00:999999999 GMT"),
        Some(1_420_070_400_000.0)
    );
}

#[test]
fn provider_error_projects_onto_module_error() {
    let mut canonical = JsObject::new();
    canonical.insert("code".into(), "rate_limit_error".into());
    canonical.insert("message".into(), "slow down".into());
    canonical.insert("retryable".into(), JsValue::Bool(true));
    canonical.insert("retryAfterMs".into(), JsValue::Number(20.0));
    let module = ModuleError::from(ModelStreamError::Provider(canonical.clone()));
    assert_eq!(module.code, Some("rate_limit_error".into()));
    assert_eq!(module.retryable, Some(true));
    assert_eq!(module.retry_after_ms, Some(20.0));
    assert_eq!(module.canonical, Some(canonical));
    let request = ModuleError::from(ModelStreamError::Request {
        code: "provider_not_found".into(),
        message: "Provider x does not exist.".into(),
        details: None,
    });
    assert_eq!(request.retryable, Some(false));
}

#[tokio::test]
async fn invocation_audit_records_each_attempt_terminal() {
    let (request, config) = stream_config();
    let root = std::env::temp_dir().join(format!("pdrs-audit-{}", uuid::Uuid::new_v4()));
    let sink = Arc::new(crate::storage::invocation_log::JsonlInvocationLogSink::new(
        crate::storage::invocation_log::InvocationStorageConfig {
            root: root.clone(),
            storage_config_version: "env".into(),
        },
    ));
    let mut context = JsObject::new();
    for (key, value) in [
        ("workspaceId", "ws"),
        ("sessionId", "s1"),
        ("turnId", "t1"),
        ("runId", "t1"),
        ("logicalCallId", "call-1"),
        ("caller", "agent"),
        ("storageConfigVersion", "env"),
    ] {
        context.insert(key.into(), value.into());
    }
    let options = StreamOptions {
        invocation: Some(InvocationAudit {
            sink: sink.clone(),
            context,
        }),
        ..StreamOptions::default()
    };
    let partial: &'static [u8] = b"data: {\"type\":\"message_start\"}\n\n";
    let transport = Scripted(std::sync::Mutex::new(vec![
        (200, vec![Some(partial)]),
        (200, vec![Some(partial)]),
        (200, vec![Some(partial)]),
        (400, vec![Some(b"{\"error\":\"nope\"}".as_slice())]),
    ]));
    let mut sink_events = |_: StreamOutput| {};
    let _ = stream_model(&transport, &request, &config, &options, &mut sink_events).await;
    let _ = stream_model(&transport, &request, &config, &options, &mut sink_events).await;
    let dir = root.join("workspaces/ws/sessions/s1/llm");
    let records: Vec<JsValue> = std::fs::read_to_string(dir.join("invocations.jsonl"))
        .unwrap()
        .lines()
        .map(|line| parse_json(line).unwrap())
        .collect();
    let summary: Vec<String> = records
        .iter()
        .map(|record| {
            format!(
                "{}|{}|{}|{}",
                crate::js_value::number_string(field(record, "attempt").as_number().unwrap()),
                String::from_utf16_lossy(field(record, "outcome").as_string().unwrap().units()),
                crate::js_value::number_string(field(record, "httpStatus").as_number().unwrap()),
                String::from_utf16_lossy(
                    field(record, "responseBody").as_string().unwrap().units()
                ),
            )
        })
        .collect();
    let partial_text = String::from_utf8_lossy(partial);
    assert_eq!(
        summary,
        [
            format!("1|incomplete|200|{partial_text}"),
            format!("2|incomplete|200|{partial_text}"),
            format!("3|incomplete|200|{partial_text}"),
            "1|provider_error|400|{\"error\":\"nope\"}".to_owned(),
        ]
    );
    assert_eq!(
        field(&records[0], "responseBytes").as_number(),
        Some(partial.len() as f64)
    );
    assert_eq!(std::fs::read_dir(dir.join(".pending")).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(root);
}

struct DumpScratch {
    dir: std::path::PathBuf,
    token: String,
}
impl DumpScratch {
    fn acquire(tag: &str) -> Self {
        let root = std::env::var("PDRS_DUMP_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        for _ in 0..8 {
            let id = uuid::Uuid::new_v4();
            let dir = root.join(format!("pdrs-dump-test-{tag}-{id}"));
            // Exclusive create (no create_dir_all): a missing root fails
            // loudly instead of reaching into an unexpected tree.
            if std::fs::create_dir(&dir).is_ok() {
                let token = format!("owner:{id}\n");
                if std::fs::write(dir.join(".owner"), &token).is_err() {
                    let _ = std::fs::remove_dir_all(&dir);
                    panic!("dump scratch: cannot write owner token into {dir:?}");
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
                }
                return Self { dir, token };
            }
            // Colliding with a pre-existing entry: try the next UUID. Never
            // remove the colliding entry — it is not ours.
        }
        panic!("dump scratch: could not exclusively create a test directory in {root:?}");
    }
}
impl Drop for DumpScratch {
    fn drop(&mut self) {
        // Remove only what this guard acquired, and only while the owner
        // token still matches (a directory without it was never ours).
        if let Ok(token) = std::fs::read_to_string(self.dir.join(".owner"))
            && token == self.token
        {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
impl std::ops::Deref for DumpScratch {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.dir
    }
}

fn fixed_names<'a>(names: &'a [&'a str]) -> impl FnMut() -> String + 'a {
    let mut index = 0;
    move || {
        let name = names[index].to_string();
        index += 1;
        name
    }
}

#[cfg(unix)]
#[test]
fn dump_creation_refuses_to_follow_a_pre_existing_symlink() {
    let dir = DumpScratch::acquire("symlink");
    // The victim stands in for another user's (or our own) sensitive data.
    let victim = dir.join("victim.json");
    std::fs::write(&victim, "SECRET-VICTIM-DATA").unwrap();
    std::os::unix::fs::symlink(&victim, dir.join("taken.json")).unwrap();
    let path = write_dump_exclusively(
        &dir,
        "{\"dump\":true}",
        &mut fixed_names(&["taken.json", "free.json"]),
        8,
    )
    .expect("second candidate must be used");
    assert_eq!(path, dir.join("free.json"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"dump\":true}");
    // O_CREAT|O_EXCL refused the symlink: the victim was neither truncated
    // nor turned into the dump.
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "SECRET-VICTIM-DATA"
    );
}

#[test]
fn dump_creation_refuses_to_truncate_an_existing_file() {
    let dir = DumpScratch::acquire("existing");
    let taken = dir.join("taken.json");
    std::fs::write(&taken, "PRE-EXISTING").unwrap();
    let path = write_dump_exclusively(
        &dir,
        "{\"dump\":true}",
        &mut fixed_names(&["taken.json", "free.json"]),
        8,
    )
    .expect("second candidate must be used");
    assert_eq!(path, dir.join("free.json"));
    // A same-millisecond collision (TS `Date.now()` alone) truncates; the
    // exclusive create moves to the next name and leaves the first intact.
    assert_eq!(std::fs::read_to_string(&taken).unwrap(), "PRE-EXISTING");
}

#[test]
fn dump_creation_reports_failure_when_all_candidates_exist() {
    let dir = DumpScratch::acquire("exhausted");
    for name in ["taken1.json", "taken2.json"] {
        std::fs::write(dir.join(name), "PRE-EXISTING").unwrap();
    }
    assert!(
        write_dump_exclusively(
            &dir,
            "{\"dump\":true}",
            &mut fixed_names(&["taken1.json", "taken2.json"]),
            2,
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("taken1.json")).unwrap(),
        "PRE-EXISTING"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("taken2.json")).unwrap(),
        "PRE-EXISTING"
    );
}

#[test]
fn dump_files_are_owner_only() {
    let dir = DumpScratch::acquire("permissions");
    let path = write_dump_exclusively(&dir, "{}", &mut fixed_names(&["fresh.json"]), 8)
        .expect("fresh candidate must be used");
    let file = std::fs::File::open(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "dump must stay private to this user");
    }
}

#[test]
fn dump_names_stay_unique_within_one_millisecond() {
    const NOW_MS: u128 = 1_758_000_000_000;
    const THREADS: usize = 32;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
    let (sender, receiver) = std::sync::mpsc::channel();
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let barrier = barrier.clone();
            let sender = sender.clone();
            std::thread::spawn(move || {
                barrier.wait();
                sender.send(next_dump_name(NOW_MS)).unwrap();
            })
        })
        .collect();
    drop(sender);
    let mut names: Vec<String> = receiver.iter().collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let pid = std::process::id();
    for name in &names {
        assert!(
            name.starts_with(&format!("pilotdeck_request_{NOW_MS}_{pid}_")),
            "name lost its timestamp/pid shape: {name}"
        );
        assert!(name.ends_with(".json"));
    }
    names.sort();
    names.dedup();
    assert_eq!(
        names.len(),
        THREADS,
        "same-millisecond names must stay distinct"
    );
}

#[test]
fn dump_failure_maps_errnos_through_the_libuv_table() {
    for (raw, code, description) in [
        (libc::ENOENT, "ENOENT", "no such file or directory"),
        (libc::EACCES, "EACCES", "permission denied"),
        (libc::ENOTDIR, "ENOTDIR", "not a directory"),
        (libc::ENAMETOOLONG, "ENAMETOOLONG", "name too long"),
        (libc::ELOOP, "ELOOP", "too many symbolic links encountered"),
        (libc::EFBIG, "EFBIG", "file too large"),
        (libc::EIO, "EIO", "i/o error"),
    ] {
        let failure = DumpFailure::from_io(
            &std::io::Error::from_raw_os_error(raw),
            "open",
            Some(std::path::Path::new("/tmp/pilotdeck_request_1.json")),
        );
        assert_eq!(failure.code, Some(code), "errno {raw}");
        assert_eq!(
            failure.description.as_deref(),
            Some(description),
            "errno {raw}"
        );
        assert_eq!(failure.errno, Some(-raw), "libuv-signed errno for {raw}");
        assert_eq!(
            failure.message(),
            format!("{code}: {description}, open '/tmp/pilotdeck_request_1.json'"),
            "errno {raw}"
        );
    }
    // A write-stage failure (open succeeded, write failed) has no path in
    // Node's error and no `open '<path>'` argument in the message (TS probe:
    // `EFBIG: file too large, write` with `raw:{errno,code,syscall}`).
    let write = DumpFailure::from_io(
        &std::io::Error::from_raw_os_error(libc::EFBIG),
        "write",
        None,
    );
    assert_eq!(write.errno, Some(-libc::EFBIG));
    assert_eq!(write.message(), "EFBIG: file too large, write");
}

#[cfg(unix)]
#[test]
fn sigxfsz_registration_degrades_without_a_runtime() {
    let attempt = std::thread::spawn(crate::register_sigxfsz)
        .join()
        .expect("registration attempt must not panic without a runtime");
    assert!(
        attempt.is_err(),
        "no-runtime registration must degrade, got: {:?}",
        attempt.map(|_| ())
    );
}

pub(super) const RETRY_WORDS_ORACLE: &str =
    include_str!("../../../tests/fixtures/model-stream-retry-words/oracle.json");

pub(super) fn oracle_text(value: &JsValue) -> String {
    value
        .as_string()
        .expect("oracle string")
        .to_utf8()
        .expect("oracle UTF-8")
}

#[test]
fn real_ts_router_transient_thrown_words_match_fallback() {
    let oracle = crate::test_baseline::oracle(RETRY_WORDS_ORACLE);
    let cases = super::super::count_body::array(field(&oracle, "routerCases"));
    assert_eq!(cases.len(), 30, "router case count");
    let request = parse_json(r#"{"provider":"test","model":"m","messages":[]}"#).unwrap();
    for case in cases {
        let error_name = oracle_text(field(case, "errorName"));
        let message = oracle_text(field(case, "message"));
        let canonical = super::super::fallback::canonical_error(
            ModelStreamError::Thrown {
                name: error_name.clone().into(),
                message: message.clone().into(),
            },
            &request,
            "openai".into(),
        );
        assert_eq!(
            oracle_text(field(&canonical, "code")),
            oracle_text(field(case, "code")),
            "canonical code for {error_name}: {message:?}"
        );
        assert_eq!(
            field(&canonical, "retryable").as_bool().expect("retryable"),
            field(case, "retryable").as_bool().expect("retryable"),
            "canonical retryable for {error_name}: {message:?}"
        );
    }
}

#[tokio::test]
async fn google_stream_invocation_audit_records_sdk_bodies_and_settles_each_attempt() {
    let (request, mut config) = stream_config();
    let mut provider = field(field(&config, "providers"), "p").clone();
    put(&mut provider, "protocol", "google".into());
    put(
        &mut provider,
        "retry",
        parse_json(r#"{"streamMaxRetries":0}"#).unwrap(),
    );
    let mut providers = JsObject::new();
    providers.insert("p".into(), provider);
    put(&mut config, "providers", JsValue::Object(providers));
    let root = std::env::temp_dir().join(format!("pdrs-google-audit-{}", uuid::Uuid::new_v4()));
    let sink = Arc::new(crate::storage::invocation_log::JsonlInvocationLogSink::new(
        crate::storage::invocation_log::InvocationStorageConfig {
            root: root.clone(),
            storage_config_version: "env".into(),
        },
    ));
    let mut context = JsObject::new();
    for (key, value) in [
        ("workspaceId", "ws"),
        ("sessionId", "s1"),
        ("turnId", "turn"),
        ("runId", "turn"),
        ("logicalCallId", "call"),
        ("caller", "agent"),
        ("storageConfigVersion", "env"),
    ] {
        context.insert(key.into(), value.into());
    }
    let options = StreamOptions {
        invocation: Some(InvocationAudit { sink, context }),
        ..StreamOptions::default()
    };
    struct GoogleAuditTransport(Scripted);
    impl ModelHttpTransport for GoogleAuditTransport {
        fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
            let future = self.0.send(request);
            TransportFuture::io(async move {
                let mut response = future.await?;
                response
                    .headers_mut()
                    .insert("x-z", "last".parse().unwrap());
                response
                    .headers_mut()
                    .insert("content-type", "text/event-stream".parse().unwrap());
                Ok(response)
            })
        }
    }
    let transport = GoogleAuditTransport(Scripted(std::sync::Mutex::new(vec![
        (200, vec![Some(b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]}}]}\n\ndata: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n")]),
        (400, vec![Some(b"{\"error\":{\"message\":\"bad\"}}")]),
    ])));
    for _ in 0..2 {
        stream_model(&transport, &request, &config, &options, &mut |_| {})
            .await
            .unwrap();
    }
    let dir = root.join("workspaces/ws/sessions/s1/llm");
    let records: Vec<JsValue> = std::fs::read_to_string(dir.join("invocations.jsonl"))
        .unwrap()
        .lines()
        .map(|line| parse_json(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    let request_body = field(&records[0], "requestBody").as_string().unwrap();
    let request_text = String::from_utf16_lossy(request_body.units());
    assert_eq!(
        request_text,
        r#"{"model":"m","contents":[{"parts":[{"text":""}],"role":"user"}],"config":{"maxOutputTokens":8,"automaticFunctionCalling":{"disable":true},"abortSignal":{}}}"#
    );
    let response_text = String::from_utf16_lossy(
        field(&records[0], "responseBody")
            .as_string()
            .unwrap()
            .units(),
    );
    assert_eq!(
        response_text,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]}}],"sdkHttpResponse":{"headers":{"content-type":"text/event-stream","x-z":"last"}}}{"candidates":[{"finishReason":"STOP"}],"sdkHttpResponse":{"headers":{"content-type":"text/event-stream","x-z":"last"}}}"#
    );
    assert_eq!(
        field(&records[0], "requestBytes").as_number(),
        Some(request_text.len() as f64)
    );
    assert_eq!(
        field(&records[0], "responseBytes").as_number(),
        Some(response_text.len() as f64)
    );
    for record in &records {
        assert_eq!(field(record, "runId"), &JsValue::from("turn"));
        assert!(field(record, "httpStatus").is_undefined());
    }
    assert_eq!(field(&records[0], "outcome"), &JsValue::from("success"));
    assert_eq!(field(&records[0], "responseComplete"), &JsValue::Bool(true));
    assert_eq!(field(&records[1], "outcome"), &JsValue::from("incomplete"));
    assert_eq!(
        field(&records[1], "responseComplete"),
        &JsValue::Bool(false)
    );
    assert_eq!(std::fs::read_dir(dir.join(".pending")).unwrap().count(), 0);
    // Audit's shallow extraBody merge must retain temperature, even though
    // the Google wire mapper deliberately sanitizes it from transport JSON.
    let mut extra_config = config.clone();
    let mut extra_provider = field(field(&extra_config, "providers"), "p").clone();
    put(
        &mut extra_provider,
        "extraBody",
        parse_json(r#"{"temperature":0.25,"generationConfig":{"temperature":0.5}}"#).unwrap(),
    );
    let mut providers = JsObject::new();
    providers.insert("p".into(), extra_provider);
    put(&mut extra_config, "providers", JsValue::Object(providers));
    let extra = Scripted(std::sync::Mutex::new(vec![(
        200,
        vec![Some(
            b"data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n",
        )],
    )]));
    stream_model(&extra, &request, &extra_config, &options, &mut |_| {})
        .await
        .unwrap();
    let settled = std::fs::read_to_string(dir.join("invocations.jsonl")).unwrap();
    let last = parse_json(settled.lines().last().unwrap()).unwrap();
    let raw = String::from_utf16_lossy(field(&last, "requestBody").as_string().unwrap().units());
    assert_eq!(
        raw,
        r#"{"model":"m","contents":[{"parts":[{"text":""}],"role":"user"}],"config":{"maxOutputTokens":8,"automaticFunctionCalling":{"disable":true},"abortSignal":{}},"temperature":0.25,"generationConfig":{"temperature":0.5}}"#
    );
    // The SDK appends a received chunk before streamGuard rejects duration.
    let mut guarded_config = config.clone();
    let mut guarded_provider = field(field(&guarded_config, "providers"), "p").clone();
    put(
        &mut guarded_provider,
        "retry",
        parse_json(r#"{"streamMaxRetries":0,"maxStreamingDurationMs":1}"#).unwrap(),
    );
    let mut providers = JsObject::new();
    providers.insert("p".into(), guarded_provider);
    put(&mut guarded_config, "providers", JsValue::Object(providers));
    let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let guarded_options = StreamOptions {
        clock: Arc::new(move || {
            (ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) * 10) as f64
        }),
        ..options.clone()
    };
    let partial = Scripted(std::sync::Mutex::new(vec![(
        200,
        vec![Some(
            b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"received\"}]}}]}\n\n",
        )],
    )]));
    stream_model(
        &partial,
        &request,
        &guarded_config,
        &guarded_options,
        &mut |_| {},
    )
    .await
    .unwrap();
    let settled = std::fs::read_to_string(dir.join("invocations.jsonl")).unwrap();
    let last = parse_json(settled.lines().last().unwrap()).unwrap();
    assert_eq!(field(&last, "outcome"), &JsValue::from("incomplete"));
    let received =
        String::from_utf16_lossy(field(&last, "responseBody").as_string().unwrap().units());
    assert_eq!(
        received,
        r#"{"candidates":[{"content":{"parts":[{"text":"received"}]}}],"sdkHttpResponse":{"headers":{}}}"#
    );
    assert_eq!(std::fs::read_dir(dir.join(".pending")).unwrap().count(), 0);
    // An in-flight parent cancellation follows TS throwIfGoogleAbort before
    // finishInvocation: retain its stage, without inventing a terminal row.
    let abort = AbortFlag::new();
    let cancelled = StreamOptions {
        abort: Some(abort.clone()),
        ..options.clone()
    };
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let never = Never(dropped.clone());
    let mut discard = |_: StreamOutput| {};
    let mut flight = Box::pin(stream_model(
        &never,
        &request,
        &config,
        &cancelled,
        &mut discard,
    ));
    poll_pending(flight.as_mut()).await;
    abort.abort();
    assert!(flight.await.unwrap_err().is_abort());
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        std::fs::read_to_string(dir.join("invocations.jsonl"))
            .unwrap()
            .lines()
            .count(),
        4
    );
    assert_eq!(std::fs::read_dir(dir.join(".pending")).unwrap().count(), 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dump_failure_error_projects_the_node_fs_error() {
    let provider = parse_json(r#"{"id":"dump-provider","protocol":"google"}"#).unwrap();
    let failure = DumpFailure::from_io(
        &std::io::Error::from_raw_os_error(libc::ENOENT),
        "open",
        Some(std::path::Path::new("/tmp/pilotdeck_request_1.json")),
    );
    let ModelStreamError::Provider(canonical) = dump_failure_error(&provider, &failure) else {
        panic!("a dump write failure is a provider error");
    };
    assert_eq!(
        canonical.get("code").and_then(JsValue::as_string),
        Some(&JsString::from("ENOENT"))
    );
    assert_eq!(canonical.get("retryable"), Some(&JsValue::Bool(false)));
    let raw = canonical.get("raw").expect("raw survives normalization");
    assert_eq!(
        raw.get("errno"),
        Some(&JsValue::Number(f64::from(-libc::ENOENT)))
    );
    assert_eq!(
        raw.get("code").and_then(JsValue::as_string),
        Some(&JsString::from("ENOENT"))
    );
    assert_eq!(
        raw.get("syscall").and_then(JsValue::as_string),
        Some(&JsString::from("open"))
    );
    assert_eq!(
        raw.get("path").and_then(JsValue::as_string),
        Some(&JsString::from("/tmp/pilotdeck_request_1.json"))
    );
}

/// Abort while waiting for response headers drops the in-flight exchange.
struct Never(Arc<std::sync::atomic::AtomicBool>);
impl ModelHttpTransport for Never {
    fn send(&self, _request: Request<Full<Bytes>>) -> TransportFuture {
        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let flag = Flag(self.0.clone());
        TransportFuture::io(async move {
            let _flag = flag;
            std::future::pending().await
        })
    }
}
