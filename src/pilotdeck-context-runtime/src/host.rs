//! Narrow runtime-local error and cancellation vocabulary.
use pilotdeck_protocol::JsString;

#[derive(Debug)]
pub struct HostError {
    pub message: String,
    pub module_error: ModuleError,
}
#[derive(Debug)]
pub struct ModuleError(pub JsString);
impl ModuleError {
    pub fn new(value: impl Into<JsString>) -> Self {
        Self(value.into())
    }
}
impl From<ModuleError> for HostError {
    fn from(value: ModuleError) -> Self {
        Self {
            message: String::from_utf16_lossy(value.0.units()),
            module_error: value,
        }
    }
}
/// Read-only view of a host-owned cancellation signal.
pub trait Cancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn reason(&self) -> Option<JsString>;
}
