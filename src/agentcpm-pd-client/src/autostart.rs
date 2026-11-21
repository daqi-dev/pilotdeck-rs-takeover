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
    /// Kills and reaps any unready child spawned by `spawn` when startup times
    /// out, fails, or is cancelled before `ready.json` appears.
    fn cleanup_unready(&self) {}
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
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Detach from the CLI foreground process group so Ctrl-C in a
            // terminal session never SIGINTs the background daemon.
            cmd.process_group(0);
        }

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

    fn cleanup_unready(&self) {
        let child = match self.child.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        let Some(mut child) = child else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        #[cfg(unix)]
        {
            let _ = signal_pid_term(child.id());
            for _ in 0..5 {
                std::thread::sleep(Duration::from_millis(20));
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
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

async fn acquire_daemon_flock(lock_path: PathBuf, timeout: Duration) -> Result<File, ClientError> {
    let open_path = lock_path.clone();
    let file = tokio::task::spawn_blocking(move || {
        if let Some(parent) = open_path.parent() {
            ensure_secure_dir(parent)?;
        }
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&open_path)
            .map_err(|e| {
                ClientError::connection_failed(format!(
                    "failed to open daemon lock {}: {e}",
                    open_path.display()
                ))
            })
    })
    .await
    .map_err(|e| ClientError::connection_failed(format!("flock join error: {e}")))??;

    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(ClientError::connection_failed(format!(
                        "timed out after {}ms waiting for daemon lock {}",
                        timeout.as_millis(),
                        lock_path.display()
                    )));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(ClientError::connection_failed(format!(
                    "failed to lock daemon lock {}: {e}",
                    lock_path.display()
                )));
            }
        }
    }
}

struct SpawnCleanupGuard {
    spawner: Arc<dyn GatewaySpawner>,
    armed: bool,
}

impl Drop for SpawnCleanupGuard {
    fn drop(&mut self) {
        if self.armed {
            let spawner = self.spawner.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn_blocking(move || spawner.cleanup_unready());
            } else {
                spawner.cleanup_unready();
            }
        }
    }
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

    let deadline = Instant::now() + startup_timeout;
    let lock_guard = acquire_daemon_flock(daemon_lock_path(home), startup_timeout).await?;

    // Double-checked locking: another process holding the lock earlier may have finished booting it.
    if let Some(ready) = check_ready_live(home, expected_instance_id).await? {
        drop(lock_guard);
        return Ok(ready);
    }

    let mut cleanup_guard = SpawnCleanupGuard {
        spawner: spawner.clone(),
        armed: true,
    };
    spawner.spawn(home).await?;

    let res = loop {
        if let Err(err) = spawner.check_early_exit() {
            break Err(err);
        }
        match check_ready_live(home, expected_instance_id).await {
            Ok(Some(ready)) => break Ok(ready),
            Ok(None) => {}
            Err(err) => break Err(err),
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

    cleanup_guard.armed = false;
    if res.is_err() {
        let spawner_cleanup = spawner.clone();
        let _ = tokio::task::spawn_blocking(move || spawner_cleanup.cleanup_unready()).await;
    }

    drop(lock_guard);
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn temp_home(tag: &str) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agentcpm-pd-autostart-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_secure_dir(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn daemon_flock_respects_startup_timeout() {
        let home = temp_home("flock-timeout");
        let lock_path = daemon_lock_path(&home);
        ensure_secure_dir(lock_path.parent().unwrap()).unwrap();
        let held = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        held.lock().unwrap();

        let spawner: Arc<dyn GatewaySpawner> = Arc::new(ProcessGatewaySpawner::new(PathBuf::from(
            "/nonexistent-binary",
        )));
        let start = Instant::now();
        let err = ensure_local_gateway_running_with_timeout(
            &home,
            "inst-1",
            false,
            spawner,
            Duration::from_millis(120),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ClientError::ConnectionFailed(_)));
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(held);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_child_uses_isolated_process_group_and_is_cleaned_on_timeout() {
        use std::os::unix::fs::PermissionsExt;

        let home = temp_home("pgid-cleanup");
        let script = home.join("fake-gateway.sh");
        let pid_file = home.join("child.pid");
        let pgid_file = home.join("child.pgid");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > \"{}\"\nps -o pgid= -p $$ > \"{}\"\nsleep 30\n",
                pid_file.display(),
                pgid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let spawner = Arc::new(ProcessGatewaySpawner::new(script));
        let err = ensure_local_gateway_running_with_timeout(
            &home,
            "inst-1",
            false,
            spawner.clone(),
            Duration::from_millis(350),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ClientError::ConnectionFailed(_)));

        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("script wrote pid")
            .trim()
            .parse()
            .unwrap();
        let pgid: i32 = std::fs::read_to_string(&pgid_file)
            .expect("script wrote pgid")
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            pgid, pid as i32,
            "child must run in its own isolated process group (pgid == pid)"
        );
        // Wait briefly for kernel process teardown after reap.
        for _ in 0..20 {
            if !is_pid_alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !is_pid_alive(pid),
            "timed-out unready child pid {pid} must be terminated and reaped"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn cancelled_autostart_invokes_cleanup_unready() {
        struct CancelProbeSpawner {
            cleaned: Arc<AtomicBool>,
        }
        impl GatewaySpawner for CancelProbeSpawner {
            fn spawn(&self, _home: &Path) -> SpawnGatewayFuture {
                Box::pin(async { Ok(()) })
            }
            fn cleanup_unready(&self) {
                self.cleaned.store(true, Ordering::SeqCst);
            }
        }

        let home = temp_home("cancel-cleanup");
        let cleaned = Arc::new(AtomicBool::new(false));
        let spawner: Arc<dyn GatewaySpawner> = Arc::new(CancelProbeSpawner {
            cleaned: cleaned.clone(),
        });
        let fut = ensure_local_gateway_running_with_timeout(
            &home,
            "inst-1",
            false,
            spawner,
            Duration::from_secs(30),
        );
        let timed = tokio::time::timeout(Duration::from_millis(60), fut).await;
        assert!(timed.is_err());
        for _ in 0..20 {
            if cleaned.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            cleaned.load(Ordering::SeqCst),
            "cancelling autostart future must trigger cleanup_unready"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
