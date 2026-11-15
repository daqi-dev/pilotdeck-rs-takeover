//! Synchronous product-host services used at the original driver call points.
//! None of these ports grants network authority or changes timer scheduling.
use crate::driver_transport::HostOperationError;
use crate::headers::ModelHeaderService;
use pilotdeck_model_core::codec::normalize_error::JsError;
use pilotdeck_protocol::{JsObject, JsString, JsValue};

/// The product owns audit storage; the driver retains record construction and
/// ordering. A stage failure prevents dispatch, while append failure is logged.
/// This is the trusted owner's fact-recording/cleanup authority: recording a
/// denied attempt remains permitted after execution authority is revoked. The
/// sink must not reinterpret an execution denial as an ordinary I/O error.
pub trait InvocationAuditSink: Send + Sync {
    fn stage(&self, record: &JsObject) -> std::io::Result<()>;
    fn append(&self, record: &JsObject) -> std::io::Result<()>;
}

/// Product policy and privileged local operations, without a Gateway dependency.
/// Callbacks are synchronous because the original operations were synchronous.
pub trait ModelHostServices: ModelHeaderService {
    /// The caller invokes this only for streaming, at the original dump point.
    /// The host checks whether dumping is enabled and retains secure creation,
    /// TMPDIR, Node errno projection and logging behavior.
    fn dump_request_if_enabled(
        &self,
        body: &JsValue,
        model: &JsValue,
    ) -> Result<(), HostOperationError<JsError, Self::Rejection>>;

    /// Preserve the product's admitted Node Date.parse/TZ behavior.
    fn parse_retry_after_date(&self, value: &JsString) -> Result<Option<f64>, JsString>;

    /// The driver retains the existing floor and i64 conversion before this call.
    fn format_epoch_iso(&self, epoch_ms: i64) -> String;

    /// Read at each original StreamGuard construction, not once per session.
    fn streaming_duration_limit(&self) -> Option<String>;

    /// Preserve existing diagnostic text without giving the driver an output sink.
    fn diagnostic(&self, message: &str);
}
