//! Receive-only permessage-deflate for Node 22's native WebSocket contract.
//! The connector owns proxy/TLS; tungstenite still owns WS messages and writes.
use std::{
    io::{self, Cursor},
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll, ready},
};

use bytes::{Buf, BytesMut};
use hyper::http::{HeaderMap, HeaderValue};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::{
    Error,
    handshake::client::{Request, Response},
    protocol::frame::{
        FrameHeader,
        coding::{Data, OpCode},
    },
};
use zlib_rs::{Inflate, InflateFlush, Status};

use crate::runtime::model::http::ProxyIo;

#[cfg(test)]
mod tests;

pub(super) async fn handshake<S: ProxyIo + 'static>(
    mut request: Request,
    stream: S,
) -> Result<(super::ChannelWebSocket, Response), Error> {
    add_default_headers(&mut request);
    let (stream, negotiation) = ReceiveStream::new(stream);
    let stream: Box<dyn ProxyIo> = Box::new(stream);
    // Native Node's receiver has neither tungstenite's default 16 MiB frame
    // limit nor its 64 MiB message limit.
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_frame_size(None)
        .max_message_size(None);
    let (socket, response) =
        tokio_tungstenite::client_async_with_config(request, stream, Some(config)).await?;
    let extension = Negotiation::from_headers(response.headers()).map_err(Error::Io)?;
    // No frame bytes have entered tungstenite, including a frame in the same
    // read as the HTTP headers. Its next poll observes this negotiated state.
    let _ = negotiation.set(extension);
    Ok((socket, response))
}

