use super::*;
use network::{Response, SetupNetwork};
use pilotdeck_gateway::channel::weixin::WeixinLoginResult;
use std::collections::VecDeque;

struct Temp {
    root: PathBuf,
    paths: Paths,
}
impl Temp {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("c1-setup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        Self {
            paths: Paths {
                yaml: root.join("pilot/pilotdeck.yaml"),
                weixin_credentials: root.join("pilot/weixin-credentials.json"),
            },
            root,
        }
    }
    fn config(&self, text: &str) {
        std::fs::create_dir_all(self.paths.yaml.parent().unwrap()).unwrap();
        std::fs::write(&self.paths.yaml, text).unwrap();
    }
    fn value(&self) -> JsValue {
        load_yaml_config(&self.paths).unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
struct Answers(VecDeque<String>);
impl Answers {
    fn new(values: &[&str]) -> Self {
        Self(values.iter().map(|value| (*value).into()).collect())
    }
}
impl QuestionInput for Answers {
    fn ask(&mut self, _prompt: &str) -> Step<String> {
        self.0.pop_front().ok_or(Stop::Abandoned)
    }
}
struct RecordedRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
    require_ok: bool,
}

#[derive(Default)]
struct FakeNetwork {
    responses: VecDeque<Result<Response, String>>,
    requests: Vec<RecordedRequest>,
    elapsed: Duration,
    login: Option<Result<WeixinLoginResult, String>>,
    logins: usize,
}
impl FakeNetwork {
    fn response(&mut self, status: u16, body: &str) {
        self.responses.push_back(Ok(Response {
            status,
            body: body.into(),
        }));
    }
}
impl SetupNetwork for FakeNetwork {
    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
        require_ok: bool,
    ) -> Result<Response, String> {
        self.requests.push(RecordedRequest {
            method: method.into(),
            url: url.into(),
            headers: headers
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
            body: body.into(),
            require_ok,
        });
        self.responses
            .pop_front()
            .expect("unexpected external request")
    }
    fn weixin_login(&mut self) -> Result<WeixinLoginResult, String> {
        self.logins += 1;
        self.login.take().expect("unexpected iLink login")
    }
    fn now(&self) -> Duration {
        self.elapsed
    }
    fn sleep(&mut self, duration: Duration) {
        self.elapsed += duration;
    }
}
fn credentials() -> WeixinLoginResult {
    WeixinLoginResult {
        base_url: "https://fixture.invalid".into(),
        bot_token: "fixture-token".into(),
        account_id: "fixture-account".into(),
    }
}

