//! Per-request context assembly state, separate from routing and turn policy.
use crate::ProjectedMessage;
use crate::ports::{
    CallControl, Captured, ContextPrepare, HostPort, ModelMetadata, PortResult, Route,
    ToolDescriptor,
};
use crate::turn::{TurnInput, TurnRuntimeConfig};
use pilotdeck_protocol::JsString;
use pilotdeck_protocol::model::BoundarySnapshot;
#[derive(Debug, Clone)]
pub struct RequestPreparationState {
    pub tools: Vec<ToolDescriptor>,
    pub metadata: Vec<Captured<ModelMetadata>>,
    pub plan_todo_addendum: Option<JsString>,
    pub elicitation_available: bool,
    pub token_caps: Vec<RouteTokenCaps>,
}
#[derive(Debug, Clone)]
pub struct PreparedRequestIntent {
    pub route: Route,
    pub request: BoundarySnapshot,
    pub diagnostics: Vec<BoundarySnapshot>,
    pub has_system_prompt: bool,
}
/// The original announces loaded instructions after context acknowledgement,
/// before media/cache finalization can fail. The composition owner supplies
/// this notice port; request assembly neither dispatches hooks nor publishes IO.
pub trait RequestPreparationObserver {
    fn instructions_loaded(&mut self, has_system_prompt: bool) -> PortResult<()>;
    /// `readFile(block.path, "utf8")` for a top-level `media_reference`
    /// (materializeMediaReferences.ts:50-51): any path the process can read,
    /// a relative one against the process working directory, decoded as
    /// UTF-8 with replacement characters. `Err` is the thrown error's message.
    fn read_media_reference(
        &mut self,
        path: &pilotdeck_protocol::JsValue,
    ) -> Result<JsString, JsString> {
        read_media_file(path)
    }
    /// One materialization diagnostic line, which the loop writes with
    /// `console.warn` (AgentLoop.ts:2313-2318).
    fn media_reference_diagnostic(&mut self, _line: &JsString) {}
}
pub struct RequestAssemblyInput<'a> {
    pub control: &'a CallControl,
    pub config: &'a TurnRuntimeConfig,
    pub input: &'a TurnInput,
    pub messages: &'a [ProjectedMessage],
}
pub trait RequestAssembler {
    fn assemble(
        &self,
        state: &mut RequestPreparationState,
        host: &mut dyn HostPort,
        parameters: RequestAssemblyInput<'_>,
        observer: &mut dyn RequestPreparationObserver,
    ) -> PortResult<PreparedRequestIntent>;
}
pub struct OriginalRequestAssembler;

/// Original per-call request options. Preview and notification suppression are
/// independent: compaction previews suppress the notice without mutating host
/// lifecycle state. The ordinary request retains the default notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestAssemblyOptions {
    pub preview_only: Option<bool>,
    pub emit_instruction_events: bool,
}
impl Default for RequestAssemblyOptions {
    fn default() -> Self {
        Self {
            preview_only: None,
            emit_instruction_events: true,
        }
    }
}
impl OriginalRequestAssembler {
    /// AgentLoop-owned limits, distinct from raw request fallback fields.
    pub fn compaction_limits(
        &self,
        state: &RequestPreparationState,
        config: &TurnRuntimeConfig,
        route: &Route,
        pre_route: bool,
    ) -> (f64, f64) {
        crate::request_assembly::compaction_limits(state, config, route, pre_route)
    }
    /// Pure source prompt intent; budget evaluators need this alongside their
    /// preview request to rematerialize changed candidate message windows.
    pub fn context_preparation(
        &self,
        state: &RequestPreparationState,
        parameters: RequestAssemblyInput<'_>,
    ) -> PortResult<ContextPrepare> {
        crate::request_assembly::context_preparation(state, parameters, None)
    }
    pub fn assemble_with_options(
        &self,
        state: &mut RequestPreparationState,
        host: &mut dyn HostPort,
        parameters: RequestAssemblyInput<'_>,
        options: RequestAssemblyOptions,
        observer: &mut dyn RequestPreparationObserver,
    ) -> PortResult<PreparedRequestIntent> {
        crate::request_assembly::assemble_with_options(state, host, parameters, options, observer)
    }
}

/// Frozen read projection of session-owned recovery caps. Assembly reads it;
/// only the owning turn recovery machine may replace cap state.
#[derive(Debug, Clone)]
pub struct RouteTokenCaps {
    pub route: Route,
    pub max_context_tokens: Option<f64>,
    pub attempt_max_output_tokens: Option<f64>,
    pub requested_max_output_tokens: Option<f64>,
    pub session_max_output_tokens: Option<f64>,
    pub hard_max_output_tokens: Option<f64>,
}

