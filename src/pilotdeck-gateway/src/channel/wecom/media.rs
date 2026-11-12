//! Native inbound aibot media download, AES-CBC decoding and private cache.
use super::*;
use crate::runtime::model::http::fetch::fetch;
use aes::Aes256;
use base64::{
    Engine as _,
    alphabet::STANDARD,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use bytes::Bytes;
use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::NoPadding};
use http_body_util::BodyExt;
use hyper::{HeaderMap, Method};
const DECODER: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);
pub(super) fn base64_bytes(text: &str) -> Vec<u8> {
    let mut normalized = String::new();
    for c in text.chars() {
        match c {
            '=' => break,
            '-' => normalized.push('+'),
            '_' => normalized.push('/'),
            c if c.is_ascii_alphanumeric() || c == '+' || c == '/' => normalized.push(c),
            _ => {}
        }
    }
    if normalized.len() % 4 == 1 {
        normalized.pop();
    }
    DECODER.decode(normalized).unwrap_or_default()
}
pub(super) fn decrypt(data: &[u8], key: &str) -> Result<Vec<u8>, String> {
    let key = base64_bytes(key);
    if key.len() != 32 {
        return Err(format!(
            "Invalid WeCom AES key length: expected 32 bytes, got {}",
            key.len()
        ));
    }
    let mut bytes = data.to_vec();
    let decrypted = cbc::Decryptor::<Aes256>::new_from_slices(&key, &key[..16])
        .map_err(|e| e.to_string())?
        .decrypt_padded_mut::<NoPadding>(&mut bytes)
        .map_err(|e| e.to_string())?;
    let pad = decrypted.last().copied().unwrap_or(0) as usize;
    if pad == 0 || pad > 32 || pad > decrypted.len() {
        return Err(format!("Invalid PKCS#7 padding value: {pad}"));
    }
    if decrypted[decrypted.len() - pad..]
        .iter()
        .any(|v| *v as usize != pad)
    {
        return Err("Invalid PKCS#7 padding bytes".into());
    }
    Ok(decrypted[..decrypted.len() - pad].to_vec())
}
pub(super) fn safe_name(name: &str) -> String {
    let name = std::path::Path::new(name)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("wecom_media");
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric()
                || matches!(c, '_' | '.' | '-' | '(' | ')')
                || ('\u{4e00}'..='\u{9fff}').contains(&c)
            {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        "wecom_media".into()
    } else {
        name
    }
}
pub(super) fn mime(filename: &str, content: &str) -> String {
    let normalized = content
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !normalized.is_empty()
        && normalized != "application/octet-stream"
        && normalized != "text/plain"
    {
        return normalized;
    }
    let extension = std::path::Path::new(filename)
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_lowercase();
    let detected = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "amr" => "audio/amr",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => "",
    };
    if !detected.is_empty() {
        detected.into()
    } else if !normalized.is_empty() {
        normalized
    } else {
        "application/octet-stream".into()
    }
}
pub(super) fn media_refs(body: &JsValue) -> Vec<(&'static str, JsValue)> {
    let mut refs = Vec::new();
    let mut add = |kind, value: Option<&JsValue>| {
        if let Some(JsValue::Object(_)) = value {
            refs.push((kind, value.unwrap().clone()));
        }
    };
    let kind = lower(&string(body, "msgtype"));
    if kind == "mixed" {
        if let Some(JsValue::Array(items)) = body.get("mixed").and_then(|v| v.get("msg_item")) {
            for item in items {
                match lower(&string(item, "msgtype")).as_str() {
                    "image" => add("image", item.get("image")),
                    "file" => add("file", item.get("file")),
                    _ => {}
                }
            }
        }
    } else {
        add("image", body.get("image"));
        match kind.as_str() {
            "file" => add("file", body.get("file")),
            "voice" => add("voice", body.get("voice")),
            "video" => add("video", body.get("video")),
            "appmsg" => {
                if let Some(app) = body.get("appmsg") {
                    add("file", app.get("file"));
                    add("image", app.get("image"));
                }
            }
            _ => {}
        }
    }
    if let Some(quote) = body.get("quote") {
        match lower(&string(quote, "msgtype")).as_str() {
            "image" => add("image", quote.get("image")),
            "file" => add("file", quote.get("file")),
            _ => {}
        }
    }
    refs
}
impl Shared {
    pub(super) async fn download(&self, url: &str) -> Result<(Vec<u8>, String), String> {
        let url = url::Url::parse(url).map_err(|e| e.to_string())?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(format!("unsupported media URL protocol: {}:", url.scheme()));
        }
        let future = async {
            let mut headers = HeaderMap::new();
            headers.insert("user-agent", "PilotDeck/1.0".parse().unwrap());
            headers.insert("accept", "*/*".parse().unwrap());
            let response = fetch(
                self.network.as_ref(),
                url,
                Method::GET,
                headers,
                Bytes::new(),
            )
            .await
            .map_err(|e| e.message.to_string())?;
            if !response.status().is_success() {
                return Err(format!("HTTP {}", response.status().as_u16()));
            }
            if let Some(length) = response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
            {
                if length.parse::<u64>().is_ok_and(|v| v > 20 * 1024 * 1024) {
                    return Err(format!(
                        "remote media exceeds limit: {length} bytes > 20971520"
                    ));
                }
            }
            let content = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let mut body = response.into_body();
            let mut data = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| e.to_string())?;
                if let Some(bytes) = frame.data_ref() {
                    if data.len() + bytes.len() > 20 * 1024 * 1024 {
                        return Err(format!(
                            "remote media exceeds limit: {} bytes > 20971520",
                            data.len() + bytes.len()
                        ));
                    }
                    data.extend_from_slice(bytes);
                }
            }
            Ok((data, content))
        };
        tokio::select! {biased;_=self.stopped()=>Err("WeCom adapter stopped".into()),result=tokio::time::timeout(Duration::from_secs(30),future)=>result.map_err(|_|"The operation was aborted due to timeout".to_owned())?}
    }
    pub(super) async fn inbound_attachments(&self, body: &JsValue) -> Vec<JsValue> {
        let mut result = Vec::new();
        for (kind, media) in media_refs(body) {
            match self.cache_media(kind, &media).await {
                Ok(Some(attachment)) => result.push(attachment),
                Ok(None) => {}
                Err(e) => self.log(
                    "warn",
                    &format!("wecom: failed to cache inbound {kind}: {e}"),
                ),
            }
        }
        result
    }
    async fn cache_media(&self, kind: &str, media: &JsValue) -> Result<Option<JsValue>, String> {
        let mut data = if media.get("base64").is_some_and(sms::truthy) {
            base64_bytes(&String::from_utf16_lossy(string(media, "base64").units()))
        } else {
            let url = trim(string(media, "url"));
            if url.is_empty() {
                return Ok(None);
            }
            self.download(&String::from_utf16_lossy(url.units()))
                .await?
                .0
        };
        let aeskey = trim(string(media, "aeskey"));
        if !aeskey.is_empty() {
            data = decrypt(&data, &String::from_utf16_lossy(aeskey.units()))?;
        }
        let fallback = format!(
            "wecom_{kind}{}",
            match kind {
                "image" => ".jpg",
                "voice" => ".amr",
                "video" => ".mp4",
                _ => ".bin",
            }
        );
        let name = media
            .get("filename")
            .filter(|v| !sms::nullish(v))
            .or_else(|| media.get("name"))
            .filter(|v| !sms::nullish(v))
            .map(sms::js_string_of)
            .map(trim)
            .filter(|v| !v.is_empty())
            .map(|v| String::from_utf16_lossy(v.units()))
            .unwrap_or(fallback);
        let name = safe_name(&name);
        let dir = std::env::temp_dir().join("pilotdeck-wecom-media");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| e.to_string())?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let path = dir.join(format!("{now}-{}-{name}", uuid::Uuid::new_v4().simple()));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).await.map_err(|e| e.to_string())?;
        use tokio::io::AsyncWriteExt;
        file.write_all(&data).await.map_err(|e| e.to_string())?;
        let content = media
            .get("content_type")
            .filter(|v| !sms::nullish(v))
            .or_else(|| media.get("contentType"))
            .map(sms::js_string_of)
            .unwrap_or_else(|| "".into());
        let mime = mime(&name, &String::from_utf16_lossy(content.units()));
        Ok(Some(object([
            (
                "type",
                JsValue::from(if kind == "image" { "image" } else { "file" }),
            ),
            ("name", JsValue::from(name.as_str())),
            ("path", JsValue::from(path.to_string_lossy().as_ref())),
            ("mimeType", JsValue::from(mime.as_str())),
            ("bytes", JsValue::Number(data.len() as f64)),
            (
                "metadata",
                object([
                    ("channelKey", JsValue::from("wecom")),
                    ("source", JsValue::from("wecom")),
                    ("mediaType", JsValue::from(kind)),
                ]),
            ),
        ])))
    }
}

