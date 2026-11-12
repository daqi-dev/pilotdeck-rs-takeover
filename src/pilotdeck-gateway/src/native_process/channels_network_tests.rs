//! Causal coverage for the real ConfigStore -> NativeChannels -> shared HTTP
//! dispatcher path. Only the peer endpoints and channel lifecycle are doubles.
//! The process owns every socket/home and never changes global proxy variables.
use super::{NativeChannels, NativeChannelsOptions};
use crate::{
    channel::{
        loader::ChannelLoaderMounts,
        protocol::{
            BoxFuture, ChannelAdapter, ChannelError, ChannelHandle, ChannelKey, ChannelStartDeps,
        },
    },
    config::{Env, store::ConfigStore, store::StoreOptions},
    runtime::model::http::ModelHttpTransport,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::{JoinHandle, JoinSet},
};

const BUDGET: Duration = Duration::from_secs(5);
const CHILD_ROOT: &str = "PDRS_H1_CHANNEL_NETWORK_CHILD_ROOT";
const STORE_BASE: &str = "schemaVersion: 1\nagent: {model: fixture/model}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://127.0.0.1:1/v1\n      apiKey: mock-only\n      models:\n        model: {}\n";

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pdrs-h1-channel-network-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).expect("H1 isolated network-test root");
        for name in ["home", "pilot", "project", "tmp"] {
            std::fs::create_dir(root.join(name)).expect("H1 isolated child directory");
        }
        Self(root)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<String>>>);
impl Trace {
    fn push(&self, value: impl Into<String>) {
        self.0.lock().expect("H1 network trace").push(value.into());
    }
    fn rows(&self) -> Vec<String> {
        self.0.lock().expect("H1 network trace").clone()
    }
    fn clear(&self) {
        self.0.lock().expect("H1 network trace").clear();
    }
}

struct KeptAdapter {
    key: ChannelKey,
    lifecycle: Trace,
}
impl ChannelAdapter for KeptAdapter {
    fn channel_key(&self) -> &ChannelKey {
        &self.key
    }
    fn start(
        &self,
        _deps: ChannelStartDeps,
    ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
        self.lifecycle.push("start");
        let lifecycle = self.lifecycle.clone();
        Box::pin(async move { Ok(Arc::new(KeptHandle(lifecycle)) as Arc<dyn ChannelHandle>) })
    }
}
struct KeptHandle(Trace);
impl ChannelHandle for KeptHandle {
    fn stop(&self, reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        self.0.push(format!("stop:{reason}"));
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Copy)]
enum Peer {
    Origin,
    Proxy(SocketAddr),
}

struct OwnedPeer {
    address: SocketAddr,
    trace: Trace,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), String>>,
}
impl OwnedPeer {
    async fn start(peer: Peer) -> Self {
        let address = match peer {
            // Unlike 127.0.0.1/localhost, ::1 is not in buildNoProxy's fixed
            // bypass list. Binding it needs no macOS loopback alias changes.
            Peer::Origin => SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0),
            Peer::Proxy(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        };
        let listener = TcpListener::bind(address)
            .await
            .unwrap_or_else(|error| panic!("H1 owned loopback prerequisite {address}: {error}"));
        let address = listener.local_addr().expect("H1 owned local port");
        let trace = Trace::default();
        let recorded = trace.clone();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut workers = JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let (stream, remote) = accepted.map_err(|error| error.to_string())?;
                        if !remote.ip().is_loopback() {
                            return Err(format!("unexpected non-loopback client {remote}"));
                        }
                        let trace = recorded.clone();
                        workers.spawn(async move {
                            tokio::time::timeout(BUDGET, serve_peer(peer, stream, trace))
                                .await
                                .map_err(|error| format!("owned peer deadline: {error}"))?
                        });
                    }
                    completed = workers.join_next(), if !workers.is_empty() => {
                        completed.expect("H1 nonempty worker set")
                            .map_err(|error| error.to_string())??;
                    }
                }
            }
            drop(listener);
            while let Some(completed) = workers.join_next().await {
                completed.map_err(|error| error.to_string())??;
            }
            Ok(())
        });
        Self {
            address,
            trace,
            stop: Some(stop),
            task,
        }
    }
    async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let result = tokio::time::timeout(BUDGET, &mut self.task)
            .await
            .expect("H1 owned peer joins before deadline")
            .expect("H1 owned peer task must not panic");
        assert!(
            result.is_ok(),
            "H1 peer evidence/cleanup error: {result:?}; {:?}",
            self.trace.rows()
        );
    }
}
impl Drop for OwnedPeer {
    fn drop(&mut self) {
        // Dropping the task also drops its JoinSet and aborts its owned
        // connections. It never scans or stops any other process/socket.
        self.task.abort();
    }
}

