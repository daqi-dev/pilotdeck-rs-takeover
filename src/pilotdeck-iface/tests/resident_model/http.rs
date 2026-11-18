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
                    Err(_) => {
                        // WouldBlock (empty accept queue) is the steady state, and
                        // transient errors must be retried too: under parallel load a
                        // queued client connection can be reset before accept
                        // (ECONNABORTED) or fd pressure can surface EMFILE. Panicking
                        // here would drop the listener and stall the resident model
                        // turn. Same hardening as the default e2e mock in 3123ee09e.
                        std::thread::sleep(Duration::from_millis(5))
                    }
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
/// I/O failures on an individual connection (clone / header reads / body read /
/// response writes) drop that request instead of panicking: `handle` runs
/// inline in the accept thread, so a panic would tear down the whole listener
/// and stall every later model request.
fn handle(mut stream: TcpStream, requests: &Mutex<Vec<Value>>) {
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut length = None;
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        let Some((name, value)) = line.trim_end().split_once(':') else {
            return;
        };
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse::<usize>().ok();
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let Some(length) = length.filter(|length| *length <= 1024 * 1024) else {
        return;
    };
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
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
    if write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\nX-Fixture: real-stream\r\n\r\n", bytes.len())
        .and_then(|()| stream.flush())
        .is_err()
    {
        return;
    }
    // 拆开 JSON 和 SSE 分隔符，覆盖真实 socket 的增量输入而非一次性字符串适配。
    for part in bytes.chunks(13) {
        if stream
            .write_all(part)
            .and_then(|()| stream.flush())
            .is_err()
        {
            return;
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