pub(super) fn add_default_headers(request: &mut Request) {
    let secure = request.uri().scheme_str() == Some("wss");
    let headers = request.headers_mut();
    for (name, value) in [
        ("connection", "upgrade"),
        ("upgrade", "websocket"),
        (
            "sec-websocket-extensions",
            "permessage-deflate; client_max_window_bits",
        ),
        ("accept", "*/*"),
        ("accept-language", "*"),
        ("sec-fetch-mode", "websocket"),
        ("user-agent", "node"),
        ("pragma", "no-cache"),
        ("cache-control", "no-cache"),
        (
            "accept-encoding",
            if secure {
                "br, gzip, deflate"
            } else {
                "gzip, deflate"
            },
        ),
    ] {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    // IntoClientRequest supplies "Upgrade"; native fetch serializes this value
    // in lower case. Preserve caller headers such as Mattermost's Origin.
    headers.insert("connection", HeaderValue::from_static("upgrade"));
}

#[derive(Clone, Debug)]
pub(super) struct Negotiation {
    window: Option<Result<u8, &'static str>>,
}

impl Negotiation {
    pub(super) fn from_headers(headers: &HeaderMap) -> io::Result<Self> {
        let values = headers.get_all("sec-websocket-extensions");
        let mut fields = Vec::new();
        for value in values {
            fields.push(value.to_str().map_err(invalid)?.to_owned());
        }
        if fields.is_empty() {
            return Ok(Self { window: None });
        }
        // Native Undici 6.23 uses a semicolon map, with last duplicate winning;
        // it does not reject unknown parameters beside permessage-deflate.
        let extension = fields.join(", ");
        let mut found = false;
        let mut window = "";
        for field in extension.split(';') {
            let mut pair = field.split('=');
            let name = pair
                .next()
                .unwrap_or_default()
                .trim_start_matches(http_space);
            let value = pair.next().unwrap_or_default().trim_end_matches(http_space);
            match name {
                "permessage-deflate" => found = true,
                "server_max_window_bits" => window = value,
                _ => {}
            }
        }
        if !found {
            return Err(invalid("Server did not negotiate permessage-deflate"));
        }
        // Node creates its inflater lazily. Even an invalid window parameter
        // does not prevent uncompressed traffic. The baseline's window 0/16
        // process crash is replaced by an error on the first compressed frame.
        let window = if window.is_empty() {
            Ok(15)
        } else if window.bytes().all(|byte| byte.is_ascii_digit()) {
            window
                .trim_start_matches('0')
                .parse::<u8>()
                .ok()
                .filter(|bits| (8..=15).contains(bits))
                .ok_or("Invalid server_max_window_bits")
        } else {
            Err("Invalid server_max_window_bits")
        };
        Ok(Self {
            window: Some(window),
        })
    }
}

fn http_space(value: char) -> bool {
    matches!(value, '\t' | '\n' | '\r' | ' ')
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

struct Decoder {
    negotiation: Negotiation,
    inflater: Option<Inflate>,
    compressed: bool,
    yielded_for_frame: bool,
}

enum DecodeStep {
    NeedInput,
    Frame,
    Yield,
}

impl Decoder {
    fn new(negotiation: Negotiation) -> Self {
        Self {
            negotiation,
            inflater: None,
            compressed: false,
            yielded_for_frame: false,
        }
    }

    fn frame(&mut self, input: &mut BytesMut, output: &mut BytesMut) -> io::Result<DecodeStep> {
        let mut cursor = Cursor::new(&input[..]);
        let Some((mut header, length)) = FrameHeader::parse(&mut cursor).map_err(invalid)? else {
            return Ok(DecodeStep::NeedInput);
        };
        if header.mask.is_some() {
            return Err(invalid("Frame cannot be masked"));
        }
        if header.rsv2 || header.rsv3 {
            return Err(invalid("RSV2 and RSV3 must be clear"));
        }
        if header.rsv1 && self.negotiation.window.is_none() {
            return Err(invalid("Expected RSV1 to be clear"));
        }
        if matches!(header.opcode, OpCode::Control(_)) && (!header.is_final || input[1] & 127 > 125)
        {
            return Err(invalid("Control frame either too large or fragmented"));
        }
        let offset = cursor.position() as usize;
        let length = usize::try_from(length).map_err(invalid)?;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| invalid("Frame length overflow"))?;
        if input.len() < end {
            return Ok(DecodeStep::NeedInput);
        }
        if matches!(header.opcode, OpCode::Data(Data::Text | Data::Binary)) {
            self.compressed = header.rsv1;
        }
        let compressed = matches!(header.opcode, OpCode::Data(_)) && self.compressed;
        if compressed && !self.yielded_for_frame {
            // Native zlib resumes the receiver asynchronously for each
            // compressed fragment. Expose that boundary to ready-read batch
            // consumers without delaying control frames or adding a timer.
            // Consumers must finish their admitted continuations at the batch
            // boundary; a Pending poll alone does not drain their tasks.
            self.yielded_for_frame = true;
            return Ok(DecodeStep::Yield);
        }
        let payload = if compressed {
            self.inflate(&input[offset..end], header.is_final)?
        } else {
            input[offset..end].to_vec()
        };
        // The fixed native receiver also permits RSV1 on control/continuation
        // frames when negotiated; control frames never enter the inflater.
        header.rsv1 = false;
        let mut encoded_header = Vec::with_capacity(14);
        header
            .format(payload.len() as u64, &mut encoded_header)
            .map_err(invalid)?;
        output.extend_from_slice(&encoded_header);
        output.extend_from_slice(&payload);
        input.advance(end);
        self.yielded_for_frame = false;
        Ok(DecodeStep::Frame)
    }

    fn inflate(&mut self, payload: &[u8], final_frame: bool) -> io::Result<Vec<u8>> {
        if self.inflater.is_none() {
            let bits = self
                .negotiation
                .window
                .ok_or_else(|| invalid("Compressed frame without negotiation"))?
                .map_err(invalid)?;
            self.inflater = Some(Inflate::new(false, bits));
        }
        let inflater = self.inflater.as_mut().expect("initialized above");
        let mut input = payload.to_vec();
        if final_frame {
            input.extend_from_slice(&[0, 0, 255, 255]);
        }
        let mut offset = 0;
        let mut output = Vec::new();
        loop {
            let before_in = inflater.total_in();
            let before_out = inflater.total_out();
            let mut chunk = [0; 8192];
            let status = inflater
                .decompress(&input[offset..], &mut chunk, InflateFlush::SyncFlush)
                .map_err(|error| invalid(inflater.error_message().unwrap_or(error.as_str())))?;
            let consumed = (inflater.total_in() - before_in) as usize;
            let written = (inflater.total_out() - before_out) as usize;
            offset += consumed;
            output.extend_from_slice(&chunk[..written]);
            if status == Status::StreamEnd || (offset == input.len() && written < chunk.len()) {
                break;
            }
            if consumed == 0 && written == 0 {
                return Err(invalid("Inflater made no progress"));
            }
        }
        // Node 22 stores server_no_context_takeover but does not reset its
        // inflater. In particular ignored Binary messages seed later Text.
        Ok(output)
    }
}

pub(super) struct ReceiveStream<S> {
    inner: S,
    negotiation: Arc<OnceLock<Negotiation>>,
    headers_done: bool,
    decoder: Option<Decoder>,
    input: BytesMut,
    output: BytesMut,
}

impl<S> ReceiveStream<S> {
    pub(super) fn new(inner: S) -> (Self, Arc<OnceLock<Negotiation>>) {
        let negotiation = Arc::new(OnceLock::new());
        (
            Self {
                inner,
                negotiation: negotiation.clone(),
                headers_done: false,
                decoder: None,
                input: BytesMut::new(),
                output: BytesMut::new(),
            },
            negotiation,
        )
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ReceiveStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if !this.output.is_empty() {
                let count = buffer.remaining().min(this.output.len());
                buffer.put_slice(&this.output[..count]);
                this.output.advance(count);
                return Poll::Ready(Ok(()));
            }
            if !this.headers_done {
                if let Some(end) = this.input.windows(4).position(|part| part == b"\r\n\r\n") {
                    this.output = this.input.split_to(end + 4);
                    this.headers_done = true;
                    continue;
                }
                // Keep only the possible delimiter prefix here. The existing
                // handshake parser receives the header incrementally and owns
                // its validation; this filter must not accumulate an endless
                // unterminated HTTP header before that parser gets to run.
                if this.input.len() > 3 {
                    this.output = this.input.split_to(this.input.len() - 3);
                    continue;
                }
            } else {
                if this.decoder.is_none() {
                    let Some(negotiation) = this.negotiation.get() else {
                        // The HTTP handshake finishes before its caller sets
                        // negotiation, so no frame can enter tungstenite early.
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    };
                    this.decoder = Some(Decoder::new(negotiation.clone()));
                }
                match this
                    .decoder
                    .as_mut()
                    .expect("initialized above")
                    .frame(&mut this.input, &mut this.output)?
                {
                    DecodeStep::Frame => continue,
                    DecodeStep::Yield => {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    DecodeStep::NeedInput => {}
                }
            }
            let mut chunk = [0; 8192];
            let mut read = ReadBuf::new(&mut chunk);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut read))?;
            if read.filled().is_empty() {
                return Poll::Ready(if this.input.is_empty() {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "WebSocket ended in an incomplete header or frame",
                    ))
                });
            }
            this.input.extend_from_slice(read.filled());
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ReceiveStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
