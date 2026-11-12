use super::{Options, assembly::Recorders};
use crate::{
    config::Env,
    metadata::{object, resolve_path, resolve_units, text_field},
    runtime::{
        context::compaction::js_truthy,
        project::{ProjectResources, ProjectRuntimeRegistry},
        snapshot::{SnapshotProjectResolver, SnapshotProjects},
        turn::{
            submit::{NativeSubmitSnapshots, SubmitError, SubmitFuture, SubmitSnapshots},
            submit_host::{SubmitAttachmentLease, SubmitCwdService, SubmitHostServices},
        },
    },
    storage::{ContentAddressedWorkspaceSnapshotRecorder, RunRef},
    upload::UploadStore,
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::collections::HashSet;
use std::fs;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Services {
    pub options: Options,
    pub project: Arc<ProjectRuntimeRegistry>,
    pub env: Env,
    pub recorders: Recorders,
    pub owner: crate::runtime::session::SessionOwner,
    /// Extracted once at startup from `PersistenceResources`; `None` when
    /// `--storage-root` was not supplied and snapshots are disabled.
    pub snapshot_recorder: Option<Arc<ContentAddressedWorkspaceSnapshotRecorder>>,
    pub upload_store: Arc<UploadStore>,
    /// The gateway's session selections, including the live permission modes
    /// (`GatewaySessionPermissionModeRegistry`).
    pub session_overrides: Arc<super::SessionOverrides>,
    /// createLocalGateway's `configStore` and its `startWatching()` handle:
    /// `refreshConfigBeforeTurn` reloads it at every turn start.
    pub config_store: Option<NativeConfigStore>,
}

/// The process config store with its file watcher kept alive.
pub struct NativeConfigStore {
    pub store: ConfigStore,
    /// Held for its `Drop`, which stops the watcher.
    pub _watch: Option<crate::config::store::WatchHandle>,
}
fn base64_encode(bytes: &[u8]) -> String {
    BASE64_STANDARD.encode(bytes)
}

/// TS AttachmentResolver.ts:36 — `DEFAULT_MAX_FILE_BYTES = 1_000_000` (1 MB):
/// larger file attachments are skipped with an `Attachment … skipped`
/// diagnostic instead of being resolved inline.
const MAX_INLINE_FILE_BYTES: u64 = 1_000_000;
/// TS AttachmentResolver.ts:37 — `DEFAULT_MAX_IMAGE_BYTES = 5_242_880` (5 MiB):
/// larger images are skipped with an `Image … skipped` diagnostic instead of
/// being base64-inlined (verified against the TS source by the
/// `_tmp/20261105-issue-1321-fix-r1/ts-probe/` probe).
const MAX_INLINE_IMAGE_BYTES: u64 = 5_242_880;
/// TS AttachmentResolver.ts:38 — `DEFAULT_BYTES_PER_PDF_PAGE = 102_400`
/// (100 KB): the divisor of the pdf size→page estimate
/// `Math.max(1, Math.round(size / bytesPerPdfPage))` (resolver :304).
const BYTES_PER_PDF_PAGE: u64 = 102_400;
/// TS GatewayAttachmentTurnComposer.ts:21 — `ATTACHMENT_PATH_NOTE_MARKER`.
const ATTACHMENT_PATH_NOTE_MARKER: &str = "[Registered attachment files in this session:]";
/// TS GatewayAttachmentTurnComposer.ts:95 — the funasr install command the
/// audio guidance names (`installCommand = "npm run install:asr"` default).
/// The native process has no TS package root to compute
/// `getPilotDeckInstallCommand()` (projectMcpSpec.ts:27-37), so the composer
/// default is used verbatim.
const FUNASR_INSTALL_COMMAND: &str = "npm run install:asr";
/// TS AttachmentResolver.ts:40-61 — `TEXT_EXTENSIONS`: the whitelisted
/// plain-text extensions a path-based file attachment is inlined from.
const TEXT_EXTENSIONS: &[&str] = &[
    ".txt", ".md", ".json", ".yaml", ".yml", ".csv", ".tsv", ".xml", ".html", ".css", ".ts",
    ".tsx", ".js", ".jsx", ".py", ".go", ".rs", ".java", ".sh", ".log",
];
/// TS GatewayAttachmentTurnComposer.ts:22-40 —
/// `READ_FILE_BINARY_ATTACHMENT_EXTENSIONS` (the same list as
/// AttachmentResolver.ts:62-80 `BINARY_CONTAINER_EXTENSIONS`): attachments
/// the guidance calls out as not directly inspectable with read_file.
const BINARY_CONTAINER_EXTENSIONS: &[&str] = &[
    ".zip", ".gz", ".tar", ".7z", ".rar", ".doc", ".docx", ".ppt", ".pptx", ".xls", ".xlsx",
    ".odt", ".ods", ".odp", ".pages", ".key", ".numbers",
];
/// TS GatewayAttachmentTurnComposer.ts:283 — the audio suffixes
/// `isAudioAttachment` matches case-insensitively.
const AUDIO_EXTENSIONS: &[&str] = &[
    ".aac", ".flac", ".m4a", ".mp3", ".oga", ".ogg", ".opus", ".wav", ".webm",
];
/// TS GatewayAttachmentTurnComposer.ts:118 — the plain (no-diagnostics)
/// guidance closing the path note.
const PLAIN_PATH_NOTE_GUIDANCE: &str = "These are path references for reuse. If an image/PDF is \
     already visible in this turn, do not call read_file just to view it.";

/// One attachment as the composer sees it (the TS ChannelAttachment slice
/// read by GatewayAttachmentTurnComposer.ts:211-253). A missing, empty, or
/// non-UTF-8 path leaves `path` empty; the composer's own
/// `if (!attachment.path) continue;` (:229) drops those silently.
struct AttachEntry {
    /// `attachment.type === "image"` (composer :212, :234).
    image_type: bool,
    /// `attachment.type === "text"` (composer :224).
    text_type: bool,
    path: String,
    /// A truthy-but-non-UTF-8 `path` (a lone surrogate), kept as the raw
    /// UTF-16 for the not-found diagnostic (P1-d/#1461: the outer path is
    /// the TS `path.resolve` form of it — surrogate included — and only the
    /// fs error spelling is lossy): it never reaches the lease guard, an
    /// open, or allowedReadFiles.
    invalid_path: Option<JsString>,
    /// Raw `attachment.name` ("" when absent or not UTF-8).
    name: String,
    mime: String,
    /// Raw nonempty `attachment.content`, inlined before any path handling
    /// (composer :212, :224).
    content: Option<String>,
    /// Raw `attachment.bytes`: only the content image block forwards it
    /// verbatim (composer :218); path-based blocks report the bytes actually
    /// read (AttachmentResolver.ts:274).
    declared_bytes: Option<JsValue>,
    /// Truthy `metadata.channelKey` (composer :181, :238, :244, :250): only
    /// channel-registered attachments enter allowedReadFiles and the path
    /// note. Uploaded lease attachments (bundle :57-62 — uploadId,
    /// attachmentId, relativePath, sha256, no channelKey) are never
    /// registered.
    channel_key: bool,
}

/// A path attachment that resolved inside the lease root and opened as a
/// regular file. allowedReadFiles comes from these, mirroring TS
/// `collectRegisteredAttachmentReadFiles` (GatewayAttachmentTurnComposer.ts
/// :173-193, `kind === "file"` only). #2495 — the note lines and the
/// hasInspectable accumulation read the whole attachment list below, not
/// this table: TS gates both only on the allowedReadFiles membership test
/// (`safeAllowedAttachmentPath`, composer :103/:144), so a second entry
/// with the same lexical spelling (or an unregistered entry colliding with
/// a registered spelling) still contributes.
struct RegisteredAttachment {
    /// #1446 — the Node `path.resolve` spelling of the client-sent path
    /// (`allowed.add(resolve(attachment.path))`, composer :182). This is
    /// the allowedReadFiles lexical spelling; the #2495 note pass re-derives
    /// the same spelling from `entries` through the same resolve
    /// (`attachment_lexical` below — the two are equal by construction,
    /// `safeAllowedAttachmentPath`, composer :167-171); a symlinked project
    /// spelling or a mid-path link keeps it distinct from `resolved`.
    lexical: PathBuf,
    /// The canonicalized spelling (`allowed.add(resolve(await
    /// port.realpath(attachment.path)))`, composer :183) — the containment
    /// authority and the second allowedReadFiles entry.
    resolved: PathBuf,
}

/// Node `extname` on the last path segment, lowercased with its leading dot
/// ("" when there is no extension) — the shape both TS files compare against
/// (AttachmentResolver.ts:148, GatewayAttachmentTurnComposer.ts:161).
fn extension_of(source: &str) -> String {
    Path::new(source)
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

/// TS AttachmentResolver.ts:148 — prefer the trimmed attachment name's
/// extension, else the path's: upload stores use an opaque id on disk, so
/// the original filename carries the format.
fn resolver_extension(entry: &AttachEntry) -> String {
    let trimmed = entry.name.trim();
    extension_of(if trimmed.is_empty() {
        entry.path.as_str()
    } else {
        trimmed
    })
}

/// `detectImageMime` (AttachmentResolver.ts): the magic bytes of the four
/// accepted formats; fewer than 12 bytes is never an image.
fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 12 {
        return None;
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// TS AttachmentResolver.ts:81-87 — the `IMAGE_MIME` extension map.
fn image_mime_for_ext(ext: &str) -> Option<&'static str> {
    match ext {
        ".png" => Some("image/png"),
        ".jpg" | ".jpeg" => Some("image/jpeg"),
        ".gif" => Some("image/gif"),
        ".webp" => Some("image/webp"),
        _ => None,
    }
}

/// The resolver diagnostic severities the composer filters on
/// (AttachmentResolver.ts:12-22): every pre-existing diagnostic in this
/// file is TS `warning` severity there; only the two info diagnostics
/// (#1458 `pdf_size_estimate`, #1469 binary-container
/// `attachment_unsupported`) carry `Info`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DiagnosticSeverity {
    Warning,
    Info,
}

/// TS GatewayAttachmentTurnComposer.ts:259-263 — the one composer filter
/// both info diagnostics are held to: `severity === "error" ||
/// severity === "warning" || !item.registered`. Warnings (and errors) always
/// enter the `[Attachment diagnostics]` block; info only when the attachment
/// is not registered (`registered = Boolean(metadata?.channelKey)`,
/// composer :238/:244/:250 — `entry.channel_key` here). #1458 and #1469 must
/// share this single criterion, not grow one each.
fn diagnostic_enters(severity: DiagnosticSeverity, registered: bool) -> bool {
    match severity {
        DiagnosticSeverity::Warning => true,
        DiagnosticSeverity::Info => !registered,
    }
}

/// TS AttachmentResolver.ts:312-318 — the `pdf_size_estimate` info
/// diagnostic; the `[PDF attachment: …]` block above carries the same
/// estimate (`{pages}` from `BYTES_PER_PDF_PAGE`, resolver :304).
fn pdf_size_estimate(size: u64, pages: u64) -> String {
    format!(
        "Estimated {pages} pages from {size} bytes (PilotDeck does not invoke pdfinfo, \
         intentional_difference §4.5)."
    )
}

/// TS AttachmentResolver.ts:323-329 — `unsupportedFileMessage`: the
/// non-whitelisted-extension diagnostic of `resolveFile`
/// (severity per :158 — binary container info, otherwise warning).
/// TS `BINARY_CONTAINER_EXTENSIONS.has(ext)` in all three call sites — the
/// message form, the text-whitelist exclusion, and the info severity.
fn is_binary_container(ext: &str) -> bool {
    BINARY_CONTAINER_EXTENSIONS.contains(&ext)
}

fn unsupported_file_message(absolute: &str, ext: &str) -> String {
    let display_ext = if ext.is_empty() { "(none)" } else { ext };
    if is_binary_container(ext) {
        format!(
            "Attachment {absolute} has Office/archive/binary extension {display_ext}; it was \
             registered as a file path but not shown inline, and read_file cannot inspect this \
             format directly."
        )
    } else {
        format!(
            "File extension {display_ext} is not in the inline text whitelist; skipped. If \
             this is plain text, use read_file with the exact path; otherwise convert it \
             before inspection."
        )
    }
}

/// The image request's mime: the declared `mimeType`, else the extension
/// map, else none (AttachmentResolver.ts:214-227).
fn image_mime(entry: &AttachEntry) -> Option<String> {
    if !entry.mime.is_empty() {
        return Some(entry.mime.clone());
    }
    image_mime_for_ext(&extension_of(&entry.path)).map(str::to_owned)
}

/// TS GatewayAttachmentTurnComposer.ts:280-284 — the mime prefix first, else
/// the audio suffixes matched case-insensitively on the path (name as the
/// fallback). `is_audio_shape` takes the spelling explicitly so the
/// lone-surrogate branch (P1-d) can test the lossy path the way TS tests the
/// surrogate one (path wins over name there too).
fn is_audio_shape(mime: &str, path_or_name: &str) -> bool {
    if mime.to_lowercase().starts_with("audio/") {
        return true;
    }
    let lower = path_or_name.to_lowercase();
    AUDIO_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

fn is_audio_attachment(entry: &AttachEntry) -> bool {
    let path_or_name = if entry.path.is_empty() {
        entry.name.as_str()
    } else {
        entry.path.as_str()
    };
    is_audio_shape(&entry.mime, path_or_name)
}

/// TS GatewayAttachmentTurnComposer.ts:153-165 —
/// `isReadFileInspectableAttachment`.
fn is_read_file_inspectable(entry: &AttachEntry) -> bool {
    let mime = entry.mime.to_lowercase();
    if entry.image_type || mime.starts_with("image/") {
        return true;
    }
    if mime == "application/pdf"
        || mime.starts_with("text/")
        || mime == "application/json"
        || mime.ends_with("+json")
    {
        return true;
    }
    let path_or_name = if entry.name.is_empty() {
        entry.path.as_str()
    } else {
        entry.name.as_str()
    };
    let ext = extension_of(path_or_name);
    if ext == ".pdf" || ext == ".ipynb" {
        return true;
    }
    !is_binary_container(&ext)
}

/// TS GatewayAttachmentTurnComposer.ts:286-295 — the funasr hint only names
/// audio paths inside the project (lexical `relative`, no symlink chase);
/// anything else is left to the generic "project-local" wording.
fn map_audio_path_for_funasr(path: &str, project_root: &Path) -> Option<String> {
    let relative = Path::new(path).strip_prefix(project_root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(path.to_string())
}

/// TS GatewayAttachmentTurnComposer.ts:108-109 —
/// `String(name || fallback).replace(/[\r\n]+/g, " ").trim() || fallback`.
fn clean_attachment_name(raw: &str, fallback: &str) -> String {
    let base = if raw.is_empty() { fallback } else { raw };
    let mut cleaned = String::with_capacity(base.len());
    let mut previous_was_break = false;
    for character in base.chars() {
        if character == '\r' || character == '\n' {
            if !previous_was_break {
                cleaned.push(' ');
            }
            previous_was_break = true;
        } else {
            cleaned.push(character);
            previous_was_break = false;
        }
    }
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// The flat canonical image block (TS AttachmentResolver.ts:267-275):
/// `{type, source: "base64", data, mimeType, bytes}`. Every in-repo consumer
/// reads `source`/`data`/`mimeType` at the top level
/// (runtime/session/factory.rs `valid_snapshot_block`,
/// runtime/turn/projector.rs `canonical_content_text`,
/// runtime/model/count_body.rs).
fn flat_image_block(data: &str, mime: &str, bytes: Option<f64>) -> JsValue {
    let mut block = JsObject::new();
    block.insert("type".into(), "image".into());
    block.insert("source".into(), "base64".into());
    block.insert("data".into(), JsValue::from(data));
    block.insert("mimeType".into(), JsValue::from(mime));
    if let Some(bytes) = bytes {
        block.insert("bytes".into(), JsValue::Number(bytes));
    }
    JsValue::Object(block)
}

/// `Image attachment not found: ${absolute} (${error}).` — the missing-file
/// diagnostic of TS AttachmentResolver (AttachmentResolver.ts:191-200). A
/// read failure here (lease TTL cleanup, permissions, a directory swap)
/// uses the same channel so the attachment is never silently dropped.
fn image_not_found(path: &str, error: &str) -> String {
    format!("Image attachment not found: {path} ({error}).")
}

/// `Attachment not found: ${absolute} (${error}).` — the missing-file
/// diagnostic of TS AttachmentResolver for non-image files
/// (AttachmentResolver.ts:134-143).
fn file_not_found(path: &str, error: &str) -> String {
    format!("Attachment not found: {path} ({error}).")
}

/// The resolver request kind the composer picks for a path attachment
/// (GatewayAttachmentTurnComposer.ts:230-252): audio is skipped before any
/// request, so it is not a route.
enum AttachmentRoute {
    Image,
    Pdf,
    File,
}

/// composer :234-251 — the image request first (`type === "image"` or an
/// `image/` mime), then the pdf request (`application/pdf` mime or a `.pdf`
/// path suffix, case-insensitive), else the file request.
fn attachment_route(entry: &AttachEntry, path: &str) -> AttachmentRoute {
    if entry.image_type || entry.mime.starts_with("image/") {
        AttachmentRoute::Image
    } else if entry.mime == "application/pdf" || path.to_lowercase().ends_with(".pdf") {
        AttachmentRoute::Pdf
    } else {
        AttachmentRoute::File
    }
}

/// The not-found diagnostic in the TS resolver's own routing: image requests
/// say "Image attachment not found" (AttachmentResolver.ts:191-200), pdf
/// requests "PDF attachment not found" (:293-302), file requests "Attachment
/// not found" (:134-143).
fn attachment_not_found(entry: &AttachEntry, path: &str, error: &str) -> String {
    match attachment_route(entry, path) {
        AttachmentRoute::Image => image_not_found(path, error),
        AttachmentRoute::Pdf => format!("PDF attachment not found: {path} ({error})."),
        AttachmentRoute::File => file_not_found(path, error),
    }
}

/// #1461 (P1-d, ts-probe `_tmp/20261108-issue-1460-r2/probe/composer-probe.out`
/// "Probe B"): the not-found diagnostic of a lone-surrogate path keeps two
/// spellings. The outer path is the `path.resolve` form of the raw UTF-16 —
/// Node's lexical resolve keeps an unpaired surrogate and adds the absolute
/// prefix, so the diagnostic text carries it verbatim — and only the fs layer
/// mangles the surrogate to U+FFFD: Node converts the string to a native path
/// lossily before the stat, so the ENOENT message carries that spelling. The
/// resolve runs over UTF-16 units (`resolve_units`) because a lone surrogate
/// is not representable in a Rust `String` at all, and the route words the
/// line (image/pdf/file) like `attachment_not_found` does.
fn surrogate_path_not_found(entry: &AttachEntry, original: &JsString, cwd: &Path) -> JsString {
    let resolved = resolve_units(&cwd.to_string_lossy(), original);
    let lossy = String::from_utf16_lossy(resolved.units());
    // #1461 fix (DS r1 boundary P2): TS composer :234-251 routes the
    // diagnostic words on the *original* attachment path, and the resolve
    // above can fold dot segments — `report.pdf/\uD800/..` resolves to a
    // `.pdf` tail while the raw spelling ends in `..` and routes as File.
    // The raw lossy spelling (the same one `is_audio_shape` reads) keeps
    // the route on the unfixed input.
    let raw_lossy = String::from_utf16_lossy(original.units());
    let prefix = match attachment_route(entry, &raw_lossy) {
        AttachmentRoute::Image => "Image attachment not found: ",
        AttachmentRoute::Pdf => "PDF attachment not found: ",
        AttachmentRoute::File => "Attachment not found: ",
    };
    let mut units: Vec<u16> = prefix.encode_utf16().collect();
    units.extend_from_slice(resolved.units());
    units.extend(format!(" (ENOENT: no such file or directory, stat '{lossy}').").encode_utf16());
    JsString::from_utf16(units)
}

/// #1461 r2 (DS concurrency P1): Node `process.cwd()` caches the first
/// successful read (until chdir) — after the cwd is removed, `process.cwd()`
/// and `path.resolve("rel")` keep returning the cached directory; only a
/// failed *first* read throws `uv_cwd` (measured Node 22.22; the same cache
/// form as `skills::process_cwd`,
/// specs/gateway-parity/diffs/A7.4-TOOLBUDGET-CWD-001-R1.md).
///
/// #1461 r3 honest residual (DS boundary P1): this cache is *lazy* — it is
/// seeded by the first relative surrogate diagnostic that finds the cwd
/// readable, not at process start the way Node's is. The snapshot therefore
/// covers the *second and later* diagnostics after a mid-run deletion; the
/// FIRST relative surrogate diagnostic after the deletion still sees
/// `current_dir()` fail, returns `None`, and stays silent where a warm TS
/// process would resolve against its startup-seeded cache and emit the
/// diagnostic. A cwd that was never readable (cold on both sides) stays
/// silent either way — TS `path.resolve` throws `uv_cwd` there, and #1464
/// keeps an attachment failure from failing the turn. Closing the gap
/// needs a crate-level cache seeded at gateway start and shared with
/// `skills::process_cwd` (the A7.4-TOOLBUDGET-CWD-001 area) — tracked as a
/// followup, not changed here.
fn cached_process_cwd() -> Option<PathBuf> {
    static CACHED: Mutex<Option<PathBuf>> = Mutex::new(None);
    let mut cached = CACHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cwd) = &*cached {
        return Some(cwd.clone());
    }
    let cwd = std::env::current_dir().ok()?;
    *cached = Some(cwd.clone());
    Some(cwd)
}

/// The Node `path.resolve` spelling of a client-sent attachment path —
/// `allowed.add(resolve(attachment.path))`, composer :182. The registration
/// pass and the #2495 note pass must derive the identical spelling for the
/// membership test, so both call this one helper instead of re-spelling
/// the resolve (a drift here silently drops note lines, not a compile
/// error).
fn attachment_lexical(cwd: &Path, entry: &AttachEntry) -> PathBuf {
    resolve_path(cwd, Path::new(&entry.path))
}

/// TS AttachmentResolver.ts:171 — the file too-large diagnostic.
fn file_too_large(path: &str, bytes: u64, limit: u64) -> String {
    format!("Attachment {path} is {bytes} bytes (limit {limit}); skipped.")
}

/// TS AttachmentResolver.ts:209 — the image too-large diagnostic.
fn image_too_large(path: &str, bytes: u64, limit: u64) -> String {
    format!(
        "Image {path} is {bytes} bytes (limit {limit}); skipped \
         (PilotDeck does not resize, intentional_difference §4.5)."
    )
}

/// TS GatewayAttachmentTurnComposer.ts:125-151 — `attachmentDiagnosticsGuidance`.
fn attachment_diagnostics_guidance(
    entries: &[AttachEntry],
    has_inspectable: bool,
    project_root: &Path,
) -> String {
    // :131-140 — the audio form wins whenever ANY audio attachment exists
    // (P1-b: presence decides, not path mappability); a form with no
    // mappable path takes the generic hint (:138).
    let audio_attachments: Vec<&AttachEntry> = entries
        .iter()
        .filter(|entry| is_audio_attachment(entry))
        .collect();
    if !audio_attachments.is_empty() {
        let audio_paths: Vec<String> = audio_attachments
            .iter()
            .filter(|entry| !entry.path.is_empty())
            .filter_map(|entry| map_audio_path_for_funasr(&entry.path, project_root))
            .collect();
        let mapped_hint = if audio_paths.is_empty() {
            " Pass a project-local host path to transcribe_audio; paths outside this project \
             are rejected."
                .to_string()
        } else if audio_paths.len() == 1 {
            format!(
                " Pass the registered project-local path {} to transcribe_audio.",
                audio_paths[0]
            )
        } else {
            format!(
                " Pass the registered project-local paths {} to transcribe_audio.",
                audio_paths.join(", ")
            )
        };
        return format!(
            "Audio attachments are not readable with read_file. When the user asks for \
             transcription, subtitles, or audio analysis, use the funasr MCP server's \
             mcp__funasr__transcribe_audio tool.{mapped_hint} If that tool reports that its \
             runtime is missing, run {FUNASR_INSTALL_COMMAND} and retry the tool in this session."
        );
    }
    // :142-151 — no attachment with an allowedReadFiles spelling left
    // inspectable → the conversion form; otherwise the read_file-scoped form
    // (#2495: the accumulation is the caller's whole-list `some(...)`
    // pass, never gated by the note dedup).
    if !has_inspectable {
        return "Some attachments were not shown inline. These registered files are not \
                directly inspectable with read_file; ask for a supported export or \
                convert them before inspection."
            .to_string();
    }
    "Some attachments were not shown inline. Use read_file with the exact path only for \
     readable text, image, PDF, or notebook attachments; Office/archive/binary files \
     need conversion before inspection."
        .to_string()
}

/// Composes the blocks-type agent input for a turn with attachments,
/// mirroring TS GatewayAttachmentTurnComposer.prepare: a text block for the
/// message, direct content blocks and inlined image/text blocks, the
/// `[Attachment diagnostics]` text block (composer :270-274), and the
/// registered-path note (composer :89-123). Falls back to a plain
/// `{type:"text"}` turn when nothing but the message remains (composer
/// :67-72). Extracted from `prepare_attachment_turn` so the inline pipeline
/// is unit-testable without the full native assembly;
/// runs entirely on a blocking thread.
pub(super) fn compose_attachment_turn(
    project_dir: &Path,
    message: &JsValue,
    attachments: &[JsValue],
) -> Result<JsValue, SubmitError> {
    let mut entries: Vec<AttachEntry> = Vec::new();
    for attachment in attachments {
        let kind = text_field(attachment, "type").unwrap_or_default();
        // P1-d (codex): a truthy-but-non-UTF-8 path (a lone surrogate) is not
        // a silent skip in TS — Node keeps it, `path.resolve` keeps it, fs
        // mangles it to U+FFFD and the stat fails ENOENT, so the resolver
        // emits its not-found diagnostic (ts-probe/probe.out
        // "P1-d composer lone-surrogate"). #1461: the raw UTF-16 feeds that
        // diagnostic — the outer path keeps the surrogate and the resolve
        // prefix, only the fs error spelling is lossy.
        let (path, invalid_path) = match attachment.get("path") {
            Some(JsValue::String(value)) if !value.is_empty() => match value.to_utf8() {
                Ok(path) => (path, None),
                Err(_) => (String::new(), Some(value.clone())),
            },
            _ => (String::new(), None),
        };
        entries.push(AttachEntry {
            image_type: kind == "image",
            text_type: kind == "text",
            path,
            invalid_path,
            name: text_field(attachment, "name").unwrap_or_default(),
            mime: text_field(attachment, "mimeType").unwrap_or_default(),
            content: text_field(attachment, "content").filter(|content| !content.is_empty()),
            declared_bytes: attachment
                .get("bytes")
                .filter(|bytes| bytes.as_number().is_some())
                .cloned(),
            channel_key: attachment
                .get("metadata")
                .and_then(|metadata| metadata.get("channelKey"))
                .is_some_and(js_truthy),
        });
    }
    // The project spelling the funasr audio hint is held against.
    let project_canon = fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());

    let mut direct_blocks: Vec<JsValue> = Vec::new();
    let mut inline_blocks: Vec<JsValue> = Vec::new();
    // #1461: the lines are `JsString`s — a lone-surrogate path keeps its
    // unpaired surrogate in the outer diagnostic text, which a Rust `String`
    // cannot hold at all.
    let mut diagnostics: Vec<JsString> = Vec::new();
    let mut registered: Vec<RegisteredAttachment> = Vec::new();

    // TS composer :211-227 — content blocks are produced before any path
    // handling, and the producing attachment's `continue` (:221/:226 — P1-a:
    // the guard, open, read and diagnostics of the resolver projection are
    // all skipped, so a root-escaping path cannot fail or read a content
    // turn) skips only that projection. #1460: the registration collection
    // (`collectRegisteredAttachmentReadFiles`, composer :57/:173-193) is a
    // pass of its own that runs over every attachment — a content
    // attachment with a valid path and a channelKey is registered
    // (allowedReadFiles + path note) exactly like a path-only one.
    let mut produced_direct = vec![false; entries.len()];
    for (index, entry) in entries.iter().enumerate() {
        if entry.image_type
            && let Some(content) = &entry.content
            && !entry.mime.is_empty()
        {
            let declared_bytes = entry.declared_bytes.as_ref().and_then(JsValue::as_number);
            direct_blocks.push(flat_image_block(content, &entry.mime, declared_bytes));
            produced_direct[index] = true;
        } else if entry.text_type
            && let Some(content) = &entry.content
        {
            direct_blocks.push(object([
                ("type", "text".into()),
                ("text", JsValue::from(content.as_str())),
            ]));
            produced_direct[index] = true;
        }
    }

    // One cwd read for the whole prepare (Node `path.resolve` against the
    // process cwd, cached like Node's).
    let process_cwd = cached_process_cwd().unwrap_or_default();
    for (index, entry) in entries.iter().enumerate() {
        // #1460: `content_only` marks an attachment whose block the pass
        // above already emitted. Its registration still runs; the resolver
        // projection the TS `continue` skips does not.
        let content_only = produced_direct[index];
        // TS composer :229 — a missing or empty path is skipped silently;
        // the content branches above already handled their blocks.
        if entry.path.is_empty() {
            // P1-d: the lone-surrogate spelling is truthy in TS, so the
            // resolver stats it and diagnoses not-found. An audio-shaped
            // spelling stays silent, matching the audio skip (:230-233); a
            // content-produced one is equally silent.
            if let Some(original) = &entry.invalid_path
                && !content_only
                && !is_audio_shape(&entry.mime, &String::from_utf16_lossy(original.units()))
            {
                // Node `path.resolve(path)` resolves a relative spelling
                // against the process cwd; an absolute one never reads it.
                if original.units().first() == Some(&(b'/' as u16)) {
                    diagnostics.push(surrogate_path_not_found(entry, original, Path::new("/")));
                } else if let Some(cwd) = cached_process_cwd() {
                    diagnostics.push(surrogate_path_not_found(entry, original, &cwd));
                }
            }
            continue;
        }
        // The baseline composer (GatewayAttachmentTurnComposer.ts) and its
        // resolver (AttachmentResolver.ts) put no directory policy on a path
        // attachment: `resolve(path)` against the process cwd, `stat`, then
        // read. Any path the client names is inspected the same way.
        let absolute = attachment_lexical(&process_cwd, entry);
        let absolute_text = absolute.to_string_lossy().into_owned();
        let stat = fs::metadata(&absolute);
        // `collectRegisteredAttachmentReadFiles` (composer :173-193): a
        // channel attachment that stats as a file registers its resolved
        // spelling and its realpath; a stat/realpath failure is silent.
        if entry.channel_key
            && let Ok(metadata) = &stat
            && metadata.is_file()
            && let Ok(real) = fs::canonicalize(&absolute)
        {
            registered.push(RegisteredAttachment {
                lexical: absolute.clone(),
                resolved: real,
            });
        }
        // TS composer :230-233 — audio stays a registered path reference and
        // is never resolved; a content-produced attachment (#1460) already
        // emitted its block.
        if is_audio_attachment(entry) || content_only {
            continue;
        }
        let route = attachment_route(entry, &entry.path);
        let metadata = match stat {
            Ok(metadata) => metadata,
            Err(error) => {
                diagnostics.push(
                    attachment_not_found(
                        entry,
                        &absolute_text,
                        &crate::node_fs_message_or_unknown(&error, "stat", Some(&absolute_text)),
                    )
                    .into(),
                );
                continue;
            }
        };
        match route {
            AttachmentRoute::Pdf => {
                // resolver :287-320: stat only; `Math.max(1, Math.round(size /
                // bytesPerPdfPage))` estimates the pages.
                let estimated_pages = ((metadata.len() as f64) / BYTES_PER_PDF_PAGE as f64)
                    .round()
                    .max(1.0) as u64;
                inline_blocks.push(object([
                    ("type", "text".into()),
                    (
                        "text",
                        JsValue::from(
                            format!(
                                "[PDF attachment: {absolute_text}, {} bytes, estimated {estimated_pages} \
                                 pages. Use read_file on this registered attachment path to \
                                 inspect it.]",
                                metadata.len()
                            )
                            .as_str(),
                        ),
                    ),
                ]));
                // #1458 — the `pdf_size_estimate` info diagnostic enters only
                // for an unregistered attachment (composer :259-263).
                if diagnostic_enters(DiagnosticSeverity::Info, entry.channel_key) {
                    diagnostics.push(pdf_size_estimate(metadata.len(), estimated_pages).into());
                }
            }
            AttachmentRoute::File => {
                // resolver :145-183: whitelisted text extensions only (the
                // trimmed name's extension first), then the size cap, then
                // `readText`.
                let ext = resolver_extension(entry);
                if !TEXT_EXTENSIONS.contains(&ext.as_str()) {
                    let severity = if is_binary_container(&ext) {
                        DiagnosticSeverity::Info
                    } else {
                        DiagnosticSeverity::Warning
                    };
                    if diagnostic_enters(severity, entry.channel_key) {
                        diagnostics.push(unsupported_file_message(&absolute_text, &ext).into());
                    }
                    continue;
                }
                if metadata.len() > MAX_INLINE_FILE_BYTES {
                    diagnostics.push(
                        file_too_large(&absolute_text, metadata.len(), MAX_INLINE_FILE_BYTES)
                            .into(),
                    );
                    continue;
                }
                // The read is outside the resolver's stat try/catch: a read
                // failure (EISDIR for a directory) rejects the turn with
                // Node's own message.
                let bytes = fs::read(&absolute).map_err(|error| {
                    failure(crate::node_fs_message_or_unknown(&error, "read", None))
                })?;
                let text = String::from_utf8_lossy(&bytes);
                inline_blocks.push(object([
                    ("type", "text".into()),
                    (
                        "text",
                        JsValue::from(
                            format!("<attachment path=\"{absolute_text}\">\n{text}\n</attachment>")
                                .as_str(),
                        ),
                    ),
                ]));
            }
            AttachmentRoute::Image => {
                // resolver :185-285.
                if metadata.len() > MAX_INLINE_IMAGE_BYTES {
                    diagnostics.push(
                        image_too_large(&absolute_text, metadata.len(), MAX_INLINE_IMAGE_BYTES)
                            .into(),
                    );
                    continue;
                }
                let Some(declared) = image_mime(entry) else {
                    diagnostics.push(
                        format!(
                            "Cannot determine image mime type from {absolute_text}; provide \
                             mimeType explicitly."
                        )
                        .into(),
                    );
                    continue;
                };
                let bytes = fs::read(&absolute).map_err(|error| {
                    failure(crate::node_fs_message_or_unknown(&error, "read", None))
                })?;
                let Some(actual) = detect_image_mime(&bytes) else {
                    diagnostics.push(
                        format!(
                            "Image {absolute_text} could not be identified from its bytes; \
                             skipped and left as a local path diagnostic."
                        )
                        .into(),
                    );
                    continue;
                };
                if !declared.eq_ignore_ascii_case(actual) {
                    diagnostics.push(
                        format!(
                            "Image {absolute_text} was declared as {declared} but bytes look \
                             like {actual}; skipped and left as a local path diagnostic."
                        )
                        .into(),
                    );
                    continue;
                }
                inline_blocks.push(flat_image_block(
                    &base64_encode(&bytes),
                    actual,
                    Some(bytes.len() as f64),
                ));
                // `image_no_resize` (info): enters only for an unregistered
                // attachment.
                if diagnostic_enters(DiagnosticSeverity::Info, entry.channel_key) {
                    diagnostics.push(
                        "PilotDeck does not resize images; original bytes forwarded \
                         (intentional_difference §4.5)."
                            .into(),
                    );
                }
            }
        }
    }

    // TS composer :173-193 — `collectRegisteredAttachmentReadFiles`: the
    // allowed list is the Set of both spellings per registered attachment
    // (#1446). The note pass below and this collection dedup independently,
    // so a duplicate lexical spelling with a different realpath still
    // contributes its canonical entry.
    let mut allowed: HashSet<PathBuf> = HashSet::new();
    let mut allowed_paths: Vec<JsValue> = Vec::new();
    for attachment in &registered {
        for spelling in [&attachment.lexical, &attachment.resolved] {
            if !allowed.insert(spelling.clone()) {
                continue;
            }
            allowed_paths.push(JsValue::from(
                spelling.to_string_lossy().into_owned().as_str(),
            ));
        }
    }
    // TS composer :98-111/:142-146 — the note lines and the hasInspectable
    // accumulation both cover every attachment whose `resolve(path)`
    // spelling is a member of allowedReadFiles (`safeAllowedAttachmentPath`,
    // :103/:144): registered channel attachments, and an unregistered entry
    // colliding with a registered spelling (ts-probe
    // `_tmp/20261026-issue-2495/probe/probe.out` D/E). #2495 —
    // `hasInspectableAttachment` is an `attachments.some(...)` over the whole
    // list, never gated by the note dedup: a second entry with the same
    // lexical spelling but a different inspectable still counts, while the
    // note line itself is deduped on the lexical spelling to the first
    // entry (`seen` on `normalized`, :105 — #1446).
    //
    // #2495 fix-r1 (boundary review) — an honest residual of the #1461 r3
    // lazy cwd cache extends here: a *relative* entry path read while the
    // cwd is cold-unreadable resolves against an empty cwd, so membership
    // never holds and the entry silently loses its note line and its
    // inspectable contribution, where TS `path.resolve` throws `uv_cwd`
    // (ENOENT) on that same cold read — Node v22.22.0 measured,
    // `_tmp/20261026-issue-2495/fix-r1/probe/cwd-deleted*.out`; a warm
    // process keeps returning its cached spelling on both sides). See
    // `cached_process_cwd` for the tracked followup (a crate-level cache
    // seeded at gateway start, A7.4-TOOLBUDGET-CWD-001 area).
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut note_lines: Vec<String> = Vec::new();
    let mut has_inspectable = false;
    // One cwd read for the whole pass — the cache is immutable after its
    // seed (#1461 r2/r3), so a per-entry lock+clone would buy nothing.
    for entry in &entries {
        if entry.path.is_empty() {
            continue;
        }
        let lexical = attachment_lexical(&process_cwd, entry);
        if !allowed.contains(&lexical) {
            continue;
        }
        has_inspectable |= is_read_file_inspectable(entry);
        if !seen.insert(lexical.clone()) {
            continue;
        }
        let normalized = lexical.to_string_lossy().into_owned();
        // composer :108 — `normalized.split(/[\\/]/).pop() || "attachment"`.
        let fallback_name = normalized
            .split(['/', '\\'])
            .next_back()
            .filter(|segment| !segment.is_empty())
            .unwrap_or("attachment");
        let name = clean_attachment_name(&entry.name, fallback_name);
        note_lines.push(format!("- {name}: {normalized}"));
    }

    // TS composer :114-122 — the guidance takes a diagnostics form when any
    // diagnostic was collected, any attachment is audio, or any attachment
    // is not read_file-inspectable; otherwise the plain form.
    let guidance = if diagnostics.is_empty()
        && !entries.iter().any(is_audio_attachment)
        && entries.iter().all(is_read_file_inspectable)
    {
        PLAIN_PATH_NOTE_GUIDANCE.to_string()
    } else {
        attachment_diagnostics_guidance(&entries, has_inspectable, &project_canon)
    };

    // TS composer :67-72 — no content block, no inline block, no diagnostic,
    // and no note leaves a plain text turn.
    if direct_blocks.is_empty()
        && inline_blocks.is_empty()
        && diagnostics.is_empty()
        && note_lines.is_empty()
    {
        return Ok(object([("type", "text".into()), ("text", message.clone())]));
    }
    let mut blocks: Vec<JsValue> = Vec::new();
    if let JsValue::String(text) = message
        && !text.is_empty()
    {
        blocks.push(object([("type", "text".into()), ("text", message.clone())]));
    }
    blocks.extend(direct_blocks);
    blocks.extend(inline_blocks);
    if !diagnostics.is_empty() {
        // TS composer :270-274 — assembled over raw UTF-16 (#1461): a line
        // keeps a lone surrogate the way the JS string carries it, not as a
        // replacement character.
        let mut units: Vec<u16> = "[Attachment diagnostics]\n".encode_utf16().collect();
        for (index, line) in diagnostics.iter().enumerate() {
            if index > 0 {
                units.push(u16::from(b'\n'));
            }
            units.extend("- ".encode_utf16());
            units.extend_from_slice(line.units());
        }
        blocks.push(object([
            ("type", "text".into()),
            ("text", JsValue::String(JsString::from_utf16(units))),
        ]));
    }
    if !note_lines.is_empty() {
        // TS composer :119-122 — `\n\n${MARKER}\n${lines}\n${guidance}`.
        let note = format!(
            "\n\n{ATTACHMENT_PATH_NOTE_MARKER}\n{lines}\n{guidance}",
            lines = note_lines.join("\n")
        );
        blocks.push(object([
            ("type", "text".into()),
            ("text", JsValue::from(note.as_str())),
        ]));
    }
    let mut result = object([
        ("type", "blocks".into()),
        ("content", JsValue::Array(blocks)),
    ]);
    if !allowed_paths.is_empty()
        && let JsValue::Object(ref mut o) = result
    {
        o.insert("allowedReadFiles".into(), JsValue::Array(allowed_paths));
    }
    Ok(result)
}

fn failure(message: impl Into<String>) -> SubmitError {
    SubmitError::new("gateway_submit_failed", message)
}
fn unavailable(message: &str) -> SubmitError {
    SubmitError::new("CAPABILITY_UNAVAILABLE", message)
}
/// `setSessionCwd` (createLocalGateway.ts:1481): the requested cwd is stored
/// as the session's override, unvalidated, and only takes effect when the
/// session is next composed.
struct Cwd(Arc<super::SessionOverrides>);
impl SubmitCwdService for Cwd {
    fn set(&self, session: &JsString, cwd: &JsString) -> Result<(), SubmitError> {
        self.0.set_session_cwd(session, cwd);
        Ok(())
    }
}
impl SubmitHostServices for Services {
    fn default_permission_mode(&self) -> Option<JsString> {
        // `resolveLocalGatewayBootConfig`: `input.permissionMode ?? "default"`
        // (LocalGatewayBootConfig.ts:53); `pilotdeck server` passes none.
        Some("default".into())
    }
    fn live_permission_mode(&self, session: &JsString) -> Option<JsString> {
        self.session_overrides.live_mode(session)
    }
    fn live_base_permission_mode(&self, session: &JsString) -> Option<JsString> {
        self.session_overrides.live_base_mode(session)
    }
    fn workspace_id(&self, input: &JsValue) -> Result<Option<JsString>, SubmitError> {
        // #2562: the filter skips `undefined` *and* `null`, the same nullish
        // read the request admission gate got in #2272 (requests.rs). The
        // submit path's TS source is `workspaceIdResolver(input.projectKey)`
        // (InProcessGateway.ts:954) and the local resolver is
        // `resolveProjectStorageId(requestedProjectKey ?? fallbackProjectRoot,
        // pilotHome)` (createLocalGateway.ts:1026-1028), so an explicit null is
        // "not provided" and takes the configured-project fallback instead of
        // the type error. Node v22.22.0 probes:
        // `_tmp/20261026-issue-2562/ts-probe/probe-{resolver,submit}.out`.
        // A non-string keeps the type error (the TS resolver chain throws on
        // it too — `resolve(7)` is ERR_INVALID_ARG_TYPE).
        let requested = match input.get("projectKey") {
            None | Some(JsValue::Undefined | JsValue::Null) => None,
            Some(project) => Some(
                project
                    .as_string()
                    .filter(|key| key.to_utf8().is_ok())
                    .ok_or_else(|| failure("projectKey must be a valid path string."))?,
            ),
        };
        // The composition does not expose the SDK setter yet. Refuse it
        // before session admission instead of reporting a false selection.
        if input.get("sdkConfig").is_some_and(|v| !v.is_undefined()) {
            return Err(unavailable(
                "Per-turn SDK overrides are not installed in this isolated process.",
            ));
        }
        // When snapshots are enabled (--storage-root was supplied), resolve the
        // id exactly as the management snapshot route does
        // (`PersistenceResources::snapshot_gateway`): TS `resolveProjectStorageId`
        // prefers a registered `.cwd` marker, else the legacy path slug. One
        // resolver keeps turn-captured snapshots visible to `snapshot_list`.
        if self.snapshot_recorder.is_some() {
            let projects = SnapshotProjects::new(self.options.home.clone(), PathBuf::new());
            let project = self.options.project_root_for(requested);
            Ok(Some(JsString::from(
                projects.workspace_id(&project.to_string_lossy()).as_str(),
            )))
        } else {
            Ok(None)
        }
    }
    fn snapshots(&self) -> Option<Arc<dyn SubmitSnapshots>> {
        self.snapshot_recorder.as_ref().map(|recorder| {
            Arc::new(NativeSubmitSnapshots(recorder.clone())) as Arc<dyn SubmitSnapshots>
        })
    }
    fn snapshots_required(&self) -> bool {
        false
    }
    fn cwd(&self) -> Option<Arc<dyn SubmitCwdService>> {
        Some(Arc::new(Cwd(self.session_overrides.clone())))
    }
    fn refresh(&self) -> SubmitFuture<'_, Result<(), SubmitError>> {
        Box::pin(async {
            // `refreshConfigBeforeTurn`: `configStore.reload("turn-start")`,
            // its failure swallowed (InProcessGateway.ts:1336-1342).
            if let Some(config) = &self.config_store {
                let _ = config.store.reload("turn-start").await;
            }
            let current = crate::runtime::project::NativeConfigSource::new(
                self.options.project.clone(),
                self.env.clone(),
                Some(self.options.homedir()),
                None,
            );
            use crate::runtime::project::ConfigSource;
            let fresh = current
                .load(&self.options.project)
                .map_err(|e| failure(e.to_string()))?;
            let lease = self
                .project
                .acquire(None)
                .map_err(|e| failure(e.to_string()))?;
            let staged = lease
                .runtime()
                .resources()
                .get::<ProjectResources>()
                .ok_or_else(|| failure("Native project resources are missing."))?
                .context()
                .snapshot();
            if fresh.get("config") != staged.get("config") {
                self.project
                    .reload()
                    .await
                    .map_err(|e| failure(e.to_string()))?;
                use crate::runtime::refresh::RefreshRouter;
                self.owner.mark_all_dirty("config_changed");
            }
            Ok(())
        })
    }
    fn run_policy(&self) -> Option<JsValue> {
        let lease = self.project.acquire(None).ok()?;
        lease
            .runtime()
            .resources()
            .get::<ProjectResources>()?
            .context()
            .snapshot()
            .get("config")?
            .get("runPolicy")
            .cloned()
    }
    fn resolve_attachments<'a>(
        &'a self,
        input: &'a JsValue,
    ) -> SubmitFuture<'a, Result<SubmitAttachmentLease, SubmitError>> {
        let store = self.upload_store.clone();
        // InProcessGateway.ts:2457: uploaded attachments never fall back to
        // the default project; a falsy `projectKey` fails before any lease.
        let has_project = input
            .get("projectKey")
            .is_some_and(crate::metadata::js_truthy);
        let project = self
            .options
            .project_root_for(input.get("projectKey").and_then(JsValue::as_string));
        Box::pin(async move {
            if !has_project {
                return Err(SubmitError::new(
                    "PROJECT_NOT_FOUND",
                    "projectKey is required for uploaded attachments.",
                ));
            }
            // `uploadedAttachments` is a non-empty array at this call site
            // (GatewaySubmitHost::prepare checks that before calling).
            let refs = match input.get("uploadedAttachments") {
                Some(JsValue::Array(refs)) => refs.clone(),
                _ => {
                    return Err(SubmitError::new(
                        "CAPABILITY_UNAVAILABLE",
                        "uploadedAttachments must be a non-empty array.",
                    ));
                }
            };
            let project_key = project.to_string_lossy().into_owned();
            // Acquire leases sequentially, mirroring TS GatewayUploadedAttachmentBundle.
            // Disk I/O runs on a blocking thread so the async executor is not blocked.
            let mut acquired: Vec<crate::upload::UploadArtifactLease> = Vec::new();
            let result: Result<Vec<crate::upload::UploadArtifactLease>, SubmitError> =
                tokio::task::spawn_blocking(move || {
                    // P1-2: On error mid-loop, release every lease acquired so far
                    // (reverse order, best-effort) before propagating. Without this
                    // the partially-acquired set is dropped without release.
                    let outcome: Result<(), SubmitError> = (|| {
                        for r in &refs {
                            let upload_id = r
                                .get("uploadId")
                                .and_then(JsValue::as_string)
                                .and_then(|s| s.to_utf8().ok())
                                .ok_or_else(|| {
                                    SubmitError::new(
                                        "CAPABILITY_UNAVAILABLE",
                                        "uploadedAttachments[].uploadId must be a string.",
                                    )
                                })?;
                            // attachmentIds: optional string array
                            let attachment_ids: Option<Vec<String>> = match r.get("attachmentIds") {
                                Some(JsValue::Array(ids)) => Some(
                                    ids.iter()
                                        .map(|v| {
                                            v.as_string().and_then(|s| s.to_utf8().ok()).ok_or_else(
                                                || {
                                                    SubmitError::new(
                                                        "CAPABILITY_UNAVAILABLE",
                                                        "attachmentIds must be strings.",
                                                    )
                                                },
                                            )
                                        })
                                        .collect::<Result<Vec<_>, _>>()?,
                                ),
                                _ => None,
                            };
                            let lease = store
                                .acquire_attachment_lease(
                                    &upload_id,
                                    &project_key,
                                    attachment_ids.as_deref(),
                                )
                                .map_err(|e| {
                                    SubmitError::new(
                                        e.code.as_str(),
                                        String::from_utf16_lossy(e.message.units()),
                                    )
                                })?;
                            acquired.push(lease);
                        }
                        Ok(())
                    })();
                    match outcome {
                        Ok(()) => Ok(acquired),
                        Err(e) => {
                            for lease in acquired.iter().rev() {
                                if let Err(rel) = lease.release() {
                                    // #1850: `eprintln!` panics when stderr is
                                    // unwritable, which would abort this
                                    // blocking task mid-release. The whole
                                    // loop is best-effort, so a dropped line
                                    // must not cost the remaining leases.
                                    let _ = writeln!(
                                        std::io::stderr(),
                                        "native upload lease release on error: {rel}"
                                    );
                                }
                            }
                            Err(e)
                        }
                    }
                })
                .await
                .map_err(|e| SubmitError::new("CAPABILITY_UNAVAILABLE", e.to_string()))?;
            let leases = result?;
            // Merge attachments from all leases (TS: flatMap over acquired).
            let attachments: Vec<JsValue> = leases
                .iter()
                .flat_map(|lease| lease.attachments.iter().cloned())
                .collect();
            // Release is idempotent on all acquired leases (reverse order like TS).
            let release: crate::runtime::turn::submit_host::SubmitLeaseRelease =
                Box::new(move || {
                    Box::pin(async move {
                        // Concurrency P2a (gate): `lease.release()` removes the
                        // whole lease directory; like the acquire path it runs
                        // on a blocking thread so the serial host event loop is
                        // never blocked on unlink churn.
                        tokio::task::spawn_blocking(move || {
                            for lease in leases.iter().rev() {
                                // Best-effort: log failures but continue.
                                if let Err(e) = lease.release() {
                                    // #1850: same as the error path above —
                                    // this runs detached from the submit
                                    // call, where an `eprintln!` panic would
                                    // silently kill the remaining releases.
                                    let _ = writeln!(
                                        std::io::stderr(),
                                        "native upload lease release: {e}"
                                    );
                                }
                            }
                        })
                        .await
                        .map_err(|e| SubmitError::new("CAPABILITY_UNAVAILABLE", e.to_string()))
                    })
                });
            Ok(SubmitAttachmentLease {
                attachments,
                release,
            })
        })
    }
    fn prepare_attachment_turn<'a>(
        &'a self,
        message: &'a JsValue,
        attachments: Vec<JsValue>,
        project: Option<&JsString>,
        _session: Option<&JsString>,
    ) -> SubmitFuture<'a, Result<JsValue, SubmitError>> {
        let project_dir = self.options.project_root_for(project);
        Box::pin(async move {
            super::assembly::validate_profile(&self.options, &self.env)
                .await
                .map_err(|error| unavailable(&error.to_string()))?;
            if !matches!(message, JsValue::String(_)) {
                return Err(failure("message must be a string."));
            }
            if attachments.is_empty() {
                return Ok(object([("type", "text".into()), ("text", message.clone())]));
            }
            let message = message.clone();
            // P1-3/concurrency#5: the whole composition — the once-per-prepare
            // lease-root canonicalize (simplify#2), the per-attachment stat and
            // read — runs on a blocking thread so the async executor is not
            // blocked.
            tokio::task::spawn_blocking(move || {
                compose_attachment_turn(&project_dir, &message, &attachments)
            })
            .await
            .map_err(|e| failure(e.to_string()))?
        })
    }
    fn resolve_model_selection<'a>(
        &'a self,
        input: &'a JsValue,
    ) -> SubmitFuture<'a, Result<JsValue, SubmitError>> {
        Box::pin(async move {
            if let Some(turn) =
                super::methods::config::turn_model_selection(input, &self.options.home, &self.env)
                    .map_err(|e| {
                    SubmitError::new(&e.code, String::from_utf16_lossy(e.message.units()))
                })?
            {
                return Ok(turn);
            }
            let metadata = crate::metadata::Metadata {
                home: self.options.home.clone(),
                cwd: std::path::PathBuf::new(),
                files: crate::metadata::NativeFiles,
            };
            let params = object([
                (
                    // `input.projectKey ?? this.options.fallbackProjectKey`
                    // (GatewaySessionModelBundle.ts:85).
                    "projectKey",
                    match input.get("projectKey") {
                        Some(JsValue::String(key)) => JsValue::String(key.clone()),
                        _ => JsValue::String(
                            self.options.project.to_string_lossy().into_owned().into(),
                        ),
                    },
                ),
                (
                    "sessionKey",
                    input
                        .get("sessionKey")
                        .cloned()
                        .unwrap_or(JsValue::Undefined),
                ),
            ]);
            let state =
                crate::session_model::execute(&metadata, &params, &self.env).map_err(|e| {
                    SubmitError::new(&e.code, String::from_utf16_lossy(e.message.units()))
                })?;
            let saved = state.get("saved").cloned().unwrap_or(JsValue::Undefined);
            if saved.get("mode") == Some(&"model".into()) {
                Ok(object([("source", "session".into()), ("selection", saved)]))
            } else {
                Ok(object([(
                    "source",
                    state
                        .get("effective")
                        .and_then(|v| v.get("source"))
                        .cloned()
                        .unwrap_or("default".into()),
                )]))
            }
        })
    }
    fn record_failure<'a>(
        &'a self,
        run: &'a RunRef,
        status: &'a JsValue,
    ) -> SubmitFuture<'a, Result<(), SubmitError>> {
        let recorder = self
            .recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&run.session())
            .and_then(std::sync::Weak::upgrade);
        let run = run.clone();
        let status = status.clone();
        Box::pin(async move {
            let recorder =
                recorder.ok_or_else(|| failure("Native session recorder is unavailable."))?;
            tokio::task::spawn_blocking(move || {
                recorder
                    .writer()
                    .record_agent_status_message(&run.session(), &run.run_id, &status)
            })
            .await
            .map_err(|e| failure(e.to_string()))?
            .map_err(|e| failure(e.to_string()))
        })
    }
    fn after_turn(&self, _: &RunRef) -> Result<(), SubmitError> {
        self.recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, recorder| recorder.strong_count() > 0);
        Ok(())
    }
    fn now_ms(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0
    }
    fn abort_turn_timeout_ms(&self) -> u64 {
        self.options.drain_budget.as_millis() as u64
    }
}

