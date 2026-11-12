//! Rust gateway entry. The legacy proxy requires an explicit deployment setting.
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use pilotdeck_gateway::{ProxyError, http_proxy::Proxy};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpListener;
#[tokio::main]
async fn main() -> Result<(), ProxyError> {
    // Node's process-wide SIGXFSZ ignore (a write past RLIMIT_FSIZE reports
    // EFBIG instead of killing the process); register the same disposition
    // at startup through tokio's safe signal API so every fs write in this
    // process matches Node (#621). The registration is held for the
    // process lifetime (a static OnceLock in the library).
    #[cfg(unix)]
    pilotdeck_gateway::ensure_sigxfsz_ignored();
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    let explicit_native = pilotdeck_gateway::native_process::native_requested(first.as_deref());
    // Native is the production default. The explicit --native flag still
    // outranks a legacy proxy deployment setting; malformed settings are
    // rejected before either path starts. Proxy mode remains an explicit
    // diagnostic choice and never supplies an implicit business fallback.
    let mode_native = pilotdeck_gateway::native_process::gateway_mode(
        std::env::var_os("PILOTDECK_GATEWAY_MODE").as_deref(),
    )?;
    if explicit_native || mode_native {
        return pilotdeck_gateway::native_process::run(
            pilotdeck_gateway::native_process::Options::parse(native_arguments(
                explicit_native,
                first,
                args,
            ))?,
        )
        .await;
    }
    let listen: SocketAddr = std::env::var("PILOTDECK_PROXY_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:28789".into())
        .parse()?;
    if listen.ip() != std::net::Ipv4Addr::LOCALHOST {
        return Err("proxy must listen on 127.0.0.1".into());
    }
    let upstream = std::env::var("PILOTDECK_PROXY_UPSTREAM")
        .unwrap_or_else(|_| "http://127.0.0.1:28790".into());
    let uri: hyper::Uri = upstream.parse()?;
    if uri.scheme_str() != Some("http") || uri.authority().is_none() || uri.path() != "/" {
        return Err("upstream must be an http origin (no path)".into());
    }
    let proxy = Arc::new(Proxy {
        upstream: upstream.trim_end_matches('/').to_owned(),
        client: Client::builder(TokioExecutor::new()).build_http(),
    });
    let listener = TcpListener::bind(listen).await?;
    let address = listener.local_addr()?;
    println!("PilotDeck server listening: http://{address}");
    println!("WebSocket: ws://{address}/ws");
    eprintln!("metadata cwd: {}", std::env::current_dir()?.display());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        eprintln!("downstream accept: {error}");
                        if fatal_accept_error(&error) {
                            return Err(error.into());
                        }
                        // EMFILE and transient network errors must not stop the listener
                        // or spin at full CPU while resources remain exhausted.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let proxy = proxy.clone();
                tokio::spawn(async move {
                    if let Err(error) = proxy.serve_connection(socket).await {
                        eprintln!("downstream HTTP: {error}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

fn fatal_accept_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::Unsupported
    )
}

/// #1354: `--native` is consumed by the router itself, so only the remaining
/// arguments reach the native options parser; the config item's routing
/// leaves every argument — `first` included — to that parser.
fn native_arguments(
    explicit: bool,
    first: Option<std::ffi::OsString>,
    rest: impl Iterator<Item = std::ffi::OsString>,
) -> impl Iterator<Item = std::ffi::OsString> {
    if explicit { None } else { first }.into_iter().chain(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transient_accept_errors_do_not_stop_listener() {
        for error in [
            std::io::Error::from_raw_os_error(24),
            std::io::ErrorKind::ConnectionAborted.into(),
            std::io::ErrorKind::Interrupted.into(),
        ] {
            assert!(!fatal_accept_error(&error));
        }
        assert!(fatal_accept_error(&std::io::ErrorKind::InvalidInput.into()));
    }
}
