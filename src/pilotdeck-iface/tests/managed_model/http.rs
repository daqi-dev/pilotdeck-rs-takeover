use crate::WAIT;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

pub struct LocalHttp {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    hold_second: Arc<AtomicBool>,
    release_second: Arc<AtomicBool>,
    held_second: Arc<AtomicBool>,
    accepted: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl LocalHttp {
    pub fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let hold_second = Arc::new(AtomicBool::new(false));
        let release_second = Arc::new(AtomicBool::new(false));
        let held_second = Arc::new(AtomicBool::new(false));
        let held = held_second.clone();
        let accepted = Arc::new(AtomicUsize::new(0));
        let observed_accepts = accepted.clone();
        let (observed, stopping) = (requests.clone(), stop.clone());
        let (hold, release) = (hold_second.clone(), release_second.clone());
        let thread = std::thread::spawn(move || {
            let mut workers = Vec::new();
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        observed_accepts.fetch_add(1, Ordering::SeqCst);
                        let (observed, hold, release, held, stopping) = (
                            observed.clone(),
                            hold.clone(),
                            release.clone(),
                            held.clone(),
                            stopping.clone(),
                        );
                        workers.push(std::thread::spawn(move || {
                            handle(stream, &observed, &hold, &release, &held, &stopping)
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("local HTTP accept: {error}"),
                }
            }
            for worker in workers {
                worker.join().expect("local HTTP handler panicked");
            }
        });
        Self {
            addr,
            requests,
            stop,
            hold_second,
            release_second,
            held_second,
            accepted,
            thread: Some(thread),
        }
    }
    pub fn hold_second_request(&self) {
        self.hold_second.store(true, Ordering::SeqCst);
    }
    pub fn second_is_held(&self) -> bool {
        self.held_second.load(Ordering::SeqCst) && !self.release_second.load(Ordering::SeqCst)
    }
    pub fn release_second_request(&self) {
        self.release_second.store(true, Ordering::SeqCst);
    }
    fn accepted_count(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
    pub fn requests_evidence(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.release_second.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
impl Drop for LocalHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.release_second.store(true, Ordering::SeqCst);
    }
}
fn read_line_until(
    reader: &mut BufReader<TcpStream>,
    line: &mut String,
    stop: &AtomicBool,
) -> std::io::Result<usize> {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(0);
        }
        match reader.read_line(line) {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && std::time::Instant::now() < deadline =>
            {
                continue;
            }
            result => return result,
        }
    }
}
fn handle(
    mut stream: TcpStream,
    requests: &Mutex<Vec<Value>>,
    hold: &AtomicBool,
    release: &AtomicBool,
    held: &AtomicBool,
    stop: &AtomicBool,
) {
    // An accepted idle socket must not block unrelated requests or shutdown.
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    let first_result = read_line_until(&mut reader, &mut first, stop);
    if stop.load(Ordering::SeqCst) {
        return;
    }
    match first_result {
        Ok(0) if first.is_empty() => return,
        Err(error)
            if first.is_empty()
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
        {
            return;
        }
        Ok(0) => panic!("HTTP request line closed early"),
        Err(error) => panic!("HTTP request line: {error}"),
        Ok(_) => {}
    }
    let mut raw_head = first.clone();
    let mut length = None;
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        let read = read_line_until(&mut reader, &mut line, stop);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        assert_ne!(read.unwrap(), 0, "HTTP request headers closed early");
        raw_head.push_str(&line);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.trim_end().split_once(':').expect("HTTP header");
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = length.expect("bounded request Content-Length");
    assert!(length <= 1024 * 1024);
    let mut body = vec![0; length];
    let deadline = std::time::Instant::now() + WAIT;
    let mut offset = 0;
    while offset < body.len() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match reader.read(&mut body[offset..]) {
            Ok(0) => panic!("HTTP request body closed early"),
            Ok(n) => offset += n,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && std::time::Instant::now() < deadline =>
            {
                continue;
            }
            Err(error) => panic!("HTTP request body: {error}"),
        }
    }
    let request_number = {
        let mut requests = requests.lock().unwrap();
        let observed_epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|v| v.as_millis())
            .unwrap_or_default();
        requests.push(json!({"observed_epoch_ms":observed_epoch_ms,"peer_addr":stream.peer_addr().ok().map(|v| v.to_string()),"local_addr":stream.local_addr().ok().map(|v| v.to_string()),"request_line":first.trim_end(),"raw_head":raw_head,"headers":headers,"body":serde_json::from_slice::<Value>(&body).unwrap(),"body_hex":body.iter().map(|byte| format!("{byte:02x}")).collect::<String>()}));
        requests.len()
    };
    let frames = [
        json!({"id":"fixture-response","model":"m","debugEcho":"fixture-secret","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello "},"finish_reason":null}]}),
        json!({"id":"fixture-response","model":"m","choices":[{"index":0,"delta":{"content":"fixture."},"finish_reason":null}]}),
        json!({"id":"fixture-response","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"id":"fixture-response","model":"m","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}),
    ];
    let bytes = format!(
        "{}data: [DONE]\n\n",
        frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>()
    )
    .into_bytes();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\nX-Fixture: real-stream\r\n\r\n", bytes.len()).unwrap();
    stream.flush().unwrap();
    // 拆开 JSON 和 SSE 分隔符，覆盖真实 socket 的增量输入而非一次性字符串适配。
    for (index, part) in bytes.chunks(3).enumerate() {
        if request_number == 2 && index == 1 && hold.load(Ordering::SeqCst) {
            held.store(true, Ordering::SeqCst);
            while !release.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        if stream
            .write_all(part)
            .and_then(|()| stream.flush())
            .is_err()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
pub fn check_request(request: &Value) -> Result<(), String> {
    if request["request_line"] != "POST /v1/chat/completions HTTP/1.1" {
        return Err(format!("wrong endpoint: {request}"));
    }
    let body = &request["body"];
    if body["model"] != "m" || body["stream"] != true || body["max_tokens"] != 123 {
        return Err(format!("model, stream or limit changed: {body}"));
    }
    let messages = body["messages"]
        .as_array()
        .ok_or("missing model messages")?;
    if !messages
        .iter()
        .any(|message| message["role"] == "user" && message["content"] == "Echo fixture input.")
    {
        return Err(format!("user input lost: {messages:?}"));
    }
    if !messages.iter().any(|message| {
        message["role"] == "system"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("Preserve the fixture instruction."))
    }) {
        return Err(format!("system prompt lost: {messages:?}"));
    }
    let authorization: Vec<_> = request["headers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|pair| pair[0].as_str() == Some("authorization"))
        .collect();
    if authorization.len() != 1 || authorization[0][1].as_str() != Some("Bearer fixture-secret") {
        return Err(
            "trusted fixture credential did not reach the approved loopback provider".into(),
        );
    }
    if request["headers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|pair| matches!(pair[0].as_str(), Some("x-api-key" | "x-goog-api-key")))
    {
        return Err("unexpected credential family".into());
    }
    Ok(())
}

pub fn verify_idle_connection_does_not_block_another_request_or_shutdown() {
    let server = LocalHttp::start();
    let _idle = TcpStream::connect(server.addr).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while server.accepted_count() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "idle socket was not accepted"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut active = TcpStream::connect(server.addr).unwrap();
    active
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    active
        .write_all(b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}")
        .unwrap();
    let mut line = String::new();
    BufReader::new(active).read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
    assert_eq!(server.requests().len(), 1);
    let before = std::time::Instant::now();
    server.shutdown();
    assert!(
        before.elapsed() < Duration::from_secs(3),
        "idle connection delayed shutdown"
    );
}
