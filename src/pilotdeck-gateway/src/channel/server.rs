//! The channel lifecycle portion of baseline `cli/pilotdeckServer.ts`.
//!
//! Operations run independently of their callers' result futures, like JS
//! promises. Each key has a FIFO driver; idle drivers and their map entries are
//! removed. Call from a Tokio runtime and close before shutting that runtime down.
use super::protocol::*;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot, watch};

struct Running {
    key: ChannelKey,
    adapter: Arc<dyn ChannelAdapter>,
    handle: Arc<dyn ChannelHandle>,
}
#[derive(Default)]
struct State {
    closing: bool,
    stop_prefixes: usize,
    running: Vec<Running>, // TS Map insertion order, including stop failure restoration.
}
enum Operation {
    Replace(Arc<dyn ChannelAdapter>),
    Disable,
}
struct FirstPoll(Option<oneshot::Sender<()>>);
impl Drop for FirstPoll {
    fn drop(&mut self) {
        if let Some(receipt) = self.0.take() {
            let _ = receipt.send(());
        }
    }
}
struct Request {
    first_poll: Option<FirstPoll>,
    operation: Operation,
    result: oneshot::Sender<Result<bool, ChannelError>>,
}
#[derive(Default)]
struct Reports {
    pending: VecDeque<ChannelStatus>,
    dispatching: bool,
}
#[cfg(test)]
type TransitionHook = Arc<dyn Fn(&str, &ChannelKey) + Send + Sync>;
struct Inner {
    state: Mutex<State>,
    prefix_changed: watch::Sender<()>,
    #[cfg(test)]
    hook: Mutex<Option<TransitionHook>>,
    queues: Mutex<HashMap<ChannelKey, mpsc::UnboundedSender<Request>>>,
    deps: ChannelStartDeps,
    reports: Mutex<Reports>,
    close_host: CloseHost,
    closed: watch::Sender<Option<Result<(), ChannelError>>>,
    startup_publication: Mutex<Option<watch::Sender<bool>>>,
}
#[derive(Clone)]
pub struct ChannelServer(Arc<Inner>);

struct StartupCancellation(Option<ChannelServer>);
impl Drop for StartupCancellation {
    fn drop(&mut self) {
        if let Some(server) = self.0.take() {
            server.publish_startup();
        }
    }
}