// ------------------------------------------------ U7 native settings (#1320)
use crate::config::store::{ConfigStore, StoreOptions};
use crate::runtime::refresh::{
    ConsoleLog, NoMemoryMaintenance, RefreshBundle, RefreshLog, RefreshOptions, RefreshRegistry,
    RefreshRouter, RegistryRefresh,
};
use crate::runtime::session::SessionOwner;
use crate::runtime::settings::{ResolvedSettingsPort, SettingsGateway, SettingsGatewayOptions};

/// The native U7 settings composition inputs. `router` and `registry` are the
/// bundle's collaborators as ports: production binds the session owner and
/// `RegistryRefresh` over the project runtime registry, tests substitute
/// recording collaborators to observe the dirty/broadcast order.
pub struct NativeSettingsOptions {
    /// The gateway's admission/close port: `hasActiveTurn` drives the
    /// SESSION_BUSY gate and `close` is the post-success session close.
    pub owner: SessionOwner,
    /// `getRouter`: the bundle's dirty router.
    pub router: Arc<dyn RefreshRouter>,
    /// The bundle's runtime registry.
    pub registry: Arc<dyn RefreshRegistry>,
    /// `createPilotConfigStoreSync({ projectRoot, env })`: the caller loads
    /// the store (a load failure is a boot failure, like the TS
    /// constructor's throw).
    pub store: ConfigStore,
    /// `projectRoot` for the bundle's log lines and diagnostics.
    pub project_root: JsString,
    /// The bundle's `log` (createLocalGateway.ts:806: `getConsoleLog()`).
    /// Production passes [`ConsoleLog`]; the #1320 replay passes a recorder
    /// because the golden pins the bundle's exact log lines.
    pub log: Arc<dyn RefreshLog>,
    /// `options.applyFlagSettings` (`registry.applyFlagSettingsForSdk`).
    pub apply_flag_settings: Option<Arc<dyn crate::runtime::settings::ApplyFlagSettings>>,
    /// `options.updateSettings` (`updatePilotLocalSettings` + store reload).
    pub update_settings: Option<crate::runtime::settings::UpdateSettings>,
    /// `options.reloadOutputStyles` (`registry.reloadOutputStylesForSdk`).
    pub reload_output_styles: Option<Arc<dyn crate::runtime::settings::ReloadOutputStyles>>,
}

