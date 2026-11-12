//! Bounded physical socket output and a separately owned logical engine input.
use pilotdeck_protocol::{JsValue, stringify_json};
use std::{
    io::{self, BufRead, Read, Write},
    net::{Shutdown, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender},
    },
};

pub(super) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
pub(super) fn line(value: &JsValue) -> io::Result<Vec<u8>> {
    let mut value = stringify_json(value)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("Module message has no JSON representation."))?
        .into_bytes();
    value.push(b'\n');
    Ok(value)
}
pub(super) struct PeerOutput {
    pub socket: TcpStream,
    sender: SyncSender<Vec<u8>>,
    bytes: AtomicUsize,
    limit: usize,
    pub closed: AtomicBool,
}
impl PeerOutput {
    pub fn new(socket: TcpStream, sender: SyncSender<Vec<u8>>, limit: usize) -> Self {
        Self {
            socket,
            sender,
            bytes: AtomicUsize::new(0),
            limit,
            closed: AtomicBool::new(false),
        }
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.socket.shutdown(Shutdown::Both);
    }
    pub fn send(&self, message: &JsValue) -> io::Result<()> {
        self.send_line(line(message)?)
    }
    pub fn send_line(&self, bytes: Vec<u8>) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TCP peer is closed.",
            ));
        }
        let size = bytes.len();
        let mut current = self.bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(size).filter(|next| *next <= self.limit) else {
                self.close();
                return Err(io::Error::other("TCP peer output byte limit exceeded."));
            };
            match self.bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        if self.sender.try_send(bytes).is_err() {
            self.bytes.fetch_sub(size, Ordering::AcqRel);
            self.close();
            return Err(io::Error::other("TCP peer output queue is full or closed."));
        }
        Ok(())
    }
    pub fn drive(self: &Arc<Self>, receiver: Receiver<Vec<u8>>, mut socket: TcpStream) {
        while !self.closed.load(Ordering::Acquire) {
            let bytes = match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(bytes) => bytes,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let result = socket.write_all(&bytes);
            self.bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
            if result.is_err() {
                break;
            }
        }
        self.close();
    }
}

pub(super) struct EngineInput {
    receiver: Receiver<Vec<u8>>,
    owner: crate::stdio::ConnectionOwner,
    current: Vec<u8>,
    position: usize,
}
impl EngineInput {
    pub fn new(receiver: Receiver<Vec<u8>>, owner: crate::stdio::ConnectionOwner) -> Self {
        Self {
            receiver,
            owner,
            current: Vec::new(),
            position: 0,
        }
    }
}
impl Read for EngineInput {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let current = self.fill_buf()?;
        let length = output.len().min(current.len());
        output[..length].copy_from_slice(&current[..length]);
        self.consume(length);
        Ok(length)
    }
}
impl BufRead for EngineInput {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.position == self.current.len() {
            self.current = loop {
                if self.owner.is_closed() {
                    break Vec::new();
                }
                match self
                    .receiver
                    .recv_timeout(std::time::Duration::from_millis(50))
                {
                    Ok(bytes) => break bytes,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Vec::new(),
                }
            };
            self.position = 0;
        }
        Ok(&self.current[self.position..])
    }
    fn consume(&mut self, amount: usize) {
        self.position = (self.position + amount).min(self.current.len());
    }
}

/// Reject before allocation grows beyond the configured frame bound. The
/// public NDJSON protocol stays unchanged; oversized peers lose their socket.
pub(super) fn read_frame(input: &mut impl BufRead, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return Ok((!frame.is_empty()).then_some(frame));
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let length = end.map_or(available.len(), |end| end + 1);
        if frame.len().saturating_add(length) > limit {
            return Err(io::Error::other("TCP module frame limit exceeded."));
        }
        frame.extend_from_slice(&available[..length]);
        input.consume(length);
        if end.is_some() {
            return Ok(Some(frame));
        }
    }
}
