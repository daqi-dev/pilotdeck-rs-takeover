//! Optional Git Web updater. This is not a Rust self-updater.
use super::{CommandError, UpdateResult};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

const MISSING: &str = "This installation does not include the Git Web updater. Use application settings or update manually.";
fn root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("scripts/update-web.mjs").is_file())
        .map(Path::to_path_buf)
}
pub(super) async fn run(
    executable: Option<&Path>,
    check: bool,
) -> Result<UpdateResult, CommandError> {
    let root = executable
        .and_then(Path::parent)
        .and_then(root)
        .ok_or_else(|| CommandError(MISSING.into()))?;
    let mut child = tokio::process::Command::new("node")
        .arg(root.join("scripts/update-web.mjs"))
        .arg(if check { "--check" } else { "--restart" })
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            CommandError(
                format!(
                    "spawn node {}",
                    match error.kind() {
                        std::io::ErrorKind::NotFound => "ENOENT",
                        std::io::ErrorKind::PermissionDenied => "EACCES",
                        _ => "failed",
                    }
                )
                .into(),
            )
        })?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut a = [0u8; 8192];
    let mut b = [0u8; 8192];
    let mut a_open = true;
    let mut b_open = true;
    let mut tail = Vec::<u16>::new();
    while a_open || b_open {
        let bytes = tokio::select! {
            read=stdout.read(&mut a), if a_open => { let n=read.map_err(|e|CommandError(e.to_string().into()))?; if n==0 {a_open=false;} String::from_utf8_lossy(&a[..n]).into_owned() },
            read=stderr.read(&mut b), if b_open => { let n=read.map_err(|e|CommandError(e.to_string().into()))?; if n==0 {b_open=false;} String::from_utf8_lossy(&b[..n]).into_owned() },
        };
        tail.extend(bytes.encode_utf16());
        if tail.len() > 12000 {
            tail.drain(..tail.len() - 12000);
        }
    }
    let status = child
        .wait()
        .await
        .map_err(|e| CommandError(e.to_string().into()))?;
    Ok(UpdateResult {
        code: Some(status.code().unwrap_or(1)),
        output: super::trim(&pilotdeck_protocol::JsString::from_utf16(tail)),
    })
}
