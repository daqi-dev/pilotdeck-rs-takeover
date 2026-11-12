use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn server(
    responses: Vec<(u16, &'static str)>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            requests.push(String::from_utf8(request).unwrap());
            socket.write_all(format!("HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    (format!("http://{address}"), task)
}
fn callbacks() -> WeixinLoginCallbacks {
    WeixinLoginCallbacks {
        on_qr_code: Box::new(|_| {}),
        on_status_change: Box::new(|_| {}),
    }
}

// Fixed Node 22.22.0 / weixin-ilink 0.1.0 execution evidence is retained in
// _tmp/20261113-H1/weixin-auth/baseline.json; these run the native socket path.
#[tokio::test]
async fn login_http_and_json_failures_keep_the_baseline_error_shapes() {
    const QR: &str = "{\"qrcode\":\"token\",\"qrcode_img_content\":\"qr\"}";
    for (responses, name, message) in [
        (
            vec![(503, "ignored")],
            "Error",
            "Failed to fetch QR code: 503",
        ),
        (
            vec![(200, QR), (403, "ignored")],
            "Error",
            "QR status poll failed: 403",
        ),
        (
            vec![(200, "{")],
            "SyntaxError",
            "Expected property name or '}' in JSON at position 1 (line 1 column 2)",
        ),
        (
            vec![(200, QR), (200, "bad")],
            "SyntaxError",
            "Unexpected token 'b', \"bad\" is not valid JSON",
        ),
    ] {
        let (url, mock) = server(responses).await;
        let error = tokio::time::timeout(Duration::from_secs(3), login_with_qr(callbacks(), &url))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.name, name);
        assert_eq!(error.message, message);
        let requests = mock.await.unwrap();
        assert!(requests[0].starts_with("GET /ilink/bot/get_bot_qrcode?bot_type=3 HTTP/1.1\r\n"));
        if let Some(poll) = requests.get(1) {
            assert!(poll.starts_with("GET /ilink/bot/get_qrcode_status?qrcode=token HTTP/1.1\r\n"));
            assert!(
                poll.to_lowercase()
                    .contains("ilink-app-clientversion: 1\r\n")
            );
        }
    }
}

#[tokio::test]
async fn login_json_accepts_the_fetch_utf8_bom() {
    let (url, mock) = server(vec![
        (
            200,
            "\u{feff}{\"qrcode\":\"token\",\"qrcode_img_content\":\"qr\"}",
        ),
        (
            200,
            "{\"status\":\"confirmed\",\"ilink_bot_id\":\"test-id\",\"bot_token\":\"test-token\"}",
        ),
    ])
    .await;
    let result = tokio::time::timeout(Duration::from_secs(3), login_with_qr(callbacks(), &url))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.account_id, "test-id");
    assert_eq!(result.bot_token, "test-token");
    assert_eq!(result.base_url, url);
    assert_eq!(mock.await.unwrap().len(), 2);
}

#[tokio::test]
async fn qr_http_error_does_not_wait_for_an_unconsumed_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (close, closed) = tokio::sync::oneshot::channel();
    let mock = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 2048];
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = socket.read(&mut bytes).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&bytes[..count]);
        }
        socket
            .write_all(b"HTTP/1.1 503 Mock\r\nContent-Length: 100\r\n\r\n")
            .await
            .unwrap();
        let _ = closed.await;
    });
    let result =
        tokio::time::timeout(Duration::from_secs(2), login_with_qr(callbacks(), &url)).await;
    let _ = close.send(());
    mock.await.unwrap();
    assert_eq!(
        result.unwrap().unwrap_err().message,
        "Failed to fetch QR code: 503"
    );
}
