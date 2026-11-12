//! A user-supplied Node bridge is an optional external tool. No PilotDeck TS
//! implementation is executed here. The owner retains its child PID until
//! its final group signal, then reaps it; numeric PGIDs are never reused.
use std::{
    ffi::OsString,
    io::{self, Read},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::task::JoinHandle;

use crate::channel::protocol::{ChannelError, ChannelLog};

#[derive(Debug)]
pub(super) enum SpawnFailure {
    /// Node validates malformed arguments before allocating a child.
    Synchronous(ChannelError),
    /// Node emits child `error` for an OS spawn failure, then readiness runs.
    Child(ChannelError),
}

pub(super) struct BridgeTask {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    task: Option<JoinHandle<Result<(), ChannelError>>>,
}

impl BridgeTask {
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
    }

    pub fn activate(&self) {
        self.running.store(true, Ordering::Release);
    }

    pub async fn join(mut self) -> Result<(), ChannelError> {
        self.stop();
        self.task
            .take()
            .expect("owned bridge worker")
            .await
            .map_err(|error| {
                ChannelError::error(format!("whatsapp: bridge worker failed: {error}"))
            })?
    }
}

impl Drop for BridgeTask {
    fn drop(&mut self) {
        self.stop();
    }
}

struct OwnedChild {
    child: Child,
    #[cfg(unix)]
    group: Option<rustix::process::Pid>,
    reaped: bool,
}

impl OwnedChild {
    fn new(child: Child) -> Self {
        Self {
            #[cfg(unix)]
            group: rustix::process::Pid::from_raw(child.id() as i32),
            child,
            reaped: false,
        }
    }

    #[cfg(unix)]
    fn exited(&mut self) -> io::Result<bool> {
        use rustix::process::{WaitId, WaitIdOptions, waitid};
        let Some(group) = self.group else {
            return Err(io::Error::other("bridge child ownership was lost"));
        };
        match waitid(
            WaitId::Pid(group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(status) => Ok(status.is_some()),
            Err(rustix::io::Errno::INTR) => Ok(false),
            Err(error) => {
                if error == rustix::io::Errno::CHILD {
                    self.group = None;
                    self.reaped = true;
                }
                Err(error.into())
            }
        }
    }

    #[cfg(not(unix))]
    fn exited(&mut self) -> io::Result<bool> {
        let exited = self.child.try_wait()?.is_some();
        self.reaped |= exited;
        Ok(exited)
    }

    fn signal(&mut self, force: bool) -> io::Result<()> {
        #[cfg(unix)]
        {
            use rustix::process::{Signal, kill_process_group};
            if let Some(group) = self.group {
                match kill_process_group(group, if force { Signal::KILL } else { Signal::TERM }) {
                    Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                    #[cfg(target_os = "macos")]
                    Err(rustix::io::Errno::PERM)
                        if self.exited()? && macos_single_zombie(group).unwrap_or(false) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = force;
            if !self.reaped {
                self.child.kill()?;
            }
            Ok(())
        }
    }

    fn reap(&mut self) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        {
            self.group = None;
        }
        let result = self.child.wait();
        self.reaped = result.is_ok();
        result
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.signal(true);
            let _ = self.reap();
        }
    }
}

#[cfg(unix)]
fn nonblocking(pipe: &impl std::os::fd::AsFd) -> io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let flags = fcntl_getfl(pipe)?;
    fcntl_setfl(pipe, flags | OFlags::NONBLOCK)?;
    Ok(())
}

