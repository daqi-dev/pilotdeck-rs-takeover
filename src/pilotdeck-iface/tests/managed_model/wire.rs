use crate::WAIT;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};
use std::sync::{Arc, Mutex};

pub struct Wire {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    id: u64,
    trace: Option<(String, Arc<Mutex<Vec<Value>>>)>,
}
impl Wire {
    pub fn new(addr: SocketAddr) -> Self {
        Self::connect(addr, false, None)
    }
    pub fn new_with_local_resource_facts(addr: SocketAddr) -> Self {
        Self::connect(addr, true, None)
    }
    pub fn new_traced(addr: SocketAddr, label: &str, trace: Arc<Mutex<Vec<Value>>>) -> Self {
        Self::connect(addr, true, Some((label.to_owned(), trace)))
    }
    fn connect(addr: SocketAddr, facts: bool, trace: Option<(String, Arc<Mutex<Vec<Value>>>)>) -> Self {
        let writer = TcpStream::connect_timeout(&addr, WAIT).expect("resident TCP");
        writer.set_read_timeout(Some(WAIT)).unwrap();
        writer.set_write_timeout(Some(WAIT)).unwrap();
        let mut wire = Self {
            reader: BufReader::new(writer.try_clone().unwrap()),
            writer,
            id: 0,
            trace,
        };
        let mut features = vec!["model-session/1.0"];
        if facts {
            features.push(agentcpm_iface::model_session::MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1);
        }
        let init = wire.call(
            "initialize",
            json!({"abi_version":2,"api_version":agentcpm_iface::API_VERSION,"features":features}),
        );
        if facts {
            assert!(
                init["supported_features"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value.as_str()
                        == Some(
                            agentcpm_iface::model_session::MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1
                        ))
            );
        }
        assert_eq!(init["abi_version"], 2);
        assert_eq!(
            init["server_capabilities"]["scheduler"]["protocol"],
            agentcpm_scheduler_protocol::PROTOCOL
        );
        wire
    }
    fn trace_line(&self, direction: &str, raw: &str) {
        if let Some((label, trace)) = &self.trace {
            let epoch_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_millis()).unwrap_or_default();
            trace.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(json!({
                "connection":label, "direction":direction, "observed_epoch_ms":epoch_ms,
                "raw":raw, "frame":serde_json::from_str::<Value>(raw).ok(),
            }));
        }
    }
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let frame = json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params});
        let raw = self.trace.as_ref().map(|_| format!("{frame}\n"));
        if let Some(raw) = &raw { self.trace_line("send_attempt", raw); }
        writeln!(self.writer, "{frame}").unwrap();
        self.writer.flush().unwrap();
        if let Some(raw) = &raw { self.trace_line("send_flushed", raw); }
        loop {
            let mut line = String::new();
            assert_ne!(
                self.reader.read_line(&mut line).unwrap(),
                0,
                "{method}: connection closed"
            );
            self.trace_line("receive", &line);
            let frame: Value = serde_json::from_str(&line).expect("JSON RPC reply");
            if frame["id"] != self.id {
                continue;
            }
            assert!(frame.get("error").is_none(), "{method}: {frame}");
            assert!(
                frame.get("result").is_some(),
                "missing method result: {frame}"
            );
            return frame["result"].clone();
        }
    }
    pub fn expect_error(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let frame = json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params});
        let raw = self.trace.as_ref().map(|_| format!("{frame}\n"));
        if let Some(raw) = &raw { self.trace_line("send_attempt", raw); }
        writeln!(self.writer, "{frame}").unwrap();
        self.writer.flush().unwrap();
        if let Some(raw) = &raw { self.trace_line("send_flushed", raw); }
        loop {
            let mut line = String::new();
            assert_ne!(
                self.reader.read_line(&mut line).unwrap(),
                0,
                "error response connection closed"
            );
            self.trace_line("receive", &line);
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["id"] == self.id {
                assert!(
                    frame.get("error").is_some(),
                    "expected real failure: {frame}"
                );
                return frame["error"].clone();
            }
        }
    }
    pub fn try_stream_frame(&mut self, timeout: Duration) -> Option<Value> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
            .unwrap();
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => panic!("stream closed while observing credit"),
            Ok(_) => {
                self.trace_line("receive", &line);
                let frame = serde_json::from_str(&line).expect("public stream frame");
                Some(frame)
            },
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && line.is_empty() =>
            {
                None
            }
            Err(error) => panic!("stream read failed: {error}"),
        }
    }
    pub fn next_stream_frame(&mut self, deadline: Instant) -> (Value, String) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "stream deadline expired");
        self.reader
            .get_ref()
            .set_read_timeout(Some(remaining))
            .unwrap();
        let mut line = String::new();
        assert_ne!(
            self.reader.read_line(&mut line).unwrap(),
            0,
            "stream closed before expected fact"
        );
        self.trace_line("receive", &line);
        let frame = serde_json::from_str(&line).expect("valid public stream frame");
        (frame, line)
    }
    pub fn wait(&mut self, method: &str, params: Value) -> Value {
        let end = Instant::now() + WAIT;
        loop {
            let view = self.call(method, params.clone());
            if matches!(
                view["state"].as_str(),
                Some("completed" | "failed" | "cancelled" | "unknown")
            ) {
                return view;
            }
            assert!(Instant::now() < end, "execution timed out: {view}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
