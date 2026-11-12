//! Actual optional-tool processes, with isolated home and task-owned PIDs.
use super::*;
use std::fs;

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("pilotdeck-whatsapp-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join("home")).unwrap();
        fs::create_dir_all(path.join("pilot")).unwrap();
        Self(path)
    }
    fn env(&self, mode: &str) -> Vec<(OsString, OsString)> {
        vec![
            ("HOME".into(), self.0.join("home").into_os_string()),
            ("PILOT_HOME".into(), self.0.join("pilot").into_os_string()),
            ("TMPDIR".into(), self.0.clone().into_os_string()),
            ("PDRS_BRIDGE_MODE".into(), mode.into()),
        ]
    }
    fn pid(&self, name: &str) -> rustix::process::Pid {
        rustix::process::Pid::from_raw(
            fs::read_to_string(self.0.join(name))
                .unwrap()
                .parse()
                .unwrap(),
        )
        .unwrap()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
async fn wait_file(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("isolated bridge created its readiness file");
}
fn spawn(dir: &Sandbox, mode: &str) -> bridge::BridgeTask {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/h1-whatsapp/process-bridge.mjs");
    let node = std::env::var_os("NODE22").unwrap_or_else(|| "node".into());
    bridge::spawn(
        &PathBuf::from(node),
        script.to_str().unwrap(),
        None,
        &dir.env(mode),
        None,
    )
    .unwrap()
}

#[tokio::test]
async fn drains_large_stdout_before_readiness_and_reaps_the_bridge() {
    let dir = Sandbox::new();
    let bridge = spawn(&dir, "stdout");
    wait_file(&dir.0.join("ready")).await;
    let pid = dir.pid("bridge-pid");
    bridge.join().await.unwrap();
    assert!(
        rustix::process::test_kill_process(pid).is_err(),
        "owned leader reaped"
    );
}

#[tokio::test]
async fn stop_kills_owned_descendants_before_reaping_the_group_anchor() {
    let dir = Sandbox::new();
    let bridge = spawn(&dir, "descendant");
    wait_file(&dir.0.join("descendant-pid")).await;
    let parent = dir.pid("bridge-pid");
    let descendant = dir.pid("descendant-pid");
    bridge.join().await.unwrap();
    assert!(rustix::process::test_kill_process(parent).is_err());
    // A non-child may briefly remain a zombie until init reaps it. It must
    // have no live execution; /proc gives that distinction on Linux.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if rustix::process::test_kill_process(descendant).is_err() {
                break;
            }
            #[cfg(target_os = "linux")]
            if fs::read_to_string(format!("/proc/{}/stat", descendant.as_raw_nonzero())).is_ok_and(
                |stat| {
                    stat.rsplit_once(") ")
                        .is_some_and(|(_, tail)| tail.starts_with("Z "))
                },
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned descendant stopped");
}

#[tokio::test]
async fn ignored_term_is_killed_and_joined_after_the_five_second_grace() {
    let dir = Sandbox::new();
    let bridge = spawn(&dir, "ignore-term");
    wait_file(&dir.0.join("ready")).await;
    let pid = dir.pid("bridge-pid");
    let began = std::time::Instant::now();
    bridge.join().await.unwrap();
    assert!(began.elapsed() >= Duration::from_secs(5));
    assert!(began.elapsed() < Duration::from_secs(7));
    assert!(rustix::process::test_kill_process(pid).is_err());
}

struct PendingTransport;
impl ModelHttpTransport for PendingTransport {
    fn send(
        &self,
        _request: hyper::Request<http_body_util::Full<bytes::Bytes>>,
    ) -> crate::runtime::model::http::TransportFuture {
        crate::runtime::model::http::TransportFuture::io(std::future::pending())
    }
}

#[tokio::test]
async fn dropping_start_during_readiness_does_not_lose_the_child_owner() {
    let dir = Sandbox::new();
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/h1-whatsapp/process-bridge.mjs");
    let responder = Arc::new(super::tests::Responder::default());
    let channel = WhatsAppChannel::new(
        WhatsAppChannelOptions {
            bridge_path: Some(script.to_string_lossy().as_ref().into()),
            bridge_url: Some("http://127.0.0.1:18081".into()),
            node_executable: Some(PathBuf::from(
                std::env::var_os("NODE22").unwrap_or_else(|| "node".into()),
            )),
            bridge_env: dir.env("normal"),
            env: Some(Arc::new(|_| None)),
            transport: Some(Arc::new(PendingTransport)),
            ..Default::default()
        },
        js_helper_interaction(responder),
    );
    let waiter = tokio::spawn(channel.start(ChannelStartDeps::default()));
    wait_file(&dir.0.join("ready")).await;
    let pid = dir.pid("bridge-pid");
    waiter.abort();
    let _ = waiter.await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while rustix::process::test_kill_process(pid).is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled start's independent cleanup reaped its child");
    channel.0.stop_receipt(None).await.unwrap();
}
