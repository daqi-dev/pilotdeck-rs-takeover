//! Local Gateway single-winner auto-start via OS flock and `run/ready.json` (`007 §8, A3`).

use std::fs::{File, OpenOptions};
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use url::Url;

use crate::config::{ReadyFile, daemon_lock_path, ensure_secure_dir, load_ready_file};
use crate::error::ClientError;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

pub type SpawnGatewayFuture = Pin<Box<dyn Future<Output = Result<(), ClientError>> + Send>>;

/// Abstraction allowing both real subprocess launch and in-process async test spawners.
pub trait GatewaySpawner: Send + Sync {
    fn spawn(&self, home: &Path) -> SpawnGatewayFuture;
    fn check_early_exit(&self) -> Result<(), ClientError> {
        Ok(())
    }
}

/// Default production spawner invoking `<exe> server serve --data-root <home>`.
pub struct ProcessGatewaySpawner {
    pub executable: PathBuf,
    child: std::sync::Mutex<Option<Child>>,
}

impl ProcessGatewaySpawner {
    pub fn current_exe() -> Result<Self, ClientError> {
        let exe = std::env::current_exe().map_err(|e| {
            ClientError::connection_failed(format!("failed to locate current executable: {e}"))
        })?;
        Ok(Self::new(exe))
    }

    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            child: std::sync::Mutex::new(None),
        }
    }
}

impl ProcessGatewaySpawner {
    /// Synchronously spawns `<exe> server serve --data-root <home>` and keeps
    /// the child handle for the early-exit probe.
    fn spawn_gateway_process(&self, home_dir: &Path) -> Result<(), ClientError> {
        let logs_dir = home_dir.join("logs");
        ensure_secure_dir(&logs_dir)?;
        let stdout_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs_dir.join("gateway.stdout.log"))
            .map_err(|e| ClientError::connection_failed(format!("open gateway stdout log: {e}")))?;
        let stderr_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs_dir.join("gateway.stderr.log"))
            .map_err(|e| ClientError::connection_failed(format!("open gateway stderr log: {e}")))?;

        let exe = &self.executable;
        let mut cmd = Command::new(exe);
        cmd.arg("server")
            .arg("serve")
            .arg("--data-root")
            .arg(home_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));

        let child = cmd.spawn().map_err(|e| {
            ClientError::connection_failed(format!(
                "failed to spawn `{}` server serve: {e}",
                exe.display()
            ))
        })?;
        // Keep the handle so `check_early_exit` can fail fast when the daemon
        // dies before writing ready.json (wp4fix2 review P2): the spawn used
        // to drop the child, making the early-exit probe a permanent no-op
        // and leaving callers to burn the full startup timeout instead of
        // surfacing the exit status.
        if let Ok(mut guard) = self.child.lock() {
            *guard = Some(child);
        }
        Ok(())
    }
}

impl GatewaySpawner for ProcessGatewaySpawner {
    fn spawn(&self, home: &Path) -> SpawnGatewayFuture {
        let outcome = self.spawn_gateway_process(home);
        Box::pin(async move { outcome })
    }

    fn check_early_exit(&self) -> Result<(), ClientError> {
        if let Ok(mut guard) = self.child.lock()
            && let Some(child) = guard.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            return Err(ClientError::connection_failed(format!(
                "gateway daemon exited prematurely with status {status}"
            )));
        }
        Ok(())
    }
}