impl ChannelServer {
    /// Creates an unopened component. `start_channels` performs TS background startup.
    pub fn new(deps: ChannelStartDeps, close_host: CloseHost) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State::default()),
            prefix_changed: watch::Sender::new(()),
            #[cfg(test)]
            hook: Mutex::new(None),
            queues: Mutex::new(HashMap::new()),
            deps,
            reports: Mutex::new(Reports::default()),
            close_host,
            closed: watch::Sender::new(None),
            startup_publication: Mutex::new(None),
        }))
    }

    /// `deliverCronResult` (pilotdeckServer.ts:305-309): the running channel
    /// keyed by `originChannelKey ?? channelKey` takes the delivery; no such
    /// channel (or one without the optional method) is `false`.
    pub fn deliver_cron_result(
        &self,
        delivery: super::im_cron::CronResultDelivery,
    ) -> BoxFuture<bool> {
        let key = delivery
            .origin_channel_key
            .clone()
            .unwrap_or_else(|| delivery.channel_key.clone());
        let adapter = self
            .0
            .state
            .lock()
            .expect("channel state")
            .running
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.adapter.clone());
        match adapter {
            Some(adapter) => Box::pin(async move { adapter.deliver_cron_result(&delivery).await }),
            None => Box::pin(async { false }),
        }
    }

    pub fn start_channels(&self, channels: Vec<Arc<dyn ChannelAdapter>>) {
        let _ = self.launch_startup(channels, None, false);
    }

    /// Initial host startup only: wait until each unqueued adapter has actually
    /// been polled once, without waiting for a pending network start to finish.
    /// Completion logs are published explicitly after the embedding's ready log.
    pub fn start_channels_staged(
        &self,
        channels: Vec<Arc<dyn ChannelAdapter>>,
    ) -> BoxFuture<Result<(), ChannelError>> {
        let state = self.0.state.lock().expect("channel state");
        if state.closing {
            return Box::pin(async { Err(closing_error()) });
        }
        #[cfg(test)]
        self.0.transition("startup-checked", &ChannelKey::new(""));
        let publication = watch::Sender::new(false);
        let mut slot = self
            .0
            .startup_publication
            .lock()
            .expect("startup publication");
        if slot.is_some() {
            return Box::pin(async {
                Err(ChannelError::error(
                    "initial channel startup already staged",
                ))
            });
        }
        *slot = Some(publication.clone());
        drop(slot);
        drop(state);
        let (completed, wait) = oneshot::channel();
        let owner = self.clone();
        tokio::spawn(async move {
            let result = async {
                // JS initial operation microtasks invoke adapter prefixes in
                // input order. Only the first poll, never network completion,
                // separates the launches of distinct initial adapters.
                let mut initial_keys = HashSet::new();
                for channel in channels {
                    let key = channel.channel_key().clone();
                    let first = initial_keys.insert(key.clone());
                    #[cfg(test)]
                    owner.0.transition("startup-before-launch", &key);
                    for receipt in
                        owner.launch_startup(vec![channel], Some(publication.subscribe()), first)
                    {
                        receipt.await.map_err(|_| {
                            ChannelError::error("channel initial poll driver dropped")
                        })?;
                        #[cfg(test)]
                        owner.0.transition("startup-prefix-received", &key);
                    }
                }
                Ok(())
            }
            .await;
            let _ = completed.send(result);
        });
        let cancellation = StartupCancellation(Some(self.clone()));
        Box::pin(async move {
            let mut cancellation = cancellation;
            receive(wait).await?;
            // The host now owns the publication stage. Dropping a caller's
            // unpolled/pending waiter releases the gate but never cancels starts.
            cancellation.0 = None;
            Ok(())
        })
    }

    pub fn publish_startup(&self) {
        if let Some(publication) = self
            .0
            .startup_publication
            .lock()
            .expect("startup publication")
            .as_ref()
        {
            publication.send_replace(true);
        }
    }

    fn launch_startup(
        &self,
        channels: Vec<Arc<dyn ChannelAdapter>>,
        publication: Option<watch::Receiver<bool>>,
        wait_prefix: bool,
    ) -> Vec<oneshot::Receiver<()>> {
        let mut receipts = Vec::new();
        for channel in channels {
            let key = channel.channel_key().clone();
            let prefix = wait_prefix.then(|| {
                let (tx, rx) = oneshot::channel();
                receipts.push(rx);
                FirstPoll(Some(tx))
            });
            let operation =
                enqueue_with_prefix(&self.0, key.clone(), Operation::Replace(channel), prefix);
            let inner = self.0.clone();
            let mut publication = publication.clone();
            tokio::spawn(async move {
                let result = receive(operation).await;
                if let Some(gate) = publication.as_mut() {
                    #[cfg(test)]
                    inner.transition("startup-publication-wait", &key);
                    while !*gate.borrow_and_update() {
                        if gate.changed().await.is_err() {
                            break;
                        }
                    }
                }
                match result {
                    Ok(_) => inner.log(
                        "info",
                        &format!("[adapters] channel {key} startup task completed"),
                    ),
                    Err(error) => inner.log(
                        "error",
                        &format!("[adapters] channel {key} start failed: {}", error.message()),
                    ),
                }
            });
        }
        receipts
    }

    pub fn hot_start_channel(
        &self,
        channel: Arc<dyn ChannelAdapter>,
    ) -> BoxFuture<Result<(), ChannelError>> {
        if self.0.state.lock().expect("channel state").closing {
            return Box::pin(async { Err(closing_error()) });
        }
        let key = channel.channel_key().clone();
        let result = enqueue(&self.0, key, Operation::Replace(channel));
        Box::pin(async move { receive(result).await.map(|_| ()) })
    }

    pub fn reconcile_channels(
        &self,
        input: ReconcileInput,
    ) -> BoxFuture<Result<ReconcileResult, ChannelError>> {
        if self.0.state.lock().expect("channel state").closing {
            return Box::pin(async { Err(closing_error()) });
        }
        let mut desired = HashSet::new();
        for channel in &input.channels {
            if !desired.insert(channel.channel_key().clone()) {
                let error = ChannelError::error(format!(
                    "duplicate channel adapter in reconciliation: {}",
                    channel.channel_key()
                ));
                return Box::pin(async move { Err(error) });
            }
        }
        let mut managed = HashSet::new();
        let mut operations = Vec::new();
        for key in input.managed_channel_keys {
            if managed.insert(key.clone()) && !desired.contains(&key) {
                operations.push((key, Operation::Disable));
            }
        }
        for channel in input.channels {
            operations.push((channel.channel_key().clone(), Operation::Replace(channel)));
        }
        let inner = self.0.clone();
        let mut operations = operations.into_iter();
        // JS async functions execute through their first await before returning.
        let first = operations.next().map(|(key, operation)| {
            let replacing = matches!(operation, Operation::Replace(_));
            let result = enqueue(&inner, key.clone(), operation);
            (key, replacing, result)
        });
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut outcome = ReconcileResult::default();
            let mut next = first;
            let result = async {
                while let Some((key, replacing, result)) = next {
                    let changed = receive(result).await?;
                    if replacing {
                        outcome.started.push(key);
                    } else if changed {
                        outcome.stopped.push(key);
                    }
                    next = operations.next().map(|(key, operation)| {
                        let replacing = matches!(operation, Operation::Replace(_));
                        let result = enqueue(&inner, key.clone(), operation);
                        (key, replacing, result)
                    });
                }
                Ok(outcome)
            }
            .await;
            let _ = tx.send(result);
        });
        Box::pin(receive(rx))
    }

    /// Eager and idempotent. Claims handles before invoking stop, so concurrent
    /// replacement and shutdown cannot stop the same handle twice. Like TS,
    /// close does not wait for a start that has not yet produced a live handle;
    /// that start stops its own late handle and rejects when it eventually resolves.
    pub fn close(&self) -> BoxFuture<Result<(), ChannelError>> {
        #[cfg(test)]
        self.0.transition("close-entered", &ChannelKey::new(""));
        let claimed = {
            let mut state = self.0.state.lock().expect("channel state");
            if state.closing {
                return Box::pin(wait_closed(self.0.clone()));
            }
            state.closing = true;
            std::mem::take(&mut state.running)
        };
        self.publish_startup();
        let mut stops = Vec::new();
        for entry in claimed {
            let stop = begin_stop(entry.handle.clone(), "server-shutdown");
            let inner = self.0.clone();
            stops.push(tokio::spawn(async move {
                finish_stop(&inner, entry, stop, "server-shutdown").await
            }));
        }
        let inner = self.0.clone();
        tokio::spawn(async move {
            let mut errors = Vec::new();
            // Futures are already running concurrently; collect errors in Map order.
            for stop in stops {
                match stop.await {
                    Ok(Ok(_)) => (),
                    Ok(Err(error)) => errors.push(error),
                    Err(error) => errors.push(ChannelError::error(format!(
                        "channel stop task failed: {error}"
                    ))),
                }
            }
            wait_stop_prefixes(&inner).await;
            let close_host = inner.close_host.clone();
            if let Err(error) = task_result(
                tokio::spawn(async move { close_host().await }),
                "host close",
            )
            .await
            {
                errors.push(error);
            }
            let result = if errors.is_empty() {
                Ok(())
            } else {
                Err(ChannelError::Aggregate {
                    message: "PilotDeck server shutdown failed".into(),
                    errors,
                })
            };
            inner.closed.send_replace(Some(result));
        });
        Box::pin(wait_closed(self.0.clone()))
    }
}

