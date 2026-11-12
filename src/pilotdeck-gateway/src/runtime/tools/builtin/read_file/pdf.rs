use super::*;
use crate::pdf::Document;

fn base_data(resolved: &ResolvedPath) -> JsObject {
    object([
        ("filePath", string(resolved.relative.clone())),
        ("kind", string("pdf")),
    ])
}
fn output(message: String, data: JsObject) -> ToolOutput {
    ToolOutput {
        content: vec![JsValue::Object(object([
            ("type", string("text")),
            ("text", string(message)),
        ]))],
        data: Some(JsValue::Object(data)),
        ..Default::default()
    }
}
fn failure(error: crate::pdf::Error) -> ToolError {
    tool_error(
        if matches!(error, crate::pdf::Error::Unavailable) {
            "unsupported_tool"
        } else {
            "tool_execution_failed"
        },
        error.to_string(),
    )
}

pub(super) async fn read(
    input: &JsValue,
    context: &ToolRunContext,
    resolved: &ResolvedPath,
    stat: &super::super::Stat,
    key: &JsString,
) -> Result<ToolOutput, ToolError> {
    let multimodal = context.fields.get("modelMultimodal");
    let supports = |kind: &str| {
        matches!(multimodal.and_then(|m| m.get("input")), Some(JsValue::Array(items))
        if items.iter().any(|item| item.as_string().is_some_and(|s| s.equals_str(kind))))
    };
    let supports_pdf = supports("pdf");
    let supports_image = supports("image");
    let pages = field(input, "pages")
        .as_string()
        .map(text)
        .filter(|s| !s.trim().is_empty());
    let range = pages
        .as_deref()
        .map(|value| {
            parse_pdf_page_range(value).ok_or_else(|| {
                tool_error(
                    "invalid_tool_input",
                    format!("Invalid PDF page range: {value}."),
                )
            })
        })
        .transpose()?;
    if range.is_some_and(|(a, b)| b - a + 1.0 > MAX_PDF_PAGES_PER_REQUEST as f64) {
        return Err(tool_error(
            "invalid_tool_input",
            format!("pages exceeds the maximum of {MAX_PDF_PAGES_PER_REQUEST} pages per request."),
        ));
    }
    if stat.size > crate::pdf::MAX_INPUT {
        return Err(tool_error(
            "result_too_large",
            "PDF exceeds the native component input size limit.",
        ));
    }
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(os_path(&resolved.absolute))
        .await
        .map_err(|error| failed(error, "open", &resolved.absolute))?;
    let mut bytes = Vec::new();
    file.take(crate::pdf::MAX_INPUT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| failed(error, "read", &resolved.absolute))?;
    let document = Document::from_bytes(&bytes).map_err(failure)?;
    let count = document.pages().await.map_err(failure)?;
    let relative = text(&resolved.relative);
    let suffix = if count == 0 {
        String::new()
    } else {
        format!(", {count} pages")
    };
    if let Some((_, last)) = range
        && last > count as f64
    {
        return Err(tool_error(
            "invalid_tool_input",
            format!(
                "PDF page range {} exceeds the detected page count ({count}).",
                pages.as_deref().unwrap_or("")
            ),
        ));
    }
    if range.is_none() && count > 10 {
        return Err(tool_error(
            "invalid_tool_input",
            format!(
                "This PDF has {count} pages, which is too many to read at once. Use the pages parameter to read specific page ranges (e.g., pages: \"1-5\"). Maximum {MAX_PDF_PAGES_PER_REQUEST} pages per request."
            ),
        ));
    }
    let should_render = range.is_some() || !supports_pdf || stat.size > PDF_EXTRACT_SIZE_THRESHOLD;
    if should_render {
        let mut data = base_data(resolved);
        if !supports_image {
            let reason = if range.is_some() {
                "Current model does not support image input; cannot render requested pages."
            } else {
                data.insert("modelSupportsPdf".into(), JsValue::Bool(false));
                "Current model does not support PDF input or image input."
            };
            data.insert("modelSupportsImage".into(), JsValue::Bool(false));
            data.insert("pageCount".into(), JsValue::Number(count as f64));
            return Ok(output(
                format!(
                    "[PDF file: {relative}, {} bytes{suffix}. {reason}]",
                    stat.size
                ),
                data,
            ));
        }
        let (first, last) = range.map_or((1, count.min(20)), |(first, last)| {
            (first as u32, last as u32)
        });
        let budget = multimodal
            .and_then(|m| m.get("maxImageBytes"))
            .and_then(JsValue::as_number)
            .unwrap_or(MAX_IMAGE_BYTES as f64)
            .clamp(0.0, MAX_IMAGE_BYTES as f64) as usize;
        let detail = multimodal.and_then(|m| m.get("imageDetail")).cloned();
        let mut images = Vec::new();
        let mut render_error = None;
        for page in first..=last {
            match document.render(page, budget).await {
                Ok(bytes) => {
                    let mut image = object([
                        ("type", string("image")),
                        ("mimeType", string("image/jpeg")),
                        ("data", string(BASE64.encode(&bytes))),
                        ("bytes", JsValue::Number(bytes.len() as f64)),
                    ]);
                    if let Some(detail) = &detail {
                        image.insert("detail".into(), detail.clone());
                    }
                    images.push(JsValue::Object(image));
                }
                Err(error) => {
                    render_error = Some(error.to_string());
                    break;
                }
            }
        }
        if images.is_empty() && render_error.is_none() {
            render_error = Some(format!("mupdf produced no page images for {relative}"));
        }
        if let Some(error) = render_error {
            if range.is_none() {
                data.insert("modelSupportsPdf".into(), JsValue::Bool(supports_pdf));
            }
            data.insert("pageCount".into(), JsValue::Number(count as f64));
            data.insert("renderError".into(), string(error.clone()));
            let message = if range.is_some() {
                format!("[PDF file: {relative}. PDF page rendering failed: {error}]")
            } else {
                format!(
                    "[PDF file: {relative}, {} bytes{suffix}. PDF page rendering failed: {error}]",
                    stat.size
                )
            };
            return Ok(output(message, data));
        }
        let total = if count == 0 {
            String::new()
        } else {
            format!(" of {count}")
        };
        let message = if let Some(pages) = pages {
            data.insert("pdfPagesRendered".into(), JsValue::Bool(true));
            data.insert("pageCount".into(), JsValue::Number(count as f64));
            data.insert("requestedPages".into(), string(pages));
            format!(
                "PDF pages extracted: {} page(s) from {relative} (pages {first}-{last}{total}).",
                last - first + 1
            )
        } else {
            let reason = if !supports_pdf {
                "model does not support PDF input"
            } else {
                "file exceeds 3MB threshold"
            };
            data.insert("modelSupportsPdf".into(), JsValue::Bool(supports_pdf));
            data.insert("pdfPagesRendered".into(), JsValue::Bool(true));
            data.insert("degradeReason".into(), string(reason));
            data.insert("pageCount".into(), JsValue::Number(count as f64));
            format!("[PDF pages rendered from {relative}: {first}-{last}{total} ({reason}).]")
        };
        data.insert(
            "renderedPages".into(),
            JsValue::Object(object([
                ("firstPage", JsValue::Number(first as f64)),
                ("lastPage", JsValue::Number(last as f64)),
            ])),
        );
        data.insert("truncated".into(), JsValue::Bool(false));
        let mut result = output(message, data);
        result.supplemental_messages = Some(vec![JsValue::Object(object([
            ("role", string("user")),
            ("content", JsValue::Array(images)),
            ("isMeta", JsValue::Bool(true)),
        ]))]);
        result.metadata = Some(object([("truncated", JsValue::Bool(false))]));
        read_state_set(
            context,
            key.clone(),
            read_state_value(stat.mtime_ms, ReadKind::Pdf, input),
        );
        return Ok(result);
    }
    let mut data = base_data(resolved);
    data.insert("bytes".into(), JsValue::Number(bytes.len() as f64));
    data.insert("pageCount".into(), JsValue::Number(count as f64));
    let mut result = output(
        format!("PDF file read: {relative} ({} bytes{suffix})", stat.size),
        data,
    );
    result.supplemental_messages = Some(vec![JsValue::Object(object([
        ("role", string("user")),
        (
            "content",
            JsValue::Array(vec![JsValue::Object(object([
                ("type", string("pdf")),
                ("mimeType", string("application/pdf")),
                ("data", string(BASE64.encode(&bytes))),
                ("bytes", JsValue::Number(bytes.len() as f64)),
                ("pages", JsValue::Number(count as f64)),
            ]))]),
        ),
        ("isMeta", JsValue::Bool(true)),
    ]))]);
    read_state_set(
        context,
        key.clone(),
        read_state_value(stat.mtime_ms, ReadKind::Pdf, input),
    );
    Ok(result)
}
