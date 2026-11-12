//! Channel WebSocket dialing through the shared HTTP proxy and CA connector.
//! Only the dial URI changes scheme; the original WebSocket handshake request
//! keeps its path, query, Origin and authentication headers.
use hyper::Uri;
use hyper_util::rt::TokioIo;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Error, client::IntoClientRequest, error::UrlError, handshake::client::Response},
};
use tower_service::Service as _;

mod deflate;

use crate::runtime::model::http::{
    ProxyConfig, ProxyIo, ProxySettings, proxy_https_connector, webpki_client_config,
};

pub(crate) type ChannelWebSocket = WebSocketStream<Box<dyn ProxyIo>>;

pub(crate) async fn connect_async<R>(
    request: R,
    proxy: Option<&ProxySettings>,
) -> Result<(ChannelWebSocket, Response), Error>
where
    R: IntoClientRequest + Unpin,
{
    connect_with(
        request,
        ProxyConfig::from_settings(proxy),
        webpki_client_config(),
    )
    .await
}

async fn connect_with<R>(
    request: R,
    proxy: Option<ProxyConfig>,
    tls: rustls::ClientConfig,
) -> Result<(ChannelWebSocket, Response), Error>
where
    R: IntoClientRequest + Unpin,
{
    let request = request.into_client_request()?;
    let mut parts = request.uri().clone().into_parts();
    parts.scheme = Some(match request.uri().scheme_str() {
        Some("ws") => hyper::http::uri::Scheme::HTTP,
        Some("wss") => hyper::http::uri::Scheme::HTTPS,
        _ => return Err(Error::Url(UrlError::UnsupportedUrlScheme)),
    });
    let destination = Uri::from_parts(parts)
        .map_err(|error| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error)))?;
    let mut connector = proxy_https_connector(tls, proxy);
    let stream = connector
        .call(destination)
        .await
        .map_err(|error| Error::Io(std::io::Error::other(error)))?;
    // HttpsConnector has already performed target TLS (and proxy TLS when
    // configured). client_async_with_config performs only the WS handshake.
    deflate::handshake(request, TokioIo::new(stream)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    use std::{path::PathBuf, sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        task::JoinSet,
    };
    use tokio_tungstenite::tungstenite::Message;

    use crate::runtime::model::http::rooted_client_config;

    struct ExactHandshake;
    impl tokio_tungstenite::tungstenite::handshake::server::Callback for ExactHandshake {
        fn on_request(
            self,
            request: &tokio_tungstenite::tungstenite::handshake::server::Request,
            response: tokio_tungstenite::tungstenite::handshake::server::Response,
        ) -> Result<
            tokio_tungstenite::tungstenite::handshake::server::Response,
            tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
        > {
            assert_eq!(
                request.uri().to_string(),
                "/prefix/api/websocket?token=a%2Fb%27"
            );
            assert_eq!(request.headers()["origin"], "https://fixture.invalid");
            assert!(!request.headers().contains_key("proxy-authorization"));
            Ok(response)
        }
    }

    const BUDGET: Duration = Duration::from_secs(10);

    // Fixed cc7c4e6 HA/MM adapter probes use real WS peers: ::1 routes
    // through CONNECT, NO_PROXY=[::1] bypasses it, and 127.0.0.1 always
    // bypasses it. No DNS, hosts changes, fixed ports or external services.
    #[tokio::test]
    async fn websocket_proxy_no_proxy_and_loopback_preserve_the_handshake() {
        for (host, no_proxy, proxied) in [
            ("::1", "", true),
            ("::1", "[::1]", false),
            ("127.0.0.1", "", false),
        ] {
            let target = TcpListener::bind((host, 0)).await.expect("H1 WS peer");
            let target_address = target.local_addr().expect("H1 WS address");
            let proxy = TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("H1 CONNECT proxy");
            let proxy_url = format!("http://fixture:secret@{}", proxy.local_addr().unwrap());
            let destination = format!("ws://{target_address}/prefix/api/websocket?token=a%2Fb%27");
            let mut peers = JoinSet::new();
            peers.spawn(async move {
                let (stream, _) = target.accept().await.expect("H1 WS accepted");
                let mut socket = tokio_tungstenite::accept_hdr_async(stream, ExactHandshake)
                    .await
                    .expect("H1 WS handshake");
                let message = socket.next().await.unwrap().expect("H1 WS frame");
                assert_eq!(message.to_text().unwrap(), "fixture-message");
                socket.send(message).await.expect("H1 WS echo");
                assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
            });
            // Keep the unused listener bound: routing a bypass case through
            // it cannot accidentally pass by falling back after refusal.
            let mut unused_proxy = Some(proxy);
            if proxied {
                let proxy = unused_proxy.take().unwrap();
                peers.spawn(async move {
                    let (mut client, _) = proxy.accept().await.expect("H1 proxy accepted");
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(client.read_u8().await.expect("H1 CONNECT header"));
                        assert!(header.len() < 8192);
                    }
                    let header = String::from_utf8(header).unwrap();
                    assert!(header.starts_with(&format!("CONNECT {target_address} HTTP/1.1\r\n")));
                    assert!(header.lines().filter_map(|line| line.split_once(':')).any(
                        |(key, value)| {
                            key.eq_ignore_ascii_case("proxy-authorization")
                                && value.trim() == "Basic Zml4dHVyZTpzZWNyZXQ="
                        }
                    ));
                    let mut upstream = TcpStream::connect(target_address).await.unwrap();
                    client
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
            let proxy = ProxyConfig::from_lookup(|key| match key {
                "PILOTDECK_PROXY" => Some(proxy_url.clone()),
                "NO_PROXY" => Some(no_proxy.to_owned()),
                _ => None,
            });
            tokio::time::timeout(BUDGET, async {
                let mut request = destination.into_client_request().unwrap();
                request
                    .headers_mut()
                    .insert("origin", "https://fixture.invalid".parse().unwrap());
                let (mut socket, response) = connect_with(
                    request,
                    proxy,
                    rooted_client_config(rustls::RootCertStore::empty()),
                )
                .await
                .expect("H1 client connects through the selected route");
                assert_eq!(response.status(), hyper::StatusCode::SWITCHING_PROTOCOLS);
                socket
                    .send(Message::Text("fixture-message".into()))
                    .await
                    .unwrap();
                assert_eq!(
                    socket.next().await.unwrap().unwrap().to_text().unwrap(),
                    "fixture-message"
                );
                socket.close(None).await.unwrap();
                drop(socket);
                while let Some(result) = peers.join_next().await {
                    result.expect("H1 owned peer finishes");
                }
            })
            .await
            .expect("H1 proxy/WS exchange is bounded");
            drop(unused_proxy);
        }
    }

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn certificates(dir: &std::path::Path) {
        std::fs::write(
            dir.join("openssl.cnf"),
            concat!(
                "[req]\ndistinguished_name=dn\n[dn]\n[ca]\n",
                "basicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\n",
                "[leaf]\nbasicConstraints=critical,CA:FALSE\n",
                "keyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n",
                "subjectAltName=IP:127.0.0.1\n",
            ),
        )
        .unwrap();
        for args in [
            vec![
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                "ca.key",
                "-out",
                "ca.pem",
                "-days",
                "2",
                "-subj",
                "/CN=H1 throwaway CA",
                "-config",
                "openssl.cnf",
                "-extensions",
                "ca",
            ],
            vec![
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                "leaf.key",
                "-out",
                "leaf.csr",
                "-subj",
                "/CN=H1 loopback WS",
                "-config",
                "openssl.cnf",
            ],
            vec![
                "x509",
                "-req",
                "-in",
                "leaf.csr",
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-out",
                "leaf.pem",
                "-days",
                "2",
                "-extfile",
                "openssl.cnf",
                "-extensions",
                "leaf",
            ],
        ] {
            let mut command = tokio::process::Command::new("openssl");
            command
                .args(args)
                .current_dir(dir)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", dir)
                .env("PILOT_HOME", dir.join("pilot"))
                .kill_on_drop(true);
            let output = tokio::time::timeout(BUDGET, command.output())
                .await
                .expect("H1 certificate generation is bounded")
                .expect("local openssl");
            assert!(
                output.status.success(),
                "H1 certificate generation: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    // A fresh process is essential: the shared CA loader intentionally
    // snapshots NODE_EXTRA_CA_CERTS once. Never mutate test-runner env or
    // disable certificate validation to make the WSS fixture connect.
    #[tokio::test]
    async fn websocket_extra_ca_uses_the_shared_default_roots() {
        const CHILD: &str = "PDRS_H1_WS_CA_CHILD";
        if let Some(url) = std::env::var_os(CHILD) {
            let expected = std::env::var("PDRS_H1_WS_CA_EXPECT").unwrap() == "trusted";
            let result = tokio::time::timeout(BUDGET, connect_async(url.to_str().unwrap(), None))
                .await
                .expect("H1 child WSS connection is bounded");
            assert_eq!(result.is_ok(), expected, "H1 extra CA determines WSS trust");
            if let Ok((mut socket, _)) = result {
                assert_eq!(
                    socket.next().await.unwrap().unwrap().to_text().unwrap(),
                    "tls-ready"
                );
                socket.close(None).await.unwrap();
            }
            return;
        }
        let scratch = Scratch(
            std::env::temp_dir().join(format!("pilotdeck-h1-ws-ca-{}", uuid::Uuid::new_v4())),
        );
        std::fs::create_dir(&scratch.0).unwrap();
        certificates(&scratch.0).await;
        let certs = CertificateDer::pem_file_iter(scratch.0.join("leaf.pem"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_file(scratch.0.join("leaf.key")).unwrap();
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("wss://{}/api/websocket", listener.local_addr().unwrap());
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let mut peers = JoinSet::new();
        peers.spawn(async move {
            for trusted in [false, true] {
                let (stream, _) = listener.accept().await.unwrap();
                let result = acceptor.accept(stream).await;
                if trusted {
                    let mut socket =
                        tokio_tungstenite::accept_async(result.expect("H1 trusted TLS"))
                            .await
                            .expect("H1 trusted WS handshake");
                    socket
                        .send(Message::Text("tls-ready".into()))
                        .await
                        .unwrap();
                    assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
                } else {
                    assert!(
                        result.is_err(),
                        "H1 untrusted CA must reject before WS upgrade"
                    );
                }
            }
        });
        for trusted in [false, true] {
            let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "channel::ws_transport::tests::websocket_extra_ca_uses_the_shared_default_roots", "--nocapture"])
                .env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &scratch.0).env("PILOT_HOME", scratch.0.join("pilot"))
                .env("NODE_OPTIONS", "--use-bundled-ca")
                .env(CHILD, &url).env("PDRS_H1_WS_CA_EXPECT", if trusted { "trusted" } else { "untrusted" })
                .kill_on_drop(true);
            if trusted {
                command.env("NODE_EXTRA_CA_CERTS", scratch.0.join("ca.pem"));
            }
            let output = tokio::time::timeout(BUDGET + Duration::from_secs(2), command.output())
                .await
                .expect("H1 CA child exits within its budget")
                .expect("H1 CA child starts");
            assert!(
                output.status.success(),
                "H1 CA child: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"));
        }
        tokio::time::timeout(BUDGET, async {
            while let Some(result) = peers.join_next().await {
                result.unwrap();
            }
        })
        .await
        .expect("H1 TLS peers finish");
    }
}