impl Inner {
    fn log(&self, level: &str, text: &str) {
        if let Some(log) = &self.deps.log {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log(level, text)));
        }
    }
    // Queue under the state lock at each linearization point. Callbacks run
    // outside both locks, in FIFO order, and may reenter the lifecycle API.
    fn queue_report(
        &self,
        key: &ChannelKey,
        state: ChannelState,
        message: String,
        error: Option<String>,
    ) {
        self.reports
            .lock()
            .expect("channel reports")
            .pending
            .push_back(ChannelStatus {
                key: key.clone(),
                state,
                message,
                error,
            });
    }
    fn flush_reports(&self) {
        {
            let mut reports = self.reports.lock().expect("channel reports");
            if reports.dispatching {
                return;
            }
            reports.dispatching = true;
        }
        loop {
            let next = {
                let mut reports = self.reports.lock().expect("channel reports");
                match reports.pending.pop_front() {
                    Some(next) => next,
                    None => {
                        reports.dispatching = false;
                        return;
                    }
                }
            };
            if let Some(report) = &self.deps.report_status {
                let key = next.key.clone();
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| report(next))).is_err()
                {
                    self.log(
                        "warn",
                        &format!(
                            "[adapters] failed to write runtime status for {key}: reporter panicked"
                        ),
                    );
                }
            }
        }
    }
    fn report(
        &self,
        key: &ChannelKey,
        state: ChannelState,
        message: String,
        error: Option<String>,
    ) {
        self.queue_report(key, state, message, error);
        self.flush_reports();
    }
}
async fn task_result<T>(
    task: tokio::task::JoinHandle<Result<T, ChannelError>>,
    operation: &str,
) -> Result<T, ChannelError> {
    task.await.unwrap_or_else(|_| {
        Err(ChannelError::error(format!(
            "channel {operation} task panicked"
        )))
    })
}
fn begin_stop(handle: Arc<dyn ChannelHandle>, reason: &str) -> BoxFuture<Result<(), ChannelError>> {
    // stop's synchronous prefix closes ingress immediately. Isolate both that
    // prefix and its future so a Rust adapter panic cannot strand close waiters.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.stop(reason))) {
        Ok(future) => Box::pin(task_result(tokio::spawn(future), "stop")),
        Err(_) => Box::pin(async { Err(ChannelError::error("channel stop task panicked")) }),
    }
}

