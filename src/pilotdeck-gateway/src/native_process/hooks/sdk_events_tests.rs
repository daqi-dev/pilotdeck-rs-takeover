use super::*;
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::{mpsc, oneshot},
};
struct HttpMock {
    url: String,
    received: mpsc::UnboundedReceiver<serde_json::Value>,
    release: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}
impl HttpMock {
    async fn start(count: usize, hold: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook?retained=1", listener.local_addr().unwrap());
        let (send, received) = mpsc::unbounded_channel();
        let (release, mut gate) = oneshot::channel();
        let task = tokio::spawn(async move {
            for _ in 0..count {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut first = String::new();
                stream.read_line(&mut first).await.unwrap();
                let path = first.split_whitespace().nth(1).unwrap().to_owned();
                let mut headers = serde_json::Map::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    stream.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (key, value) = line.trim_end().split_once(':').unwrap();
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                    if key.eq_ignore_ascii_case("content-type")
                        || key.eq_ignore_ascii_case("x-fixture")
                    {
                        headers.insert(key.to_ascii_lowercase(), value.trim().into());
                    }
                }
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                send.send(serde_json::json!({"path":path,"headers":headers,"body":serde_json::from_slice::<serde_json::Value>(&body).unwrap()})).unwrap();
                if hold {
                    let _ = (&mut gate).await;
                    break;
                }
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
            }
        });
        Self {
            url,
            received,
            release: Some(release),
            task,
        }
    }
    async fn next(&mut self) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(5), self.received.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