pub(super) struct Prepared {
    data: Vec<u8>,
    kind: &'static str,
    name: String,
    reject: Option<String>,
    downgrade: Option<String>,
}
pub(super) fn prepare(data: Vec<u8>, content: &str, name: &str) -> Prepared {
    let content = mime(name, content);
    let detected = if content.starts_with("image/") {
        "image"
    } else if content.starts_with("video/") {
        "video"
    } else if content.starts_with("audio/") || content == "application/ogg" {
        "voice"
    } else {
        "file"
    };
    let size = data.len();
    let mb = size as f64 / (1024.0 * 1024.0);
    let mut kind = detected;
    let mut reject = None;
    let mut downgrade = None;
    if size > 20 * 1024 * 1024 {
        reject = Some(format!(
            "文件大小 {mb:.2}MB 超过企业微信 20MB 限制，无法发送。"
        ));
    } else if detected == "image" && size > 10 * 1024 * 1024 {
        kind = "file";
        downgrade = Some(format!(
            "图片大小 {mb:.2}MB 超过 10MB 限制，已转为文件格式发送。"
        ));
    } else if detected == "video" && size > 10 * 1024 * 1024 {
        kind = "file";
        downgrade = Some(format!(
            "视频大小 {mb:.2}MB 超过 10MB 限制，已转为文件格式发送。"
        ));
    } else if detected == "voice" {
        if !content.is_empty() && content != "audio/amr" {
            kind = "file";
            downgrade = Some(format!(
                "语音格式 {content} 不支持，企业微信原生语音仅支持 AMR，已转为文件格式发送。"
            ));
        } else if size > 2 * 1024 * 1024 {
            kind = "file";
            downgrade = Some(format!(
                "语音大小 {mb:.2}MB 超过 2MB 限制，已转为文件格式发送。"
            ));
        }
    }
    Prepared {
        data,
        kind,
        name: name.into(),
        reject,
        downgrade,
    }
}
impl Shared {
    async fn upload(&self, media: &Prepared) -> Result<JsString, ChannelError> {
        use md5::{Digest, Md5};
        if media.data.is_empty() {
            return Err(ChannelError::error("Cannot upload empty media"));
        }
        let chunks = media.data.len().div_ceil(512 * 1024);
        if chunks > 100 {
            return Err(ChannelError::error(format!(
                "File too large: {chunks} chunks exceeds maximum of 100"
            )));
        }
        let init = self
            .request(
                "aibot_upload_media_init",
                req_id("aibot_upload_media_init"),
                object([
                    ("type", JsValue::from(media.kind)),
                    ("filename", JsValue::from(media.name.as_str())),
                    ("total_size", JsValue::Number(media.data.len() as f64)),
                    ("total_chunks", JsValue::Number(chunks as f64)),
                    (
                        "md5",
                        JsValue::from(format!("{:x}", Md5::digest(&media.data)).as_str()),
                    ),
                ]),
            )
            .await?;
        if let Some(e) = wire::response_error(&init) {
            return Err(ChannelError::error(format!(
                "media upload init failed: {e}"
            )));
        }
        let upload = init
            .get("body")
            .map(|body| {
                body.get("upload_id")
                    .filter(|v| !sms::nullish(v))
                    .or_else(|| body.get("uploadId"))
                    .map(sms::js_string_of)
                    .map(trim)
                    .unwrap_or_else(|| "".into())
            })
            .unwrap_or_else(|| "".into());
        if upload.is_empty() {
            return Err(ChannelError::error(
                "media upload init failed: missing upload_id",
            ));
        }
        for (index, chunk) in media.data.chunks(512 * 1024).enumerate() {
            let response = self
                .request(
                    "aibot_upload_media_chunk",
                    req_id("aibot_upload_media_chunk"),
                    object([
                        ("upload_id", JsValue::String(upload.clone())),
                        ("chunk_index", JsValue::Number(index as f64)),
                        (
                            "base64_data",
                            JsValue::from(
                                base64::engine::general_purpose::STANDARD
                                    .encode(chunk)
                                    .as_str(),
                            ),
                        ),
                    ]),
                )
                .await?;
            if let Some(e) = wire::response_error(&response) {
                return Err(ChannelError::error(format!(
                    "media upload chunk {index} failed: {e}"
                )));
            }
        }
        let finish = self
            .request(
                "aibot_upload_media_finish",
                req_id("aibot_upload_media_finish"),
                object([("upload_id", JsValue::String(upload))]),
            )
            .await?;
        if let Some(e) = wire::response_error(&finish) {
            return Err(ChannelError::error(format!(
                "media upload finish failed: {e}"
            )));
        }
        let media_id = finish
            .get("body")
            .and_then(|v| {
                v.get("media_id")
                    .filter(|v| !sms::nullish(v))
                    .or_else(|| v.get("mediaId"))
            })
            .or_else(|| {
                finish
                    .get("media_id")
                    .filter(|v| !sms::nullish(v))
                    .or_else(|| finish.get("mediaId"))
            })
            .map(sms::js_string_of)
            .map(trim)
            .unwrap_or_else(|| "".into());
        if media_id.is_empty() {
            return Err(ChannelError::error("media upload did not return media_id"));
        }
        Ok(media_id)
    }
    pub(super) async fn send_prepared(&self, input: &Inbound, prepared: Prepared) -> bool {
        if let Some(reject) = &prepared.reject {
            self.send_reply(
                &input.chat,
                &JsString::from(reject.clone()),
                Some(&input.message_id),
                input.group,
            )
            .await;
            return false;
        }
        let result = async {
            let media_id = self.upload(&prepared).await?;
            let mut media_object = JsObject::new();
            media_object.insert("msgtype".into(), JsValue::from(prepared.kind));
            media_object.insert(
                prepared.kind.into(),
                object([("media_id", JsValue::String(media_id))]),
            );
            let reply_id = self.reply_id(&input.chat, Some(&input.message_id));
            let response = if let Some(id) = reply_id {
                self.request("aibot_respond_msg", id, JsValue::Object(media_object))
                    .await?
            } else if input.group {
                return Ok(false);
            } else {
                media_object.insert("chatid".into(), JsValue::String(input.chat.clone()));
                self.request(
                    "aibot_send_msg",
                    req_id("aibot_send_msg"),
                    JsValue::Object(media_object),
                )
                .await?
            };
            if let Some(error) = wire::response_error(&response) {
                return Err(ChannelError::error(error));
            }
            Ok::<_, ChannelError>(true)
        }
        .await;
        match result {
            Ok(ok) => {
                if let Some(note) = prepared.downgrade {
                    if ok {
                        self.send_reply(
                            &input.chat,
                            &note.into(),
                            Some(&input.message_id),
                            input.group,
                        )
                        .await;
                    }
                }
                ok
            }
            Err(e) => {
                self.log("error", &format!("wecom: send media failed: {e}"));
                false
            }
        }
    }
    pub(super) async fn send_event_media(&self, input: &Inbound, event: &JsValue) {
        if !string(event, "type").equals_str("tool_call_finished") {
            return;
        }
        let Some(JsValue::Array(images)) = event.get("images") else {
            return;
        };
        for (index, image) in images.iter().enumerate() {
            let content = string(image, "mimeType");
            let content = String::from_utf16_lossy(content.units());
            let ext = match content.as_str() {
                "image/jpeg" => ".jpg",
                "image/gif" => ".gif",
                "image/webp" => ".webp",
                "image/png" => ".png",
                "application/pdf" => ".pdf",
                _ => ".png",
            };
            let id = event
                .get("toolCallId")
                .filter(|v| sms::truthy(v))
                .map(sms::js_string_of)
                .unwrap_or_else(|| "image".into());
            let name = format!(
                "tool-{}-{}{ext}",
                String::from_utf16_lossy(id.units()),
                index + 1
            );
            let prepared = prepare(
                base64_bytes(&String::from_utf16_lossy(string(image, "data").units())),
                &content,
                &name,
            );
            if !self.send_prepared(input, prepared).await {
                self.send_reply(
                    &input.chat,
                    &"图片结果生成成功，但发送到企业微信失败。".into(),
                    Some(&input.message_id),
                    input.group,
                )
                .await;
            }
        }
    }
    pub(super) async fn send_local_file(&self, input: &Inbound, path: &std::path::Path) -> bool {
        match tokio::fs::read(path).await {
            Ok(data) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("wecom_media");
                let mut prepared = prepare(data, "", name);
                let ext = path
                    .extension()
                    .and_then(|v| v.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if !matches!(
                    ext.as_str(),
                    "png"
                        | "jpg"
                        | "jpeg"
                        | "gif"
                        | "webp"
                        | "bmp"
                        | "tiff"
                        | "svg"
                        | "mp4"
                        | "mov"
                        | "avi"
                        | "mkv"
                        | "webm"
                        | "mp3"
                        | "wav"
                        | "ogg"
                        | "opus"
                        | "m4a"
                        | "flac"
                        | "amr"
                ) {
                    prepared.kind = "file";
                }
                self.send_prepared(input, prepared).await
            }
            Err(e) => {
                self.log("error", &format!("wecom: send media source failed: {e}"));
                false
            }
        }
    }
}
