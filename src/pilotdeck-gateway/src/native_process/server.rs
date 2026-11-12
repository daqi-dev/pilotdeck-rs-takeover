//! The product entry behind `pilotdeck server`: the baseline's
//! `startGatewayServer` options (src/gateway/server/GatewayServer.ts:47-80)
//! over the same assembly, listener and stop machine the isolated `--native`
//! entry uses. None of the `--native` isolation rules apply here — the pilot
//! home, the project root, the bind host and the token come from the caller
//! exactly as the baseline CLI resolves them (src/cli/pilotdeck.ts:35-299),
//! and the inherited environment is handed to the assembly unchanged.
use super::{Options, assembly, production_api_web, serve_until_signal};
use crate::{ProxyError, config::Env};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

/// GatewayServer.ts `options.port ?? 18789`; the CLI applies the same default.
pub const DEFAULT_PORT: f64 = 18789.0;

/// The baseline has no drain budget of its own (its stop waits on each owner);
/// this is the `--native` entry's default bound for the same stop machine.
const DRAIN_BUDGET: Duration = Duration::from_millis(5000);

/// What `pilotdeck server` resolved before starting the gateway.
pub struct ServerOptions {
    /// `resolvePilotHome(env)`.
    pub pilot_home: PathBuf,
    /// `process.cwd()`.
    pub project: PathBuf,
    /// `os.homedir()`; `None` falls back to the process's own lookup.
    pub os_home: Option<PathBuf>,
    /// `--host` / `PILOTDECK_GATEWAY_HOST`; `None` is `127.0.0.1`.
    pub host: Option<String>,
    /// `--port` / `PILOTDECK_GATEWAY_PORT` / 18789, already a finite number
    /// (`parseInt`). The range is judged at listen time, like Node.
    pub port: f64,
    /// `PILOTDECK_GATEWAY_ALLOW_REMOTE === "1"`.
    pub allow_remote_host: bool,
    /// `PILOTDECK_GATEWAY_TOKEN || PILOTDECK_TOKEN`: `None` when both are
    /// unset or empty.
    pub token: Option<String>,
    /// `resolveGatewayTokenPath()`, consulted only without `token`.
    pub token_path: PathBuf,
    /// The agent-loop sidecar executable each session is run in.
    pub sidecar: PathBuf,
    /// `process.env`.
    pub env: Env,
    /// `resolveBuiltinSkillsRoot()`: the bundled skills directory.
    pub builtin_skills_root: Option<PathBuf>,
    /// The builtin plugins directory; `None` ships no builtin plugins.
    pub builtin_plugins_root: Option<PathBuf>,
}

/// The `GatewayServer` fields the CLI prints once the listener is up.
pub struct Listening {
    pub url: String,
    pub ws_url: String,
    /// Set only when the token came from `ensureGatewayAuthToken()`.
    pub token_path: Option<PathBuf>,
}

/// `Number.parseInt(text, 10)` narrowed by `Number.isFinite`, the reading both
/// `--port` and `PILOTDECK_GATEWAY_PORT` get (src/cli/pilotdeck.ts:212,517-525).
pub fn parse_port(text: &str) -> Option<f64> {
    let port = crate::runtime::mcp::client::parse_int(text);
    port.is_finite().then_some(port)
}

/// Start the gateway, call `listening` once it accepts connections, and
/// return after SIGTERM / SIGINT stopped it. Blocks the calling thread on its
/// own runtime. Every `Err` is a startup or stop failure whose text the CLI
/// prints as the baseline prints a thrown `Error`'s message.
pub fn serve(options: ServerOptions, listening: impl FnOnce(&Listening)) -> Result<(), ProxyError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            // Node ignores SIGXFSZ process-wide; see the gateway binary's entry.
            #[cfg(unix)]
            crate::ensure_sigxfsz_ignored();
            serve_on_runtime(options, listening).await
        })
}

