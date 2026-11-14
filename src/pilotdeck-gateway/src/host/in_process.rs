//! The gateway's built-in loop inside this process (the baseline runs it in
//! process, `createAgentSession.ts:132`). The loop is the same sidecar server
//! the `pilotdeck-agent-loop-sidecar` executable serves
//! ([`pilotdeck_transport::hosted`]); only the byte stream between it and the
//! host changes, from a child's stdin / stdout to a pair of in-memory pipes.
//! The NDJSON frames on them are unchanged, so both ends keep their code.
//!
//! The server is blocking-thread code: it runs on its own thread, whose end
//! the connection observes the way it observed a child's exit. A panic on
//! that thread or on any thread the server starts stays there: the server's
//! own execution guard turns a loop panic into the turn's failure terminal,
//! [`launch`] catches anything that escapes, and the panic report is kept off
//! the gateway's stderr (a child's stderr went nowhere visible either).
use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    sync::{Arc, Condvar, Mutex, MutexGuard, Once},
    task::{Context, Poll, Waker},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Bytes one pipe holds before its writer waits, as an OS pipe would.
const PIPE_CAPACITY: usize = 64 * 1024;

#[derive(Default)]
struct State {
    bytes: VecDeque<u8>,
    writer_closed: bool,
    reader_closed: bool,
    reader_waker: Option<Waker>,
    writer_waker: Option<Waker>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
    /// Wake both kinds of waiter after `state` changed; call without the lock.
    fn notify(&self, wakers: [Option<Waker>; 2]) {
        self.changed.notify_all();
        for waker in wakers.into_iter().flatten() {
            waker.wake();
        }
    }
}

/// The writing end of an in-memory pipe: blocking [`io::Write`] for the
/// server's threads, [`AsyncWrite`] for the host. Dropping it is EOF.
pub(crate) struct PipeWriter(Arc<Shared>);
/// The reading end: blocking [`io::Read`] or [`AsyncRead`]. Dropping it makes
/// later writes fail with `BrokenPipe`.
pub(crate) struct PipeReader(Arc<Shared>);

pub(crate) fn pipe() -> (PipeWriter, PipeReader) {
    let shared = Arc::new(Shared::default());
    (PipeWriter(shared.clone()), PipeReader(shared))
}

impl PipeWriter {
    /// Copy what fits; `None` when the pipe is full.
    fn try_write(&self, data: &[u8], waker: Option<&Waker>) -> Option<io::Result<usize>> {
        let mut state = self.0.lock();
        if state.reader_closed {
            return Some(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if data.is_empty() {
            return Some(Ok(0));
        }
        let room = PIPE_CAPACITY.saturating_sub(state.bytes.len());
        if room == 0 {
            if let Some(waker) = waker {
                state.writer_waker = Some(waker.clone());
            }
            return None;
        }
        let size = room.min(data.len());
        state.bytes.extend(&data[..size]);
        let reader = state.reader_waker.take();
        drop(state);
        self.0.notify([reader, None]);
        Some(Ok(size))
    }
    fn close(&self) {
        let mut state = self.0.lock();
        state.writer_closed = true;
        let reader = state.reader_waker.take();
        drop(state);
        self.0.notify([reader, None]);
    }
}
impl io::Write for PipeWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        loop {
            if let Some(result) = self.try_write(data, None) {
                return result;
            }
            let state = self.0.lock();
            if state.reader_closed || state.bytes.len() < PIPE_CAPACITY {
                continue;
            }
            drop(self.0.changed.wait(state));
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl AsyncWrite for PipeWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.try_write(data, Some(cx.waker())) {
            Some(result) => Poll::Ready(result),
            None => Poll::Pending,
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.close();
        Poll::Ready(Ok(()))
    }
}
impl Drop for PipeWriter {
    fn drop(&mut self) {
        self.close();
    }
}

impl PipeReader {
    /// Copy what is buffered; `None` when empty and still open.
    fn try_read(&self, out: &mut [u8], waker: Option<&Waker>) -> Option<usize> {
        let mut state = self.0.lock();
        if state.bytes.is_empty() {
            if state.writer_closed || out.is_empty() {
                return Some(0);
            }
            if let Some(waker) = waker {
                state.reader_waker = Some(waker.clone());
            }
            return None;
        }
        let size = out.len().min(state.bytes.len());
        for (slot, byte) in out.iter_mut().zip(state.bytes.drain(..size)) {
            *slot = byte;
        }
        let writer = state.writer_waker.take();
        drop(state);
        self.0.notify([None, writer]);
        Some(size)
    }
}
impl io::Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(size) = self.try_read(out, None) {
                return Ok(size);
            }
            let state = self.0.lock();
            if !state.bytes.is_empty() || state.writer_closed {
                continue;
            }
            drop(self.0.changed.wait(state));
        }
    }
}
impl AsyncRead for PipeReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.try_read(buf.initialize_unfilled(), Some(cx.waker())) {
            Some(size) => {
                buf.advance(size);
                Poll::Ready(Ok(()))
            }
            None => Poll::Pending,
        }
    }
}
impl Drop for PipeReader {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.reader_closed = true;
        state.bytes.clear();
        let writer = state.writer_waker.take();
        drop(state);
        self.0.notify([None, writer]);
    }
}

