//! Bounded native MuPDF component. The helper owns no application state;
//! dropping its future cancels the owned process and releases its input file.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

pub(crate) const UNAVAILABLE: &str = "PDF capability unavailable: install the MuPDF component (pilotdeck-pdf) or set PILOTDECK_PDF_TOOL.";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 32 * 1024 * 1024;
pub(crate) const MAX_INPUT: u64 = 128 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum Error {
    Unavailable,
    Failed(String),
    Timeout,
    TooLarge,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => UNAVAILABLE,
            Self::Failed(message) => message,
            Self::Timeout => "PDF operation exceeded its 30 second time limit.",
            Self::TooLarge => "PDF operation exceeded its output size limit.",
        })
    }
}

fn tool_path() -> PathBuf {
    if let Some(path) = std::env::var_os("PILOTDECK_PDF_TOOL").filter(|p| !p.is_empty()) {
        return path.into();
    }
    if let Some(bin) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        for path in [
            bin.join("pilotdeck-pdf"),
            bin.join("components/mupdf/bin/pilotdeck-pdf"),
            bin.join("../components/mupdf/bin/pilotdeck-pdf"),
        ] {
            if path.is_file() {
                return path;
            }
        }
    }
    PathBuf::from("pilotdeck-pdf")
}

pub(crate) struct Document {
    directory: PathBuf,
    path: PathBuf,
}
impl Drop for Document {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
impl Document {
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() as u64 > MAX_INPUT {
            return Err(Error::TooLarge);
        }
        let directory =
            std::env::temp_dir().join(format!("pilotdeck-pdf-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&directory)
            .map_err(|e| Error::Failed(e.to_string()))?;
        let document = Self {
            path: directory.join("input.pdf"),
            directory,
        };
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&document.path)
            .map_err(|e| Error::Failed(e.to_string()))?;
        std::io::Write::write_all(&mut file, bytes).map_err(|e| Error::Failed(e.to_string()))?;
        Ok(document)
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
        self.run("render", Some(page), budget.min(MAX_OUTPUT)).await
    }
    async fn run(&self, mode: &str, page: Option<u32>, limit: usize) -> Result<Vec<u8>, Error> {
        let mut command = Command::new(tool_path());
        command.arg(mode).arg(&self.path);
        if let Some(page) = page {
            command.arg(page.to_string());
        }
        run(command, limit, TIMEOUT).await
    }
}

async fn read_bounded(mut stream: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = stream
            .read(&mut chunk)
            .await
            .map_err(|e| Error::Failed(e.to_string()))?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(count) > limit {
            return Err(Error::TooLarge);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

async fn run(mut command: Command, limit: usize, timeout: Duration) -> Result<Vec<u8>, Error> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        pilotdeck_child_signals::install_child_detach_session(command.as_std_mut());
        pilotdeck_child_signals::install_child_signal_reset(command.as_std_mut());
    }
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Unavailable
        } else {
            Error::Failed(e.to_string())
        }
    })?;
    let mut group = crate::process_group::GroupGuard(
        child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(id as i32)),
    );
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let result = tokio::time::timeout(timeout, async {
        tokio::try_join!(
            read_bounded(stdout, limit),
            read_bounded(stderr, 16_384),
            async { child.wait().await.map_err(|e| Error::Failed(e.to_string())) }
        )
    })
    .await;
    match result {
        Ok(Ok((stdout, stderr, status))) => {
            group.disarm();
            if status.success() {
                Ok(stdout)
            } else {
                Err(Error::Failed(
                    String::from_utf8_lossy(&stderr).trim().to_owned(),
                ))
            }
        }
        failed => {
            group.signal(rustix::process::Signal::KILL);
            let _ = child.kill().await;
            let _ = child.wait().await;
            match failed {
                Ok(Err(error)) => Err(error),
                _ => Err(Error::Timeout),
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn component_failures_are_explicit_and_output_is_bounded() {
        assert!(matches!(
            run(Command::new("/missing/pilotdeck-pdf"), 4, TIMEOUT).await,
            Err(Error::Unavailable)
        ));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 12345"]);
        assert!(matches!(
            run(command, 4, TIMEOUT).await,
            Err(Error::TooLarge)
        ));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 10"]);
        assert!(matches!(
            run(command, 4, Duration::from_millis(20)).await,
            Err(Error::Timeout)
        ));
    }
    #[test]
    fn document_snapshot_is_private_and_released() {
        let doc = Document::from_bytes(b"%PDF-fixture").unwrap();
        let path = doc.path.clone();
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
