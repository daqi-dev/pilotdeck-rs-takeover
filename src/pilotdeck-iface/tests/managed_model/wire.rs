use crate::WAIT;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub struct Wire {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    id: u64,
}
impl Wire {
    pub fn new(addr: SocketAddr) -> Self {
        Self::connect(addr, false)
    }
    pub fn new_with_local_resource_facts(addr: SocketAddr) -> Self {
        Self::connect(addr, true)
    }
    fn connect(addr: SocketAddr, facts: bool) -> Self {
        let writer = TcpStream::connect_timeout(&addr, WAIT).expect("resident TCP");
        writer.set_read_timeout(Some(WAIT)).unwrap();
        writer.set_write_timeout(Some(WAIT)).unwrap();
        let mut wire = Self {
            reader: BufReader::new(writer.try_clone().unwrap()),
            writer,
            id: 0,
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
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.writer,
            "{}",
            json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
        )
        .unwrap();
        self.writer.flush().unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(
                self.reader.read_line(&mut line).unwrap(),
                0,
                "{method}: connection closed"
            );
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
        writeln!(
            self.writer,
            "{}",
            json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
        )
        .unwrap();
        self.writer.flush().unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(
                self.reader.read_line(&mut line).unwrap(),
                0,
                "error response connection closed"
            );
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
            Ok(_) => Some(serde_json::from_str(&line).expect("public stream frame")),
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
        (
            serde_json::from_str(&line).expect("valid public stream frame"),
            line,
        )
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
