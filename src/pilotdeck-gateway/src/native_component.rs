//! Private file snapshots and bounded, cancellable native helper processes.
use crate::{process_group, runtime::network::Cancellation};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

#[derive(Debug)]
pub(crate) enum Error {
    Unavailable,
    Failed(String),
    Timeout,
    TooLarge,
    Cancelled,
}

struct Input {
    directory: PathBuf,
    path: PathBuf,
}
impl Drop for Input {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
#[derive(Clone)]
pub(crate) struct Snapshot(Arc<Input>);
impl Snapshot {
    pub(crate) fn new(kind: &str, bytes: &[u8], limit: usize) -> Result<Self, Error> {
        if bytes.len() > limit {
            return Err(Error::TooLarge);
        }
        let directory =
            std::env::temp_dir().join(format!("pilotdeck-{kind}-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory).map_err(failed)?;
        let input = Self(Arc::new(Input {
            path: directory.join("input"),
            directory,
        }));
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(input.path()).map_err(failed)?;
        std::io::Write::write_all(&mut file, bytes).map_err(failed)?;
        Ok(input)
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0.path
    }
}

pub(crate) fn tool_path(variable: &str, component: &str, executable: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(variable).filter(|p| !p.is_empty()) {
        return path.into();
    }
    if let Some(bin) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        for path in [
            bin.join(executable),
            bin.join(format!("components/{component}/bin/{executable}")),
            bin.join(format!("../components/{component}/bin/{executable}")),
        ] {
            if path.is_file() {
                return path;
            }
        }
    }
    PathBuf::from(executable)
}
fn failed(error: impl std::fmt::Display) -> Error {
    Error::Failed(error.to_string())
}
struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn read_bounded(mut stream: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = stream.read(&mut chunk).await.map_err(failed)?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(count) > limit {
            return Err(Error::TooLarge);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

/// The task, rather than the waiting caller, owns the process and snapshot.
/// Cancellation drops the readers, kills the owned group, and reaps the leader.
/// The leader stays unreaped until the final group signal, preventing PGID reuse.
pub(crate) async fn run(
    command: Command,
    input: Option<Snapshot>,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>, Error> {
    let cancel = Cancellation::new();
    let _guard = CancelOnDrop(cancel.clone());
    tokio::spawn(run_owned(command, input, limit, timeout, cancel))
        .await
        .map_err(failed)?
}

async fn run_owned(
    mut command: Command,
    _input: Option<Snapshot>,
    limit: usize,
    timeout: Duration,
    cancel: Cancellation,
) -> Result<Vec<u8>, Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
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
            failed(e)
        }
    })?;
    let mut group = process_group::GroupGuard(
        child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(id as i32)),
    );
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = tokio::time::timeout(timeout, async {
            tokio::try_join!(read_bounded(stdout, limit), read_bounded(stderr, 16_384), async {
                let pid = group.0.ok_or_else(|| Error::Failed("Native component has no process id.".into()))?;
                process_group::wait_exit_unreaped(pid).await.map_err(failed)
            })
        }) => result.unwrap_or(Err(Error::Timeout)),
    };
    // Helpers never own a legitimate background service. Kill descendants even
    // after a successful leader exit before releasing its process identity.
    group.signal(rustix::process::Signal::KILL);
    let status = child.wait().await.map_err(failed);
    group.disarm();
    let (stdout, stderr, _) = result?;
    let status = status?;
    if status.success() {
        Ok(stdout)
    } else {
        let message = String::from_utf8_lossy(&stderr).trim().to_owned();
        Err(Error::Failed(if message.is_empty() {
            format!("Native component exited with {status}.")
        } else {
            message
        }))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    const TIMEOUT: Duration = Duration::from_secs(2);
    #[tokio::test]
    async fn component_limits_and_exit_failures_are_not_success() {
        assert!(matches!(
            run(
                Command::new("/missing/pilotdeck-component"),
                None,
                4,
                TIMEOUT
            )
            .await,
            Err(Error::Unavailable)
        ));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 12345"]);
        assert!(matches!(
            run(command, None, 4, TIMEOUT).await,
            Err(Error::TooLarge)
        ));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 10"]);
        assert!(matches!(
            run(command, None, 4, Duration::from_millis(20)).await,
            Err(Error::Timeout)
        ));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 7"]);
        assert!(
            matches!(run(command, None, 4, TIMEOUT).await, Err(Error::Failed(message)) if message.contains('7'))
        );
    }
    #[tokio::test]
    async fn dropped_waiter_cancels_owned_process_and_retains_snapshot_until_reaped() {
        let input = Snapshot::new("component-test", b"private", 7).unwrap();
        let path = input.path().to_path_buf();
        let pid_path = path.with_file_name("pid");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf '%s' $$ > \"$1\"; exec sleep 10",
                "component-test",
            ])
            .arg(&pid_path);
        let waiter = tokio::spawn(run(command, Some(input), 4, TIMEOUT));
        let pid = tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&pid_path).await
                    && let Ok(pid) = text.parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(path.exists());
        waiter.abort();
        let _ = waiter.await;
        tokio::time::timeout(TIMEOUT, async {
            while path.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap())
                .is_err()
        );
    }
    #[test]
    fn snapshot_is_private_bounded_and_shared() {
        assert!(matches!(
            Snapshot::new("component-test", b"abc", 2),
            Err(Error::TooLarge)
        ));
        let input = Snapshot::new("component-test", b"abc", 3).unwrap();
        let path = input.path().to_path_buf();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let clone = input.clone();
        drop(input);
        assert!(path.exists());
        drop(clone);
        assert!(!path.exists());
    }
}