fn config(url: &str) -> JsValue {
    pilotdeck_protocol::parse_json(&serde_json::json!({"hooks":{"url":url,"headers":{"x-fixture":"value"},"events":{"ConfigChange":[{}],"FileChanged":[{"matcher":"a.txt"}]}}}).to_string()).unwrap()
}
fn saved(created: bool, name: &str) -> JsObject {
    let mut update = JsObject::new();
    update.insert("relativePath".into(), name.into());
    update.insert(
        "absolutePath".into(),
        JsValue::String(format!("/fixture/project/{name}").into()),
    );
    update.insert(
        "changeKind".into(),
        if created {
            "create".into()
        } else {
            "update".into()
        },
    );
    update
}
#[tokio::test]
async fn actual_http_sdk_events_match_fixed_ts_and_unregister_on_dispose() {
    let mut mock = HttpMock::start(3, false).await;
    let bus = Arc::new(SdkEventBus::default());
    let events = SdkSessionEvents::new(
        Some(&config(&mock.url)),
        bus.clone(),
        "session-fixture".into(),
        "/fixture/project".into(),
        &Default::default(),
    )
    .unwrap();
    let payload = pilotdeck_protocol::parse_json(
        r#"{"changedPaths":["agent.model"],"changeClasses":["runtime"]}"#,
    )
    .unwrap();
    bus.publish(payload.as_object().unwrap().clone());
    let first = mock.next().await;
    events.file_changed(&saved(true, "a.txt")).await.unwrap();
    let second = mock.next().await;
    events.file_changed(&saved(false, "a.txt")).await.unwrap();
    let third = mock.next().await;
    events.file_changed(&saved(true, "no.md")).await.unwrap();
    events.close().await.unwrap();
    assert_eq!(bus.len(), 0);
    bus.publish(payload.as_object().unwrap().clone());
    events.file_changed(&saved(true, "a.txt")).await.unwrap();
    mock.task.await.unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/h1-sdk-events/expected.json"
    ))
    .unwrap();
    assert_eq!(fixture["requests"].as_array().unwrap().len(), 3);
    for (actual, expected) in [first, second, third]
        .iter()
        .zip(fixture["requests"].as_array().unwrap())
    {
        let url = url::Url::parse(expected["url"].as_str().unwrap()).unwrap();
        assert_eq!(
            actual["path"],
            format!("{}?{}", url.path(), url.query().unwrap())
        );
        assert_eq!(actual["headers"], expected["headers"]);
        assert_eq!(actual["body"], expected["body"]);
    }
}
#[tokio::test]
async fn closing_session_owns_and_cancels_detached_config_callback() {
    let mut mock = HttpMock::start(1, true).await;
    let bus = Arc::new(SdkEventBus::default());
    let events = SdkSessionEvents::new(
        Some(&config(&mock.url)),
        bus.clone(),
        "held".into(),
        "/fixture/project".into(),
        &Default::default(),
    )
    .unwrap();
    bus.publish(JsObject::new());
    mock.next().await;
    tokio::time::timeout(Duration::from_secs(5), events.close())
        .await
        .expect("SDK hook owner drains without HTTP response")
        .unwrap();
    assert_eq!(bus.len(), 0);
    mock.release.take().unwrap().send(()).unwrap();
    mock.task.await.unwrap();
}
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn formal_native_submit_write_tool_invokes_sdk_file_changed_http_hook() {
    use crate::native_process::{
        LocalGateway, LocalOptions, tests::sdk_output_controls_tests::ControlProvider,
    };
    let scratch = Scratch(
        std::env::temp_dir().join(format!("pilotdeck-sdk-file-event-{}", uuid::Uuid::new_v4())),
    );
    let home = scratch.0.join("home");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir(&project).unwrap();
    // A non-General workspace must be registered in the real project catalog.
    // Seeding this marker follows assembly::register_project's storage contract.
    let chats = crate::session_chat_dir(&home, &project, &project.to_string_lossy());
    let storage = chats
        .parent()
        .expect("registered project storage directory");
    std::fs::create_dir_all(storage).unwrap();
    std::fs::write(storage.join(".cwd"), project.to_string_lossy().as_bytes()).unwrap();
    let provider = ControlProvider::start_with_arguments(
        "write_file",
        r#"{"file_path":"a.txt","content":"saved by native tool"}"#,
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mock = runtime.block_on(HttpMock::start(1, false));
    let config_path = project.join("pilotdeck.yaml");
    std::fs::write(&config_path,format!("agent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://{}/v1\n      apiKey: fixture-local-only\n      models: {{mock: {{capabilities: {{supportsToolUse: true}}}}}}\ntelemetry: {{enabled: false}}\n",provider.address)).unwrap();
    let env = [
        ("HOME", home.to_string_lossy().into_owned()),
        ("PILOT_HOME", home.to_string_lossy().into_owned()),
        (
            "PILOTDECK_CONFIG_PATH",
            config_path.to_string_lossy().into_owned(),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value))
    .collect();
    let gateway = LocalGateway::start(LocalOptions {
        pilot_home: home.clone(),
        project: project.clone(),
        os_home: Some(home),
        sidecar: scratch.0.join("absent-sidecar"),
        env,
        builtin_skills_root: None,
        builtin_plugins_root: None,
        telemetry_context: None,
    })
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let capture = events.clone();
    let submit = serde_json::json!({"sessionKey":"formal-sdk-file","channelKey":"test","message":"write a.txt","mode":"bypassPermissions","basePermissionMode":"bypassPermissions","canPrompt":false,"sdkSessionConfig":{"hooks":{"url":mock.url.clone(),"events":{"FileChanged":[{"matcher":"a.txt"}]}}}});
    gateway
        .submit_turn(
            pilotdeck_protocol::parse_json(&submit.to_string()).unwrap(),
            Arc::new(move |event| capture.lock().unwrap().push(event)),
        )
        .unwrap();
    gateway.shutdown(Duration::from_secs(10)).unwrap();
    let observed = events.lock().unwrap().clone();
    assert!(
        !observed
            .iter()
            .any(|event| event.get("type") == Some(&"error".into())),
        "formal SDK submit failed before the write: events={observed:?}; model requests={:?}",
        provider.requests.lock().unwrap()
    );
    assert!(
        !observed
            .iter()
            .any(|event| event.get("type") == Some(&"permission_request".into())),
        "explicit outer bypass mode must not leave a fixture permission waiter: {observed:?}"
    );
    assert!(
        observed.iter().any(
            |event| event.get("type") == Some(&"tool_call_finished".into())
                && event.get("toolCallId") == Some(&"native-call".into())
                && event.get("ok") == Some(&JsValue::Bool(true))
        ),
        "the real write tool must finish successfully before file/hook assertions: {observed:?}"
    );
    let model_requests = provider.requests.lock().unwrap().clone();
    assert!(
        model_requests
            .iter()
            .any(|request| request["messages"]
                .as_array()
                .is_some_and(
                    |messages| messages.iter().any(|message| message["role"] == "tool"
                        && message["tool_call_id"] == "native-call"
                        && message["content"].as_str() == Some("Created a.txt."))
                )),
        "formal SDK submit must execute the real write tool before continuation: events={observed:?}; model requests={model_requests:?}"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("a.txt")).unwrap(),
        "saved by native tool"
    );
    runtime.block_on(async move {
        let mut mock = mock;
        let request = mock.next().await;
        assert_eq!(request["body"]["hookEventName"], "FileChanged");
        assert_eq!(request["body"]["filePath"], "a.txt");
        assert_eq!(request["body"]["changeType"], "created");
        assert_eq!(request["body"]["sessionId"], "formal-sdk-file");
        assert_eq!(
            request["body"]["absolutePath"],
            project.join("a.txt").to_string_lossy().as_ref()
        );
        mock.task.await.unwrap();
    });
}
