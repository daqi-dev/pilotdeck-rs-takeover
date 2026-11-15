use crate::WAIT;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub struct LocalHttp {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl LocalHttp {
    pub fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (observed, stopping) = (requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => handle(stream, &observed),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("local HTTP accept: {error}"),
                }
            }
        });
        Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
        }
    }
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
impl Drop for LocalHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
fn handle(mut stream: TcpStream, requests: &Mutex<Vec<Value>>) {
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut length = None;
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "HTTP request headers closed early"
        );
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
    reader.read_exact(&mut body).unwrap();
    requests.lock().unwrap().push(json!({"request_line":first.trim_end(),"headers":headers,"body":serde_json::from_slice::<Value>(&body).unwrap()}));
    let frames = [
        json!({"id":"fixture-response","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello "},"finish_reason":null}]}),
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
    for part in bytes.chunks(13) {
        stream.write_all(part).unwrap();
        stream.flush().unwrap();
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
    if request["headers"].as_array().unwrap().iter().any(|pair| {
        matches!(
            pair[0].as_str(),
            Some("authorization" | "x-api-key" | "x-goog-api-key")
        )
    }) {
        return Err("unexpected credentials in isolated fixture".into());
    }
    Ok(())
}