impl RequestAssembler for OriginalRequestAssembler {
    fn assemble(
        &self,
        state: &mut RequestPreparationState,
        host: &mut dyn HostPort,
        parameters: RequestAssemblyInput<'_>,
        observer: &mut dyn RequestPreparationObserver,
    ) -> PortResult<PreparedRequestIntent> {
        request_assembly_bridge(state, host, parameters, observer)
    }
}
fn request_assembly_bridge(
    state: &mut RequestPreparationState,
    host: &mut dyn HostPort,
    parameters: RequestAssemblyInput<'_>,
    observer: &mut dyn RequestPreparationObserver,
) -> PortResult<PreparedRequestIntent> {
    crate::request_assembly::assemble(
        state,
        host,
        parameters.control,
        parameters.config,
        parameters.input,
        parameters.messages,
        observer,
    )
}
/// `fs.readFile(path, "utf8")` with Node's error messages for the usual
/// failures (`<CODE>: <libuv description>, <syscall> '<path>'`). A directory
/// opens and fails on read, which names no path.
pub fn read_media_file(path: &pilotdeck_protocol::JsValue) -> Result<JsString, JsString> {
    use std::io::{ErrorKind, Read};
    let path = match path {
        pilotdeck_protocol::JsValue::String(path) => path,
        pilotdeck_protocol::JsValue::Undefined => {
            return Err("The \"path\" argument must be of type string or an instance of Buffer or URL. Received undefined".into());
        }
        pilotdeck_protocol::JsValue::Null => {
            return Err("The \"path\" argument must be of type string or an instance of Buffer or URL. Received null".into());
        }
        pilotdeck_protocol::JsValue::Number(_) | pilotdeck_protocol::JsValue::Bool(_) => {
            let kind = if matches!(path, pilotdeck_protocol::JsValue::Number(_)) {
                "number"
            } else {
                "boolean"
            };
            let shown = pilotdeck_protocol::js_coerce::to_js_string(path)
                .map(|s| String::from_utf16_lossy(s.units()))
                .unwrap_or_default();
            return Err(format!("The \"path\" argument must be of type string or an instance of Buffer or URL. Received type {kind} ({shown})").into());
        }
        pilotdeck_protocol::JsValue::Array(_) => {
            return Err("The \"path\" argument must be of type string or an instance of Buffer or URL. Received an instance of Array".into());
        }
        pilotdeck_protocol::JsValue::Object(_) => {
            return Err("The \"path\" argument must be of type string or an instance of Buffer or URL. Received an instance of Object".into());
        }
    };
    let text = String::from_utf16_lossy(path.units());
    if text.contains('\0') {
        return Err(format!("The argument 'path' must be a string, Uint8Array, or URL without null bytes. Received {}", js_inspect_string(&text)).into());
    }
    let describe = |error: std::io::Error, syscall: &str, with_path: bool| -> JsString {
        // ELOOP / ENAMETOOLONG: 62 / 63 on macOS, 40 / 36 on Linux.
        let (eloop, enametoolong) = if cfg!(target_os = "macos") {
            (62, 63)
        } else {
            (40, 36)
        };
        let (code, description) = match (error.raw_os_error(), error.kind()) {
            (Some(raw), _) if raw == eloop => ("ELOOP", "too many symbolic links encountered"),
            (Some(raw), _) if raw == enametoolong => ("ENAMETOOLONG", "name too long"),
            (_, ErrorKind::NotFound) => ("ENOENT", "no such file or directory"),
            (_, ErrorKind::PermissionDenied) => ("EACCES", "permission denied"),
            (_, ErrorKind::IsADirectory) => ("EISDIR", "illegal operation on a directory"),
            (_, ErrorKind::NotADirectory) => ("ENOTDIR", "not a directory"),
            _ => ("EIO", "i/o error"),
        };
        if with_path {
            format!("{code}: {description}, {syscall} '{text}'").into()
        } else {
            format!("{code}: {description}, {syscall}").into()
        }
    };
    let mut file = std::fs::File::open(&text).map_err(|error| describe(error, "open", true))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| describe(error, "read", false))?;
    Ok(String::from_utf8_lossy(&bytes).as_ref().into())
}
/// util.inspect of a string inside an error message: single quotes.
fn js_inspect_string(text: &str) -> String {
    format!(
        "'{}'",
        text.replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\0', "\\x00")
    )
}

#[cfg(test)]
mod media_file_tests {
    use super::read_media_file;
    use pilotdeck_protocol::{JsString, JsValue};

    #[test]
    fn reads_like_node_read_file_utf8() {
        let dir = std::env::temp_dir().join(format!("pd-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"ok\xff").unwrap();
        let path = |p: &std::path::Path| JsValue::String(p.to_str().unwrap().into());
        assert_eq!(
            read_media_file(&path(&file)),
            Ok(JsString::from("ok\u{fffd}"))
        );
        let missing = dir.join("missing.txt");
        assert_eq!(
            read_media_file(&path(&missing)),
            Err(JsString::from(format!(
                "ENOENT: no such file or directory, open '{}'",
                missing.display()
            )))
        );
        assert_eq!(
            read_media_file(&path(&dir)),
            Err(JsString::from(
                "EISDIR: illegal operation on a directory, read"
            ))
        );
        assert_eq!(
            read_media_file(&JsValue::Undefined),
            Err(JsString::from(
                "The \"path\" argument must be of type string or an instance of Buffer or URL. Received undefined"
            ))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
