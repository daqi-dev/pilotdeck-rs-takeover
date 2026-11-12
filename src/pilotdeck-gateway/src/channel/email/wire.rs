//! Native IMAP and SMTP transport. Each connection owns its socket.
use super::Config;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{io, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
};

trait Socket: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Socket for T {}
type Io = Box<dyn Socket>;
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
struct Connection {
    io: BufReader<Io>,
}
impl Connection {
    async fn connect(host: &str, port: u16, tls: bool) -> io::Result<Self> {
        let socket = TcpStream::connect((host, port)).await?;
        let mut connection = Self {
            io: BufReader::new(Box::new(socket)),
        };
        if tls {
            connection.upgrade(host).await?;
        }
        Ok(connection)
    }
    async fn upgrade(&mut self, host: &str) -> io::Result<()> {
        // No application bytes may precede the server's TLS handshake.
        if !self.io.buffer().is_empty() {
            return Err(invalid("mail STARTTLS left buffered plaintext"));
        }
        let config = crate::runtime::model::http::webpki_client_config();
        let name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|_| invalid("invalid mail TLS server name"))?;
        let (placeholder, _other) = tokio::io::duplex(1);
        let old = std::mem::replace(&mut self.io, BufReader::new(Box::new(placeholder)));
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, old.into_inner())
            .await?;
        self.io = BufReader::new(Box::new(tls));
        Ok(())
    }
    async fn write(&mut self, text: &str) -> io::Result<()> {
        self.io.write_all(text.as_bytes()).await?;
        self.io.flush().await
    }
    async fn line(&mut self) -> io::Result<String> {
        let mut line = Vec::new();
        if self.io.read_until(b'\n', &mut line).await? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "mail connection closed",
            ));
        }
        Ok(String::from_utf8_lossy(&line).into_owned())
    }
}
fn quote(text: &str) -> io::Result<String> {
    if text.contains(['\r', '\n', '\0']) {
        return Err(invalid("invalid mail command value"));
    }
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
#[derive(Debug)]
pub(super) struct Message {
    pub uid: u64,
    pub from: String,
    pub source: String,
}
pub(super) struct Imap {
    connection: Connection,
    tag: u64,
    poisoned: bool,
}
impl Imap {
    pub async fn connect(config: &Config) -> io::Result<Self> {
        let mut connection =
            Connection::connect(&config.imap_host, config.imap_port, config.imap_tls).await?;
        let greeting = connection.line().await?;
        if !greeting.to_ascii_uppercase().starts_with("* OK") {
            return Err(invalid(greeting));
        }
        let mut imap = Self {
            connection,
            tag: 0,
            poisoned: false,
        };
        if !config.imap_tls {
            let capability = imap.command("CAPABILITY").await?;
            if capability.iter().any(|v| {
                v.to_ascii_uppercase()
                    .split_whitespace()
                    .any(|w| w == "STARTTLS")
            }) {
                imap.command("STARTTLS").await?;
                imap.connection.upgrade(&config.imap_host).await?;
            }
        }
        imap.command(&format!(
            "LOGIN {} {}",
            quote(&config.address)?,
            quote(&config.password)?
        ))
        .await?;
        imap.command("SELECT INBOX").await?;
        Ok(imap)
    }
    async fn command(&mut self, command: &str) -> io::Result<Vec<String>> {
        if self.poisoned {
            return Err(invalid("IMAP connection lost framing"));
        }
        self.poisoned = true;
        self.tag += 1;
        let tag = format!("P{}", self.tag);
        self.connection
            .write(&format!("{tag} {command}\r\n"))
            .await?;
        let mut result = Vec::new();
        loop {
            let mut line = self.connection.line().await?;
            let done = line.starts_with(&format!("{tag} "));
            if done {
                self.poisoned = false;
                if !line[tag.len()..]
                    .trim_start()
                    .get(..2)
                    .is_some_and(|status| status.eq_ignore_ascii_case("OK"))
                {
                    return Err(invalid(line));
                }
                return Ok(result);
            }
            // FETCH literals are byte-counted, not line-delimited. Keep them in
            // the same response for the structural tokenizer below.
            loop {
                let literal = line
                    .trim_end()
                    .rsplit_once('{')
                    .and_then(|(_, n)| n.strip_suffix('}'))
                    .and_then(|n| n.parse::<usize>().ok());
                let Some(count) = literal else {
                    break;
                };
                // Read in chunks rather than allocating the server-declared
                // length upfront. No channel-specific message-size policy.
                let mut bytes = Vec::new();
                let mut remaining = count;
                let mut chunk = [0u8; 8192];
                while remaining > 0 {
                    let length = remaining.min(chunk.len());
                    self.connection.io.read_exact(&mut chunk[..length]).await?;
                    bytes.extend_from_slice(&chunk[..length]);
                    remaining -= length;
                }
                let decoded = String::from_utf8_lossy(&bytes);
                // The channel observes Node Buffer.toString("utf8"). If a
                // damaged UTF-8 literal expands to replacement characters,
                // retain the decoded byte count for our structural tokenizer.
                if decoded.len() != count {
                    let marker = line.rfind('{').expect("literal marker");
                    line.truncate(marker);
                    line.push_str(&format!("{{{}}}\r\n", decoded.len()));
                }
                line.push_str(&decoded);
                line.push_str(&self.connection.line().await?);
            }
            result.push(line);
        }
    }
    pub async fn prime(&mut self) -> io::Result<Vec<u64>> {
        let status = self.command("STATUS INBOX (UIDNEXT)").await?;
        let next = regex::Regex::new(r"(?i)UIDNEXT\s+(\d+)").expect("static regex");
        let next = status
            .iter()
            .find_map(|s| next.captures(s).and_then(|c| c[1].parse::<u64>().ok()))
            .unwrap_or(1);
        if next <= 1 {
            return Ok(Vec::new());
        }
        // Baseline fetch() uses a sequence set here rather than uid=true.
        let responses = self
            .command(&format!(
                "FETCH {}:{} (UID)",
                next.saturating_sub(300).max(1),
                next - 1
            ))
            .await?;
        Ok(responses.iter().filter_map(|line| uid(line)).collect())
    }
    pub async fn poll(&mut self) -> io::Result<Vec<Message>> {
        let search = self.command("SEARCH UNSEEN").await?;
        let ids: Vec<_> = search
            .iter()
            .filter_map(|s| {
                let prefix = "* SEARCH ";
                s.get(..prefix.len())
                    .filter(|head| head.eq_ignore_ascii_case(prefix))
                    .map(|_| &s[prefix.len()..])
            })
            .flat_map(|s| s.split_whitespace())
            .filter_map(|s| s.parse::<u64>().ok())
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let responses = self
            .command(&format!("FETCH {ids} (UID ENVELOPE BODY.PEEK[])"))
            .await?;
        responses
            .iter()
            .filter(|line| fetch_fields(line).is_some())
            .map(|line| {
                let id = uid(line).ok_or_else(|| invalid("IMAP FETCH missing UID"))?;
                let fields =
                    fetch_fields(line).ok_or_else(|| invalid("IMAP invalid FETCH attributes"))?;
                let envelope = field(&fields, "ENVELOPE");
                let from = envelope
                    .and_then(Value::list)
                    .and_then(|v| v.get(2))
                    .and_then(Value::list)
                    .and_then(|v| v.first())
                    .and_then(Value::list)
                    .and_then(|v| Some(format!("{}@{}", v.get(2)?.text()?, v.get(3)?.text()?)))
                    .unwrap_or_else(|| "unknown".into());
                let source = field(&fields, "BODY[]")
                    .or_else(|| field(&fields, "RFC822"))
                    .and_then(Value::text)
                    .unwrap_or_default()
                    .to_owned();
                Ok(Message {
                    uid: id,
                    from,
                    source,
                })
            })
            .collect()
    }
    pub fn usable(&self) -> bool {
        !self.poisoned
    }
    pub async fn logout(&mut self) -> io::Result<()> {
        self.command("LOGOUT").await.map(|_| ())
    }
}
fn fetch_fields(line: &str) -> Option<Vec<Value>> {
    let prefix = regex::Regex::new(r"(?i)^\* \d+ FETCH ")
        .expect("static regex")
        .find(line)?;
    match parse_value(line.as_bytes(), &mut prefix.end())? {
        Value::List(fields) => Some(fields),
        _ => None,
    }
}
fn field<'a>(fields: &'a [Value], name: &str) -> Option<&'a Value> {
    fields
        .chunks_exact(2)
        .find(|pair| {
            pair[0]
                .text()
                .is_some_and(|key| key.eq_ignore_ascii_case(name))
        })
        .map(|pair| &pair[1])
}
fn uid(line: &str) -> Option<u64> {
    let fields = fetch_fields(line)?;
    field(&fields, "UID")?.text()?.parse().ok()
}
#[derive(Debug)]
enum Value {
    Nil,
    Text(String),
    List(Vec<Value>),
}
impl Value {
    fn text(&self) -> Option<&str> {
        if let Self::Text(s) = self {
            Some(s)
        } else {
            None
        }
    }
    fn list(&self) -> Option<&[Self]> {
        if let Self::List(v) = self {
            Some(v)
        } else {
            None
        }
    }
}
fn parse_value(input: &[u8], at: &mut usize) -> Option<Value> {
    parse_value_depth(input, at, 0)
}
fn parse_value_depth(input: &[u8], at: &mut usize, depth: usize) -> Option<Value> {
    if depth > 64 {
        return None;
    }
    while input.get(*at).is_some_and(u8::is_ascii_whitespace) {
        *at += 1;
    }
    match *input.get(*at)? {
        b'(' => {
            *at += 1;
            let mut list = Vec::new();
            loop {
                while input.get(*at).is_some_and(u8::is_ascii_whitespace) {
                    *at += 1;
                }
                if *input.get(*at)? == b')' {
                    *at += 1;
                    break;
                }
                list.push(parse_value_depth(input, at, depth + 1)?);
            }
            Some(Value::List(list))
        }
        b'"' => {
            *at += 1;
            let mut text = Vec::new();
            loop {
                let mut b = *input.get(*at)?;
                *at += 1;
                if b == b'"' {
                    break;
                }
                if b == b'\\' {
                    b = *input.get(*at)?;
                    *at += 1;
                }
                text.push(b);
            }
            Some(Value::Text(String::from_utf8_lossy(&text).into_owned()))
        }
        b'{' => {
            *at += 1;
            let start = *at;
            while input.get(*at).is_some_and(u8::is_ascii_digit) {
                *at += 1;
            }
            let count = std::str::from_utf8(&input[start..*at])
                .ok()?
                .parse::<usize>()
                .ok()?;
            if *input.get(*at)? != b'}' {
                return None;
            }
            *at += 1;
            if input.get(*at..*at + 2)? != b"\r\n" {
                return None;
            }
            *at += 2;
            let bytes = input.get(*at..(*at).checked_add(count)?)?;
            *at += count;
            Some(Value::Text(String::from_utf8_lossy(bytes).into_owned()))
        }
        _ => {
            let start = *at;
            while input
                .get(*at)
                .is_some_and(|b| !b.is_ascii_whitespace() && *b != b')')
            {
                *at += 1;
            }
            let text = String::from_utf8_lossy(&input[start..*at]);
            if text.eq_ignore_ascii_case("NIL") {
                Some(Value::Nil)
            } else {
                Some(Value::Text(text.into_owned()))
            }
        }
    }
}
async fn smtp_response(connection: &mut Connection, expected: u16) -> io::Result<Vec<String>> {
    let mut response = Vec::new();
    loop {
        let line = connection.line().await?;
        let code = line.get(..3).and_then(|n| n.parse::<u16>().ok());
        if code != Some(expected)
            && !(expected == 251 && code.is_some_and(|n| (200..300).contains(&n)))
        {
            return Err(invalid(line));
        }
        let more = line.as_bytes().get(3) == Some(&b'-');
        response.push(line);
        if !more {
            return Ok(response);
        }
    }
}
async fn smtp_command(
    connection: &mut Connection,
    command: &str,
    code: u16,
) -> io::Result<Vec<String>> {
    connection.write(&format!("{command}\r\n")).await?;
    smtp_response(connection, code).await
}
async fn smtp_connect(config: &Config) -> io::Result<Connection> {
    smtp_connect_endpoint(config, config.smtp_port).await
}
// The protocol selector remains the configured port. The separate socket
// endpoint lets loopback tests cover implicit TLS without binding port 465.
async fn smtp_connect_endpoint(config: &Config, endpoint_port: u16) -> io::Result<Connection> {
    let mut connection =
        Connection::connect(&config.smtp_host, endpoint_port, config.smtp_port == 465).await?;
    smtp_response(&mut connection, 220).await?;
    let mut ehlo = smtp_command(&mut connection, "EHLO localhost", 250).await?;
    let starttls = ehlo
        .iter()
        .any(|s| s.to_ascii_uppercase().contains("STARTTLS"));
    if config.smtp_port != 465 && (config.smtp_tls || starttls) {
        smtp_command(&mut connection, "STARTTLS", 220).await?;
        connection.upgrade(&config.smtp_host).await?;
        ehlo = smtp_command(&mut connection, "EHLO localhost", 250).await?;
    }
    // Prefer PLAIN; LOGIN remains available for servers advertising only it.
    let auth = ehlo
        .iter()
        .find(|s| s.to_ascii_uppercase().contains("AUTH"));
    if auth.is_some_and(|s| s.to_ascii_uppercase().contains("PLAIN")) {
        let token = STANDARD.encode(format!("\0{}\0{}", config.address, config.password));
        smtp_command(&mut connection, &format!("AUTH PLAIN {token}"), 235).await?;
    } else {
        smtp_command(&mut connection, "AUTH LOGIN", 334).await?;
        smtp_command(&mut connection, &STANDARD.encode(&config.address), 334).await?;
        smtp_command(&mut connection, &STANDARD.encode(&config.password), 235).await?;
    }
    Ok(connection)
}
pub(super) async fn verify(config: &Config) -> io::Result<()> {
    let mut connection = smtp_connect(config).await?;
    smtp_command(&mut connection, "QUIT", 221).await?;
    Ok(())
}
pub(super) async fn send(config: &Config, to: &str, text: &str) -> io::Result<()> {
    // Header and command injection is rejected before opening a connection.
    for value in [&config.address, to, &config.subject] {
        if value.contains(['\r', '\n', '\0']) {
            return Err(invalid("invalid email header"));
        }
    }
    let mut connection = smtp_connect(config).await?;
    smtp_command(
        &mut connection,
        &format!("MAIL FROM:<{}>", config.address),
        250,
    )
    .await?;
    // 251 selects successful recipient acceptance (including forwarding).
    smtp_command(&mut connection, &format!("RCPT TO:<{to}>"), 251).await?;
    smtp_command(&mut connection, "DATA", 354).await?;
    let subject = if config.subject.is_ascii() {
        config.subject.clone()
    } else {
        format!("=?UTF-8?B?{}?=", STANDARD.encode(&config.subject))
    };
    let body = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|line| {
            if line.starts_with('.') {
                format!(".{line}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n");
    connection.write(&format!("From: {}\r\nTo: {to}\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{body}\r\n.\r\n",config.address)).await?;
    smtp_response(&mut connection, 250).await?;
    // Acceptance of DATA is the delivery result; a failed QUIT cannot unsend it.
    let _ = smtp_command(&mut connection, "QUIT", 221).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;
    fn config(imap_port: u16, smtp_port: u16) -> Config {
        Config {
            address: "agent@example.test".into(),
            password: "mock-password".into(),
            imap_host: "127.0.0.1".into(),
            smtp_host: "127.0.0.1".into(),
            imap_port,
            smtp_port,
            imap_tls: false,
            smtp_tls: false,
            subject: "Message".into(),
            poll_interval: Duration::from_millis(20),
        }
    }
    #[tokio::test(start_paused = true)]
    async fn slow_line_literal_and_write_have_no_invented_thirty_second_deadline() {
        let (client, mut server) = tokio::io::duplex(1);
        let mut connection = Connection {
            io: BufReader::new(Box::new(client)),
        };
        let peer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(31)).await;
            server.write_all(b"slow line\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(31)).await;
            let mut bytes = [0u8; 4];
            server.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"slow");
        });
        assert_eq!(connection.line().await.unwrap(), "slow line\r\n");
        connection.write("slow").await.unwrap();
        peer.await.unwrap();
        let (client, server) = tokio::io::duplex(1024);
        let mut imap = Imap {
            connection: Connection {
                io: BufReader::new(Box::new(client)),
            },
            tag: 0,
            poisoned: false,
        };
        let peer = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut command = String::new();
            server.read_line(&mut command).await.unwrap();
            server
                .write_all(b"* 1 FETCH (UID 4 BODY[] {4}\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(31)).await;
            server.write_all(b"slow)\r\nP1 OK done\r\n").await.unwrap();
        });
        let response = imap.command("FETCH 1 (UID BODY.PEEK[])").await.unwrap();
        assert_eq!(
            field(&fetch_fields(&response[0]).unwrap(), "BODY[]")
                .unwrap()
                .text(),
            Some("slow")
        );
        peer.await.unwrap();
    }
    #[tokio::test]
    async fn valid_large_mail_literal_is_not_truncated_at_thirty_two_mebibytes() {
        let size = 33 * 1024 * 1024;
        let (client, server) = tokio::io::duplex(16384);
        let mut imap = Imap {
            connection: Connection {
                io: BufReader::new(Box::new(client)),
            },
            tag: 0,
            poisoned: false,
        };
        let peer = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut command = String::new();
            server.read_line(&mut command).await.unwrap();
            server
                .write_all(format!("* 1 FETCH (UID 4 BODY[] {{{size}}}\r\n").as_bytes())
                .await
                .unwrap();
            let chunk = vec![b'x'; 1024 * 1024];
            for _ in 0..33 {
                server.write_all(&chunk).await.unwrap();
            }
            server.write_all(b")\r\nP1 OK done\r\n").await.unwrap();
        });
        let response = imap.command("FETCH 1 (UID BODY.PEEK[])").await.unwrap();
        let fields = fetch_fields(&response[0]).unwrap();
        let source = field(&fields, "BODY[]").unwrap().text().unwrap();
        assert_eq!(source.len(), size);
        assert_eq!(source.as_bytes().last(), Some(&b'x'));
        assert!(imap.usable());
        peer.await.unwrap();
    }
    #[test]
    fn fetch_attributes_ignore_uid_and_envelope_text_inside_body() {
        let source = "UID 999 ENVELOPE (NIL) BODY[] bogus";
        let response = format!(
            "* 1 FETCH (BODY[] {{{}}}\r\n{source} UID 4 ENVELOPE (NIL NIL ((NIL NIL \"sender\" \"example.test\")) NIL NIL NIL NIL NIL NIL NIL))\r\n",
            source.len()
        );
        assert_eq!(uid(&response), Some(4));
        let fields = fetch_fields(&response).unwrap();
        assert_eq!(field(&fields, "BODY[]").unwrap().text(), Some(source));
        assert!(field(&fields, "ENVELOPE").unwrap().list().is_some());
    }
    #[tokio::test]
    async fn imap_lost_literal_framing_never_reuses_connection() {
        let (client, mut server) = tokio::io::duplex(1024);
        let mut imap = Imap {
            connection: Connection {
                io: BufReader::new(Box::new(client)),
            },
            tag: 0,
            poisoned: false,
        };
        let mock = tokio::spawn(async move {
            let mut line = String::new();
            let mut reader = BufReader::new(&mut server);
            reader.read_line(&mut line).await.unwrap();
            reader
                .write_all(b"* 1 FETCH (UID 4 BODY[] {100}\r\nshort")
                .await
                .unwrap();
            // Closing in a literal poisons framing; no subsequent command may be written.
        });
        assert!(imap.command("FETCH 1 (UID BODY.PEEK[])").await.is_err());
        assert!(!imap.usable());
        assert!(
            imap.command("SEARCH UNSEEN")
                .await
                .unwrap_err()
                .to_string()
                .contains("lost framing")
        );
        mock.await.unwrap();
    }
    #[tokio::test]
    async fn native_imap_uid_priming_unseen_envelope_and_literal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            socket.write_all(b"* OK local mock\r\n").await.unwrap();
            let mut commands = Vec::new();
            loop {
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert!(!line.is_empty());
                let (tag, command) = line.trim_end().split_once(' ').unwrap();
                commands.push(command.to_owned());
                let extra = if command == "CAPABILITY" {
                    "* CAPABILITY IMAP4rev1\r\n".to_owned()
                } else if command.starts_with("STATUS ") {
                    "* STATUS INBOX (UIDNEXT 4)\r\n".to_owned()
                } else if command == "FETCH 1:3 (UID)" {
                    "* 1 FETCH (UID 3)\r\n".to_owned()
                } else if command == "SEARCH UNSEEN" {
                    "* SEARCH 2\r\n".to_owned()
                } else if command.starts_with("FETCH 2 ") {
                    let source = "Content-Type: text/plain\r\n\r\nhello";
                    format!(
                        "* 2 FETCH (UID 4 ENVELOPE (NIL NIL ((NIL NIL \"sender\" \"example.test\")) NIL NIL NIL NIL NIL NIL NIL) BODY[] {{{}}}\r\n{source})\r\n",
                        source.len()
                    )
                } else {
                    String::new()
                };
                socket
                    .write_all(format!("{extra}{tag} OK done\r\n").as_bytes())
                    .await
                    .unwrap();
                if command == "LOGOUT" {
                    break;
                }
            }
            commands
        });
        let mut imap = Imap::connect(&config(port, 1)).await.unwrap();
        assert_eq!(imap.prime().await.unwrap(), vec![3]);
        let messages = imap.poll().await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].uid, 4);
        assert_eq!(messages[0].from, "sender@example.test");
        assert_eq!(messages[0].source, "Content-Type: text/plain\r\n\r\nhello");
        imap.logout().await.unwrap();
        let commands = mock.await.unwrap();
        assert!(commands.contains(&"LOGIN \"agent@example.test\" \"mock-password\"".into()));
        assert!(commands.contains(&"SELECT INBOX".into()));
    }
    #[tokio::test]
    async fn native_smtp_verify_auth_plain_data_and_dot_stuffing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = tokio::spawn(async move {
            let mut transcript = Vec::new();
            for _ in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                socket.write_all(b"220 local mock\r\n").await.unwrap();
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    assert!(!line.is_empty());
                    transcript.push(line.clone());
                    let response = if line.starts_with("EHLO ") {
                        "250-local\r\n250 AUTH PLAIN LOGIN\r\n"
                    } else if line.starts_with("AUTH PLAIN ") {
                        "235 authenticated\r\n"
                    } else if line.starts_with("MAIL ") || line.starts_with("RCPT ") {
                        "250 accepted\r\n"
                    } else if line == "DATA\r\n" {
                        "354 continue\r\n"
                    } else if line == "QUIT\r\n" {
                        "221 bye\r\n"
                    } else {
                        panic!("unexpected SMTP command: {line:?}")
                    };
                    socket.write_all(response.as_bytes()).await.unwrap();
                    if line == "DATA\r\n" {
                        let mut data = String::new();
                        loop {
                            let mut row = String::new();
                            socket.read_line(&mut row).await.unwrap();
                            if row == ".\r\n" {
                                break;
                            }
                            data.push_str(&row);
                        }
                        transcript.push(data);
                        socket.write_all(b"250 queued\r\n").await.unwrap();
                    }
                    if line == "QUIT\r\n" {
                        break;
                    }
                }
            }
            transcript
        });
        let config = config(1, port);
        verify(&config).await.unwrap();
        send(&config, "sender@example.test", "hello\n.dot")
            .await
            .unwrap();
        let transcript = mock.await.unwrap();
        assert!(
            transcript
                .iter()
                .any(|line| line == "AUTH PLAIN AGFnZW50QGV4YW1wbGUudGVzdABtb2NrLXBhc3N3b3Jk\r\n")
        );
        assert!(
            transcript
                .iter()
                .any(|line| line.contains("Subject: Message\r\n")
                    && line.ends_with("hello\r\n..dot\r\n"))
        );
    }
    #[tokio::test]
    async fn smtp_header_injection_rejected_before_dial() {
        assert!(
            send(
                &config(1, 1),
                "target@example.test\r\nBCC: injected",
                "text"
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid email header")
        );
    }
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tls_tests;
