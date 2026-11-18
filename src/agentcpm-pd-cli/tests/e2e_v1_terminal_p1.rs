//! Terminal echo latency measurement (t015 P1, brief §5 / 007 §10.3):
//! single-keypress round trips through the REAL product path —
//! host `term.input` -> attach stdio -> attach<->Gateway `/v1/ws`
//! (`terminals.input`) -> pty master write -> kernel line-discipline ECHO ->
//! master read -> ring -> `sub.event` -> attach -> host `term.output`.
//!
//! The host is this test process (spawned attach `--machine`), so both ends
//! of the measured window share one clock. The environment is isolated:
//! temp `AGENTCPM_PD_HOME`, an isolated `HOME` (the gateway's bash never
//! reads user dotfiles), and `TERM=dumb` with a fixed prompt.
//!
//! Modes (brief §5 CI policy):
//! - smoke (default): N=50, asserts p95 < 50 ms — the PR-gate variant that
//!   tolerates CI jitter;
//! - full (`AGENTCPM_PD_P1_FULL=1`): N=300, asserts p95 < 30 ms — the frozen
//!   P1 gate, to be run and archived on cb8 and mbp2.
//!
//! Both modes print a JSON summary (p50/p95/max + histogram) to stdout.

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::time::{Duration, Instant};

use base64::Engine;
use common_v1::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Keys pressed per round: printable ASCII only (skipping control chars and
/// the frame-delimiter-sensitive ones), cycled.
const KEYSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Inter-key jitter so the pty cannot batch echoes.
const KEY_JITTER: Duration = Duration::from_millis(6);

/// Warmup samples discarded before measuring (bash banner/prompt noise).
const WARMUP: usize = 20;

fn decode_bytes(value: &Value) -> Vec<u8> {
    value
        .as_str()
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .unwrap_or_default()
}

struct Sample {
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
    count: usize,
    histogram: Vec<(u64, usize)>,
}

fn measure(samples: &[f64]) -> Sample {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        let idx = ((q * sorted.len() as f64).ceil() as usize).saturating_sub(1);
        sorted[idx.min(sorted.len() - 1)]
    };
    let mut histogram = Vec::new();
    let mut bucket = 0u64;
    let mut count = 0usize;
    for sample in &sorted {
        let b = (*sample as u64) / 5;
        if b != bucket && count > 0 {
            histogram.push((bucket, count));
            count = 0;
        }
        bucket = b;
        count += 1;
    }
    if count > 0 {
        histogram.push((bucket, count));
    }
    Sample {
        p50_ms: at(0.50),
        p95_ms: at(0.95),
        max_ms: sorted.last().copied().unwrap_or(0.0),
        count: sorted.len(),
        histogram,
    }
}