async fn read_head(stream: &mut TcpStream) -> Result<String, String> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 16 * 1024 {
            return Err("owned HTTP headers exceed fixture bound".into());
        }
        let mut byte = [0];
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|error| error.to_string())?;
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

async fn serve_peer(peer: Peer, mut stream: TcpStream, trace: Trace) -> Result<(), String> {
    let head = read_head(&mut stream).await?;
    let line = head.lines().next().ok_or("missing HTTP request line")?;
    match peer {
        Peer::Origin => {
            let words = line.split_whitespace().collect::<Vec<_>>();
            if words.len() != 3 || words[0] != "GET" || words[2] != "HTTP/1.1" {
                return Err(format!("unexpected origin request {line}"));
            }
            trace.push(words[1]);
            let body = format!("owned-origin:{}", words[1]);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .map_err(|error| error.to_string())?;
            stream.shutdown().await.map_err(|error| error.to_string())?;
        }
        Peer::Proxy(origin) => {
            let expected = format!("CONNECT {origin} HTTP/1.1");
            if line != expected {
                return Err(format!(
                    "proxy refuses unowned target {line}; expected {expected}"
                ));
            }
            // Never forward the client-supplied authority. The only allowed
            // destination is the listener this test has already bound.
            let mut target = TcpStream::connect(origin)
                .await
                .map_err(|error| error.to_string())?;
            trace.push(line);
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .map_err(|error| error.to_string())?;
            tokio::io::copy_bidirectional(&mut stream, &mut target)
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn configuration(proxy: Option<(SocketAddr, Option<&str>)>, origin: SocketAddr) -> String {
    let proxy = proxy.map_or_else(String::new, |(address, bypass)| {
        let bypass = bypass.map_or_else(String::new, |value| format!("  noProxy: '{value}'\n"));
        format!("proxy:\n  url: http://{address}\n{bypass}")
    });
    format!(
        "{STORE_BASE}{proxy}adapters:\n  signal:\n    enabled: true\n    extra:\n      restUrl: http://{origin}/signal\n      account: owned-fixture\n"
    )
}

async fn request(network: &impl ModelHttpTransport, origin: SocketAddr, name: &str) {
    let response = tokio::time::timeout(BUDGET, async {
        network
            .send(
                Request::builder()
                    .method("GET")
                    .uri(format!("http://{origin}/{name}"))
                    .body(Full::new(Bytes::new()))
                    .expect("H1 owned GET request"),
            )
            .await
    })
    .await
    .expect("H1 next real HTTP exchange is bounded")
    .expect("H1 next real HTTP exchange succeeds");
    assert_eq!(response.status(), 200, "H1 owned origin HTTP status");
    let body = tokio::time::timeout(BUDGET, response.into_body().collect())
        .await
        .expect("H1 owned response body is bounded")
        .expect("H1 owned response body completes")
        .to_bytes();
    assert_eq!(body.as_ref(), format!("owned-origin:/{name}").as_bytes());
}

async fn publish(
    store: &ConfigStore,
    root: &Path,
    origin: SocketAddr,
    proxy: Option<(SocketAddr, Option<&str>)>,
    observed: &Trace,
    expected_path: &str,
) {
    let before = observed.rows().len();
    std::fs::write(
        root.join("pilot/pilotdeck.yaml"),
        configuration(proxy, origin),
    )
    .expect("H1 write only the isolated config file");
    tokio::time::timeout(BUDGET, store.reload("h1-owned-proxy-update"))
        .await
        .expect("H1 real ConfigStore publication is bounded")
        .expect("H1 real ConfigStore reload succeeds");
    let rows = observed.rows();
    assert_eq!(
        &rows[before..],
        &[expected_path],
        "H1 observed actual ConfigStore changed_paths"
    );
}

async fn exercise(root: &Path) {
    let origin = OwnedPeer::start(Peer::Origin).await;
    let first = OwnedPeer::start(Peer::Proxy(origin.address)).await;
    let second = OwnedPeer::start(Peer::Proxy(origin.address)).await;
    std::fs::write(
        root.join("pilot/pilotdeck.yaml"),
        configuration(Some((first.address, None)), origin.address),
    )
    .expect("H1 initial isolated config");
    let env = Env::from([
        (
            "HOME".into(),
            root.join("home").to_string_lossy().into_owned(),
        ),
        (
            "PILOT_HOME".into(),
            root.join("pilot").to_string_lossy().into_owned(),
        ),
    ]);
    let mut options = StoreOptions::new(root.join("pilot"), root.join("project"), env);
    options.loaded_at = Arc::new(|| "1970-01-01T00:00:00.000Z".into());
    let store = ConfigStore::new(options).expect("H1 load the real ConfigStore");
    let config = store
        .snapshot()
        .get("config")
        .cloned()
        .expect("H1 loaded config object");
    let lifecycle = Trace::default();
    let constructor_trace = lifecycle.clone();
    let mut mounts = ChannelLoaderMounts::default();
    mounts.register(
        "signal",
        Arc::new(move |_options| {
            constructor_trace.push("construct");
            Ok(Arc::new(KeptAdapter {
                key: ChannelKey::new("signal"),
                lifecycle: constructor_trace.clone(),
            }) as Arc<dyn ChannelAdapter>)
        }),
    );
    let channels = Arc::new(NativeChannels::new_with_loader_mounts(
        NativeChannelsOptions {
            pilot_home: root.join("pilot"),
            config: Arc::new(config),
            weixin: Default::default(),
            weixin_maker: None,
            feishu: Default::default(),
            log: Some(Arc::new(|_, _| {})),
            report_status: Some(Arc::new(|_, _| {})),
            close_host: None,
        },
        mounts,
    ));
    let adapters = channels.create_startup_configured();
    assert_eq!(adapters.len(), 1, "H1 one controlled startup adapter");
    for adapter in adapters {
        channels
            .server()
            .hot_start_channel(adapter)
            .await
            .expect("H1 adapter starts");
    }
    assert_eq!(lifecycle.rows(), ["construct", "start"]);
    // This handle predates every config event. All subsequent requests use
    // it, so reconstructing only a new network cannot make the test pass.
    let network = channels.network.clone();
    channels
        .attach_config_store(&store)
        .expect("H1 attach actual native reload callback");
    // The watcher can publish before the server enables adapter reload.
    // A held network must nevertheless use the new proxy immediately.
    std::fs::write(
        root.join("pilot/pilotdeck.yaml"),
        configuration(Some((second.address, None)), origin.address),
    )
    .unwrap();
    store.reload("h1-early-proxy").await.unwrap();
    request(network.as_ref(), origin.address, "early-proxy").await;
    assert_eq!(second.trace.rows().len(), 1);
    std::fs::write(
        root.join("pilot/pilotdeck.yaml"),
        configuration(Some((first.address, None)), origin.address),
    )
    .unwrap();
    store.reload("h1-early-proxy-restore").await.unwrap();
    second.trace.clear();
    channels.activate_config_reload();
    let observed = Trace::default();
    let paths = observed.clone();
    let observer = store.subscribe(Arc::new(move |event| {
        paths.push(
            event
                .changed_paths
                .iter()
                .map(|value| value.to_utf8().expect("H1 ASCII changed path"))
                .collect::<Vec<_>>()
                .join(","),
        );
    }));
    assert_eq!(store.listener_count(), 2);

    request(network.as_ref(), origin.address, "startup").await;
    assert_eq!(
        (first.trace.rows().len(), second.trace.rows().len()),
        (1, 0),
        "H1 ::1 is not a fixed loopback bypass"
    );

    publish(
        &store,
        root,
        origin.address,
        Some((second.address, None)),
        &observed,
        "proxy.url",
    )
    .await;
    request(network.as_ref(), origin.address, "new-proxy").await;
    assert_eq!(
        (first.trace.rows().len(), second.trace.rows().len()),
        (1, 1),
        "H1 proxy.url publishes to the already-held dispatcher"
    );

    publish(
        &store,
        root,
        origin.address,
        Some((second.address, Some("[::1]"))),
        &observed,
        "proxy.noProxy",
    )
    .await;
    request(network.as_ref(), origin.address, "explicit-bypass").await;
    assert_eq!(
        (first.trace.rows().len(), second.trace.rows().len()),
        (1, 1),
        "H1 proxy.noProxy changes the next request route"
    );

    // Restore a genuinely proxied route before deleting proxy. Removing a
    // proxy while noProxy already bypasses it would not prove replacement by
    // the direct dispatcher, and could hide an ignored root-level event.
    publish(
        &store,
        root,
        origin.address,
        Some((second.address, None)),
        &observed,
        "proxy.noProxy",
    )
    .await;
    request(network.as_ref(), origin.address, "bypass-removed").await;
    assert_eq!(
        (first.trace.rows().len(), second.trace.rows().len()),
        (1, 2),
        "H1 removing noProxy restores the current proxy before deletion"
    );

    publish(&store, root, origin.address, None, &observed, "proxy").await;
    request(network.as_ref(), origin.address, "removed-proxy").await;
    assert_eq!(
        (first.trace.rows().len(), second.trace.rows().len()),
        (1, 2),
        "H1 proxy removal installs direct transport"
    );

    publish(
        &store,
        root,
        origin.address,
        Some((first.address, None)),
        &observed,
        "proxy",
    )
    .await;
    request(network.as_ref(), origin.address, "replacement-proxy").await;
    assert_eq!(
        (first.trace.rows().len(), second.trace.rows().len()),
        (2, 2),
        "H1 root proxy replacement reaches the retained dispatcher"
    );
    assert_eq!(
        lifecycle.rows(),
        ["construct", "start"],
        "H1 proxy-only publication never reconstructs/stops an adapter"
    );

    tokio::time::timeout(BUDGET, channels.close())
        .await
        .expect("H1 native close is bounded")
        .expect("H1 native close succeeds");
    assert_eq!(
        store.listener_count(),
        1,
        "H1 native network callback unsubscribes"
    );
    publish(
        &store,
        root,
        origin.address,
        Some((second.address, None)),
        &observed,
        "proxy.url",
    )
    .await;
    request(network.as_ref(), origin.address, "closed-url-change").await;
    publish(&store, root, origin.address, None, &observed, "proxy").await;
    request(network.as_ref(), origin.address, "closed-removal").await;
    assert_eq!(
        (first.trace.rows().len(), second.trace.rows().len()),
        (4, 2),
        "H1 real post-close store events cannot alter retained network"
    );
    assert_eq!(
        lifecycle.rows(),
        ["construct", "start", "stop:server-shutdown"],
        "H1 no post-close reconstruction and one owned stop"
    );
    assert_eq!(
        origin.trace.rows(),
        [
            "/early-proxy",
            "/startup",
            "/new-proxy",
            "/explicit-bypass",
            "/bypass-removed",
            "/removed-proxy",
            "/replacement-proxy",
            "/closed-url-change",
            "/closed-removal"
        ]
    );
    observer.unsubscribe();
    drop(network);
    first.finish().await;
    second.finish().await;
    origin.finish().await;
}

#[tokio::test]
async fn config_store_proxy_events_reach_the_existing_network() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        exercise(Path::new(&root)).await;
        println!("H1_CHANNEL_NETWORK_CHILD_COMPLETED");
        return;
    }
    let root = Root::new();
    let module = module_path!()
        .split_once("::")
        .expect("H1 crate-qualified module path")
        .1;
    let exact = format!("{module}::config_store_proxy_events_reach_the_existing_network");
    let mut command =
        tokio::process::Command::new(std::env::current_exe().expect("H1 current test executable"));
    command
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env_clear()
        .env(CHILD_ROOT, &root.0)
        .env("HOME", root.0.join("home"))
        .env("PILOT_HOME", root.0.join("pilot"))
        .env("TMPDIR", root.0.join("tmp"))
        .current_dir(root.0.join("project"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Cargo uses these platform loader variables for dynamically linked test
    // dependencies. This narrow whitelist never inherits PATH, proxy/no_proxy,
    // NODE_EXTRA_CA_CERTS, or the caller's HOME. Normal static builds need none.
    for key in if cfg!(target_os = "macos") {
        &["DYLD_LIBRARY_PATH"][..]
    } else if cfg!(target_os = "linux") {
        &["LD_LIBRARY_PATH"][..]
    } else {
        &[][..]
    } {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().expect("H1 exact isolated helper starts");
    let mut stdout = child.stdout.take().expect("H1 child stdout pipe");
    let mut stderr = child.stderr.take().expect("H1 child stderr pipe");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        )
    })
    .await;
    match result {
        Ok(Ok((status, _, _))) => assert!(
            status.success()
                && String::from_utf8_lossy(&out).contains("H1_CHANNEL_NETWORK_CHILD_COMPLETED"),
            "H1 exact helper status={status}; stdout={}\nstderr={}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        ),
        failure => {
            let kill = child.start_kill();
            let reaped = tokio::time::timeout(BUDGET, child.wait()).await;
            let drained = tokio::time::timeout(BUDGET, async {
                tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))
            })
            .await;
            panic!(
                "H1 exact helper original failure={failure:?}; kill={kill:?}; reap={reaped:?}; drain={drained:?}; stdout={}\nstderr={}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&err)
            );
        }
    }
}