/// The composed U7 settings stack: the #1318 [`SettingsGateway`] over the
/// real refresh bundle and config store.
pub struct NativeSettings {
    pub gateway: Arc<SettingsGateway>,
    // The delegates inside the gateway keep the bundle alive; the process
    // assembly reads the handle once, to `bind_server` the gateway server's
    // connection set (`ConnectionBroadcast`, the baseline's
    // `bindServer(server)`). This composition itself binds nothing (#1320
    // fix-r1 F2): tests that observe the dirty/broadcast order `bind_server`
    // a recording server themselves and assert `has_notification_server()`
    // (`native_settings_production_binds_no_notification_server`).
    pub bundle: Arc<RefreshBundle>,
    /// `configStore`: the turn-start reload and the file watcher read it.
    pub store: ConfigStore,
}

/// Compose the native U7 settings stack (createLocalGateway.ts:795-831 and
/// 1383-1410): the real refresh bundle over the supplied registry, router
/// and store (attached exactly like `runtimeRefresh.attach()`), and the
/// settings gateway with the delegates the isolated native process owns —
/// `resolve_settings` over the store snapshot and the two reloads over the
/// bundle. `apply_flag_settings` / `update_settings` /
/// `reload_output_styles` stay unconfigured (their delegates are the SDK
/// session registry and the production local-settings writer, neither
/// assembled into the isolated native process) and answer the TS
/// unconfigured shapes through the component's own option handling.
///
/// #1320 fix-r1 F2: this composition binds no notification server itself.
/// The process assembly binds the gateway server's connection set right
/// after it (`ConnectionBroadcast`, the baseline's `bindServer(server)`);
/// tests that observe the dirty/broadcast order `bind_server` a recording
/// server themselves and assert `has_notification_server()`.
pub fn native_settings(options: NativeSettingsOptions) -> NativeSettings {
    let NativeSettingsOptions {
        owner,
        router,
        registry,
        store,
        project_root,
        log,
        apply_flag_settings,
        update_settings,
        reload_output_styles,
    } = options;
    let bundle = Arc::new(RefreshBundle::new(RefreshOptions {
        config_store: Arc::new(store.clone()),
        registry,
        memory_maintenance: Arc::new(NoMemoryMaintenance),
        router: Arc::new(move || Some(router.clone())),
        project_root,
        memory_diagnostics: None,
        sdk: None,
        log,
    }));
    // `runtimeRefresh.attach()` (createLocalGateway.ts:831): without the
    // subscription, an explicit `reload_config` still answers, but its
    // config change would never refresh runtimes or mark sessions dirty.
    bundle.attach();
    let gateway = Arc::new(SettingsGateway::new(SettingsGatewayOptions {
        owner,
        apply_flag_settings,
        update_settings,
        resolve_settings: Some(Arc::new(ResolvedSettingsPort {
            store: store.clone(),
        })),
        reload_output_styles,
        reload_config: Some(bundle.clone()),
        reload_extensions: Some(bundle.clone()),
    }));
    NativeSettings {
        gateway,
        bundle,
        store,
    }
}

