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
    // Node path.basename on the supported POSIX hosts is lexical, including
    // dot names; replace each *run* outside the TS allowed class once.
    let name = name.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let mut result = String::new();
    let mut replacing = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric()
            || matches!(c, '_' | '.' | '-' | '(' | ')')
            || ('\u{4e00}'..='\u{9fff}').contains(&c)
        {
            result.push(c);
            replacing = false;
        } else if !replacing {
            result.push('_');
            replacing = true;
        }
    }
    if result.is_empty() {
        "wecom_media".into()
    } else {
        result
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
    pub(super) async fn download(
        &self,
        url: &str,
    ) -> Result<(Vec<u8>, String, Option<String>), String> {
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
            let disposition = response
                .headers()
                .get("content-disposition")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
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
            Ok((data, content, disposition))
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
        let dir = self.media_cache_dir.clone();
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
        name: safe_name(name),
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

fn disposition_name(value: Option<&str>) -> Result<Option<String>, String> {
    use std::sync::LazyLock;
    static UTF8: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new("(?i)filename\\*=UTF-8''([^;]+)").unwrap());
    static PLAIN: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new("(?i)filename=\"?([^\";]+)\"?").unwrap());
    let value = value.unwrap_or("");
    if let Some(caps) = UTF8.captures(value) {
        let value = caps.get(1).unwrap().as_str().as_bytes();
        let mut result = Vec::new();
        let mut i = 0;
        while i < value.len() {
            if value[i] == b'%' {
                if i + 2 >= value.len() {
                    return Err("URI malformed".into());
                }
                let pair =
                    std::str::from_utf8(&value[i + 1..i + 3]).map_err(|_| "URI malformed")?;
                result.push(u8::from_str_radix(pair, 16).map_err(|_| "URI malformed")?);
                i += 3;
            } else {
                result.push(value[i]);
                i += 1;
            }
        }
        return String::from_utf8(result)
            .map(Some)
            .map_err(|_| "URI malformed".into());
    }
    Ok(PLAIN
        .captures(value)
        .map(|caps| caps.get(1).unwrap().as_str().to_owned()))
}
impl Shared {
    async fn load_source(
        &self,
        source: &JsString,
        file_name: Option<&JsString>,
    ) -> Result<Prepared, String> {
        let source = trim(source.clone());
        if source.is_empty() {
            return Err("media source is required".into());
        }
        let source = String::from_utf16_lossy(source.units());
        let parsed = url::Url::parse(&source).ok();
        let (data, content, name) = match parsed {
            Some(url) if url.scheme() == "http" || url.scheme() == "https" => {
                let (data, content, disposition) = self.download(&source).await?;
                let name = disposition_name(disposition.as_deref())?.unwrap_or_else(|| {
                    url.path()
                        .rsplit('/')
                        .next()
                        .filter(|v| !v.is_empty())
                        .unwrap_or("wecom_media")
                        .to_owned()
                });
                (data, content, name)
            }
            parsed => {
                let path = if let Some(url) = parsed.filter(|url| url.scheme() == "file") {
                    url.to_file_path()
                        .map_err(|_| "Invalid file URL".to_owned())?
                } else {
                    let path = std::path::PathBuf::from(&source);
                    let path = if path.is_absolute() {
                        path
                    } else {
                        std::env::current_dir()
                            .map_err(|e| e.to_string())?
                            .join(path)
                    };
                    let info = tokio::fs::metadata(&path)
                        .await
                        .map_err(|e| e.to_string())?;
                    if !info.is_file() {
                        return Err(format!("media source is not a file: {}", path.display()));
                    }
                    path
                };
                let name = path
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("wecom_media")
                    .to_owned();
                let data = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
                (data, String::new(), name)
            }
        };
        let name = file_name
            .map(|v| String::from_utf16_lossy(v.units()))
            .unwrap_or(name);
        Ok(prepare(data, &content, &name))
    }
    pub(super) async fn send_media_source(
        &self,
        chat: JsString,
        source: JsString,
        caption: Option<JsString>,
        file_name: Option<JsString>,
        reply_to: Option<JsString>,
        force_file: bool,
    ) -> bool {
        if chat.is_empty() {
            return false;
        }
        let mut prepared = match tokio::select! {biased;_=self.stopped()=>return false,result=self.load_source(&source,file_name.as_ref())=>result}
        {
            Ok(prepared) => prepared,
            Err(error) => {
                self.log(
                    "error",
                    &format!("wecom: send media source failed: {error}"),
                );
                return false;
            }
        };
        if force_file {
            prepared.kind = "file";
        }
        let input = Inbound {
            chat: chat.clone(),
            user: "".into(),
            group: false,
            interaction: chat,
            text: "".into(),
            message_id: reply_to.unwrap_or_else(|| "".into()),
            attachments: None,
        };
        let sent = self.send_prepared(&input, prepared).await;
        if sent {
            if let Some(caption) = caption.filter(|text| !text.is_empty()) {
                self.send_reply(&input.chat, &caption, Some(&input.message_id), false)
                    .await;
            }
        }
        sent
    }
}
impl WeComChannel {
    fn media_operation(
        &self,
        chat: JsString,
        source: JsString,
        caption: Option<JsString>,
        file_name: Option<JsString>,
        reply_to: Option<JsString>,
        force_file: bool,
    ) -> BoxFuture<bool> {
        if *self.0.stopping.borrow() {
            return Box::pin(async { false });
        }
        let shared = self.0.clone();
        let (sender, result) = oneshot::channel();
        self.0.spawn_handler(async move {
            let sent = shared
                .send_media_source(chat, source, caption, file_name, reply_to, force_file)
                .await;
            let _ = sender.send(sent);
        });
        Box::pin(async move { result.await.unwrap_or(false) })
    }
    pub fn send_image(
        &self,
        chat: JsString,
        source: JsString,
        caption: Option<JsString>,
        reply_to: Option<JsString>,
    ) -> BoxFuture<bool> {
        self.media_operation(chat, source, caption, None, reply_to, false)
    }
    pub fn send_image_file(
        &self,
        chat: JsString,
        path: JsString,
        caption: Option<JsString>,
        reply_to: Option<JsString>,
    ) -> BoxFuture<bool> {
        self.send_image(chat, path, caption, reply_to)
    }
    pub fn send_document(
        &self,
        chat: JsString,
        path: JsString,
        caption: Option<JsString>,
        file_name: Option<JsString>,
        reply_to: Option<JsString>,
    ) -> BoxFuture<bool> {
        self.media_operation(chat, path, caption, file_name, reply_to, true)
    }
    pub fn send_voice(
        &self,
        chat: JsString,
        path: JsString,
        caption: Option<JsString>,
        reply_to: Option<JsString>,
    ) -> BoxFuture<bool> {
        self.media_operation(chat, path, caption, None, reply_to, false)
    }
    pub fn send_video(
        &self,
        chat: JsString,
        path: JsString,
        caption: Option<JsString>,
        reply_to: Option<JsString>,
    ) -> BoxFuture<bool> {
        self.media_operation(chat, path, caption, None, reply_to, false)
    }
}
