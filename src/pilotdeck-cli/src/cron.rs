//! Online cron commands from the fixed CLI entry (pilotdeck.ts:362-423).
//! The server connection precedes argument validation, including usage errors.
use super::cli_channel::remote::RemoteGateway;
use super::paths::{self, Env};
use super::{Failure, probe, stderr_line, write_json_pretty_line};
use pilotdeck_protocol::{JsObject, JsValue, stringify_json};
use serde_json::value::RawValue;
use std::path::Path;

const GATEWAY_URL: &str = "ws://127.0.0.1:18789/ws";
const SERVER_REQUIRED: &str = "pilotdeck cron requires a running pilotdeck server.";
const USAGE: &str = "Usage: pilotdeck cron <list|create|delete|stop>";
const CREATE_USAGE: &str = "Usage: pilotdeck cron create --session <sessionKey> --message <text> (--once <iso> | --cron <expr>)";
const DELETE_USAGE: &str = "Usage: pilotdeck cron delete <taskId> [--stop-running]";
const STOP_USAGE: &str = "Usage: pilotdeck cron stop <taskId> or pilotdeck cron stop --run <runId>";

pub fn run(argv: &[String], env: &Env, cwd: &Path) -> Result<u8, Failure> {
    let Some(token) = probe::probe_gateway_server(env, cwd) else {
        stderr_line(SERVER_REQUIRED);
        return Ok(1);
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::Error(error.to_string()))?;
    let completion = runtime.block_on(async {
        // probeServer.ts:34-46 treats every connection/hello failure as offline.
        let Ok(gateway) = RemoteGateway::connect(GATEWAY_URL, &token).await else {
            return Ok(Completion::Usage(SERVER_REQUIRED));
        };
        execute_connected(gateway, argv, cwd).await
    })?;
    match completion {
        Completion::Usage(message) => {
            stderr_line(message);
            Ok(1)
        }
        Completion::Result(result) => {
            let json = stringify_json(&result)
                .map_err(|error| Failure::Error(error.to_string()))?
                .unwrap_or_else(|| "undefined".into());
            write_json_pretty_line(&json);
            Ok(0)
        }
    }
}