fn closing_error() -> ChannelError {
    ChannelError::error("channel server is closing")
}
async fn receive<T>(rx: oneshot::Receiver<Result<T, ChannelError>>) -> Result<T, ChannelError> {
    rx.await
        .unwrap_or_else(|_| Err(ChannelError::error("channel operation driver dropped")))
}
async fn wait_closed(inner: Arc<Inner>) -> Result<(), ChannelError> {
    let mut rx = inner.closed.subscribe();
    loop {
        if let Some(result) = rx.borrow_and_update().clone() {
            return result;
        }
        rx.changed()
            .await
            .map_err(|_| ChannelError::error("channel close driver dropped"))?;
    }
}
fn enqueue(
    inner: &Arc<Inner>,
    key: ChannelKey,
    operation: Operation,
) -> oneshot::Receiver<Result<bool, ChannelError>> {
    enqueue_with_prefix(inner, key, operation, None)
}
fn enqueue_with_prefix(
    inner: &Arc<Inner>,
    key: ChannelKey,
    operation: Operation,
    mut first_poll: Option<FirstPoll>,
) -> oneshot::Receiver<Result<bool, ChannelError>> {
    let (result, rx) = oneshot::channel();
    let mut queues = inner.queues.lock().expect("channel queues");
    if queues.get(&key).is_some_and(|sender| sender.is_closed()) {
        queues.remove(&key);
    }
    if queues.contains_key(&key) {
        // A later same-key operation cannot execute before its predecessor;
        // JS startup does not wait for that predecessor's pending network I/O.
        drop(first_poll.take());
    }
    let tx = queues.entry(key.clone()).or_insert_with(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(drive(inner.clone(), key, rx));
        tx
    });
    // The driver removes the sender under this same mutex before exiting.
    let _ = tx.send(Request {
        operation,
        result,
        first_poll,
    });
    rx
}
async fn drive(inner: Arc<Inner>, key: ChannelKey, mut rx: mpsc::UnboundedReceiver<Request>) {
    loop {
        let request = {
            let mut queues = inner.queues.lock().expect("channel queues");
            match rx.try_recv() {
                Ok(request) => request,
                Err(_) => {
                    queues.remove(&key);
                    #[cfg(test)]
                    inner.transition("driver-idle", &key);
                    return;
                }
            }
        };
        let result = match request.operation {
            Operation::Replace(channel) => replace(&inner, &key, channel, request.first_poll)
                .await
                .map(|_| true),
            Operation::Disable => stop(&inner, &key, "config-disabled").await,
        };
        let _ = request.result.send(result);
    }
}
async fn start(
    inner: &Arc<Inner>,
    key: &ChannelKey,
    adapter: Arc<dyn ChannelAdapter>,
    first_poll: Option<FirstPoll>,
) -> Result<(), ChannelError> {
    {
        let state = inner.state.lock().expect("channel state");
        if state.closing {
            return Err(closing_error());
        }
        inner.queue_report(
            key,
            ChannelState::Starting,
            format!("{key}: starting in background"),
            None,
        );
    }
    inner.flush_reports();
    let starter = adapter.clone();
    let deps = inner.deps.clone();
    let handle = task_result(
        tokio::spawn(async move {
            let mut first_poll = first_poll;
            let mut future = Box::pin(async move { starter.start(deps).await });
            std::future::poll_fn(move |cx| {
                let result = future.as_mut().poll(cx);
                drop(first_poll.take());
                result
            })
            .await
        }),
        "start",
    )
    .await?;
    let closing = {
        // Keep the JS synchronous check-and-register region atomic on all runtimes.
        let mut state = inner.state.lock().expect("channel state");
        if state.closing {
            true
        } else {
            state.running.push(Running {
                key: key.clone(),
                adapter,
                handle: handle.clone(),
            });
            inner.queue_report(
                key,
                ChannelState::Connected,
                format!("{key}: started"),
                None,
            );
            false
        }
    };
    if closing {
        begin_stop(handle, "server-shutdown").await?;
        inner.report(
            key,
            ChannelState::Stopped,
            format!("{key}: startup completed after server shutdown"),
            None,
        );
        return Err(closing_error());
    }
    inner.flush_reports();
    Ok(())
}
async fn stop(inner: &Arc<Inner>, key: &ChannelKey, reason: &str) -> Result<bool, ChannelError> {
    let entry = {
        let mut state = inner.state.lock().expect("channel state");
        let Some(index) = state.running.iter().position(|entry| entry.key == *key) else {
            return Ok(false);
        };
        state.stop_prefixes += 1;
        state.running.remove(index)
    };
    stop_claimed(inner, entry, reason).await
}
async fn stop_claimed(
    inner: &Arc<Inner>,
    entry: Running,
    reason: &str,
) -> Result<bool, ChannelError> {
    // Claim and prefix reservation are atomic. Keep external stop callbacks out
    // of the state lock: a synchronous stop may itself reenter close().
    let prefix = StopPrefix(inner.clone());
    #[cfg(test)]
    inner.transition("stop-claimed", &entry.key);
    let future = begin_stop(entry.handle.clone(), reason);
    drop(prefix);
    finish_stop(inner, entry, future, reason).await
}
async fn finish_stop(
    inner: &Arc<Inner>,
    entry: Running,
    future: BoxFuture<Result<(), ChannelError>>,
    reason: &str,
) -> Result<bool, ChannelError> {
    let key = entry.key.clone();
    match future.await {
        Ok(()) => {
            inner.report(
                &key,
                ChannelState::Stopped,
                format!("{key}: stopped ({reason})"),
                None,
            );
            Ok(true)
        }
        Err(error) => {
            {
                let mut state = inner.state.lock().expect("channel state");
                // #1353 A8.5-pr2 (A10): a stop that fails while the server is
                // closing no longer restores the handle. During a switch the
                // closing gate has already rejected every later start, so a
                // restored handle would re-enter `running` with nothing able
                // to stop it again — the flush that follows must not observe
                // a live entry. Outside a close (a standalone stop), the TS
                // restore behavior is kept.
                if !state.closing && !state.running.iter().any(|item| item.key == key) {
                    state.running.push(entry);
                }
                inner.queue_report(
                    &key,
                    ChannelState::Failed,
                    format!("{key}: stop failed"),
                    Some(error.message().into()),
                );
            }
            inner.flush_reports();
            Err(error)
        }
    }
}
async fn replace(
    inner: &Arc<Inner>,
    key: &ChannelKey,
    channel: Arc<dyn ChannelAdapter>,
    first_poll: Option<FirstPoll>,
) -> Result<(), ChannelError> {
    let (previous, claimed) = {
        let mut state = inner.state.lock().expect("channel state");
        if state.closing {
            return Err(closing_error());
        }
        #[cfg(test)]
        inner.transition("replace-checked", key);
        let previous = state
            .running
            .iter()
            .find(|entry| entry.key == *key)
            .map(|entry| entry.adapter.clone());
        #[cfg(test)]
        inner.transition("replace-snapshotted", key);
        // TS snapshots the adapter and deletes the running handle before its
        // first await. Close must not claim that handle between these steps.
        let claimed = state
            .running
            .iter()
            .position(|entry| entry.key == *key)
            .map(|index| {
                state.stop_prefixes += 1;
                state.running.remove(index)
            });
        (previous, claimed)
    };
    if let Some(entry) = claimed {
        stop_claimed(inner, entry, "hot-reload").await?;
    }
    match start(inner, key, channel, first_poll).await {
        Ok(()) => Ok(()),
        Err(error) => {
            inner.report(
                key,
                ChannelState::Failed,
                format!("{key}: startup failed"),
                Some(error.message().into()),
            );
            if let Some(previous) = previous {
                if let Err(restore_error) = start(inner, key, previous, None).await {
                    inner.report(
                        key,
                        ChannelState::Failed,
                        format!("{key}: rollback failed"),
                        Some(restore_error.message().into()),
                    );
                    return Err(ChannelError::Aggregate {
                        message: format!("channel {key} replacement and rollback failed"),
                        errors: vec![error, restore_error],
                    });
                }
                inner.log(
                    "warn",
                    &format!(
                        "[adapters] channel {key} replacement failed; restored previous adapter"
                    ),
                );
            }
            Err(error)
        }
    }
}

