//! probeGatewayServer (src/gateway/client/probeServer.ts:10-31): the fixed
//! local-server probe that `pilotdeck cron`, the default CliChannel and the
//! TUI run before deciding whether a PilotDeck server is reachable.
use crate::paths::{self, Env};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

/// `options.url ?? "http://127.0.0.1:18789"`: none of the CLI callers pass a
/// URL, so the address is fixed.
const PROBE_ADDR: ([u8; 4], u16) = ([127, 0, 0, 1], 18789);
/// `options.timeoutMs ?? 200`, an abort over the whole `fetch`.
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// `GET /health` answered 2xx within the budget, and a non-empty token file
/// exists (`readGatewayAuthToken`). Any failure, including an unreadable token
/// path, is "no server" exactly like the baseline's `catch`. Returns the token
/// `connectRemoteGatewayIfAvailable` would connect with.
pub fn probe_gateway_server(env: &Env, cwd: &Path) -> Option<String> {
    if !health_ok() {
        return None;
    }
    let path = paths::gateway_token_path(env, cwd).ok()?;
    let bytes = std::fs::read(path).ok()?;
    let token = crate::paths::js_trim(&String::from_utf8_lossy(&bytes)).to_owned();
    (!token.is_empty()).then_some(token)
}

fn health_ok() -> bool {
    let started = Instant::now();
    let remaining = || PROBE_TIMEOUT.checked_sub(started.elapsed());
    let address = SocketAddr::from(PROBE_ADDR);
    let Ok(mut stream) = TcpStream::connect_timeout(&address, PROBE_TIMEOUT) else {
        return false;
    };
    let request = "GET /health HTTP/1.1\r\nHost: 127.0.0.1:18789\r\nConnection: close\r\n\r\n";
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = Vec::new();
    let mut buffer = [0_u8; 256];
    while !head.windows(2).any(|pair| pair == b"\r\n") {
        let Some(left) = remaining().filter(|left| !left.is_zero()) else {
            return false;
        };
        if stream.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return false,
            Ok(read) => head.extend_from_slice(&buffer[..read]),
        }
    }
    // `response.ok`: a status line `HTTP/x y ...` with 200 <= y <= 299.
    let line = String::from_utf8_lossy(&head);
    line.split_whitespace()
        .nth(1)
        .and_then(|status| status.parse::<u16>().ok())
        .is_some_and(|status| (200..300).contains(&status))
}
