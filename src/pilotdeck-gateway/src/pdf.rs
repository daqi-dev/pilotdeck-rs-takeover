//! Bounded native MuPDF component. The helper owns no application state;
//! dropping its future cancels the owned process and releases its input file.
use crate::native_component::{self, Snapshot};
use std::time::Duration;
use tokio::process::Command;

pub(crate) const UNAVAILABLE: &str = "PDF capability unavailable: install the MuPDF component (pilotdeck-pdf) or set PILOTDECK_PDF_TOOL.";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 32 * 1024 * 1024;
pub(crate) const MAX_INPUT: u64 = 128 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum Error {
    Unavailable,
    Failed(String),
    ParseFailed(String),
    Timeout,
    TooLarge,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => UNAVAILABLE,
            Self::Failed(message) | Self::ParseFailed(message) => message,
            Self::Timeout => "PDF operation exceeded its 30 second time limit.",
            Self::TooLarge => "PDF operation exceeded its output size limit.",
        })
    }
}

impl From<native_component::Error> for Error {
    fn from(error: native_component::Error) -> Self {
        match error {
            native_component::Error::Unavailable => Self::Unavailable,
            native_component::Error::Failed(message)
            | native_component::Error::Exited { message, .. } => Self::Failed(message),
            native_component::Error::Timeout => Self::Timeout,
            native_component::Error::TooLarge => Self::TooLarge,
            native_component::Error::Cancelled => {
                Self::Failed("PDF operation was cancelled.".into())
            }
        }
    }
}

pub(crate) struct Document(Snapshot);
impl Document {
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        Snapshot::new("pdf", bytes, MAX_INPUT as usize)
            .map(Self)
            .map_err(Error::from)
    }
    pub(crate) async fn pages(&self) -> Result<u32, Error> {
        let bytes = self.run("count", None, 1024).await?;
        std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .ok_or_else(|| Error::Failed("MuPDF returned an invalid page count.".into()))
    }
    pub(crate) async fn text(&self) -> Result<String, Error> {
        String::from_utf8(self.run("text", None, MAX_OUTPUT).await?)
            .map_err(|_| Error::Failed("MuPDF returned invalid UTF-8 text.".into()))
    }
    pub(crate) async fn render(&self, page: u32, budget: usize) -> Result<Vec<u8>, Error> {
        let bytes = self.run("render", Some(page), MAX_OUTPUT).await?;
        crate::image::compress(&bytes, "image/jpeg", budget)
            .await
            .map(|image| image.bytes)
            .map_err(Error::Failed)
    }
    async fn run(&self, mode: &str, page: Option<u32>, limit: usize) -> Result<Vec<u8>, Error> {
        let mut command = Command::new(native_component::tool_path(
            "PILOTDECK_PDF_TOOL",
            "mupdf",
            "pilotdeck-pdf",
        ));
        command.arg(mode).arg(self.0.path());
        if let Some(page) = page {
            command.arg(page.to_string());
        }
        native_component::run(command, Some(self.0.clone()), limit, TIMEOUT)
            .await
            .map_err(|error| component_error(mode, error))
    }
}

fn component_error(mode: &str, error: native_component::Error) -> Error {
    match error {
        native_component::Error::Exited {
            code: Some(1),
            message,
        } if mode == "count" => Error::ParseFailed(message),
        error => Error::from(error),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn only_count_exit_one_is_a_parse_failure() {
        for (mode, code, parse) in [
            ("count", Some(1), true),
            ("render", Some(1), false),
            ("count", Some(2), false),
            ("count", Some(127), false),
            ("count", None, false),
        ] {
            let error = component_error(
                mode,
                native_component::Error::Exited {
                    code,
                    message: "original error".into(),
                },
            );
            assert_eq!(matches!(error, Error::ParseFailed(_)), parse);
            assert_eq!(error.to_string(), "original error");
        }
        assert!(matches!(
            component_error("count", native_component::Error::Cancelled),
            Error::Failed(_)
        ));
        assert!(matches!(
            component_error(
                "count",
                native_component::Error::Failed("permission denied".into())
            ),
            Error::Failed(_)
        ));
    }
    #[test]
    fn document_snapshot_is_private_and_released() {
        let doc = Document::from_bytes(b"%PDF-fixture").unwrap();
        let path = doc.0.path().to_path_buf();
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-fixture");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(doc);
        assert!(!path.exists());
    }
}