// Only the synchronous prefix is reserved; an already claimed handle's async
// tail is deliberately outside close's drain set, as in the TS Map contract.
struct StopPrefix(Arc<Inner>);
impl Drop for StopPrefix {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().expect("channel state");
        state.stop_prefixes -= 1;
        self.0.prefix_changed.send_replace(());
    }
}
async fn wait_stop_prefixes(inner: &Arc<Inner>) {
    let mut changed = inner.prefix_changed.subscribe();
    while inner.state.lock().expect("channel state").stop_prefixes != 0 {
        #[cfg(test)]
        inner.transition("close-prefix-wait", &ChannelKey::new(""));
        changed
            .changed()
            .await
            .expect("channel prefix sender retained");
    }
}
#[cfg(test)]
impl Inner {
    fn transition(&self, point: &str, key: &ChannelKey) {
        let hook = self
            .hook
            .lock()
            .expect("#1327 transition hook mutex")
            .clone();
        if let Some(hook) = hook {
            hook(point, key);
        }
    }
}
#[cfg(test)]
impl ChannelServer {
    pub(super) fn set_hook(&self, hook: Option<TransitionHook>) {
        *self.0.hook.lock().expect("#1327 transition hook mutex") = hook;
    }
    pub(super) fn state_available(&self) -> bool {
        self.0.state.try_lock().is_ok()
    }
    /// #1353 A8.5-pr2 (A10): the keys currently in `running` — the only
    /// observable of the no-restore rule for a stop that fails while the
    /// server is closing (every public lifecycle entry is gated by then).
    pub(super) fn running_keys(&self) -> Vec<String> {
        self.0
            .state
            .lock()
            .expect("channel state")
            .running
            .iter()
            .map(|entry| entry.key.to_string())
            .collect()
    }
}

