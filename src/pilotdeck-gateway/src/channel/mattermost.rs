//! Native Mattermost HTTP/WebSocket adapter, pinned to PilotDeck cc7c4e6.
//! REST paths intentionally do not acquire an implicit `/api/v4` prefix.
use super::{
    http_client::encode_uri_component,
    im_interaction::{JsInteractionPort, ResponderRef, js_helper_interaction},
    loader::ChannelLoaderMounts,
    network::ChannelNetwork,
    protocol::{
        BoxFuture, ChannelAdapter, ChannelError, ChannelHandle, ChannelKey, ChannelStartDeps,
        TurnEventStream, TurnRequest,
    },
    sms::{self, concat_units, js_string_of, trim_units, truthy},
};
use crate::runtime::model::http::fetch::fetch;
use bytes::Bytes;
use futures_util::{FutureExt, SinkExt, StreamExt};
use hyper::{HeaderMap, Method, header};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json, stringify_json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::watch,
    task::{JoinHandle, JoinSet},
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const RECONNECT: Duration = Duration::from_secs(4);
const MAX_MESSAGE: usize = 16383;
// The formal CLI installs its undici dispatcher with 600000 ms headers/body timeouts.
const REST_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Default)]
pub struct MattermostOptions {
    pub token: Option<JsString>,
    pub server_url: Option<JsValue>,
    pub team_id: Option<JsValue>,
}
impl MattermostOptions {
    fn from_object(options: &JsObject) -> Self {
        let field = |key: &str, env: &str| {
            options
                .get(key)
                .filter(|value| !sms::nullish(value))
                .cloned()
                .or_else(|| {
                    std::env::var(env)
                        .ok()
                        .map(|value| JsValue::String(value.into()))
                })
        };
        Self {
            token: field("token", "MATTERMOST_TOKEN")
                .as_ref()
                .map(js_string_of),
            server_url: field("serverUrl", "MATTERMOST_URL"),
            team_id: field("teamId", "MATTERMOST_TEAM_ID"),
        }
    }
}

#[derive(Default)]
struct Mapper {
    active: HashMap<JsString, JsString>,
}
impl Mapper {
    fn resolve(
        &mut self,
        chat: &JsString,
        text: &JsString,
        uuid: impl FnOnce() -> String,
    ) -> (JsString, bool, JsString) {
        let text = trim_units(text.units());
        let new = text == [47, 110, 101, 119] || text.starts_with(&[47, 110, 101, 119, 32]);
        let session = if new {
            let session = concat_units(&[
                &"mattermost:chat=".encode_utf16().collect::<Vec<_>>(),
                chat.units(),
                &format!(":s_{}", uuid()).encode_utf16().collect::<Vec<_>>(),
            ]);
            self.active.insert(chat.clone(), session.clone());
            session
        } else {
            self.active.get(chat).cloned().unwrap_or_else(|| {
                concat_units(&[
                    &"mattermost:chat=".encode_utf16().collect::<Vec<_>>(),
                    chat.units(),
                    &":general".encode_utf16().collect::<Vec<_>>(),
                ])
            })
        };
        (
            session,
            new,
            JsString::from_utf16(if new { trim_units(&text[4..]) } else { text }.to_vec()),
        )
    }
}

