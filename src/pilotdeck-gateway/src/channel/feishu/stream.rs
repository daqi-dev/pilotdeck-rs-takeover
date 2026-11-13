//! Native Lark long connection. Protocol pinned by the fixed SDK local oracle.
use super::{ChannelError, FeishuOptions, Runtime, parse::Clock, wire::Frame};
use crate::{
    channel::{network::ChannelNetwork, ws_transport::ChannelWebSocket},
    runtime::model::http::ModelHttpTransport,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};
use tokio_tungstenite::tungstenite::Message;

pub(super) struct StreamOwner {
    stop: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
    receipt: watch::Sender<Option<Result<(), ChannelError>>>,
}
impl StreamOwner {
    pub fn start(
        runtime: Arc<Runtime>,
        options: FeishuOptions,
        network: Arc<ChannelNetwork>,
    ) -> Arc<Self> {
        let (stop, mut stopped) = watch::channel(false);
        let (receipt, _) = watch::channel(None);
        let owner = Arc::new(Self {
            stop,
            task: Mutex::new(None),
            receipt,
        });
        let task = tokio::spawn(async move {
            let work = run(runtime.clone(), options, network);
            tokio::select! { biased; _ = stopped.changed() => {}, result = work => {
                if let Err(error) = result {runtime.log("error", &format!("feishu: stream connection error: {error}"));}
            }}
        });
        *owner.task.lock().expect("feishu stream task") = Some(task);
        owner
    }
    /// Admission closes before returning the receipt. Cleanup survives a dropped caller.
    pub fn stop(self: &Arc<Self>) -> super::BoxFuture<Result<(), ChannelError>> {
        self.stop.send_replace(true);
        let mut receipt = self.receipt.subscribe();
        if let Some(task) = self.task.lock().expect("feishu stream task").take() {
            let owner = self.clone();
            tokio::spawn(async move {
                let result = task
                    .await
                    .map_err(|e| ChannelError::error(format!("feishu: stream task failed: {e}")));
                owner.receipt.send_replace(Some(result));
            });
        }
        Box::pin(async move {
            loop {
                if let Some(result) = receipt.borrow().clone() {
                    return result;
                }
                receipt
                    .changed()
                    .await
                    .map_err(|_| ChannelError::error("feishu: stream cleanup receipt lost"))?;
            }
        })
    }
}
#[derive(Clone)]
struct Config {
    ping: Duration,
    interval: Duration,
    nonce: Duration,
    count: i64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            ping: Duration::from_secs(120),
            interval: Duration::from_secs(120),
            nonce: Duration::from_secs(30),
            count: -1,
        }
    }
}
impl Config {
    fn update(&mut self, v: &Value) {
        fn duration(v: &Value, key: &str, old: Duration) -> Duration {
            v.get(key)
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| Duration::from_secs_f64(n.min(86400.0)))
                .unwrap_or(old)
        }
        self.ping = duration(v, "PingInterval", self.ping);
        self.interval = duration(v, "ReconnectInterval", self.interval);
        self.nonce = duration(v, "ReconnectNonce", self.nonce);
        self.count = v
            .get("ReconnectCount")
            .and_then(Value::as_i64)
            .unwrap_or(self.count);
    }
}
async fn connect(
    options: &FeishuOptions,
    network: &ChannelNetwork,
    config: &mut Config,
) -> Result<(ChannelWebSocket, u64), String> {
    let body = json!({"AppID":options.app_id,"AppSecret":options.app_secret}).to_string();
    let request = Request::post(format!(
        "{}/callback/ws/endpoint",
        options.base_url.trim_end_matches('/')
    ))
    .header("Content-Type", "application/json")
    .header("locale", "zh")
    .header("User-Agent", "oapi-node-sdk/1.65.0")
    .body(Full::new(Bytes::from(body)))
    .map_err(|e| e.to_string())?;
    let response = tokio::time::timeout(Duration::from_secs(15), async {
        let response = network
            .send(request)
            .await
            .map_err(|failure| failure.message)?;
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "pullConnectConfig timed out".to_owned())??;
    let code = response.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        return Err(format!(
            "pullConnectConfig failed: code={code}, msg={}",
            response.get("msg").and_then(Value::as_str).unwrap_or("")
        ));
    }
    let data = &response["data"];
    config.update(&data["ClientConfig"]);
    let url = data["URL"].as_str().ok_or("missing stream URL")?;
    let service = url
        .split('?')
        .nth(1)
        .unwrap_or("")
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == "service_id")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let (socket, _) = network.connect_ws(url).await.map_err(|e| e.to_string())?;
    Ok((socket, service))
}
async fn run(
    runtime: Arc<Runtime>,
    options: FeishuOptions,
    network: Arc<ChannelNetwork>,
) -> Result<(), String> {
    let mut config = Config::default();
    let mut retries = 0i64;
    loop {
        match connect(&options, &network, &mut config).await {
            Ok((mut socket, service)) => {
                retries = 0;
                if let Err(error) = communicate(&runtime, &mut socket, service, &mut config).await {
                    runtime.log("error", &format!("feishu: stream disconnected: {error}"));
                }
            }
            Err(error) => {
                runtime.log("error", &format!("feishu: {error}"));
                if error.starts_with("pullConnectConfig failed:")
                    && !error.contains("code=1000040343,")
                {
                    return Err(error);
                }
            }
        }
        if retries > 0 && config.count >= 0 && retries >= config.count {
            return Err("WebSocket reconnect exhausted".into());
        }
        retries += 1;
        // SDK first delays by a bounded random nonce, then each retry interval.
        // The local nonce is sampled without a global RNG or credential state.
        let jitter = if config.nonce.is_zero() {
            Duration::ZERO
        } else {
            Duration::from_millis(
                (super::SystemClock.now_ms() as u64) % ((config.nonce.as_millis() as u64).max(1)),
            )
        };
        tokio::time::sleep(if retries == 1 {
            jitter
        } else {
            config.interval
        })
        .await;
    }
}
struct Partial {
    created: Instant,
    parts: Vec<Option<Vec<u8>>>,
}
async fn communicate(
    runtime: &Arc<Runtime>,
    socket: &mut ChannelWebSocket,
    service: u64,
    config: &mut Config,
) -> Result<(), String> {
    let mut partials: HashMap<String, Partial> = HashMap::new();
    let mut next_ping = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _=tokio::time::sleep_until(next_ping)=>{
                let ping=Frame{service,headers:vec![("type".into(),"ping".into())],..Default::default()};
                socket.send(Message::Binary(ping.encode().into())).await.map_err(|e|e.to_string())?;
                next_ping=tokio::time::Instant::now()+config.ping.max(Duration::from_millis(1));
            }
            message=socket.next()=>{
                let message=message.ok_or("stream peer closed")?.map_err(|e|e.to_string())?;
                let bytes=match message {Message::Binary(b)=>b,Message::Ping(b)=>{socket.send(Message::Pong(b)).await.map_err(|e|e.to_string())?;continue},Message::Close(_)=>return Err("stream peer closed".into()),_=>continue};
                let mut frame=Frame::decode(&bytes)?;
                let dispatch_started=Instant::now();
                if frame.method==0 {
                    if frame.header("type")==Some("pong") && !frame.payload.is_empty(){let v:Value=serde_json::from_slice(&frame.payload).map_err(|e|e.to_string())?;config.update(&v);}
                    continue;
                }
                if frame.method!=1 || frame.header("type")!=Some("event"){continue}
                let key=frame.header("message_id").unwrap_or("").to_owned();
                let sum=frame.header("sum").and_then(|n|n.parse::<usize>().ok()).unwrap_or(0);
                let seq=frame.header("seq").and_then(|n|n.parse::<usize>().ok()).unwrap_or(usize::MAX);
                if sum==0||sum>4096||seq>=sum{return Err("invalid stream event fragment index".into())}
                partials.retain(|_,p|p.created.elapsed()<=Duration::from_secs(10));
                let pending=partials.entry(key.clone()).or_insert_with(||Partial{created:Instant::now(),parts:vec![None;sum]});
                if pending.parts.len()!=sum {return Err("inconsistent stream event fragment count".into())}
                pending.parts[seq]=Some(frame.payload.clone());
                if pending.parts.iter().any(Option::is_none){continue}
                let payload=partials.remove(&key).expect("completed fragments").parts.into_iter().flatten().flatten().collect::<Vec<_>>();
                let event:Value=serde_json::from_slice(&payload).map_err(|e|e.to_string())?;
                let event_type=event.pointer("/header/event_type").or_else(||event.get("type")).and_then(Value::as_str).unwrap_or("");
                let ack=if event_type=="im.message.receive_v1" {runtime.handle_stream_event(&event);json!({"code":200})}else{json!({"code":200,"data":STANDARD.encode(serde_json::to_vec(&format!("no {event_type} event handle")).map_err(|e|e.to_string())?)})};
                frame.payload=serde_json::to_vec(&ack).map_err(|e|e.to_string())?;frame.headers.push(("biz_rt".into(),(-(dispatch_started.elapsed().as_millis() as i128)).to_string()));
                socket.send(Message::Binary(frame.encode().into())).await.map_err(|e|e.to_string())?;
            }
        }
    }
}