#[cfg(test)]
mod startup_stage_tests {
    use super::*;
    use std::time::Duration;

    struct Probe {
        key: ChannelKey,
        id: &'static str,
        events: mpsc::UnboundedSender<String>,
        held: Option<watch::Receiver<bool>>,
        panic: bool,
        stop_held: Option<watch::Receiver<bool>>,
    }
    struct Handle(
        &'static str,
        mpsc::UnboundedSender<String>,
        Option<watch::Receiver<bool>>,
    );
    impl ChannelHandle for Handle {
        fn stop(&self, reason: &str) -> BoxFuture<Result<(), ChannelError>> {
            self.1.send(format!("stop:{}:{reason}", self.0)).unwrap();
            let mut held = self.2.clone();
            Box::pin(async move {
                if let Some(gate) = held.as_mut() {
                    while !*gate.borrow_and_update() {
                        gate.changed().await.unwrap();
                    }
                }
                Ok(())
            })
        }
    }
    impl ChannelAdapter for Probe {
        fn channel_key(&self) -> &ChannelKey {
            &self.key
        }
        fn start(
            &self,
            _deps: ChannelStartDeps,
        ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
            self.events.send(format!("construct:{}", self.id)).unwrap();
            let (id, events, mut held, panic) =
                (self.id, self.events.clone(), self.held.clone(), self.panic);
            let stop_held = self.stop_held.clone();
            Box::pin(async move {
                events.send(format!("poll:{id}")).unwrap();
                assert!(!panic, "controlled initial poll panic");
                if let Some(gate) = held.as_mut() {
                    while !*gate.borrow_and_update() {
                        gate.changed().await.unwrap();
                    }
                }
                Ok(Arc::new(Handle(id, events, stop_held)) as Arc<dyn ChannelHandle>)
            })
        }
    }
    fn fixture() -> (
        ChannelServer,
        mpsc::UnboundedSender<String>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (events, rx) = mpsc::unbounded_channel();
        let logs = events.clone();
        let deps = ChannelStartDeps {
            log: Some(Arc::new(move |_, text| {
                logs.send(format!("log:{text}")).unwrap();
            })),
            ..Default::default()
        };
        let server = ChannelServer::new(deps, Arc::new(|| Box::pin(async { Ok(()) })));
        let waits = events.clone();
        server.set_hook(Some(Arc::new(move |point, key| {
            if point == "startup-publication-wait" {
                waits.send(format!("publication-wait:{key}")).unwrap();
            }
        })));
        (server, events, rx)
    }
    fn probe(
        id: &'static str,
        key: &str,
        events: &mpsc::UnboundedSender<String>,
        held: Option<watch::Receiver<bool>>,
        panic: bool,
    ) -> Arc<dyn ChannelAdapter> {
        Arc::new(Probe {
            key: ChannelKey::new(key),
            id,
            events: events.clone(),
            held,
            panic,
            stop_held: None,
        })
    }
    async fn next(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("causal event deadline")
            .expect("event source live")
    }
    async fn through(rx: &mut mpsc::UnboundedReceiver<String>, expected: &str) -> Vec<String> {
        let mut seen = Vec::new();
        loop {
            let event = next(rx).await;
            let matched = event == expected;
            seen.push(event);
            if matched {
                return seen;
            }
        }
    }

    #[tokio::test]
    async fn ready_adapter_prefix_precedes_host_ready_but_completion_waits_publication() {
        let (server, events, mut rx) = fixture();
        server
            .start_channels_staged(vec![probe("ready", "ready", &events, None, false)])
            .await
            .unwrap();
        let before = through(&mut rx, "publication-wait:ready").await;
        assert!(before.contains(&"construct:ready".to_string()));
        assert!(before.contains(&"poll:ready".to_string()));
        assert!(!before.iter().any(|event| event.starts_with("log:")));
        assert!(rx.try_recv().is_err());
        events.send("host-ready".into()).unwrap();
        server.publish_startup();
        assert_eq!(next(&mut rx).await, "host-ready");
        assert_eq!(
            next(&mut rx).await,
            "log:[adapters] channel ready startup task completed"
        );
        server.close().await.unwrap();
        assert_eq!(next(&mut rx).await, "stop:ready:server-shutdown");
    }

