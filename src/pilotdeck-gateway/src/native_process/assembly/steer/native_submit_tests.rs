use crate::native_process::{LocalGateway, LocalOptions};
use pilotdeck_protocol::{parse_json, stringify_json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

fn value(v: serde_json::Value) -> pilotdeck_protocol::JsValue {
    parse_json(&v.to_string()).unwrap()
}

fn run_case(case: &str) {
    let root = std::env::temp_dir().join(format!("pdrs-steer-{}", uuid::Uuid::new_v4()));
    let home = root.join("home");
    let fallback = root.join("fallback");
    let project = if case == "different" {
        root.join("project")
    } else {
        fallback.clone()
    };
    for path in [&home, &fallback, &project] {
        std::fs::create_dir_all(path).unwrap();
    }
    for registered in [&fallback, &project] {
        let chats = crate::session_chat_dir(&home, registered, &registered.to_string_lossy());
        let directory = chats.parent().unwrap();
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(
            directory.join(".cwd"),
            registered.to_string_lossy().as_bytes(),
        )
        .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let received = Arc::new(Mutex::new(Vec::new()));
    let captured = received.clone();
    let server = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(40);
        while std::time::Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0u8; 8192];
            let (offset, length) = loop {
                let n = match stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(error)
                        if bytes.is_empty()
                            && matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                    {
                        break (0, 0);
                    }
                    Err(error) => panic!("mock request header: {error}"),
                };
                if n == 0 {
                    break (0, 0);
                }
                bytes.extend_from_slice(&buf[..n]);
                if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let h = String::from_utf8_lossy(&bytes[..i]);
                    let len = h
                        .lines()
                        .find_map(|l| {
                            l.split_once(':')
                                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    break (i + 4, len);
                }
            };
            if length == 0 {
                continue;
            }
            while bytes.len() < offset + length {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..n]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[offset..offset + length]).unwrap();
            if body["stream"] != true {
                let data = r#"{"id":"mock","object":"chat.completion","model":"mock","choices":[{"index":0,"message":{"role":"assistant","content":"title"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",data.len(),data).unwrap();
                continue;
            }
            let count = {
                let mut items = captured.lock().unwrap();
                items.push(body);
                items.len()
            };
            if count == 1 {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(20)).unwrap();
            }
            let data = "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n";
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",data.len(),data).unwrap();
            if count == 2 {
                break;
            }
        }
    });
    let config = home.join("pilotdeck.yaml");
    std::fs::write(&config, format!("schemaVersion: 1\nagent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://{address}/v1\n      apiKey: fixture-only\n      models: {{mock: {{}}}}\ntelemetry: {{enabled: false}}\n")).unwrap();
    let env = [
        ("HOME", home.to_string_lossy().into_owned()),
        ("PILOT_HOME", home.to_string_lossy().into_owned()),
        (
            "PILOTDECK_CONFIG_PATH",
            config.to_string_lossy().into_owned(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect();
    let gateway = Arc::new(
        LocalGateway::start(LocalOptions {
            pilot_home: home.clone(),
            project: fallback,
            os_home: Some(home),
            sidecar: root.join("none"),
            env,
            builtin_skills_root: None,
            builtin_plugins_root: None,
            telemetry_context: None,
        })
        .unwrap(),
    );
    let submitter = gateway.clone();
    let project_text = project.to_string_lossy().into_owned();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let reload_generation = case == "same";
    let reload_project = project_text.clone();
    let case = case.to_owned();
    let turn = std::thread::spawn(move || {
        let mut params = serde_json::json!({"sessionKey":"steer-test", "runId":"run-1", "channelKey":"test", "message":"first"});
        if case != "fallback" {
            params["projectKey"] = project_text.into();
        }
        let result = submitter.submit_turn(
            value(params),
            Arc::new(move |event| observed.lock().unwrap().push(event)),
        );
        let _ = done_tx.send(result.clone());
        result
    });
    if let Err(error) = entered_rx.recv_timeout(Duration::from_secs(30)) {
        panic!(
            "provider not entered: {error}; submit={:?}; events={:?}",
            done_rx.try_recv(),
            events.lock().unwrap()
        );
    }
    if reload_generation {
        let reload = gateway
            .request(
                "reload_extensions",
                value(serde_json::json!({"projectKey": reload_project})),
            )
            .expect("reload while turn is active");
        assert!(
            stringify_json(&reload)
                .unwrap()
                .unwrap()
                .contains("\"reloaded\":true")
        );
    }
    let response = gateway.request("steer_turn", value(serde_json::json!({"sessionKey":"steer-test", "runId":"run-1", "itemId":"item-1", "message":"steer-marker"}))).unwrap();
    let rendered = stringify_json(&response).unwrap().unwrap();
    let mismatch = gateway.request("steer_turn", value(serde_json::json!({"sessionKey":"steer-test", "runId":"old-run", "itemId":"stale", "message":"must-not-arrive"}))).unwrap();
    assert!(
        stringify_json(&mismatch)
            .unwrap()
            .unwrap()
            .contains("turn_mismatch")
    );
    release_tx.send(()).unwrap();
    let outcome = turn.join().unwrap();
    server.join().unwrap();
    let ended = gateway.request("steer_turn", value(serde_json::json!({"sessionKey":"steer-test", "runId":"run-1", "itemId":"late", "message":"must-not-arrive"}))).unwrap();
    assert!(
        stringify_json(&ended)
            .unwrap()
            .unwrap()
            .contains("no_active_turn")
    );
    gateway.shutdown(Duration::from_secs(10)).unwrap();
    assert!(
        rendered.contains("\"accepted\":true"),
        "{rendered}; turn={outcome:?}"
    );
    assert!(outcome.is_ok(), "turn failed: {outcome:?}");
    let emitted = events.lock().unwrap();
    assert_eq!(
        emitted
            .iter()
            .filter(|e| e.get("type") == Some(&pilotdeck_protocol::JsValue::from("turn_completed")))
            .count(),
        1,
        "{emitted:?}"
    );
    assert!(
        !emitted
            .iter()
            .any(|e| e.get("type") == Some(&pilotdeck_protocol::JsValue::from("error"))),
        "{emitted:?}"
    );
    drop(emitted);
    let requests = received.lock().unwrap();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[1].to_string().matches("steer-marker").count(), 1);
    assert!(!requests[1].to_string().contains("must-not-arrive"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn native_submit_steer_targets_session_project_and_consumes_once() {
    for case in ["fallback", "same", "different"] {
        run_case(case);
    }
}
