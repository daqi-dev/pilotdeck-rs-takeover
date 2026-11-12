//! Real HTTP replay for Google complete, including requests produced by the
//! fixed TS title/suggestion/judge consumers. No injected complete result.
#[path = "common/baseline.rs"]
mod baseline;
#[path = "common/model_mock.rs"]
mod model_mock;

use model_mock::{get, items, json, merge, text};
use pilotdeck_gateway::runtime::model::{
    http::{HyperModelTransport, StreamOptions, complete_model},
    provider_registry::NativeModelInvocationProvider,
    router::{
        AbortFlag, CacheAwareSwitching, RouterConfig, RouterDecisionInput, RouterDeps,
        RouterEventSink, RouterModelRef, RouterRuntime, SubagentPolicy, TierConfig,
        TokenSaverConfig,
    },
};
use pilotdeck_protocol::{JsObject, JsValue, parse_json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const ORACLE: &str = include_str!("fixtures/model-complete-google/oracle.json");

fn setup(
    oracle: &JsValue,
    case: &JsValue,
) -> (Arc<Mutex<model_mock::Exchange>>, model_mock::Mock, JsValue) {
    let exchange = Arc::new(Mutex::new(model_mock::Exchange {
        replies: items(get(case, "replies")).to_vec(),
        ..Default::default()
    }));
    let excluded = items(get(oracle, "transportHeaders"))
        .iter()
        .map(text)
        .collect();
    let mock = model_mock::start_mock(exchange.clone(), excluded, None);
    let base = get(oracle, "provider");
    let mut provider = merge(base, get(case, "provider"));
    let retry = merge(get(base, "retry"), get(get(case, "provider"), "retry"));
    let url = text(get(&provider, "url")).replace("$MOCK", &mock.origin);
    if let JsValue::Object(provider) = &mut provider {
        provider.insert("retry".into(), retry);
        provider.insert("url".into(), JsValue::String(url.into()));
    }
    (exchange, mock, provider)
}

fn assert_requests(exchange: &Mutex<model_mock::Exchange>, case: &JsValue) {
    let requests: Vec<_> = exchange
        .lock()
        .unwrap()
        .requests
        .iter()
        .map(|request| {
            let mut record = JsObject::new();
            for key in ["method", "path", "headers", "body"] {
                record.insert(key.into(), request.get(key).unwrap().clone());
            }
            if let Some(closed) = request.get("clientClosed") {
                record.insert("clientClosed".into(), closed.clone());
            }
            JsValue::Object(record)
        })
        .collect();
    assert_eq!(
        json(&JsValue::Array(requests)),
        json(get(case, "requests")),
        "{}: wire requests",
        text(get(case, "name"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_complete_matches_real_ts_http_and_consumer_requests() {
    let oracle = baseline::oracle(ORACLE);
    let cases = items(get(&oracle, "cases"));
    assert_eq!(cases.len(), 35);
    assert_eq!(
        cases
            .iter()
            .map(|case| items(get(case, "requests")).len())
            .sum::<usize>(),
        39
    );
    let transport = HyperModelTransport::new();
    for case in cases {
        let name = text(get(case, "name"));
        let (exchange, _mock, provider) = setup(&oracle, case);
        let mut providers = JsObject::new();
        providers.insert("p".into(), provider);
        let mut config = JsObject::new();
        config.insert("providers".into(), JsValue::Object(providers));
        let config = JsValue::Object(config);
        let abort = AbortFlag::new();
        if get(case, "abortBefore") == &JsValue::Bool(true) {
            abort.abort();
        }
        let timer = get(case, "abortAfterMs").as_number().map(|ms| {
            let abort = abort.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs_f64(ms / 1000.0)).await;
                abort.abort();
            })
        });
        let options = StreamOptions {
            abort: Some(abort),
            stream_timeout_ms: get(case, "streamTimeoutMs").as_number(),
            ..StreamOptions::default()
        };
        for call in items(get(case, "calls")) {
            assert_eq!(
                exchange.lock().unwrap().requests.len() as f64,
                get(call, "wireStart").as_number().unwrap(),
                "{name}: call start"
            );
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                complete_model(&transport, get(call, "request"), &config, &options),
            )
            .await
            .expect("complete must finish or cancel");
            match (result, call.get("response")) {
                (Ok(actual), Some(expected)) => {
                    assert_eq!(json(&actual), json(expected), "{name}: canonical response")
                }
                (Err(actual), None) => assert_eq!(
                    json(&model_mock::thrown(&actual)),
                    json(get(call, "thrown")),
                    "{name}: error"
                ),
                (actual, expected) => panic!("{name}: outcome mismatch {actual:?} / {expected:?}"),
            }
            assert_eq!(
                exchange.lock().unwrap().requests.len() as f64,
                get(call, "wireEnd").as_number().unwrap(),
                "{name}: call end"
            );
        }
        if let Some(timer) = timer {
            timer.abort();
        }
        if items(get(case, "replies")).iter().any(|reply| {
            get(reply, "end")
                .as_string()
                .is_some_and(|end| end.equals_str("hold"))
        }) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if exchange
                        .lock()
                        .unwrap()
                        .requests
                        .iter()
                        .all(|request| request.get("clientClosed") == Some(&JsValue::Bool(true)))
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("cancelled complete must close the held provider connection");
        }
        assert_requests(&exchange, case);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_native_invocation_drives_real_token_saver_judge() {
    let oracle = baseline::oracle(ORACLE);
    let reference = |model: &str| RouterModelRef {
        id: format!("p/{model}").into(),
        provider: "p".into(),
        model: model.into(),
    };
    let config = TokenSaverConfig {
        enabled: true,
        judge: reference("m"),
        default_tier: "medium".into(),
        tiers: vec![
            (
                "simple".into(),
                TierConfig {
                    model: reference("simple"),
                    description: None,
                },
            ),
            (
                "medium".into(),
                TierConfig {
                    model: reference("m"),
                    description: None,
                },
            ),
        ],
        rules: Vec::new(),
        judge_timeout_ms: 1000.0,
        subagent_policy: SubagentPolicy::Judge,
        cache_aware: CacheAwareSwitching {
            enabled: false,
            min_savings_ratio: 0.0,
        },
    };
    let message =
        parse_json(r#"{"role":"user","content":[{"type":"text","text":"hi 中文"}]}"#).unwrap();
    for case in items(get(&oracle, "cases")).iter().filter(|case| {
        get(case, "consumer")
            .as_string()
            .is_some_and(|value| value.equals_str("judge"))
    }) {
        let (exchange, _mock, provider) = setup(&oracle, case);
        let invoker =
            NativeModelInvocationProvider::new(provider, Arc::new(HyperModelTransport::new()));
        #[derive(Default)]
        struct Events(Mutex<Vec<JsValue>>);
        impl RouterEventSink for Events {
            fn emit(&self, event: JsValue) {
                self.0.lock().unwrap().push(event);
            }
        }
        let events = Arc::new(Events::default());
        let mut deps = RouterDeps::new(Arc::new(invoker));
        deps.events = events.clone();
        let router = RouterRuntime::with_deps(
            RouterConfig {
                token_saver: Some(config.clone()),
                scenarios_default: Some(reference("m")),
                ..RouterConfig::default()
            },
            deps,
        )
        .unwrap();
        let mut request = JsObject::new();
        request.insert("messages".into(), JsValue::Array(vec![message.clone()]));
        let decision = router
            .decide(&RouterDecisionInput {
                request: JsValue::Object(request),
                session_id: "s".into(),
                is_main_agent: true,
                abort: None,
                metadata: None,
            })
            .await
            .unwrap();
        let expected = get(case, "result");
        assert_eq!(text(get(&decision, "resolvedFrom")), "tokenSaver");
        assert_eq!(get(&decision, "tokenSaverTier"), get(expected, "tier"));
        assert_eq!(
            get(&decision, "model"),
            get(get(expected, "selection"), "model")
        );
        let records = events.0.lock().unwrap();
        let failures: Vec<_> = records
            .iter()
            .filter(|event| {
                get(event, "type")
                    .as_string()
                    .is_some_and(|value| value.equals_str("pilotdeck_router_token_saver_failed"))
            })
            .collect();
        if let Some(expected_failure) = expected.get("failure") {
            assert_eq!(failures.len(), 1);
            assert_eq!(get(failures[0], "reason"), get(expected_failure, "reason"));
            assert_eq!(
                get(failures[0], "attempts"),
                get(expected_failure, "attempts")
            );
        } else {
            assert!(failures.is_empty());
        }
        assert_requests(&exchange, case);
        router.shutdown();
    }
}