    #[tokio::test]
    async fn pending_network_start_does_not_hold_ready_or_close_and_stops_its_late_handle() {
        let (server, events, mut rx) = fixture();
        let (release, held) = watch::channel(false);
        server
            .start_channels_staged(vec![probe("held", "held", &events, Some(held), false)])
            .await
            .unwrap();
        assert_eq!(next(&mut rx).await, "construct:held");
        assert_eq!(next(&mut rx).await, "poll:held");
        assert!(rx.try_recv().is_err());
        // close must finish while the adapter start is still held.
        tokio::time::timeout(Duration::from_secs(2), server.close())
            .await
            .unwrap()
            .unwrap();
        release.send_replace(true);
        let seen = through(
            &mut rx,
            "log:[adapters] channel held start failed: channel server is closing",
        )
        .await;
        assert_eq!(
            seen.iter()
                .filter(|event| event.as_str() == "stop:held:server-shutdown")
                .count(),
            1
        );
        assert!(server.running_keys().is_empty());
    }

    #[tokio::test]
    async fn dropped_unpolled_waiter_keeps_owned_start_and_releases_completion_gate() {
        let (server, events, mut rx) = fixture();
        let wait =
            server.start_channels_staged(vec![probe("owned", "owned", &events, None, false)]);
        drop(wait);
        let seen = through(
            &mut rx,
            "log:[adapters] channel owned startup task completed",
        )
        .await;
        assert!(seen.contains(&"poll:owned".into()));
        server.close().await.unwrap();
        assert_eq!(next(&mut rx).await, "stop:owned:server-shutdown");
    }