pub struct MattermostChannel {
    key: ChannelKey,
    options: MattermostOptions,
    interaction: Arc<dyn JsInteractionPort>,
    mapper: Arc<Mutex<Mapper>>,
    network: Arc<ChannelNetwork>,
}
impl MattermostChannel {
    pub fn new(options: MattermostOptions, interaction: Arc<dyn JsInteractionPort>) -> Self {
        Self::new_with_network(options, interaction, Arc::new(ChannelNetwork::new(None)))
    }
    pub(crate) fn new_with_network(
        options: MattermostOptions,
        interaction: Arc<dyn JsInteractionPort>,
        network: Arc<ChannelNetwork>,
    ) -> Self {
        Self {
            key: ChannelKey::new("mattermost"),
            options,
            interaction,
            mapper: Arc::new(Mutex::new(Mapper::default())),
            network,
        }
    }
}
type RetainedStream = Arc<tokio::sync::Mutex<Box<dyn TurnEventStream>>>;
struct Shared {
    deps: ChannelStartDeps,
    interaction: Arc<dyn JsInteractionPort>,
    mapper: Arc<Mutex<Mapper>>,
    active: Arc<Mutex<std::collections::HashSet<JsString>>>,
    chats: Mutex<std::collections::HashSet<JsString>>,
    api: String,
    token: JsString,
    bot: JsValue,
    network: Arc<ChannelNetwork>,
    stop: watch::Receiver<bool>,
    stop_errors: Arc<Mutex<Vec<ChannelError>>>,
    failed_streams: Mutex<Vec<(RetainedStream, ChannelError)>>,
}
impl Shared {
    fn clear_interactions(&self) {
        for chat in self.chats.lock().expect("Mattermost chats").iter() {
            self.interaction.clear_chat_for_shutdown(chat);
        }
    }
    fn record_task_error(&self, error: tokio::task::JoinError) {
        self.log("error", &format!("mattermost: handler failed: {error}"));
        self.stop_errors
            .lock()
            .expect("Mattermost stop errors")
            .push(ChannelError::error(error.to_string()));
    }
    fn log(&self, level: &str, message: &str) {
        if let Some(log) = &self.deps.log {
            log(level, message)
        }
    }
    async fn rest(
        &self,
        method: &str,
        path: &str,
        body: Option<JsValue>,
    ) -> Result<JsValue, ChannelError> {
        let body = body
            .as_ref()
            .map(|body| {
                stringify_json(body)
                    .expect("Mattermost JSON body")
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let mut authorization = b"Bearer ".to_vec();
        for (index, &unit) in self.token.units().iter().enumerate() {
            if unit > 255 {
                return Err(ChannelError::Plain(format!(
                    "TypeError: Cannot convert argument to a ByteString because the character at index {} has a value of {unit} which is greater than 255.",
                    index + 7
                )));
            }
            authorization.push(unit as u8);
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            hyper::header::HeaderValue::from_bytes(&authorization)
                .map_err(|error| ChannelError::Plain(format!("TypeError: {error}")))?,
        );
        headers.insert(
            header::CONTENT_TYPE,
            hyper::header::HeaderValue::from_static("application/json"),
        );
        let url = url::Url::parse(&format!("{}{path}", self.api))
            .map_err(|error| ChannelError::Plain(format!("TypeError: {error}")))?;
        let response = fetch(
            self.network.as_ref(),
            url,
            Method::from_bytes(method.as_bytes()).expect("Mattermost method"),
            headers,
            Bytes::from(body),
        )
        .await
        .map_err(|error| ChannelError::Plain(format!("TypeError: {}", error.message)))?;
        let status = response.status();
        let bytes = crate::fetch_redirect::read_text_body(
            response.into_body(),
            crate::fetch_redirect::MAX_STRING_LENGTH,
            Some(REST_TIMEOUT),
        )
        .await
        .map_err(|error| {
            ChannelError::Plain(match error {
                crate::fetch_redirect::BodyError::TooLong => {
                    format!("Error: {}", crate::fetch_redirect::STRING_TOO_LONG)
                }
                crate::fetch_redirect::BodyError::Terminated => "TypeError: terminated".into(),
                crate::fetch_redirect::BodyError::Aborted => {
                    "AbortError: The operation was aborted.".into()
                }
            })
        })?;
        let text =
            String::from_utf8_lossy(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes));
        if !status.is_success() {
            let units = text.encode_utf16().take(400).collect::<Vec<_>>();
            return Err(ChannelError::error(format!(
                "HTTP {}: {}",
                status.as_u16(),
                String::from_utf16_lossy(&units)
            )));
        }
        if text.is_empty() {
            return Ok(JsValue::Object(JsObject::new()));
        }
        Ok(parse_json(&text).unwrap_or_else(|_| JsValue::String(text.into_owned().into())))
    }
    async fn reply(&self, ctx: &Context, text: &JsString) -> bool {
        let mut delivered = true;
        for chunk in chunk_text(text, MAX_MESSAGE) {
            let mut body = JsObject::new();
            body.insert("channel_id".into(), JsValue::String(ctx.channel.clone()));
            body.insert("message".into(), JsValue::String(chunk));
            if let Some(root) = &ctx.root {
                body.insert("root_id".into(), JsValue::String(root.clone()));
            }
            let mut stop = self.stop.clone();
            let response = tokio::select! {biased; _=stopped(&mut stop)=>return false, result=self.rest("POST", "/posts", Some(JsValue::Object(body)))=>result};
            if let Err(error) = response {
                self.log("error", &format!("mattermost: post failed: {error}"));
                delivered = false;
            }
        }
        delivered
    }
}
#[derive(Clone)]
struct Context {
    channel: JsString,
    root: Option<JsString>,
    chat: JsString,
}
struct Handle {
    stop: watch::Sender<bool>,
    worker: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>,
    log: Option<super::protocol::ChannelLog>,
    shared: Arc<Shared>,
}
impl ChannelHandle for Handle {
    fn stop(&self, reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        if let Some(log) = &self.log {
            log("info", &format!("mattermost: stopping ({reason})"));
        }
        self.stop.send_replace(true);
        let worker = self.worker.clone();
        let shared = self.shared.clone();
        let reason = JsString::from(reason);
        Box::pin(async move {
            let mut owner = worker.lock().await;
            if let Some(worker) = owner.as_mut() {
                let outcome = worker.await;
                owner.take();
                if let Err(error) = outcome {
                    shared
                        .stop_errors
                        .lock()
                        .expect("Mattermost stop errors")
                        .push(ChannelError::error(error.to_string()));
                }
            } else {
                let streams = shared
                    .failed_streams
                    .lock()
                    .expect("Mattermost failed streams")
                    .clone();
                for (stream, _) in streams {
                    let outcome = stream.lock().await.cancel_and_drain(reason.clone()).await;
                    let mut failed = shared
                        .failed_streams
                        .lock()
                        .expect("Mattermost failed streams");
                    if let Some(index) = failed
                        .iter()
                        .position(|(saved, _)| Arc::ptr_eq(saved, &stream))
                    {
                        match outcome {
                            Ok(()) => {
                                failed.remove(index);
                            }
                            Err(error) => failed[index].1 = error,
                        }
                    }
                }
            }
            let mut failures = shared
                .stop_errors
                .lock()
                .expect("Mattermost stop errors")
                .clone();
            failures.extend(
                shared
                    .failed_streams
                    .lock()
                    .expect("Mattermost failed streams")
                    .iter()
                    .map(|(_, error)| error.clone()),
            );
            if failures.is_empty() {
                shared.clear_interactions();
            }

            if failures.is_empty() {
                Ok(())
            } else {
                Err(ChannelError::Aggregate {
                    message: "Mattermost stop failed".into(),
                    errors: failures,
                })
            }
        })
    }
}
struct EmptyHandle;
impl ChannelHandle for EmptyHandle {
    fn stop(&self, _: &str) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
}
impl ChannelAdapter for MattermostChannel {
    fn channel_key(&self) -> &ChannelKey {
        &self.key
    }
    fn start(
        &self,
        deps: ChannelStartDeps,
    ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
        let options = self.options.clone();
        let interaction = self.interaction.clone();
        let mapper = self.mapper.clone();
        let network = self.network.clone();
        Box::pin(async move {
            let token = options.token.unwrap_or_default();
            let url = options.server_url.unwrap_or(JsValue::Undefined);
            if token.is_empty() || !truthy(&url) {
                if let Some(log) = &deps.log {
                    log(
                        "error",
                        "mattermost: serverUrl and token are required (MATTERMOST_URL, MATTERMOST_TOKEN)",
                    );
                }
                return Ok(Arc::new(EmptyHandle) as Arc<dyn ChannelHandle>);
            }
            let url = url.as_string().ok_or_else(|| {
                ChannelError::Plain("TypeError: this.serverUrl.replace is not a function".into())
            })?;
            let url = String::from_utf16_lossy(url.units());
            let api = url.strip_suffix('/').unwrap_or(&url).to_owned();
            let (stop, receiver) = watch::channel(false);
            let mut shared = Shared {
                deps,
                interaction,
                mapper,
                active: Default::default(),
                chats: Default::default(),
                api,
                token,
                bot: JsValue::Null,
                network,
                stop: receiver.clone(),
                stop_errors: Default::default(),
                failed_streams: Default::default(),
            };
            match shared.rest("GET", "/users/me", None).await {
                Ok(JsValue::Null) => {
                    shared.log("error","mattermost: token check failed: TypeError: Cannot read properties of null (reading 'id')");
                    return Ok(Arc::new(EmptyHandle) as Arc<dyn ChannelHandle>);
                }
                Ok(me) => shared.bot = me.get("id").cloned().unwrap_or(JsValue::Null),
                Err(error) => {
                    shared.log("error", &format!("mattermost: token check failed: {error}"));
                    return Ok(Arc::new(EmptyHandle) as Arc<dyn ChannelHandle>);
                }
            }
            let (ws, origin) = websocket_url(
                &url,
                &shared.token.to_utf8().expect("validated ByteString token"),
            )?;
            let team = options
                .team_id
                .filter(truthy)
                .map(|team| {
                    format!(
                        ", team={}",
                        String::from_utf16_lossy(js_string_of(&team).units())
                    )
                })
                .unwrap_or_default();
            shared.log(
                "info",
                &format!("mattermost: connected (REST={}{team})", shared.api),
            );
            let log = shared.deps.log.clone();
            let shared = Arc::new(shared);
            let worker = tokio::spawn(socket_worker(shared.clone(), ws, origin, receiver));
            Ok(Arc::new(Handle {
                stop,
                worker: Arc::new(tokio::sync::Mutex::new(Some(worker))),
                log,
                shared,
            }) as Arc<dyn ChannelHandle>)
        })
    }
    // cc7c4e6 has no deliverCronResult method; the trait's default is false.
}

fn websocket_url(base: &str, token: &str) -> Result<(String, String), ChannelError> {
    let url = url::Url::parse(&if base.starts_with("http") {
        base.to_owned()
    } else {
        format!("https://{base}")
    })
    .map_err(|error| ChannelError::Plain(format!("TypeError: {error}")))?;
    let secure = url.scheme() == "https";
    let host = &url[url::Position::BeforeHost..url::Position::AfterPort];
    let path = url.path().strip_suffix('/').unwrap_or(url.path());
    let ws = url::Url::parse(&format!(
        "{}://{host}{path}/api/v4/websocket?token={}",
        if secure { "wss" } else { "ws" },
        encode_uri_component(token)
    ))
    .map_err(|error| ChannelError::error(error.to_string()))?;
    Ok((
        ws.into(),
        format!("{}://{host}", if secure { "https" } else { "http" }),
    ))
}
async fn socket_worker(
    shared: Arc<Shared>,
    url: String,
    origin: String,
    mut stop: watch::Receiver<bool>,
) {
    let mut tasks = JoinSet::new();
    loop {
        if *stop.borrow() {
            break;
        }
        let request = (|| {
            let mut request = url
                .clone()
                .into_client_request()
                .map_err(|error| error.to_string())?;
            request.headers_mut().insert(
                "origin",
                origin.parse().map_err(|error| format!("{error}"))?,
            );
            Ok::<_, String>(request)
        })();
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                shared.log("error", &format!("mattermost: ws connect failed: {error}"));
                break;
            }
        };
        let connected = tokio::select! {biased; _=stop.changed()=>break, connected=shared.network.connect_ws(request)=>connected};
        match connected {
            Ok((mut socket, _)) => loop {
                tokio::select! {biased;
                    _=stop.changed()=>{let _=socket.close(None).now_or_never();break;}
                    Some(result)=tasks.join_next(),if !tasks.is_empty()=>{if let Err(error)=result {shared.record_task_error(error);}}
                    message=socket.next()=>match message {
                        Some(Ok(Message::Text(raw)))=>start_message(&mut tasks,shared.clone(),raw.to_string()),
                        Some(Ok(Message::Binary(_)))=>{
                            // Node global WebSocket defaults to Blob; the adapter's
                            // String(Blob) is not JSON. Transport still inflates this
                            // message first, retaining its permessage-deflate context.
                        }
                        Some(Ok(Message::Ping(data)))=>{tokio::select! {biased; _=stop.changed()=>break, _=socket.send(Message::Pong(data))=>{}}}
                        Some(Ok(Message::Close(_)))=>{tokio::select! {biased; _=stop.changed()=>{}, _=socket.flush()=>{}}break;},
                        None=>break,
                        Some(Err(error))=>{shared.log("error",&format!("mattermost: ws error: {error}"));break;}
                        Some(Ok(_))=>{}
                    }
                }
            },
            Err(error) => shared.log("error", &format!("mattermost: ws error: {error}")),
        }
        if *stop.borrow() {
            break;
        }
        tokio::select! {biased; _=stop.changed()=>break,_=tokio::time::sleep(RECONNECT)=>{}}
    }
    // Each task cancels its own submission before the owner joins it.
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            shared.record_task_error(error);
        }
    }
    if shared
        .stop_errors
        .lock()
        .expect("Mattermost stop errors")
        .is_empty()
        && shared
            .failed_streams
            .lock()
            .expect("Mattermost failed streams")
            .is_empty()
    {
        shared.clear_interactions();
    }
}