#[test]
fn feishu_uses_json_success_even_on_http_error_and_preserves_other_settings() {
    let temp = Temp::new();
    temp.config(
        "schemaVersion: 1\nadapters:\n  wecom:\n    enabled: false\n  feishu:\n    stale: true\n",
    );
    let mut net = FakeNetwork::default();
    net.response(500, r#"{"code":0,"tenant_access_token":"fixture-token"}"#);
    setup_feishu(
        &temp.paths,
        &mut Answers::new(&["lark", "app", "secret"]),
        &mut net,
    )
    .unwrap();
    let config = Some(temp.value());
    assert_eq!(at(&config, &["schemaVersion"]), Some(&JsValue::Number(1.0)));
    assert_eq!(
        at(&config, &["adapters", "wecom", "enabled"]),
        Some(&JsValue::Bool(false))
    );
    assert_eq!(
        at(&config, &["adapters", "feishu", "domainName"]),
        Some(&JsValue::from("lark"))
    );
    assert!(at(&config, &["adapters", "feishu", "stale"]).is_none());
    assert_eq!(net.requests[0].method, "POST");
    assert_eq!(
        net.requests[0].url,
        "https://open.larksuite.com/open-apis/auth/v3/tenant_access_token/internal"
    );
    assert_eq!(
        net.requests[0].headers,
        [(
            "Content-Type".into(),
            "application/json; charset=utf-8".into()
        )]
    );
    assert_eq!(
        net.requests[0].body,
        r#"{"app_id":"app","app_secret":"secret"}"#
    );
    assert!(!net.requests[0].require_ok);
}

#[test]
fn failed_feishu_validation_requires_explicit_save_and_eof_never_writes() {
    for answers in [vec!["", "app", "secret", ""], vec!["", "app", "secret"]] {
        let temp = Temp::new();
        let mut net = FakeNetwork::default();
        net.response(200, r#"{"code":"0","tenant_access_token":"fixture-token"}"#);
        let _ = setup_feishu(&temp.paths, &mut Answers::new(&answers), &mut net);
        assert!(!temp.paths.yaml.exists());
    }
    let temp = Temp::new();
    let mut net = FakeNetwork::default();
    net.responses.push_back(Err("fetch failed".into()));
    setup_feishu(
        &temp.paths,
        &mut Answers::new(&["", "app", "secret", "yes"]),
        &mut net,
    )
    .unwrap();
    assert!(temp.paths.yaml.exists());
}

#[test]
fn manual_wecom_empty_allowlist_disables_dm_and_rewrites_only_this_adapter() {
    let temp = Temp::new();
    temp.config("schemaVersion: 1\n");
    let mut net = FakeNetwork::default();
    setup_wecom(
        &temp.paths,
        &mut Answers::new(&[
            "no",
            "fixture-empty-bot",
            "fixture-empty-secret",
            "",
            "2",
            " , , ",
            "",
        ]),
        &mut net,
    )
    .unwrap();
    // Captured from the fixed TS formal entry in c1-entry/setup-ts-a.
    assert_eq!(
        std::fs::read_to_string(&temp.paths.yaml).unwrap(),
        "schemaVersion: 1\nadapters:\n  wecom:\n    enabled: true\n    token: fixture-empty-bot\n    extra:\n      secret: fixture-empty-secret\n      websocket_url: wss://openws.work.weixin.qq.com\n      dm_policy: disabled\n      group_policy: disabled\n"
    );
    assert!(net.requests.is_empty());
}

#[test]
fn existing_wecom_migrates_aliases_and_preserves_groups_without_network() {
    let temp = Temp::new();
    temp.config("adapters:\n  wecom:\n    enabled: false\n    extra:\n      bot_id: old-bot\n      botSecret: old-secret\n      websocketUrl: wss://fixture.invalid\n      dm_policy: ' ALLOWLIST '\n      allowFrom: [alice, 4, null, false]\n      group_policy: invalid\n      groupAllowFrom: ' a, , b '\n      groups:\n        group-a:\n          requireMention: false\n  weixin:\n    enabled: false\n");
    let mut net = FakeNetwork::default();
    setup_wecom(&temp.paths, &mut Answers::new(&["n"]), &mut net).unwrap();
    let config = Some(temp.value());
    assert_eq!(
        at(&config, &["adapters", "wecom", "token"]),
        Some(&JsValue::from("old-bot"))
    );
    assert_eq!(
        at(&config, &["adapters", "wecom", "extra", "allow_from"]),
        Some(&JsValue::Array(vec![
            "alice".into(),
            "4".into(),
            "null".into(),
            "false".into()
        ]))
    );
    assert_eq!(
        at(&config, &["adapters", "wecom", "extra", "dm_policy"]),
        Some(&JsValue::from("allowlist"))
    );
    assert_eq!(
        at(&config, &["adapters", "wecom", "extra", "group_policy"]),
        Some(&JsValue::from("disabled"))
    );
    assert_eq!(
        at(
            &config,
            &[
                "adapters",
                "wecom",
                "extra",
                "groups",
                "group-a",
                "requireMention"
            ]
        ),
        Some(&JsValue::Bool(false))
    );
    assert_eq!(
        at(&config, &["adapters", "weixin", "enabled"]),
        Some(&JsValue::Bool(false))
    );
    assert!(net.requests.is_empty());
}

#[test]
fn wecom_qr_retries_poll_failure_and_uses_encoded_scode_and_official_user_agent() {
    let mut net = FakeNetwork::default();
    net.response(
        200,
        r#"{"data":{"scode":"a b&","auth_url":"https://fixture.invalid/qr"}}"#,
    );
    net.responses.push_back(Err("fetch failed".into()));
    net.response(
        200,
        r#"{"data":{"status":"SUCCESS","bot_info":{"bot_id":" bot ","secret":" secret "}}}"#,
    );
    assert_eq!(scan_wecom(&mut net), Some(("bot".into(), "secret".into())));
    assert_eq!(net.elapsed, Duration::from_secs(3));
    assert_eq!(
        net.requests[1].url,
        "https://work.weixin.qq.com/ai/qc/query_result?scode=a%20b%26"
    );
    assert_eq!(
        net.requests[0].headers,
        [("User-Agent".into(), "PilotDeck/1.0".into())]
    );
    assert!(net.requests.iter().all(|request| request.require_ok));
}

#[test]
fn wecom_qr_failure_falls_back_to_manual_and_abandoned_input_does_not_write() {
    let temp = Temp::new();
    let mut net = FakeNetwork::default();
    net.response(200, r#"{"data":{}}"#);
    assert!(matches!(
        setup_wecom(&temp.paths, &mut Answers::new(&["yes", "bot"]), &mut net),
        Err(Stop::Abandoned)
    ));
    assert!(!temp.paths.yaml.exists());
}

#[test]
fn weixin_login_creates_actual_pilot_home_and_retains_credentials_on_yaml_failure() {
    let mut temp = Temp::new();
    let blocker = temp.root.join("not-a-directory");
    std::fs::write(&blocker, "blocker").unwrap();
    temp.paths.yaml = blocker.join("pilotdeck.yaml");
    let mut net = FakeNetwork {
        login: Some(Ok(credentials())),
        ..FakeNetwork::default()
    };
    setup_weixin(&temp.paths, &mut Answers::new(&["yes"]), &mut net).unwrap();
    assert_eq!(net.logins, 1);
    assert_eq!(
        weixin_account(&temp.paths).as_deref(),
        Some("fixture-account")
    );
    assert_eq!(
        std::fs::read_to_string(&temp.paths.weixin_credentials).unwrap(),
        "{\n  \"baseUrl\": \"https://fixture.invalid\",\n  \"botToken\": \"fixture-token\",\n  \"accountId\": \"fixture-account\"\n}"
    );
    assert!(!temp.paths.yaml.exists());
    temp.paths.yaml = temp.root.join("pilot/pilotdeck.yaml");
    setup_weixin(&temp.paths, &mut Answers::new(&["no"]), &mut net).unwrap();
    assert_eq!(net.logins, 1);
    assert_eq!(
        std::fs::read_to_string(&temp.paths.yaml).unwrap(),
        "adapters:\n  weixin:\n    enabled: true\n"
    );
}

#[test]
fn weixin_failure_or_cancel_leaves_no_credentials_or_config() {
    for answers in [vec!["no"], vec!["yes"]] {
        let temp = Temp::new();
        let mut net = FakeNetwork {
            login: Some(Err("Login timed out".into())),
            ..FakeNetwork::default()
        };
        setup_weixin(&temp.paths, &mut Answers::new(&answers), &mut net).unwrap();
        assert!(!temp.paths.yaml.exists());
        assert!(!temp.paths.weixin_credentials.exists());
    }
}

#[test]
fn malformed_json_reports_the_v8_parse_error() {
    assert_eq!(
        network::parse_response(&Response {
            status: 200,
            body: "".into()
        })
        .unwrap_err(),
        "Unexpected end of JSON input"
    );
}

#[test]
fn feishu_null_missing_token_and_empty_message_match_baseline_errors() {
    for (body, expected) in [
        ("null", "Cannot read properties of null (reading 'code')"),
        (r#"{"code":0,"msg":""}"#, "code=0 msg="),
        (
            r#"{"code":0,"tenant_access_token":"","msg":null}"#,
            "code=0 msg=unknown",
        ),
        (r#"{"msg":false}"#, "code=undefined msg=false"),
    ] {
        let mut net = FakeNetwork::default();
        net.response(200, body);
        assert_eq!(
            check_feishu(&mut net, "https://fixture.invalid", "app", "secret"),
            Err(expected.into())
        );
    }
}

#[test]
fn wecom_qr_rejects_isolated_utf16_surrogates_before_polling() {
    let mut net = FakeNetwork::default();
    net.response(
        200,
        r#"{"data":{"scode":"\ud800","auth_url":"https://fixture.invalid"}}"#,
    );
    assert_eq!(scan_wecom_result(&mut net), Err("URI malformed".into()));
    assert_eq!(net.requests.len(), 1);
    assert_eq!(encode_component("中文😀"), "%E4%B8%AD%E6%96%87%F0%9F%98%80");
}