#[derive(Debug)]
enum Completion {
    Usage(&'static str),
    Result(JsValue),
}

struct Request {
    method: &'static str,
    params: JsValue,
}

async fn execute_connected(
    mut gateway: RemoteGateway,
    argv: &[String],
    cwd: &Path,
) -> Result<Completion, Failure> {
    let result = execute(&mut gateway, argv, cwd).await;
    // A one-shot CLI command owns its connection. Close it on success, usage
    // errors and RPC errors, rather than retaining a live socket after return.
    gateway.close().await;
    result
}

async fn execute(
    gateway: &mut RemoteGateway,
    argv: &[String],
    cwd: &Path,
) -> Result<Completion, Failure> {
    let request = match prepare(argv, cwd) {
        Ok(request) => request,
        Err(usage) => return Ok(Completion::Usage(usage)),
    };
    let encoded = stringify_json(&request.params)
        .map_err(|error| Failure::Error(error.to_string()))?
        .unwrap_or_else(|| "null".into());
    let params =
        RawValue::from_string(encoded).map_err(|error| Failure::Error(error.to_string()))?;
    gateway
        .request(request.method, params)
        .await
        .map(Completion::Result)
        .map_err(Failure::Error)
}

fn prepare(argv: &[String], cwd: &Path) -> Result<Request, &'static str> {
    let mut params = JsObject::new();
    let method = match argv.first().map(String::as_str) {
        Some("list") => {
            params.insert(
                "includeHistory".into(),
                JsValue::Bool(has(argv, "--history")),
            );
            if let Some(limit) = read_string(argv, "--limit").and_then(parse_decimal_integer) {
                params.insert("limit".into(), JsValue::Number(limit));
            }
            "cron_list"
        }
        Some("create") => {
            let message = read_string(argv, "--message").ok_or(CREATE_USAGE)?;
            let session = read_string(argv, "--session").ok_or(CREATE_USAGE)?;
            let once = read_string(argv, "--once");
            let cron = read_string(argv, "--cron");
            let mut schedule = JsObject::new();
            if let Some(run_at) = once {
                schedule.insert("type".into(), text("once"));
                schedule.insert("runAt".into(), text(run_at));
            } else if let Some(expression) = cron {
                schedule.insert("type".into(), text("cron"));
                schedule.insert("expression".into(), text(expression));
            } else {
                return Err(CREATE_USAGE);
            }
            let inferred_channel = session
                .split_once(':')
                .map(|(channel, _)| channel)
                .filter(|channel| !channel.is_empty())
                .unwrap_or("cli");
            params.insert("message".into(), text(message));
            params.insert("sessionKey".into(), text(session));
            params.insert(
                "channelKey".into(),
                text(read_string(argv, "--channel").unwrap_or(inferred_channel)),
            );
            params.insert(
                "projectKey".into(),
                text(read_string(argv, "--project").unwrap_or(&cwd.to_string_lossy())),
            );
            params.insert("schedule".into(), JsValue::Object(schedule));
            insert_optional(&mut params, "timezone", read_string(argv, "--timezone"));
            "cron_create"
        }
        Some("delete") => {
            // TS argv[1] ?? flag, deliberately not a positional-argument scan.
            // Thus `delete --task x` sends the literal taskId "--task".
            let task = task_id(argv)
                .filter(|task| !task.is_empty())
                .ok_or(DELETE_USAGE)?;
            params.insert("taskId".into(), text(task));
            params.insert(
                "stopRunning".into(),
                JsValue::Bool(has(argv, "--stop-running")),
            );
            "cron_delete"
        }
        Some("stop") => {
            let task = task_id(argv);
            let run = read_string(argv, "--run");
            if task.is_none_or(str::is_empty) && run.is_none() {
                return Err(STOP_USAGE);
            }
            insert_optional(&mut params, "taskId", task);
            insert_optional(&mut params, "runId", run);
            "cron_stop"
        }
        _ => return Err(USAGE),
    };
    Ok(Request {
        method,
        params: JsValue::Object(params),
    })
}

fn text(value: &str) -> JsValue {
    JsValue::String(value.into())
}

fn insert_optional(object: &mut JsObject, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        object.insert(key.into(), text(value));
    }
}

fn has(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|value| value == flag)
}

fn read_string<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    let index = argv.iter().position(|value| value == flag)?;
    argv.get(index + 1)
        .map(String::as_str)
        .filter(|value| !value.is_empty() && !value.starts_with("--"))
}

fn task_id(argv: &[String]) -> Option<&str> {
    argv.get(1)
        .map(String::as_str)
        .or_else(|| read_string(argv, "--task"))
}