/// Checks OS process liveness via POSIX signal 0 (`ESRCH` => dead).
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(raw) = i32::try_from(pid) else {
            return false;
        };
        let Some(p) = rustix::process::Pid::from_raw(raw) else {
            return false;
        };
        match rustix::process::test_kill_process(p) {
            Ok(()) => true,
            Err(err) => err != rustix::io::Errno::SRCH,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Sends `SIGTERM` to a local gateway PID.
pub fn signal_pid_term(pid: u32) -> Result<(), ClientError> {
    #[cfg(unix)]
    {
        let raw = i32::try_from(pid)
            .map_err(|_| ClientError::connection_failed(format!("invalid pid {pid}")))?;
        let p = rustix::process::Pid::from_raw(raw).ok_or_else(|| {
            ClientError::connection_failed(format!("invalid non-positive pid {pid}"))
        })?;
        rustix::process::kill_process(p, rustix::process::Signal::TERM).map_err(|e| {
            ClientError::connection_failed(format!("failed to send SIGTERM to pid {pid}: {e}"))
        })?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Err(ClientError::connection_failed(
            "POSIX SIGTERM signaling is unsupported on this platform",
        ))
    }
}

pub async fn is_ws_tcp_reachable(ws_url: &str) -> bool {
    let Ok(url) = Url::parse(ws_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let Some(port) = url.port_or_known_default() else {
        return false;
    };
    let clean_host = host.trim_start_matches('[').trim_end_matches(']');
    let addr = if let Ok(ip) = clean_host.parse::<std::net::IpAddr>() {
        SocketAddr::new(ip, port)
    } else if clean_host.eq_ignore_ascii_case("localhost") {
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)
    } else {
        return tokio::time::timeout(
            Duration::from_millis(250),
            tokio::net::TcpStream::connect((clean_host, port)),
        )
        .await
        .is_ok_and(|res| res.is_ok());
    };
    tokio::time::timeout(
        Duration::from_millis(200),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .is_ok_and(|res| res.is_ok())
}

async fn check_ready_live(
    home: &Path,
    expected_instance_id: &str,
) -> Result<Option<ReadyFile>, ClientError> {
    let Some(ready) = load_ready_file(home)? else {
        return Ok(None);
    };
    if !is_pid_alive(ready.pid) {
        return Ok(None);
    }
    if ready.instance_id != expected_instance_id {
        return Err(ClientError::InstanceChanged {
            expected: expected_instance_id.to_owned(),
            actual: ready.instance_id,
        });
    }
    if is_ws_tcp_reachable(&ready.ws_url).await {
        return Ok(Some(ready));
    }
    Ok(None)
}

async fn acquire_daemon_flock(lock_path: PathBuf) -> Result<File, ClientError> {
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = lock_path.parent() {
            ensure_secure_dir(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| {
                ClientError::connection_failed(format!(
                    "failed to open daemon lock {}: {e}",
                    lock_path.display()
                ))
            })?;
        file.lock().map_err(|e| {
            ClientError::connection_failed(format!(
                "failed to lock daemon lock {}: {e}",
                lock_path.display()
            ))
        })?;
        Ok(file)
    })
    .await
    .map_err(|e| ClientError::connection_failed(format!("flock join error: {e}")))?
}

/// Ensures an already-initialized local Gateway instance is running and returns its `ReadyFile`.
/// Uses OS `flock` (`<home>/run/daemon.lock`) so concurrent callers elect a single daemon winner.
pub async fn ensure_local_gateway_running(
    home: &Path,
    expected_instance_id: &str,
    no_start: bool,
    spawner: Arc<dyn GatewaySpawner>,
) -> Result<ReadyFile, ClientError> {
    ensure_local_gateway_running_with_timeout(
        home,
        expected_instance_id,
        no_start,
        spawner,
        STARTUP_TIMEOUT,
    )
    .await
}

pub async fn ensure_local_gateway_running_with_timeout(
    home: &Path,
    expected_instance_id: &str,
    no_start: bool,
    spawner: Arc<dyn GatewaySpawner>,
    startup_timeout: Duration,
) -> Result<ReadyFile, ClientError> {
    if let Some(ready) = check_ready_live(home, expected_instance_id).await? {
        return Ok(ready);
    }

    if no_start {
        return Err(ClientError::connection_failed(format!(
            "local gateway at {} is not running and auto-start is disabled (--no-start)",
            home.display()
        )));
    }

    let lock_guard = acquire_daemon_flock(daemon_lock_path(home)).await?;

    // Double-checked locking: another process holding the lock earlier may have finished booting it.
    if let Some(ready) = check_ready_live(home, expected_instance_id).await? {
        drop(lock_guard);
        return Ok(ready);
    }

    spawner.spawn(home).await?;

    let deadline = Instant::now() + startup_timeout;
    let res = loop {
        spawner.check_early_exit()?;
        if let Some(ready) = check_ready_live(home, expected_instance_id).await? {
            break Ok(ready);
        }
        if Instant::now() >= deadline {
            break Err(ClientError::connection_failed(format!(
                "timed out after {}ms waiting for local gateway ready.json at {}",
                startup_timeout.as_millis(),
                home.display()
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };

    drop(lock_guard);
    res
}