/// How the hosted server's thread ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopEnd {
    /// `serve` returned; `complete` is its own shutdown verdict.
    Served { complete: bool, stalled: bool },
    /// The thread panicked outside the server's execution guard.
    Panicked,
}
impl LoopEnd {
    /// The exit code the executable reports for the same end (diagnostics
    /// only): 0 clean, 13 stalled on a reply that cannot come, 3 incomplete,
    /// and Rust's 101 for a panic that escaped.
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::Served { stalled: true, .. } => 13,
            Self::Served { complete: true, .. } => 0,
            Self::Served { .. } => 3,
            Self::Panicked => 101,
        }
    }
}

/// The host's ends of one hosted connection and the thread serving it.
pub(crate) struct Launched {
    /// Host → loop (the server reads it).
    pub input: PipeWriter,
    /// Loop → host.
    pub output: PipeReader,
    pub ended: tokio::sync::oneshot::Receiver<LoopEnd>,
}

/// Start one connection of a fresh hosted sidecar instance on its own thread
/// (one instance per turn, as each turn's child process was). `process_cwd`
/// is what the server falls back to when execute names no cwd, the working
/// directory the child was started in.
pub(crate) fn launch(process_cwd: pilotdeck_protocol::JsString) -> io::Result<Launched> {
    launch_with(move |input, output| {
        let report = pilotdeck_transport::hosted::serve(process_cwd, input, output);
        LoopEnd::Served {
            complete: report.complete(),
            stalled: report.stalled,
        }
    })
}

/// [`launch`] over any server body; tests drive a misbehaving one.
pub(crate) fn launch_with(
    serve: impl FnOnce(io::BufReader<PipeReader>, PipeWriter) -> LoopEnd + Send + 'static,
) -> io::Result<Launched> {
    silence_hosted_panics();
    let (input, server_input) = pipe();
    let (server_output, output) = pipe();
    let (done, ended) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("pilotdeck-agent-loop".into())
        .spawn(move || {
            pilotdeck_transport::hosted::mark_hosted_thread();
            let end = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                serve(io::BufReader::new(server_input), server_output)
            }))
            .unwrap_or(LoopEnd::Panicked);
            let _ = done.send(end);
        })?;
    Ok(Launched {
        input,
        output,
        ended,
    })
}

/// A panic on a hosted loop thread is the loop's failure, reported through
/// the protocol (or the connection's end), never on the gateway's stderr.
/// Other threads keep the hook that was installed before.
fn silence_hosted_panics() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !pilotdeck_transport::hosted::is_hosted_thread() {
                previous(info);
            }
        }));
    });
}

#[cfg(test)]
#[path = "in_process_tests.rs"]
mod tests;