/// Number.parseInt(value, 10), followed by Number.isFinite (pilotdeck.ts:546-552).
fn parse_decimal_integer(value: &str) -> Option<f64> {
    let trimmed = paths::js_trim(value);
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    let length = digits.bytes().take_while(u8::is_ascii_digit).count();
    if length == 0 {
        return None;
    }
    let prefix = trimmed.len() - digits.len();
    trimmed[..prefix + length]
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::Message;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn request_parameters_and_usage_match_executed_ts_entry() {
        let oracle: Value =
            serde_json::from_str(include_str!("../tests/c1_cron_oracle.json")).unwrap();
        let rows = oracle["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 27);
        let mut checked = 0;
        for row in rows {
            if row["available"] == false || row["rpcError"] == true {
                continue;
            }
            checked += 1;
            let argv = row["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            match prepare(&argv, Path::new("/fixture/project")) {
                Ok(request) => {
                    assert_eq!(row["calls"].as_array().unwrap().len(), 1, "{argv:?}");
                    let call = &row["calls"][0];
                    let method = match call["method"].as_str().unwrap() {
                        "cronList" => "cron_list",
                        "cronCreate" => "cron_create",
                        "cronDelete" => "cron_delete",
                        "cronStop" => "cron_stop",
                        other => panic!("unexpected baseline method: {other}"),
                    };
                    assert_eq!(request.method, method, "{argv:?}");
                    assert_eq!(
                        stringify_json(&request.params).unwrap().unwrap(),
                        call["json"].as_str().unwrap(),
                        "{argv:?}"
                    );
                    assert_eq!(row["exitCode"], 0, "{argv:?}");
                }
                Err(usage) => {
                    assert_eq!(row["calls"], json!([]), "{argv:?}");
                    assert_eq!(row["stderr"], json!([usage]), "{argv:?}");
                    assert_eq!(row["exitCode"], 1, "{argv:?}");
                }
            }
        }
        assert_eq!(checked, 24);
    }

    async fn exchange(argv: &[&str], reply: Value) -> (Result<Completion, Failure>, Vec<Value>) {
        timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let client = async {
                let gateway =
                    RemoteGateway::connect(&format!("ws://{address}/ws"), "fixture-token")
                        .await
                        .unwrap();
                execute_connected(gateway, &args(argv), Path::new("/fixture/project")).await
            };
            let server = async {
                let (socket, _) = listener.accept().await.unwrap();
                let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
                let hello = websocket.next().await.unwrap().unwrap();
                let hello: Value = serde_json::from_str(hello.to_text().unwrap()).unwrap();
                assert_eq!(hello["clientName"], "cli");
                assert_eq!(hello["clientVersion"], "0.1.0");
                assert_eq!(hello["token"], "fixture-token");
                websocket
                    .send(Message::Text(
                        json!({"type":"hello_ok","serverInfo":{}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                let mut requests = Vec::new();
                while let Some(frame) = websocket.next().await {
                    match frame.unwrap() {
                        Message::Text(raw) => {
                            let request: Value = serde_json::from_str(&raw).unwrap();
                            let mut response = reply.clone();
                            response["type"] = json!("response");
                            response["id"] = request["id"].clone();
                            requests.push(request);
                            websocket
                                .send(Message::Text(response.to_string().into()))
                                .await
                                .unwrap();
                        }
                        Message::Close(_) => {
                            let _ = websocket.flush().await;
                            return requests;
                        }
                        _ => {}
                    }
                }
                panic!("cron must close its owned socket after the command");
            };
            tokio::join!(client, server)
        })
        .await
        .expect("owned cron mock must finish within five seconds")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn every_online_command_uses_cli_transport_and_closes() {
        for (argv, method) in [
            (vec!["list", "--history", "--limit", "12tail"], "cron_list"),
            (
                vec![
                    "create",
                    "--session",
                    "cli:s",
                    "--message",
                    "hello",
                    "--cron",
                    "* * * * *",
                ],
                "cron_create",
            ),
            (vec!["delete", "task", "--stop-running"], "cron_delete"),
            (vec!["stop", "--run", "run"], "cron_stop"),
        ] {
            let (result, requests) =
                exchange(&argv, json!({"ok":true,"result":{"tasks":[]}})).await;
            let Completion::Result(result) = result.unwrap() else {
                panic!("expected RPC result");
            };
            assert_eq!(stringify_json(&result).unwrap().unwrap(), r#"{"tasks":[]}"#);
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["method"], method);
            let expected = prepare(&args(&argv), Path::new("/fixture/project")).unwrap();
            let expected: Value =
                serde_json::from_str(&stringify_json(&expected.params).unwrap().unwrap()).unwrap();
            assert_eq!(requests[0]["params"], expected);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn online_usage_and_rpc_error_also_release_socket() {
        let (result, requests) = exchange(&["create"], json!({"ok":true,"result":{}})).await;
        assert!(matches!(result, Ok(Completion::Usage(CREATE_USAGE))));
        assert!(requests.is_empty());
        let (result, requests) = exchange(
            &["list"],
            json!({"ok":false,"error":{"code":"bad_request","message":"fixture failure"}}),
        )
        .await;
        assert!(matches!(result, Err(Failure::Error(message)) if message == "fixture failure"));
        assert_eq!(requests.len(), 1);
    }
}