    #[tokio::test]
    async fn duplicate_key_does_not_wait_for_pending_predecessor_or_start_after_close() {
        let (server, events, mut rx) = fixture();
        let (release, held) = watch::channel(false);
        server
            .start_channels_staged(vec![
                probe("first", "same", &events, Some(held), false),
                probe("second", "same", &events, None, false),
            ])
            .await
            .unwrap();
        assert_eq!(next(&mut rx).await, "construct:first");
        assert_eq!(next(&mut rx).await, "poll:first");
        server.close().await.unwrap();
        release.send_replace(true);
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(next(&mut rx).await);
        }
        assert_eq!(
            seen.iter()
                .filter(|event| event.starts_with("log:"))
                .count(),
            2
        );
        assert_eq!(
            seen.iter()
                .filter(|event| event.as_str() == "stop:first:server-shutdown")
                .count(),
            1
        );
        assert!(
            !seen
                .iter()
                .any(|event| event == "construct:second" || event == "poll:second")
        );
    }

    #[tokio::test]
    async fn initial_poll_panic_releases_receipt_and_reports_failure_after_publication() {
        let (server, events, mut rx) = fixture();
        server
            .start_channels_staged(vec![probe("panic", "panic", &events, None, true)])
            .await
            .unwrap();
        let before = through(&mut rx, "publication-wait:panic").await;
        assert!(before.contains(&"poll:panic".into()));
        assert!(!before.iter().any(|event| event.starts_with("log:")));
        server.publish_startup();
        assert_eq!(
            next(&mut rx).await,
            "log:[adapters] channel panic start failed: channel start task panicked"
        );
        server.close().await.unwrap();
    }
    #[tokio::test]
    async fn replacement_remains_fifo_and_stops_each_exact_handle_once() {
        let (server, events, mut rx) = fixture();
        let (release, held) = watch::channel(false);
        server
            .start_channels_staged(vec![probe("old", "same", &events, Some(held), false)])
            .await
            .unwrap();
        assert_eq!(next(&mut rx).await, "construct:old");
        assert_eq!(next(&mut rx).await, "poll:old");
        let replacing = server.hot_start_channel(probe("new", "same", &events, None, false));
        release.send_replace(true);
        replacing.await.unwrap();
        let seen = through(&mut rx, "poll:new").await;
        let stopped = seen
            .iter()
            .position(|event| event == "stop:old:hot-reload")
            .unwrap();
        let started = seen
            .iter()
            .position(|event| event == "construct:new")
            .unwrap();
        assert!(stopped < started);
        assert_eq!(
            seen.iter()
                .filter(|event| event.as_str() == "stop:old:hot-reload")
                .count(),
            1
        );
        server.publish_startup();
        server.close().await.unwrap();
        let mut final_events = through(&mut rx, "stop:new:server-shutdown").await;
        let completion = "log:[adapters] channel same startup task completed";
        if !final_events.iter().any(|event| event == completion) {
            final_events.extend(through(&mut rx, completion).await);
        }
        assert!(
            !final_events
                .iter()
                .any(|event| event.starts_with("stop:old:"))
        );
    }

    #[tokio::test]
    async fn distinct_initial_prefixes_follow_input_order_without_waiting_pending_network() {
        let (server, events, mut rx) = fixture();
        let (release, held) = watch::channel(false);
        server
            .start_channels_staged(vec![
                probe("first", "one", &events, Some(held), false),
                probe("second", "two", &events, None, false),
            ])
            .await
            .unwrap();
        let before = through(&mut rx, "publication-wait:two").await;
        let prefixes: Vec<_> = before
            .iter()
            .filter(|event| event.starts_with("construct:") || event.starts_with("poll:"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            prefixes,
            [
                "construct:first",
                "poll:first",
                "construct:second",
                "poll:second"
            ]
        );
        assert!(!before.iter().any(|event| event.starts_with("log:")));
        server.close().await.unwrap();
        release.send_replace(true);
        let after = through(
            &mut rx,
            "log:[adapters] channel one start failed: channel server is closing",
        )
        .await;
        assert_eq!(
            after
                .iter()
                .filter(|event| event.as_str() == "stop:first:server-shutdown")
                .count(),
            1
        );
        let completion = "log:[adapters] channel two startup task completed";
        if !after.iter().any(|event| event == completion) {
            through(&mut rx, completion).await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publication_install_and_close_share_one_serial_boundary() {
        let (server, events, mut rx) = fixture();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocked = Mutex::new(blocked);
        let (launch_release, launch_blocked) = std::sync::mpsc::channel();
        let launch_blocked = Mutex::new(launch_blocked);
        let transitions = events.clone();
        server.set_hook(Some(Arc::new(move |point, _| {
            if point == "startup-checked" {
                transitions.send("checked".into()).unwrap();
                blocked.lock().unwrap().recv().unwrap();
            } else if point == "close-entered" {
                transitions.send("close-entered".into()).unwrap();
            } else if point == "startup-before-launch" {
                launch_blocked.lock().unwrap().recv().unwrap();
            }
        })));
        let runtime = tokio::runtime::Handle::current();
        let staging_server = server.clone();
        let staged_events = events.clone();
        let staging = std::thread::spawn(move || {
            let _entered = runtime.enter();
            staging_server.start_channels_staged(vec![probe(
                "late",
                "late",
                &staged_events,
                None,
                false,
            )])
        });
        assert_eq!(next(&mut rx).await, "checked");
        // A try_lock assertion, not scheduling delay, proves that close cannot
        // pass the check/install region exposed by the regression hook.
        assert!(server.0.state.try_lock().is_err());
        let runtime = tokio::runtime::Handle::current();
        let closing_server = server.clone();
        let closing = std::thread::spawn(move || {
            let _entered = runtime.enter();
            closing_server.close()
        });
        assert_eq!(next(&mut rx).await, "close-entered");
        release.send(()).unwrap();
        let staged = staging.join().unwrap();
        let closed = closing.join().unwrap();
        closed.await.unwrap();
        launch_release.send(()).unwrap();
        assert!(
            *server
                .0
                .startup_publication
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .borrow()
        );
        staged.await.unwrap();
        let failures = through(
            &mut rx,
            "log:[adapters] channel late start failed: channel server is closing",
        )
        .await;
        assert!(!failures.iter().any(|event| event == "construct:late"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeated_initial_key_does_not_wait_for_retired_queue_and_held_stop() {
        let (server, events, mut rx) = fixture();
        let (resume, paused) = std::sync::mpsc::channel();
        let paused = Mutex::new(paused);
        let once = std::sync::atomic::AtomicBool::new(false);
        let transitions = events.clone();
        server.set_hook(Some(Arc::new(move |point, key| {
            if point == "startup-prefix-received"
                && !once.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                transitions.send("first-prefix-received".into()).unwrap();
                paused.lock().unwrap().recv().unwrap();
            } else if point == "driver-idle" {
                let _ = transitions.send("queue-retired".into());
            } else if point == "startup-publication-wait" {
                transitions.send(format!("publication-wait:{key}")).unwrap();
            }
        })));
        let (stop_release, stop_held) = watch::channel(false);
        let first = Arc::new(Probe {
            key: ChannelKey::new("same"),
            id: "first",
            events: events.clone(),
            held: None,
            panic: false,
            stop_held: Some(stop_held),
        });
        let staged = server
            .start_channels_staged(vec![first, probe("second", "same", &events, None, false)]);
        let before = through(&mut rx, "queue-retired").await;
        // The first adapter has returned its handle and the per-key driver
        // entry is gone before the second same-key request can be enqueued.
        assert!(before.contains(&"poll:first".into()));
        resume.send(()).unwrap();
        staged.await.unwrap();
        let stopping = through(&mut rx, "stop:first:hot-reload").await;
        assert!(!stopping.iter().any(|event| event == "construct:second"));
        // Ready was obtained with the old exact handle's stop still pending.
        server.publish_startup();
        stop_release.send_replace(true);
        let mut completions = before
            .iter()
            .chain(stopping.iter())
            .filter(|event| event.starts_with("log:"))
            .count();
        while completions < 2 {
            let event = next(&mut rx).await;
            if event.starts_with("log:") {
                completions += 1;
            }
        }
        server.close().await.unwrap();
        through(&mut rx, "stop:second:server-shutdown").await;
    }
}