async fn serve_on_runtime(
    options: ServerOptions,
    listening: impl FnOnce(&Listening),
) -> Result<(), ProxyError> {
    // The sidecar has no baseline counterpart (the baseline runs the agent
    // loop in-process by default), so neither does this message. Checking it
    // here keeps a broken install from surfacing only on the first turn.
    if !options.sidecar.is_file() {
        return Err(format!(
            "pilotdeck server requires the agent loop sidecar at {}",
            options.sidecar.display()
        )
        .into());
    }
    ensure_router_dir(&options.pilot_home);
    let process = Options {
        home: options.pilot_home.clone(),
        // `fallbackProjectRoot: pilotHome` (src/cli/pilotdeck.ts:103-115).
        project: options.pilot_home.clone(),
        project_root: options.project.clone(),
        // `listen` and `token_file` are read by the `--native` entry's `run`
        // only; this entry binds and authenticates from `ServerOptions`.
        listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        token_file: options.token_path.clone(),
        sidecar: options.sidecar.clone(),
        sidecar_args: Vec::new(),
        drain_budget: DRAIN_BUDGET,
        // `resolveWorkspaceSnapshotConfig(env)` (workspaceSnapshot.ts:25-34)
        // reads the process environment; the entry keeps it as given and the
        // snapshot config resolves and trims it.
        storage_root: options.env.get("PILOTDECK_STORAGE_ROOT").cloned(),
        os_home: Some(
            options
                .os_home
                .clone()
                .or_else(std::env::home_dir)
                .unwrap_or_else(|| PathBuf::from("/")),
        ),
        builtin_skills_root: options.builtin_skills_root.clone(),
        builtin_plugins_root: options.builtin_plugins_root.clone(),
    };
    // createLocalGateway precedes startGatewayServer in the baseline, so the
    // host and token checks below run with the gateway already built.
    let (host, feishu_webhook, switch_handles) =
        assembly::start_with_webhook(process, options.env.clone()).await?;
    let module_host = switch_handles.module_host.clone().ok_or_else(|| {
        ProxyError::from("module host provider was not constructed during assembly")
    })?;
    let (server, started) = match bind(&options, &host, feishu_webhook, module_host).await {
        Ok(bound) => bound,
        Err(error) => {
            let _ = host.shutdown(DRAIN_BUDGET).await;
            return Err(error);
        }
    };
    let host_name = options.host.as_deref().unwrap_or("127.0.0.1").to_owned();
    serve_until_signal(
        DRAIN_BUDGET,
        host,
        server,
        switch_handles,
        |address| {
            // `http://${host}:${port}` with the host as given and the port
            // the listener actually got (GatewayServer.ts:69-72).
            let port = address.port();
            listening(&Listening {
                url: format!("http://{host_name}:{port}"),
                ws_url: format!("ws://{host_name}:{port}/ws"),
                token_path: started,
            });
        },
        || {},
    )
    .await
}

/// ProjectRouterEventBusProvider.ensureEventsPath
/// (src/cli/ProjectRouterEventBusProvider.ts:32-41), which the baseline runs
/// while building the local gateway: `<pilot home>/router` exists after a
/// start, and a legacy `router-events.jsonl` moves to `router/events.jsonl`.
/// Both steps are best-effort there too. The events themselves are appended
/// by the project router's event sink (`assembly::RouterEventLog`).
fn ensure_router_dir(pilot_home: &Path) {
    let directory = pilot_home.join("router");
    let _ = std::fs::create_dir_all(&directory);
    let (events, legacy) = (
        directory.join("events.jsonl"),
        pilot_home.join("router-events.jsonl"),
    );
    if std::fs::metadata(&events).is_err() && std::fs::metadata(&legacy).is_ok() {
        let _ = std::fs::rename(&legacy, &events);
    }
}

/// startGatewayServer up to `await listen(...)`, in its order: the remote
/// binding checks, the token, then the listener. Returns the token path when
/// the token was read or generated rather than supplied.
async fn bind(
    options: &ServerOptions,
    host: &crate::runtime::turn::gateway::TurnGateway,
    feishu_webhook: crate::native_http::WebhookResolver,
    module_host: std::sync::Arc<crate::module_host::runtime::RuntimeHost>,
) -> Result<(crate::native_ws::NativeWsServer, Option<PathBuf>), ProxyError> {
    let name = options.host.as_deref().unwrap_or("127.0.0.1");
    let loopback = name == "127.0.0.1" || name == "localhost";
    if !loopback {
        if !options.allow_remote_host {
            return Err("GatewayServer remote binding requires allowRemoteHost=true.".into());
        }
        if options
            .token
            .as_deref()
            .is_none_or(|token| crate::metadata::js_trim(token).is_empty())
        {
            return Err("GatewayServer remote binding requires an explicit auth token.".into());
        }
    }
    let (token, token_path) = match options.token.as_deref() {
        Some(token) if !token.is_empty() => (token.to_owned(), None),
        _ => (
            ensure_auth_token(&options.token_path)?,
            Some(options.token_path.clone()),
        ),
    };
    let port = listen_port(options.port)?;
    let address = resolve_host(name, port).await?;
    let server = crate::native_ws::NativeWsServer::bind_as(
        address,
        loopback,
        host.clone(),
        token.clone(),
        env!("CARGO_PKG_VERSION").into(),
    )
    .await
    .map_err(|error| listen_error(&error, address))?
    .with_webhook(feishu_webhook)
    // `publicApprovals` and `publicHostCapabilities` (pilotdeck.ts:217-218).
    .with_module_host(crate::native_http::ModuleHost {
        approvals: crate::module_host::approvals::production_config(token.clone(), &options.env),
        host: crate::module_host::production_config(token.clone(), module_host),
    })
    .with_api_web(production_api_web(token, options.pilot_home.clone()))
    // `staticAssetsPath: resolve(projectRoot, "ui/dist")` (pilotdeck.ts:224).
    .with_static_root(options.project.join("ui/dist"));
    Ok((server, token_path))
}