#[tokio::test]
async fn terminal_echo_p1_latency() {
    let full = std::env::var("AGENTCPM_PD_P1_FULL").is_ok();
    let (rounds, budget_ms) = if full { (300, 30.0) } else { (50, 50.0) };

    let home = TestHome::new("term-p1");
    init_home(home.path());
    // Isolated HOME for the gateway (and its bash): no user dotfiles, no
    // inherited prompt configuration.
    let isolated_home = home.path().join("shell-home");
    std::fs::create_dir_all(&isolated_home).unwrap();
    let instance_id = init_home_instance(home.path());
    let out = {
        let mut cmd = base_command(home.path());
        cmd.env("HOME", &isolated_home);
        cmd.env("TERM", "dumb");
        cmd.args([
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            "p-p1",
            "--name",
            "P1",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"p1\"}}"
            ),
        ]);
        match tokio::time::timeout(CLI_TIMEOUT, cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => panic!("project register spawn/wait failed: {e}"),
            Err(_) => panic!("project register timed out"),
        }
    };
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let _ = expect_ok(
        "project.register",
        &CliOutcome {
            code: out.status.code().unwrap_or(-1),
            stdout: text,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
    );

    let out = await_cli(
        home.path(),
        &[
            "terminal",
            "create",
            "--output",
            "json",
            "--project-id",
            "p-p1",
            "--rows",
            "24",
            "--columns",
            "80",
        ],
    );
    let terminal = expect_ok("terminals.create", &out)["terminal"].clone();
    let terminal_id = terminal["terminalId"].as_str().unwrap().to_owned();

    let mut cmd = base_command(home.path());
    cmd.env("HOME", &isolated_home);
    cmd.env("TERM", "dumb");
    cmd.args([
        "terminal",
        "attach",
        "--machine",
        "--terminal-id",
        &terminal_id,
    ]);
    cmd.stdin(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn terminal attach");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stdin = child.stdin.take().expect("piped stdin");
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Value>(512);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(frame) = serde_json::from_str::<Value>(&line)
                && frame_tx.send(frame).await.is_err()
            {
                break;
            }
        }
    });

    // Drain the banner/prompt frames until the stream settles: settle means
    // a 300 ms window with no new frame, re-armed by every frame that lands
    // inside one (the banner can arrive after the first quiet moment, so the
    // wait must keep re-checking, not block on the next frame).
    let settle = tokio::time::timeout(Duration::from_secs(20), async {
        #[allow(clippy::never_loop)] // the 'outer label is needed for break 'outer from inner
        'outer: loop {
            let _ = frame_rx.recv().await.expect("frame channel open");
            loop {
                // In-flight frame: restart the quiet window.
                if frame_rx.try_recv().is_ok() {
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                if frame_rx.try_recv().is_err() {
                    break 'outer;
                }
            }
        }
    })
    .await;
    assert!(settle.is_ok(), "the attach's prompt settles within 20s");

    let mut samples: Vec<f64> = Vec::with_capacity(rounds);
    for i in 0..(rounds + WARMUP) {
        let key = KEYSET[i % KEYSET.len()];
        let payload = json!({
            "jsonrpc": "2.0",
            "id": format!("p1-{i}"),
            "method": "term.input",
            "params": {"bytesBase64": base64::engine::general_purpose::STANDARD.encode([key])},
        });
        let mut line = serde_json::to_string(&payload).unwrap();
        line.push('\n');
        let started = Instant::now();
        stdin
            .write_all(line.as_bytes())
            .await
            .expect("write to attach stdin");
        stdin.flush().await.expect("flush attach stdin");
        // The echo of THIS keypress closes the measurement.
        let echoed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = frame_rx.recv().await.expect("frame channel open");
                let matched = frame["method"] == "term.output"
                    && decode_bytes(&frame["params"]["bytesBase64"]).contains(&key);
                if matched {
                    return;
                }
            }
        })
        .await;
        if echoed.is_err() {
            panic!(
                "key `{}` was not echoed within 10s (round {i})",
                key as char
            );
        }
        let dt_ms = started.elapsed().as_secs_f64() * 1000.0;
        if i >= WARMUP {
            samples.push(dt_ms);
        }
        tokio::time::sleep(KEY_JITTER).await;
    }

    let sample = measure(&samples);
    let summary = json!({
        "mode": if full { "full" } else { "smoke" },
        "rounds": sample.count,
        "warmupDiscarded": WARMUP,
        "p50Ms": sample.p50_ms,
        "p95Ms": sample.p95_ms,
        "maxMs": sample.max_ms,
        "budgetMs": budget_ms,
        "histogram5msBuckets": sample
            .histogram
            .iter()
            .map(|(bucket, count)| json!({"bucketMs": bucket * 5, "count": count}))
            .collect::<Vec<_>>(),
        "platform": std::env::consts::OS,
    });
    println!("P1 {}", serde_json::to_string(&summary).unwrap());

    assert!(
        sample.p95_ms < budget_ms,
        "echo p95 {:.2}ms exceeds the {:.0}ms budget ({:?})",
        sample.p95_ms,
        budget_ms,
        samples
    );

    // Close the terminal so the shell does not outlive the suite.
    let input_path = home.path().join("close.json");
    std::fs::write(
        &input_path,
        serde_json::to_string(&json!({
            "terminalId": terminal_id,
            "expectedServerEpoch": terminal["serverEpoch"],
            "mode": "force",
        }))
        .unwrap(),
    )
    .unwrap();
    let _ = await_cli(
        home.path(),
        &[
            "api",
            "invoke",
            "--output",
            "json",
            "terminals.close",
            "--input-json",
            input_path.to_str().unwrap(),
        ],
    );
    // The attach process waits out its stdin reader on exit; close the pipe
    // so the reader observes EOF and the child can terminate.
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
}
