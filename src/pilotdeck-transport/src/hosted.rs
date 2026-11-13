//! The agent loop sidecar's server as a library: what the
//! `pilotdeck-agent-loop-sidecar` executable serves over its stdio or TCP,
//! and what the Rust gateway runs inside its own process for its built-in
//! loop (the loop the baseline runs in process, `createAgentSession.ts:132`).
//!
//! Hosted inside another process, the server must stay out of that process's
//! shared state: it reads no stdio, writes nothing to stdout / stderr, and
//! the threads it starts carry a mark ([`is_hosted_thread`]) so the host can
//! keep a panic on one of them from reaching its own stderr.
use crate::default_factory::OriginalAgentFactory;
use crate::server::{ExecutionServer, OperationRegistry, ShutdownReport};
use crate::stdio::{ConnectionOwner, random_token};
use crate::system_clock::SystemExecutionClock;
use pilotdeck_protocol::JsString;
use pilotdeck_protocol::admission::SidecarBinding;
use std::cell::Cell;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How long [`ExecutionServer::serve`] waits for its threads after the
/// connection ended, as the executable has always used.
pub const SERVE_GRACE: Duration = Duration::from_secs(5);

/// One sidecar instance (agentLoopSidecarServer.ts:84-102, 621): a module
/// instance id and a server generation drawn once, one operation table
/// shared by every connection, and a fresh connection generation for each
/// connection it makes.
pub fn instance(process_cwd: JsString) -> impl Fn() -> Arc<ExecutionServer> + Send + Sync {
    let module_instance = format!("pilotdeck-agent-loop-instance-{}", random_token());
    let server_generation = format!("{module_instance}-{}", random_token());
    let clock = Arc::new(SystemExecutionClock);
    let factory = Arc::new(OriginalAgentFactory {
        clock: clock.clone(),
        process_cwd,
    });
    let registry = OperationRegistry::default();
    move || {
        let binding = SidecarBinding {
            module_id: "pilotdeck-agent-loop".into(),
            module_instance_id: module_instance.clone().into(),
            connection_generation: format!("{server_generation}-{}", random_token()).into(),
            capabilities_version: "1".into(),
        };
        ExecutionServer::with_registry(
            ConnectionOwner::new(binding, 64),
            factory.clone(),
            clock.clone(),
            32,
            registry.clone(),
        )
    }
}

/// Serve one connection of a fresh sidecar instance on the calling thread,
/// as the stdio executable serves its single connection. The calling thread
/// and every thread the server starts are marked as hosted.
pub fn serve(
    process_cwd: JsString,
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
) -> ShutdownReport {
    mark_hosted_thread();
    let server = instance(process_cwd)();
    server.serve(input, output, SERVE_GRACE)
}

thread_local! {
    static HOSTED: Cell<bool> = const { Cell::new(false) };
}

/// Mark the calling thread as running the hosted sidecar server; threads it
/// starts through [`spawn`] inherit the mark.
pub fn mark_hosted_thread() {
    HOSTED.with(|hosted| hosted.set(true));
}

/// The calling thread runs the hosted sidecar server (see [`serve`]).
pub fn is_hosted_thread() -> bool {
    HOSTED.with(Cell::get)
}

/// `thread::spawn` that hands the spawning thread's hosted mark on.
pub(crate) fn spawn<F, T>(f: F) -> thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let hosted = is_hosted_thread();
    thread::spawn(move || {
        HOSTED.with(|mark| mark.set(hosted));
        f()
    })
}

/// [`spawn`] through a named builder.
pub(crate) fn spawn_named<F, T>(name: &str, f: F) -> std::io::Result<thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let hosted = is_hosted_thread();
    thread::Builder::new().name(name.into()).spawn(move || {
        HOSTED.with(|mark| mark.set(hosted));
        f()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawned_threads_inherit_the_hosted_mark() {
        assert!(!spawn(is_hosted_thread).join().unwrap());
        let inner = thread::spawn(|| {
            mark_hosted_thread();
            let child = spawn(|| (is_hosted_thread(), spawn(is_hosted_thread).join().unwrap()));
            let named = spawn_named("named", is_hosted_thread).unwrap();
            (child.join().unwrap(), named.join().unwrap())
        })
        .join()
        .unwrap();
        assert_eq!(inner, ((true, true), true));
        assert!(!is_hosted_thread());
    }
}