fn drain(mut pipe: impl Read, done: &AtomicBool, logger: Option<&ChannelLog>) -> io::Result<()> {
    let mut bytes = [0_u8; 64 * 1024];
    loop {
        if done.load(Ordering::Acquire) {
            return Ok(());
        }
        match pipe.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                if let Some(log) = logger {
                    let text = String::from_utf8_lossy(&bytes[..count]);
                    let mut units: Vec<u16> = text.encode_utf16().collect();
                    while units.last().is_some_and(|unit| {
                        crate::channel::sms::trim_units(std::slice::from_ref(unit)).is_empty()
                    }) {
                        units.pop();
                    }
                    log(
                        "warn",
                        &format!("whatsapp[bridge]: {}", String::from_utf16_lossy(&units)),
                    );
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn spawn(
    executable: &PathBuf,
    script: &str,
    port: Option<u16>,
    env: &[(OsString, OsString)],
    logger: Option<ChannelLog>,
) -> Result<BridgeTask, SpawnFailure> {
    if script.as_bytes().contains(&0) {
        let message = crate::node_io::invalid_arg_value_message(
            &"The argument 'args[0]' must be a string without null bytes. Received "
                .encode_utf16()
                .collect::<Vec<_>>(),
            &script.encode_utf16().collect::<Vec<_>>(),
        );
        return Err(SpawnFailure::Synchronous(ChannelError::Plain(format!(
            "TypeError [ERR_INVALID_ARG_VALUE]: {}",
            String::from_utf16_lossy(message.units())
        ))));
    }
    let mut command = Command::new(executable);
    command
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(env.iter().cloned());
    if let Some(port) = port {
        command.env("BRIDGE_PORT", port.to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        pilotdeck_child_signals::install_child_signal_reset(&mut command);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut owner = OwnedChild::new(command.spawn().map_err(|error| {
        SpawnFailure::Child(ChannelError::error(format!(
            "spawn {} {}",
            executable.display(),
            crate::node_io::lookup(&error)
                .map(|(code, _)| code)
                .unwrap_or("UNKNOWN")
        )))
    })?);
    let stdout = owner.child.stdout.take().expect("piped bridge stdout");
    let stderr = owner.child.stderr.take().expect("piped bridge stderr");
    #[cfg(unix)]
    {
        nonblocking(&stdout)
            .and_then(|()| nonblocking(&stderr))
            .map_err(|error| {
                SpawnFailure::Synchronous(ChannelError::error(format!(
                    "whatsapp: bridge pipe setup failed: {error}"
                )))
            })?;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(false));
    let active = running.clone();
    let requested = stop.clone();
    let task = tokio::task::spawn_blocking(move || {
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let stdout_worker = scope.spawn(|| drain(stdout, &done, None));
            let stderr_worker = scope.spawn(|| drain(stderr, &done, logger.as_ref()));
            let result = (|| {
                loop {
                    if requested.load(Ordering::Acquire) {
                        break;
                    }
                    if owner.exited()? {
                        // The unreaped leader anchors this group even if its
                        // script left a grandchild behind. Kill before wait.
                        owner.signal(true)?;
                        let status = owner.reap()?;
                        if let Some(log) =
                            logger.as_ref().filter(|_| active.load(Ordering::Acquire))
                        {
                            #[cfg(unix)]
                            let signal = {
                                use std::os::unix::process::ExitStatusExt;
                                status
                                    .signal()
                                    .map(|signal| match signal {
                                        1 => "SIGHUP".into(),
                                        2 => "SIGINT".into(),
                                        3 => "SIGQUIT".into(),
                                        6 => "SIGABRT".into(),
                                        9 => "SIGKILL".into(),
                                        13 => "SIGPIPE".into(),
                                        15 => "SIGTERM".into(),
                                        value => format!("SIG{value}"),
                                    })
                                    .unwrap_or_else(|| "none".into())
                            };
                            #[cfg(not(unix))]
                            let signal = "none".to_owned();
                            let code = status
                                .code()
                                .map(|code| code.to_string())
                                .unwrap_or_else(|| "null".into());
                            log(
                                "error",
                                &format!("whatsapp: bridge exited (code={code}, signal={signal})"),
                            );
                        }
                        return Ok::<_, io::Error>(());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                owner.signal(false)?;
                let deadline = Instant::now() + Duration::from_secs(5);
                while !owner.exited()? && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                owner.signal(true)?;
                owner.reap()?;
                Ok(())
            })();
            // Readers stop independently of a detached descendant retaining
            // an inherited pipe. Unix pipes are nonblocking and bounded.
            done.store(true, Ordering::Release);
            let stdout_result = stdout_worker.join();
            let stderr_result = stderr_worker.join();
            result.map_err(|error| {
                ChannelError::error(format!("whatsapp: bridge cleanup failed: {error}"))
            })?;
            for (role, result) in [("stdout", stdout_result), ("stderr", stderr_result)] {
                result
                    .map_err(|_| {
                        ChannelError::error(format!("whatsapp: bridge {role} reader panicked"))
                    })?
                    .map_err(|error| {
                        ChannelError::error(format!("whatsapp: bridge {role} read failed: {error}"))
                    })?;
            }
            Ok(())
        })
    });
    Ok(BridgeTask {
        stop,
        running,
        task: Some(task),
    })
}

// Darwin reports EPERM (rather than ESRCH) when a group consists solely of
// an unreaped zombie. Never reinterpret EPERM based on the leader alone:
// another member could be alive and legitimately inaccessible.
#[cfg(target_os = "macos")]
fn macos_single_zombie(group: rustix::process::Pid) -> io::Result<bool> {
    let leader = group.as_raw_nonzero().get();
    let mut command = Command::new("/bin/ps");
    command
        .args(["-g", &leader.to_string(), "-o", "pid=,pgid=,stat="])
        .env_clear()
        .env("LC_ALL", "C");
    query_single_zombie(command, leader)
}

#[cfg(target_os = "macos")]
fn query_single_zombie(mut command: Command, leader: i32) -> io::Result<bool> {
    // No shell or PATH lookup. Own the query child even on timeout/read error;
    // the caller still retains the original unreaped bridge group anchor.
    struct QueryChild(Child);
    impl Drop for QueryChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = QueryChild(command.spawn()?);
    let mut stdout = child.0.stdout.take().expect("piped query stdout");
    nonblocking(&stdout)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut output = Vec::new();
    let mut eof = false;
    loop {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        let mut buffer = [0_u8; 1024];
        match stdout.read(&mut buffer) {
            Ok(0) => eof = true,
            Ok(count) => {
                output.extend_from_slice(&buffer[..count]);
                if output.len() > 8192 {
                    return Ok(false);
                }
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        if eof {
            if let Some(status) = child.0.try_wait()? {
                return Ok(single_zombie_output(
                    &output,
                    leader,
                    status.success(),
                    true,
                ));
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(any(target_os = "macos", test))]
fn single_zombie_output(output: &[u8], leader: i32, success: bool, complete: bool) -> bool {
    if !success || !complete || output.len() > 8192 {
        return false;
    }
    let Ok(text) = std::str::from_utf8(output) else {
        return false;
    };
    let mut rows = text.lines().filter(|line| !line.trim().is_empty());
    let Some(row) = rows.next() else {
        return false;
    };
    if rows.next().is_some() {
        return false;
    }
    let fields: Vec<_> = row.split_whitespace().collect();
    fields.len() == 3
        && fields[0].parse::<i32>() == Ok(leader)
        && fields[1].parse::<i32>() == Ok(leader)
        && fields[2].starts_with('Z')
}

#[cfg(test)]
mod macos_query_tests {
    use super::*;
    #[test]
    fn singleton_zombie_proof_rejects_extra_live_unknown_truncated_or_failed_queries() {
        assert!(single_zombie_output(b" 42 42 Z+\n", 42, true, true));
        for output in [
            b"42 42 S\n".as_slice(),
            b"42 42 Z\n43 42 S\n",
            b"43 42 Z\n",
            b"42 43 Z\n",
            b"42 42\n",
            b"",
            b"\xff",
        ] {
            assert!(!single_zombie_output(output, 42, true, true));
        }
        assert!(!single_zombie_output(b"42 42 Z\n", 42, false, true));
        assert!(!single_zombie_output(b"42 42 Z\n", 42, true, false));
        assert!(!single_zombie_output(&vec![b' '; 8193], 42, true, true));
    }
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn macos_query_budget_reaps_its_exact_child_on_timeout_and_output_limit() {
    let directory = std::env::temp_dir().join(format!("whatsapp-query-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let node = std::env::var_os("NODE22").unwrap_or_else(|| "node".into());
    for mode in ["timeout", "oversized"] {
        let pid_file = directory.join(mode);
        let mut command = Command::new(&node);
        command.arg("-e").arg("const fs=require('fs');fs.writeFileSync(process.argv[1],String(process.pid));if(process.argv[2]==='oversized')fs.writeSync(1,Buffer.alloc(16384,32));setInterval(()=>{},1000)")
            .arg(&pid_file).arg(mode).env_clear()
            .env("HOME", &directory).env("PILOT_HOME", &directory)
            .env("PATH",std::env::var_os("PATH").unwrap_or_default());
        let started = Instant::now();
        assert!(!query_single_zombie(command, 42).unwrap());
        assert!(started.elapsed() < Duration::from_secs(3));
        if mode == "timeout" {
            assert!(started.elapsed() >= Duration::from_secs(2));
        }
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        assert!(
            rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap())
                .is_err(),
            "query child was killed and reaped"
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}