/// `registry.applyFlagSettingsForSdk` (ProjectRuntimeRegistry.ts:914-953)
/// over the process's session selection store: `effortLevel` is the SDK
/// thinking override, `permissions.defaultMode` the stored permission mode
/// the next session composes from. The gateway validated the shape first.
pub(super) struct NativeFlagSettings {
    pub(super) overrides: Arc<super::SessionOverrides>,
}
impl crate::runtime::settings::ApplyFlagSettings for NativeFlagSettings {
    fn apply(
        &self,
        input: &JsValue,
    ) -> Result<crate::runtime::settings::FlagSettings, crate::runtime::settings::SettingsError>
    {
        let mut result = crate::runtime::settings::FlagSettings::default();
        let Some(session) = input.get("sessionKey").and_then(JsValue::as_string) else {
            return Ok(result);
        };
        let Some(JsValue::Object(settings)) = input.get("settings") else {
            return Ok(result);
        };
        if let Some(effort) = settings.get("effortLevel") {
            if matches!(effort, JsValue::Null) {
                self.overrides.clear_thinking(session);
                result.cleared.push("effortLevel".into());
            } else {
                self.overrides.set_thinking(
                    session,
                    object([("enabled", JsValue::Bool(true)), ("mode", effort.clone())]),
                );
                result.applied.push("effortLevel".into());
            }
        }
        if let Some(permissions) = settings.get("permissions") {
            let default_mode = match permissions {
                JsValue::Null => JsValue::Null,
                value => value
                    .get("defaultMode")
                    .cloned()
                    .unwrap_or(JsValue::Undefined),
            };
            match &default_mode {
                JsValue::Null | JsValue::Undefined => {
                    self.overrides.clear_permission_mode(session);
                    result.cleared.push("permissions.defaultMode".into());
                }
                JsValue::String(mode) => {
                    self.overrides.set_permission_mode(session, mode.clone());
                    result.applied.push("permissions.defaultMode".into());
                }
                // validateSdkFlagSettings admits only the three mode strings.
                _ => {}
            }
        }
        Ok(result)
    }
}

