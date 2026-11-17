//! Real-subprocess auto-start regression tests (P0, wp4fix review).
//!
//! The client's `ensure_local_gateway_running` holds an exclusive flock on
//! `<home>/run/daemon.lock` from *before* spawning `server serve` until
//! `ready.json` appears. The gateway's own single-instance lock must live on
//! a different path (`run/gateway.instance.lock`): sharing the client's
//! election lock made every auto-started gateway fail its `try_lock` against
//! the parent and exit before writing `ready.json`, so all CLI auto-starts
//! timed out. These tests spawn a real OS process with its own file-lock
//! table — in-process mocks cannot reproduce the collision.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use pilotdeck_gateway::public_v1::bootstrap::{
    self, ReadyFile, default_loopback_addr, initialize_instance, load_local_instance_and_token,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agentcpm-pd-wp4fix-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const CHILD_HOME_ENV: &str = "WP4_AUTOSTART_CHILD_HOME";
const CHILD_STOP_ENV: &str = "WP4_AUTOSTART_CHILD_STOP";

/// Child entrypoint: spawned (as this same test binary) by the parent test
/// below. Without the child environment it is a no-op so the ordinary suite
/// run stays green; the parent drives the real gateway lifecycle here.
#[test]
fn child_gateway_serve_entrypoint() {
    let (Ok(home), Ok(stop_flag)) = (std::env::var(CHILD_HOME_ENV), std::env::var(CHILD_STOP_ENV))
    else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let stop_path = PathBuf::from(&stop_flag);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let _ = bootstrap::serve_v1_gateway(
            PathBuf::from(home),
            None,
            default_loopback_addr(),
            async move {
                while !stop_path.exists() && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            },
        )
        .await
        .expect("child gateway served");
    });
}

/// P0 regression: while a parent process holds the client election lock
/// (`run/daemon.lock`, exactly like `ensure_local_gateway_running` does
/// around the spawn), a really-spawned gateway must still boot, write
/// `ready.json`, and serve a working `/v1/ws` session.
#[tokio::test]
async fn real_subprocess_autostart_survives_the_client_election_lock() {
    let tmp = TempDir::new("autostart");
    let home = tmp.0.join("pd-home");
    initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
    let (file, token) = load_local_instance_and_token(&home, None).unwrap();

    // The client's election lock: exclusive, held from before the spawn
    // until ready.json shows up (autostart.rs `ensure_local_gateway_running`).
    let run_dir = home.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let election_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(run_dir.join("daemon.lock"))
        .unwrap();
    election_lock.try_lock().expect("election lock is free");

    let stop_flag = tmp.0.join("stop.flag");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_gateway_serve_entrypoint"])
        .env(CHILD_HOME_ENV, &home)
        .env(CHILD_STOP_ENV, &stop_flag)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child gateway");

    // Wait for ready.json — with the colliding lock path the child exited
    // ALREADY_EXISTS before writing it and this deadline failed.
    let ready_path = home.join("run").join("ready.json");
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(raw) = std::fs::read_to_string(&ready_path)
                && let Ok(ready) = serde_json::from_str::<ReadyFile>(&raw)
            {
                return ready;
            }
            if let Ok(Some(status)) = child.try_wait() {
                panic!("child gateway exited early with {status}");
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("auto-started gateway wrote ready.json within 10s");

    // The client drops its election lock once ready appears.
    drop(election_lock);

    // A real connection completes the pd-connect/1 handshake.
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let mut request = ready.ws_url.as_str().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("connect to the auto-started gateway");
    let init = json!({
        "jsonrpc": "2.0",
        "id": "r1",
        "method": "initialize",
        "params": {
            "abi_version": 1,
            "api_version": "agentcpm-pd.operation/1",
            "features": ["subscriptions"],
            "client": {"name": "wp4fix-autostart", "version": "0.1.0"}
        }
    });
    ws.send(Message::Text(init.to_string().into()))
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("initialize response")
        .unwrap()
        .unwrap();
    let Message::Text(frame_txt) = frame else {
        panic!("expected a text frame");
    };
    let response: Value = serde_json::from_str(&frame_txt).unwrap();
    assert_eq!(
        response["result"]["target"]["instanceId"], file.instance_id,
        "handshake identifies the auto-started instance"
    );
    let _ = ws.close(None).await;

    // Shutdown: the stop flag ends the child's serve loop (and its ready.json
    // cleanup runs).
    std::fs::write(&stop_flag, b"stop").unwrap();
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || child.wait()),
    )
    .await
    .expect("child gateway exits after the stop flag")
    .expect("wait task joined")
    .expect("child wait succeeded");
    assert!(status.success(), "child exited cleanly: {status}");
}
