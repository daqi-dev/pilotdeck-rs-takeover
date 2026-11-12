//! Real loopback TLS peers; CA trust is isolated in fresh test processes.
use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::{path::PathBuf, time::Duration};
use tokio::net::TcpListener;
const BUDGET: Duration = Duration::from_secs(20);
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
            "/CN=H1 loopback email",
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

fn config(mode: &str, port: u16) -> Config {
    Config {
        address: "agent@example.test".into(),
        password: "mock-password".into(),
        imap_host: "127.0.0.1".into(),
        smtp_host: "127.0.0.1".into(),
        imap_port: port,
        smtp_port: if mode == "smtp-implicit" { 465 } else { port },
        imap_tls: mode == "imap-implicit",
        smtp_tls: true,
        subject: "Message".into(),
        poll_interval: Duration::from_secs(45),
    }
}
async fn peer(stream: TcpStream, acceptor: tokio_rustls::TlsAcceptor, mode: &str, trusted: bool) {
    let implicit = mode.ends_with("implicit");
    let imap = mode.starts_with("imap");
    let mut socket: BufReader<Io> = if implicit {
        let handshake = acceptor.accept(stream).await;
        if !trusted {
            assert!(handshake.is_err(), "untrusted peer must reject TLS");
            return;
        }
        BufReader::new(Box::new(handshake.expect("trusted TLS")))
    } else {
        BufReader::new(Box::new(stream))
    };
    socket
        .write_all(if imap {
            b"* OK local TLS mock\r\n"
        } else {
            b"220 local TLS mock\r\n"
        })
        .await
        .unwrap();
    socket.flush().await.unwrap();
    let mut upgraded = implicit;
    let mut auth_step = 0;
    loop {
        let mut line = String::new();
        assert!(socket.read_line(&mut line).await.unwrap() > 0);
        let command = line.trim_end();
        if imap {
            let (tag, command) = command.split_once(' ').unwrap();
            if command == "CAPABILITY" {
                assert!(!upgraded);
                socket
                    .write_all(
                        format!("* CAPABILITY IMAP4rev1 STARTTLS\r\n{tag} OK capabilities\r\n")
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
            } else if command == "STARTTLS" {
                assert!(!upgraded);
                socket
                    .write_all(format!("{tag} OK upgrade\r\n").as_bytes())
                    .await
                    .unwrap();
                socket.flush().await.unwrap();
                assert!(socket.buffer().is_empty());
                let handshake = acceptor.accept(socket.into_inner()).await;
                if !trusted {
                    assert!(
                        handshake.is_err(),
                        "untrusted STARTTLS must reject before LOGIN"
                    );
                    return;
                }
                socket = BufReader::new(Box::new(handshake.unwrap()));
                upgraded = true;
            } else if command.starts_with("LOGIN ") {
                assert!(upgraded, "credentials must follow TLS");
                assert_eq!(command, "LOGIN \"agent@example.test\" \"mock-password\"");
                socket
                    .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                    .await
                    .unwrap();
            } else if command == "SELECT INBOX" {
                socket
                    .write_all(format!("* 0 EXISTS\r\n{tag} OK selected\r\n").as_bytes())
                    .await
                    .unwrap();
            } else if command == "LOGOUT" {
                socket
                    .write_all(format!("* BYE closed\r\n{tag} OK logout\r\n").as_bytes())
                    .await
                    .unwrap();
                socket.flush().await.unwrap();
                break;
            } else {
                panic!("unexpected IMAP TLS command {command:?}");
            }
        } else if command.starts_with("EHLO ") {
            if upgraded {
                socket
                    .write_all(b"250-local\r\n250 AUTH LOGIN\r\n")
                    .await
                    .unwrap();
            } else {
                socket
                    .write_all(b"250-local\r\n250-STARTTLS\r\n250 AUTH PLAIN\r\n")
                    .await
                    .unwrap();
            }
        } else if command == "STARTTLS" {
            assert!(!upgraded);
            socket.write_all(b"220 upgrade\r\n").await.unwrap();
            socket.flush().await.unwrap();
            assert!(socket.buffer().is_empty());
            let handshake = acceptor.accept(socket.into_inner()).await;
            if !trusted {
                assert!(
                    handshake.is_err(),
                    "untrusted SMTP STARTTLS must reject before AUTH"
                );
                return;
            }
            socket = BufReader::new(Box::new(handshake.unwrap()));
            upgraded = true;
        } else if command == "AUTH LOGIN" {
            assert!(upgraded);
            auth_step = 1;
            socket.write_all(b"334 VXNlcm5hbWU6\r\n").await.unwrap();
        } else if auth_step == 1 {
            assert_eq!(STANDARD.decode(command).unwrap(), b"agent@example.test");
            auth_step = 2;
            socket.write_all(b"334 UGFzc3dvcmQ6\r\n").await.unwrap();
        } else if auth_step == 2 {
            assert_eq!(STANDARD.decode(command).unwrap(), b"mock-password");
            auth_step = 3;
            socket.write_all(b"235 authenticated\r\n").await.unwrap();
        } else if command == "QUIT" {
            assert_eq!(auth_step, 3);
            socket.write_all(b"221 bye\r\n").await.unwrap();
            socket.flush().await.unwrap();
            break;
        } else {
            panic!("unexpected SMTP TLS command {command:?}");
        }
        socket.flush().await.unwrap();
    }
}

#[tokio::test]
async fn mail_tls_four_modes_use_extra_ca_and_upgrade_before_authentication() {
    const CHILD: &str = "PDRS_H1_MAIL_TLS_CHILD";
    if let Ok(mode) = std::env::var(CHILD) {
        let port = std::env::var("PDRS_H1_MAIL_TLS_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let trusted = std::env::var("PDRS_H1_MAIL_TLS_EXPECT").unwrap() == "trusted";
        let config = config(&mode, port);
        let exchange = async {
            if mode.starts_with("imap") {
                let mut imap = Imap::connect(&config).await?;
                imap.logout().await
            } else {
                let mut smtp = smtp_connect_endpoint(&config, port).await?;
                smtp_command(&mut smtp, "QUIT", 221).await.map(|_| ())
            }
        };
        let result = tokio::time::timeout(BUDGET, exchange)
            .await
            .expect("mail TLS child bounded");
        assert_eq!(
            result.is_ok(),
            trusted,
            "{mode} CA trust result: {result:?}"
        );
        return;
    }
    let scratch = Scratch(
        std::env::temp_dir().join(format!("pilotdeck-h1-email-tls-{}", uuid::Uuid::new_v4())),
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
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    for mode in [
        "imap-implicit",
        "imap-starttls",
        "smtp-starttls",
        "smtp-implicit",
    ] {
        for trusted in [false, true] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let acceptor = acceptor.clone();
            let mock = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                peer(stream, acceptor, mode, trusted).await;
            });
            let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
            child.args(["--exact","channel::email::wire::tls_tests::mail_tls_four_modes_use_extra_ca_and_upgrade_before_authentication","--nocapture"])
                .env_clear().env("PATH",std::env::var_os("PATH").unwrap_or_default()).env("HOME",&scratch.0).env("PILOT_HOME",scratch.0.join("pilot"))
                .env("NODE_OPTIONS","--use-bundled-ca").env(CHILD,mode).env("PDRS_H1_MAIL_TLS_PORT",port.to_string())
                .env("PDRS_H1_MAIL_TLS_EXPECT",if trusted {"trusted"}else{"untrusted"}).kill_on_drop(true);
            if trusted {
                child.env("NODE_EXTRA_CA_CERTS", scratch.0.join("ca.pem"));
            }
            let output = tokio::time::timeout(BUDGET + Duration::from_secs(2), child.output())
                .await
                .expect("TLS child exits")
                .unwrap();
            assert!(
                output.status.success(),
                "{mode} trusted={trusted}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"));
            tokio::time::timeout(BUDGET, mock)
                .await
                .expect("TLS mock exits")
                .unwrap();
        }
    }
}