/// `registry.reloadOutputStylesForSdk(input)`
/// (ProjectRuntimeRegistry.ts:1111-1121; `input = {}` when the frame has no
/// params, InProcessGateway.ts:2387): resolve the runtime, reload its
/// output-style registry, then close every session holding an SDK session
/// config in that project (all of them without a `projectKey`) with
/// `sdk_output_styles_reloaded`.
pub struct NativeOutputStylesReload {
    pub options: Options,
    pub project: Arc<ProjectRuntimeRegistry>,
    pub owner: SessionOwner,
    pub interactions: Arc<crate::runtime::interaction::GatewayInteractionCoordinator>,
    pub overrides: Arc<super::SessionOverrides>,
}
impl crate::runtime::settings::ReloadOutputStyles for NativeOutputStylesReload {
    fn reload(
        &self,
        params: &JsValue,
    ) -> crate::runtime::settings::SettingsFuture<
        Result<
            crate::runtime::settings::OutputStylesReload,
            crate::runtime::settings::SettingsError,
        >,
    > {
        use crate::runtime::settings::{OutputStylesReload, SettingsError};
        let params = if params.is_undefined() {
            JsValue::Object(JsObject::new())
        } else {
            params.clone()
        };
        let options = self.options.clone();
        let project = self.project.clone();
        let owner = self.owner.clone();
        let interactions = self.interactions.clone();
        let overrides = self.overrides.clone();
        Box::pin(async move {
            let error = |e: crate::runtime::connection::RequestError| {
                SettingsError::coded(&e.code, e.message)
            };
            let plain = |message: String| SettingsError::plain("Error", message);
            let project_root =
                super::projects::route_project_with(&options, &project, &params).map_err(error)?;
            let acquire_root = project_root.clone();
            let lease = tokio::task::spawn_blocking(move || project.acquire(Some(&acquire_root)))
                .await
                .map_err(|e| plain(e.to_string()))?
                .map_err(|e| plain(e.to_string()))?;
            let resources = lease
                .runtime()
                .resources()
                .get::<ProjectResources>()
                .ok_or_else(|| plain("Native project resources are missing.".into()))?;
            let plugin_resource = resources
                .context()
                .resource(crate::runtime::project::Resource::PluginRuntime)
                .ok_or_else(|| plain("Native plugin runtime resource is missing.".into()))?;
            let plugin_runtime = plugin_resource
                .as_any()
                .downcast_ref::<crate::runtime::project::plugins::PluginResources>()
                .ok_or_else(|| plain("Native plugin runtime has the wrong type.".into()))?
                .runtime
                .clone();
            let changed = plugin_runtime.reload_output_styles().await;
            drop(lease);
            let scoped =
                matches!(params.get("projectKey"), Some(JsValue::String(key)) if !key.is_empty());
            let reason: JsString = "sdk_output_styles_reloaded".into();
            for (session, root) in overrides.sdk_config_sessions() {
                if scoped && root != project_root {
                    continue;
                }
                // `this.gateway?.closeSession(...)` (InProcessGateway.ts:2049-2054).
                owner
                    .close(&session)
                    .await
                    .map_err(|e| plain(e.to_string()))?;
                interactions.close_session(&session, &reason);
                overrides.clear_live_mode(&session);
            }
            Ok(OutputStylesReload {
                changed: changed.into_iter().map(JsString::from).collect(),
            })
        })
    }
}