// JS event callbacks run synchronously up to their first await. Poll each owned
// handler once in the reader before spawning it: FIFO mapper/admission and
// helper answer acquisition cannot be reordered by Tokio's worker scheduler.
fn start_message(tasks: &mut JoinSet<()>, shared: Arc<Shared>, raw: String) {
    let mut handler = Box::pin(async move {
        on_message(shared, &raw).await;
    });
    if handler.as_mut().now_or_never().is_none() {
        tasks.spawn(handler);
    }
}

async fn stopped(stop: &mut watch::Receiver<bool>) {
    if !*stop.borrow() {
        let _ = stop.wait_for(|stopped| *stopped).await;
    }
}

// JSON.parse accepts UTF-16 source strings containing isolated surrogates.
// Escape those units before the UTF-8 codec; do not replacement-decode them.
fn json_source(text: &JsString) -> String {
    let mut output = String::new();
    for unit in char::decode_utf16(text.units().iter().copied()) {
        match unit {
            Ok(character) => output.push(character),
            Err(error) => output.push_str(&format!("\\u{:04x}", error.unpaired_surrogate())),
        }
    }
    output
}
fn decode_post(raw: &str, bot: &JsValue) -> Option<(Context, JsString)> {
    let message = parse_json(raw).ok()?;
    if !message.get("event")?.as_string()?.equals_str("posted") {
        return None;
    }
    let post = message.get("data")?.get("post")?;
    if !truthy(post) {
        return None;
    }
    let post = parse_json(&json_source(&js_string_of(post))).ok()?;
    if let Some(user) = post.get("user_id")
        && truthy(user)
        && truthy(bot)
        && user == bot
    {
        return None;
    }
    if let Some(props) = post.get("props")
        && ["from_webhook", "from_bot"].iter().any(|key| {
            props
                .get(key)
                .and_then(JsValue::as_string)
                .is_some_and(|text| text.equals_str("true"))
        })
    {
        return None;
    }
    let channel = post
        .get("channel_id")
        .filter(|value| truthy(value))
        .map(js_string_of)?;
    let root = post
        .get("root_id")
        .filter(|value| truthy(value))
        .map(js_string_of);
    let message = post
        .get("message")
        .filter(|value| !sms::nullish(value))
        .map(js_string_of)
        .unwrap_or_default();
    let mut units = Vec::new();
    let mut iter = message.units().iter().copied().peekable();
    while let Some(unit) = iter.next() {
        if unit == 13 && iter.peek() == Some(&10) {
            continue;
        }
        units.push(unit);
    }
    let text = JsString::from_utf16(trim_units(&units).to_vec());
    if text.is_empty() {
        return None;
    }
    let chat = root
        .as_ref()
        .map(|root| concat_units(&[channel.units(), &[58], root.units()]))
        .unwrap_or_else(|| channel.clone());
    Some((
        Context {
            channel,
            root,
            chat,
        },
        text,
    ))
}
async fn on_message(shared: Arc<Shared>, raw: &str) {
    if *shared.stop.borrow() {
        return;
    }
    let mut stop = shared.stop.clone();
    let Some((ctx, text)) = decode_post(raw, &shared.bot) else {
        return;
    };
    shared
        .chats
        .lock()
        .expect("Mattermost chats")
        .insert(ctx.chat.clone());
    let helper = &shared.interaction;
    if shared.deps.turn_gateway.is_some() && helper.elicitation_pending(&ctx.chat) {
        let answer = tokio::select! {biased; _=stopped(&mut stop)=>return, answer=helper.elicitation_answer(&ctx.chat,&text)=>answer};
        match answer {
            Ok(Some(answer)) if !answer.is_empty() => {
                shared.reply(&ctx, &answer).await;
            }
            Err(error) => shared.log(
                "error",
                &format!("mattermost: elicitation answer error: {error}"),
            ),
            _ => {}
        }
        return;
    }
    if shared.deps.turn_gateway.is_some() && helper.permission_pending(&ctx.chat) {
        let mut token = None;
        let result = async {
            let answer=tokio::select! {biased; _=stopped(&mut stop)=>return Ok::<_,ChannelError>(()),answer=helper.permission_answer(&ctx.chat,&text)=>answer};
            if let Some(answer) = answer? {
                token = answer.answer_token;
                if let Some(text) = answer.text.filter(|text| !text.is_empty()) {
                    if !shared.reply(&ctx, &text).await {
                        helper.permission_release_answer(&ctx.chat, token);
                        return Ok::<_, ChannelError>(());
                    }
                    if !answer.can_advance && !answer.retry_prompt {
                        return Ok(());
                    }
                    if let Some(prompt) = helper
                        .permission_take_next_prompt(&ctx.chat, token)
                        .filter(|text| !text.is_empty())
                    {
                        let id = helper.permission_prompt_request_id(&ctx.chat, token);
                        let delivered = shared.reply(&ctx, &prompt).await;
                        helper.permission_confirm_next(&ctx.chat, delivered, id.as_ref(), token);
                    }
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            if token.is_some() {
                helper.permission_release_answer(&ctx.chat, token);
            }
            shared.log(
                "error",
                &format!("mattermost: permission answer error: {error}"),
            );
        }
        return;
    }
    let ((session, new, message), _active) = {
        let mut active = shared.active.lock().expect("Mattermost active chats");
        if active.contains(&ctx.chat) {
            shared.log(
                "info",
                &format!(
                    "mattermost: chat {} already active, skipping",
                    String::from_utf16_lossy(ctx.chat.units())
                ),
            );
            return;
        }
        let mapped =
            shared
                .mapper
                .lock()
                .expect("Mattermost mapper")
                .resolve(&ctx.chat, &text, || uuid::Uuid::new_v4().to_string());
        let guard = if mapped.2.is_empty() {
            None
        } else {
            active.insert(ctx.chat.clone());
            Some(super::ActiveChat::new(
                shared.active.clone(),
                ctx.chat.clone(),
            ))
        };
        (mapped, guard)
    };
    if new && message.is_empty() {
        shared.reply(&ctx, &JsString::from("已创建新会话。")).await;
        return;
    }
    if message.is_empty() {
        return;
    }
    process_message(&shared, &ctx, session, message).await;
}
async fn process_message(shared: &Shared, ctx: &Context, session: JsString, message: JsString) {
    let Some(gateway) = &shared.deps.turn_gateway else {
        return;
    };
    let mut reply = Vec::new();
    let mut cancel_failed = false;
    let mut stop = shared.stop.clone();
    let result = async {
        if *stop.borrow() {return Ok(())}
        let submitted=tokio::select! {biased;
            _=stopped(&mut stop)=>return Ok(()),
            result=gateway.submit(TurnRequest {session_key:session.clone(),channel_key:ChannelKey::new("mattermost"),message, ..Default::default() })=>result
        };
        let mut events=submitted?;
        loop {
            let event=tokio::select! {biased;
                _=stopped(&mut stop)=>{
                    if let Err(error)=events.cancel_and_drain(JsString::from("mattermost stopped")).await {
                        shared.failed_streams.lock().expect("Mattermost failed streams").push((Arc::new(tokio::sync::Mutex::new(events)),error.clone()));cancel_failed=true;return Err(error);
                    }
                    break
                },
                event=events.next()=>event?
            };
            let Some(event)=event else {break};
            let kind = event.get("type").and_then(JsValue::as_string);
            if kind.is_some_and(|kind| kind.equals_str("elicitation_request")) {
                let prompt = shared
                    .interaction
                    .elicitation_capture(&ctx.chat, &session, &event);
                shared.reply(ctx, &prompt).await;
                continue;
            }
            if kind.is_some_and(|kind| kind.equals_str("permission_request")) {
                if let Some(prompt) = shared
                    .interaction
                    .permission_capture(&ctx.chat, &session, &event)
                    .filter(|prompt| !prompt.is_empty())
                {
                    let delivered = shared.reply(ctx, &prompt).await;
                    shared.interaction.permission_confirm_initial(
                        &ctx.chat,
                        delivered,
                        event.get("requestId").and_then(JsValue::as_string),
                    );
                }
                continue;
            }
            if let Some(fragment) = render_event(&event) {
                reply.extend_from_slice(fragment.units());
            }
        }
        Ok::<_, ChannelError>(())
    }
    .await;
    if let Err(error) = result {
        if *shared.stop.borrow() && !cancel_failed {
            shared
                .stop_errors
                .lock()
                .expect("Mattermost stop errors")
                .push(error.clone());
        }
        shared.log("error", &format!("mattermost: submitTurn error: {error}"));
        reply = "处理消息时发生错误，请重试。".encode_utf16().collect();
    }
    if *shared.stop.borrow() {
        return;
    }
    shared.interaction.elicitation_clear(&ctx.chat);
    shared.interaction.permission_clear_after_turn(&ctx.chat);
    let reply = JsString::from_utf16(trim_units(&reply).to_vec());
    if !reply.is_empty() {
        shared.reply(ctx, &reply).await;
    }
}
fn render_event(event: &JsValue) -> Option<JsString> {
    let kind = event.get("type").and_then(JsValue::as_string)?;
    if kind.equals_str("assistant_text_delta") {
        return event
            .get("text")
            .filter(|value| !sms::nullish(value))
            .map(js_string_of);
    }
    if kind.equals_str("assistant_thinking_delta") || kind.equals_str("tool_call_started") {
        return Some(JsString::default());
    }
    if kind.equals_str("tool_call_finished") {
        if event.get("ok").is_some_and(truthy) {
            return Some(JsString::default());
        }
        let name = event
            .get("toolName")
            .filter(|value| !sms::nullish(value))
            .or_else(|| event.get("toolCallId"))
            .map(js_string_of)
            .unwrap_or_else(|| "undefined".into());
        return Some(concat_units(&[
            &"\n⚠️ ".encode_utf16().collect::<Vec<_>>(),
            name.units(),
            &" failed\n".encode_utf16().collect::<Vec<_>>(),
        ]));
    }
    if kind.equals_str("error") {
        let message = event
            .get("message")
            .map(js_string_of)
            .unwrap_or_else(|| "undefined".into());
        return Some(concat_units(&[
            &"\n❌ ".encode_utf16().collect::<Vec<_>>(),
            message.units(),
            &[10],
        ]));
    }
    if kind.equals_str("elicitation_request") {
        let mut lines = Vec::new();
        sms::elicitation::push_question_lines(
            &mut lines,
            sms::elicitation::as_array(event.get("questions")),
            false,
        );
        return Some(if lines.is_empty() {
            JsString::default()
        } else {
            concat_units(&[&[10], &sms::join_lines(&lines), &[10]])
        });
    }
    None
}
fn chunk_text(text: &JsString, max: usize) -> Vec<JsString> {
    let mut rest = text.units();
    if rest.len() <= max {
        return vec![text.clone()];
    }
    let mut result = Vec::new();
    while rest.len() > max {
        let find = |unit| {
            rest[..=max]
                .iter()
                .rposition(|candidate| *candidate == unit)
                .filter(|index| index * 2 >= max)
        };
        let split = find(10).or_else(|| find(32)).unwrap_or(max);
        result.push(JsString::from_utf16(rest[..split].to_vec()));
        rest = &rest[split..];
        while rest.first() == Some(&10) {
            rest = &rest[1..];
        }
    }
    if !rest.is_empty() {
        result.push(JsString::from_utf16(rest.to_vec()));
    }
    result
}
pub(crate) fn register(mounts: &mut ChannelLoaderMounts, responder: ResponderRef) {
    register_with_network(mounts, responder, Arc::new(ChannelNetwork::new(None)));
}
pub(crate) fn register_with_network(
    mounts: &mut ChannelLoaderMounts,
    responder: ResponderRef,
    network: Arc<ChannelNetwork>,
) {
    mounts.register(
        "mattermost",
        Arc::new(move |options| {
            Ok(Arc::new(MattermostChannel::new_with_network(
                MattermostOptions::from_object(options),
                js_helper_interaction(responder.clone()),
                network.clone(),
            )) as Arc<dyn ChannelAdapter>)
        }),
    );
}

#[cfg(test)]
#[path = "mattermost/tests.rs"]
mod tests;
