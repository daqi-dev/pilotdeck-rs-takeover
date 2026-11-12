use super::*;
#[test]
fn matrix_baseline_filter_and_session_contract() {
    let own = JsString::from("@bot:local");
    for raw in [
        r#"{"sender":"@bot:local","content":{"body":"ignore"}}"#,
        r#"{"sender":"@a:local","content":{"msgtype":"m.notice","body":"ignore"}}"#,
        r#"{"sender":"@a:local","content":{"msgtype":"m.text","body":"ignore","m.relates_to":{"rel_type":"m.replace"}}}"#,
    ] {
        assert!(wire::message(&parse_json(raw).unwrap(), Some(&own)).is_none());
    }
    let text = wire::message(
        &parse_json(r#"{"sender":"@a:local","content":{"body":" hello "}}"#).unwrap(),
        Some(&own),
    )
    .unwrap();
    assert_eq!(text, JsString::from("hello"));
    let mut mapper = MatrixSessionMapper::new(Default::default(), Box::new(|| "fixed".into()));
    let room = JsString::from("!room:local");
    assert_eq!(
        mapper.resolve(&room, &text).session_key,
        JsString::from("matrix:chat=!room:local:general")
    );
    let new = mapper.resolve(&room, &JsString::from(" /new next "));
    assert!(new.command_new);
    assert_eq!(new.message, JsString::from("next"));
    assert_eq!(
        new.session_key,
        JsString::from("matrix:chat=!room:local:s_fixed")
    );
    assert_eq!(mapper.resolve(&room, &text).session_key, new.session_key);
}
#[test]
fn matrix_chunks_count_utf16_and_preserve_space_boundary() {
    let text = JsString::from(format!("{} {}", "a".repeat(3000), "b".repeat(2000)));
    let chunks = wire::chunk_text(&text);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].units().len(), 3000);
    assert_eq!(chunks[1].units().first(), Some(&32));
    let units = vec![0xd83d; 4001];
    let chunks = wire::chunk_text(&JsString::from_utf16(units));
    assert_eq!(chunks[0].units().len(), 4000);
}

use crate::channel::im_interaction::InteractionResponder;
use crate::channel::sms::{ElicitationResponse, PermissionDecision, PermissionDecisionOutcome};
use std::collections::VecDeque;

struct Responder;
impl InteractionResponder for Responder {
    fn respond_elicitation(&self, _: ElicitationResponse) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
    fn permission_decide(
        &self,
        _: PermissionDecision,
    ) -> BoxFuture<Result<PermissionDecisionOutcome, ChannelError>> {
        Box::pin(async { Ok(PermissionDecisionOutcome { delivered: true }) })
    }
}

type Exchange = (Method, String, HeaderMap, String);
#[derive(Default)]
struct Server {
    requests: Mutex<Vec<Exchange>>,
    sync: Mutex<VecDeque<&'static str>>,
}
impl ModelHttpTransport for Server {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path_and_query().unwrap().as_str().to_owned();
        let body = String::from_utf8(
            body.collect()
                .now_or_never()
                .expect("Full body is immediately ready")
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        self.requests
            .lock()
            .unwrap()
            .push((parts.method, path.clone(), parts.headers, body));
        let body = if path.contains("whoami") {
            Some(r#"{"user_id":"@bot:local"}"#)
        } else if path.contains("/sync") {
            self.sync.lock().unwrap().pop_front()
        } else {
            Some("{}")
        };
        TransportFuture::io(async move {
            if let Some(body) = body {
                Ok(Response::new(
                    Full::new(Bytes::from_static(body.as_bytes()))
                        .map_err(|never| match never {})
                        .boxed(),
                ))
            } else {
                std::future::pending().await
            }
        })
    }
}

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mock protocol condition");
}

#[tokio::test]
async fn matrix_native_sync_join_auth_reply_storage_and_stop() {
    let storage = std::env::temp_dir().join(format!("h1-matrix-{}", uuid::Uuid::new_v4()));
    let server = Arc::new(Server::default());
    server.sync.lock().unwrap().push_back(r#"{"next_batch":"next token","rooms":{"invite":{"!invite:local":{}},"join":{"!room:local":{"timeline":{"events":[{"type":"m.room.message","sender":"@a:local","content":{"msgtype":"m.text","body":"/new"}}]}}}}}"#);
    let channel = MatrixChannel::new(
        MatrixChannelOptions {
            homeserver: Some("http://mock.invalid/".into()),
            access_token: Some("fixture".into()),
            storage_path: Some(storage.clone()),
            transport: Some(server.clone()),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder)),
    );
    let handle = channel.start(Default::default()).await.unwrap();
    until(|| {
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(_, path, _, _)| path.contains("since=next%20token"))
    })
    .await;
    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests[0].1, "/_matrix/client/v3/account/whoami");
    assert!(
        requests
            .iter()
            .all(|(_, _, headers, _)| headers.get("authorization").unwrap() == "Bearer fixture")
    );
    assert!(
        requests
            .iter()
            .any(|(method, path, _, _)| *method == Method::POST
                && path == "/_matrix/client/v3/join/!invite%3Alocal")
    );
    let (_, path, _, body) = requests
        .iter()
        .find(|(method, _, _, _)| *method == Method::PUT)
        .unwrap();
    assert!(path.starts_with("/_matrix/client/v3/rooms/!room%3Alocal/send/m.room.message/"));
    assert_eq!(
        parse_json(body).unwrap(),
        parse_json(r#"{"msgtype":"m.text","body":"已创建新会话。"}"#).unwrap()
    );
    let stored = tokio::fs::read_to_string(&storage).await.unwrap();
    assert_eq!(
        parse_json(&stored).unwrap().get("syncToken"),
        Some(&JsValue::from("next token"))
    );
    handle.stop("fixture complete").await.unwrap();
    assert!(
        !channel
            .0
            .send_reply(&"!room:local".into(), &"late".into())
            .await
    );
    tokio::fs::remove_file(storage).await.unwrap();
}

#[test]
fn matrix_environment_pilot_home_matches_baseline_path_normalization() {
    for (input, expected) in [
        ("~", "/fixture/home"),
        ("~/bots/../pilot", "/fixture/home/pilot"),
        ("/fixture/a/../pilot", "/fixture/pilot"),
    ] {
        let input = input.to_owned();
        let env: Env = Arc::new(move |key| match key {
            "HOME" => Some("/fixture/home".into()),
            "PILOT_HOME" => Some(input.clone().into()),
            _ => None,
        });
        assert_eq!(resolve_pilot_home(&env), std::path::PathBuf::from(expected));
    }
    let env: Env = Arc::new(|key| (key == "PILOT_HOME").then(|| "relative/pilot".into()));
    assert_eq!(
        resolve_pilot_home(&env),
        std::env::current_dir().unwrap().join("relative/pilot")
    );
}