/// The production composition for the native process
/// (createLocalGateway.ts:795-835): [`native_settings`] with the
/// collaborators the isolated process owns — `createPilotConfigStoreSync`
/// over the served home/project (a load failure is the TS boot failure),
/// `RegistryRefresh` over the project runtime registry and the session
/// owner as the dirty router. The store anchors exactly like the process's
/// other config reads (`NativeConfigSource`: PILOT_HOME from the
/// environment, the served project as cwd and `projectRoot`).
///
/// #1320 fix-r1 F2: this composition binds no notification server itself.
/// The process assembly binds the gateway server's connection set right
/// after it (`ConnectionBroadcast`, the baseline's `bindServer(server)`);
/// tests that observe the dirty/broadcast order `bind_server` a recording
/// server themselves and assert `has_notification_server()`.
pub fn native_settings_for_process(
    owner: SessionOwner,
    project: Arc<crate::runtime::project::ProjectRuntimeRegistry>,
    home: &std::path::Path,
    served_project: &std::path::Path,
    env: &crate::config::Env,
    overrides: Arc<super::SessionOverrides>,
    output_styles: Option<Arc<dyn crate::runtime::settings::ReloadOutputStyles>>,
) -> std::io::Result<NativeSettings> {
    // #1320 fix-r1 F3: same anchor as every other config read of the process
    // (`NativeConfigSource`): PILOT_HOME from the environment resolved
    // against the served project, `--home` only as the fallback homedir for
    // `~` / `~/` prefixes. In production the env arrives from
    // `Options::environment`, which has already overwritten PILOT_HOME with
    // `--home`, so the resolved anchor is always `--home` there; the explicit
    // `Some(home)` keeps direct callers (tests and future non-CLI
    // assemblies) anchored exactly like the CLI path.
    let pilot_home = crate::runtime::project::resolve_pilot_home(env, served_project, Some(home));
    let writer = crate::runtime::settings::PilotLocalSettingsWriter {
        pilot_home: pilot_home.clone(),
        project_root: served_project.to_path_buf(),
        env: env.clone(),
    };
    let mut store_options = StoreOptions::new(pilot_home, served_project, env.clone());
    store_options.project = Some(served_project.to_string_lossy().into_owned());
    let store = ConfigStore::new(store_options).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "pilot config store failed to load: {} {}",
                error.code,
                String::from_utf16_lossy(error.message.units())
            ),
        )
    })?;
    Ok(native_settings(NativeSettingsOptions {
        owner: owner.clone(),
        router: Arc::new(owner),
        registry: Arc::new(RegistryRefresh::new(project)),
        store: store.clone(),
        project_root: JsString::from(served_project.to_string_lossy().into_owned()),
        log: Arc::new(ConsoleLog),
        apply_flag_settings: Some(Arc::new(NativeFlagSettings { overrides })),
        // createLocalGateway.ts:1384-1401 with no organization policy.
        update_settings: Some(crate::runtime::settings::UpdateSettings {
            organization_denied: false,
            writer: Arc::new(writer),
            store: store.clone(),
        }),
        reload_output_styles: output_styles,
    }))
}