/// Node's `validatePort` on `server.listen({ port })`: an integer in
/// 0..=65535, otherwise ERR_SOCKET_BAD_PORT with the number as `inspect`
/// prints it.
fn listen_port(port: f64) -> Result<u16, ProxyError> {
    if port.fract() == 0.0 && (0.0..65536.0).contains(&port) {
        return Ok(port as u16);
    }
    let shown = if port == 0.0 && port.is_sign_negative() {
        "-0".to_owned()
    } else {
        pilotdeck_protocol::number::number_to_string(port)
    };
    Err(format!("options.port should be >= 0 and < 65536. Received type number ({shown}).").into())
}

/// `server.listen(port, host)`: an IP literal binds as is, a name goes
/// through `dns.lookup` and binds its first address in resolver order, and
/// the empty string is Node's "no host" (the unspecified IPv6 address, which
/// is dual-stack where the OS allows it).
async fn resolve_host(name: &str, port: u16) -> Result<SocketAddr, ProxyError> {
    if name.is_empty() {
        return Ok(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)));
    }
    if let Ok(ip) = name.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    tokio::net::lookup_host((name, port))
        .await
        .ok()
        .and_then(|mut addresses| addresses.next())
        .ok_or_else(|| format!("getaddrinfo ENOTFOUND {name}").into())
}

/// Node's `uvExceptionWithHostPort` message for a failed listen:
/// `listen EADDRINUSE: address already in use 127.0.0.1:18789`.
fn listen_error(error: &std::io::Error, address: SocketAddr) -> ProxyError {
    match crate::node_io::lookup(error) {
        Some((code, description)) => format!(
            "listen {code}: {description} {}:{}",
            address.ip(),
            address.port()
        )
        .into(),
        None => format!("listen {error}").into(),
    }
}

/// `ensureGatewayAuthToken()` (src/gateway/server/authToken.ts:27-41): the
/// trimmed content of an existing token file, or a fresh 32-byte base64url
/// token written as `<token>\n` with mode 0600 under a 0700 directory.
fn ensure_auth_token(path: &Path) -> Result<String, ProxyError> {
    let shown = path.to_string_lossy();
    let node = |error: &std::io::Error, syscall: &str| -> ProxyError {
        crate::node_fs_message_or_unknown(error, syscall, Some(shown.as_ref())).into()
    };
    // `existsSync` is false for every stat failure, not only ENOENT.
    if std::fs::metadata(path).is_ok() {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|error| node(&error, "open"))?
            .read_to_end(&mut bytes)
            .map_err(|error| node(&error, "read"))?;
        let content = String::from_utf8_lossy(&bytes);
        let token = crate::metadata::js_trim(&content);
        if !token.is_empty() {
            return Ok(token.to_owned());
        }
    }
    use base64::Engine;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes()?);
    if let Some(directory) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(directory).map_err(|error| {
            let directory = directory.to_string_lossy();
            ProxyError::from(crate::node_fs_message_or_unknown(
                &error,
                "mkdir",
                Some(directory.as_ref()),
            ))
        })?;
    }
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    use std::io::Write;
    open.open(path)
        .map_err(|error| node(&error, "open"))?
        .write_all(format!("{token}\n").as_bytes())
        .map_err(|error| node(&error, "write"))?;
    Ok(token)
}

/// `randomBytes(32)` from the OS generator.
fn random_bytes() -> Result<[u8; 32], ProxyError> {
    use std::io::Read;
    let mut bytes = [0_u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_reading_follows_parse_int_and_node_range() {
        assert_eq!(parse_port("38515xyz"), Some(38515.0));
        assert_eq!(parse_port(" 80"), Some(80.0));
        assert_eq!(parse_port("abc"), None);
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("--host"), None);
        assert_eq!(listen_port(0.0).ok(), Some(0));
        assert_eq!(listen_port(65535.0).ok(), Some(65535));
        assert_eq!(
            listen_port(99999.0).unwrap_err().to_string(),
            "options.port should be >= 0 and < 65536. Received type number (99999)."
        );
        assert_eq!(
            listen_port(-5.0).unwrap_err().to_string(),
            "options.port should be >= 0 and < 65536. Received type number (-5)."
        );
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_reused_or_generated_like_the_baseline() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("pdrs-server-token-{}", uuid::Uuid::new_v4()));
        let path = root.join("nested/server-token");
        let token = ensure_auth_token(&path).expect("generate");
        assert_eq!(token.len(), 43);
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            format!("{token}\n")
        );
        let mode =
            |path: &Path| std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&root.join("nested")) & 0o077, 0);
        assert_eq!(ensure_auth_token(&path).expect("reuse"), token);
        std::fs::write(&path, " \n").expect("blank");
        let fresh = ensure_auth_token(&path).expect("regenerate");
        assert_ne!(fresh, token);
        std::fs::write(&path, "  kept by hand \n").expect("manual");
        assert_eq!(ensure_auth_token(&path).expect("trim"), "kept by hand");
        let _ = std::fs::remove_dir_all(&root);
    }
}
