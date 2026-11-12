use super::{BoxFuture, TelemetryClient, TelemetryErrorValue, TelemetryFailure, lock};
use futures_util::FutureExt;
use pilotdeck_protocol::{JsString, JsValue};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, OnceLock, Weak},
};
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub enum TelemetryObservation {
    Track {
        event_name: JsString,
        properties: JsValue,
        context: JsValue,
    },
    FeatureUsed(JsValue),
    FeatureLoopStage(JsValue),
    Error {
        error: TelemetryErrorValue,
        input: JsValue,
    },
}
/// A synchronous observer and an async observer have distinct drain timing.
pub enum TelemetryObserverResult {
    Ready(Result<(), TelemetryFailure>),
    Pending(BoxFuture<Result<(), TelemetryFailure>>),
}
pub trait TelemetryObserver: Send + Sync {
    fn observe(&self, observation: TelemetryObservation) -> TelemetryObserverResult;
    fn dispose(&self) -> BoxFuture<Result<(), TelemetryFailure>> {
        Box::pin(async { Ok(()) })
    }
}
pub type ObserverFailureHandler =
    Arc<dyn Fn(&JsString, u64, &TelemetryObservation, &TelemetryFailure) + Send + Sync>;
pub type ObserverDisposer = Arc<
    dyn Fn(Arc<dyn TelemetryObserver>) -> BoxFuture<Result<(), TelemetryFailure>> + Send + Sync,
>;
type Receipt = watch::Receiver<Option<Result<(), TelemetryFailure>>>;
struct RegistryState {
    phase: &'static str,
    next: u64,
    registrations: Vec<TelemetryRegistration>,
    dispose: Option<Receipt>,
}
struct RegistryInner {
    executor: Arc<OnceLock<tokio::runtime::Handle>>,
    state: Mutex<RegistryState>,
    failure: Option<ObserverFailureHandler>,
}
#[derive(Clone)]
pub struct TelemetryObserverRegistry {
    inner: Arc<RegistryInner>,
}
struct RegistrationState {
    phase: &'static str,
    inflight: usize,
    drain: Option<watch::Sender<bool>>,
    dispose: Option<Receipt>,
    started: Option<watch::Receiver<bool>>,
}
struct RegistrationInner {
    executor: Arc<OnceLock<tokio::runtime::Handle>>,
    name: JsString,
    generation: u64,
    observer: Arc<dyn TelemetryObserver>,
    disposer: Option<ObserverDisposer>,
    registry: Weak<RegistryInner>,
    state: Mutex<RegistrationState>,
    count: watch::Sender<usize>,
}
#[derive(Clone)]
pub struct TelemetryRegistration {
    inner: Arc<RegistrationInner>,
}
pub struct TelemetryReplacement {
    pub registration: TelemetryRegistration,
    pub previous_disposed: BoxFuture<Result<(), TelemetryFailure>>,
}
impl Default for TelemetryObserverRegistry {
    fn default() -> Self {
        Self::new(None)
    }
}
impl TelemetryObserverRegistry {
    pub fn new(failure: Option<ObserverFailureHandler>) -> Self {
        let executor = Arc::new(OnceLock::new());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _ = executor.set(handle);
        }
        Self {
            inner: Arc::new(RegistryInner {
                executor,
                state: Mutex::new(RegistryState {
                    phase: "active",
                    next: 0,
                    registrations: Vec::new(),
                    dispose: None,
                }),
                failure,
            }),
        }
    }
    pub fn state(&self) -> &'static str {
        lock(&self.inner.state).phase
    }
    pub fn size(&self) -> usize {
        lock(&self.inner.state).registrations.len()
    }
    pub fn register(
        &self,
        name: &JsString,
        observer: Arc<dyn TelemetryObserver>,
        disposer: Option<ObserverDisposer>,
    ) -> Result<TelemetryRegistration, TelemetryFailure> {
        let mut state = lock(&self.inner.state);
        active(&state, "register a telemetry observer")?;
        let name = normalize_name(name)?;
        if state.registrations.iter().any(|r| r.inner.name == name) {
            return Err(TelemetryFailure(format!(
                "Telemetry observer is already registered: {}",
                String::from_utf16_lossy(name.units())
            )));
        }
        let registration = self.create(&mut state, name, observer, disposer);
        state.registrations.push(registration.clone());
        Ok(registration)
    }
    pub fn replace(
        &self,
        name: &JsString,
        observer: Arc<dyn TelemetryObserver>,
        disposer: Option<ObserverDisposer>,
    ) -> Result<TelemetryReplacement, TelemetryFailure> {
        let mut state = lock(&self.inner.state);
        active(&state, "replace a telemetry observer")?;
        let name = normalize_name(name)?;
        let Some(index) = state
            .registrations
            .iter()
            .position(|r| r.inner.name == name)
        else {
            return Err(missing(&name));
        };
        let registration = self.create(&mut state, name, observer, disposer);
        let previous = std::mem::replace(&mut state.registrations[index], registration.clone());
        drop(state);
        Ok(TelemetryReplacement {
            registration,
            previous_disposed: previous.dispose(),
        })
    }
    pub fn unregister(
        &self,
        name: &JsString,
    ) -> Result<BoxFuture<Result<(), TelemetryFailure>>, TelemetryFailure> {
        let mut state = lock(&self.inner.state);
        active(&state, "unregister a telemetry observer")?;
        let name = normalize_name(name)?;
        let Some(index) = state
            .registrations
            .iter()
            .position(|r| r.inner.name == name)
        else {
            return Err(missing(&name));
        };
        let previous = state.registrations.remove(index);
        drop(state);
        Ok(previous.dispose())
    }
    fn create(
        &self,
        state: &mut RegistryState,
        name: JsString,
        observer: Arc<dyn TelemetryObserver>,
        disposer: Option<ObserverDisposer>,
    ) -> TelemetryRegistration {
        state.next += 1;
        TelemetryRegistration {
            inner: Arc::new(RegistrationInner {
                executor: self.inner.executor.clone(),
                name,
                generation: state.next,
                observer,
                disposer,
                registry: Arc::downgrade(&self.inner),
                state: Mutex::new(RegistrationState {
                    phase: "active",
                    inflight: 0,
                    drain: None,
                    dispose: None,
                    started: None,
                }),
                count: watch::channel(0).0,
            }),
        }
    }
    pub fn observe(&self, observation: TelemetryObservation) {
        let registrations = {
            let state = lock(&self.inner.state);
            if state.phase != "active" {
                return;
            }
            state.registrations.clone()
        };
        let (dispatch, dispatched) = watch::channel(1usize);
        for registration in registrations {
            registration.observe(observation.clone(), dispatched.clone());
        }
        dispatch.send_replace(0);
    }
    pub fn dispose(&self) -> BoxFuture<Result<(), TelemetryFailure>> {
        let mut state = lock(&self.inner.state);
        if let Some(receipt) = &state.dispose {
            return Box::pin(wait(receipt.clone()));
        }
        state.phase = "draining";
        let registrations: Vec<_> = state.registrations.iter().rev().cloned().collect();
        let (done, receipt) = watch::channel(None);
        state.dispose = Some(receipt.clone());
        drop(state);
        for registration in &registrations {
            registration.stop();
        }
        let mut previous_ready = None;
        let mut futures = Vec::new();
        for registration in registrations {
            let ready = registration.in_flight() == 0;
            let (future, started) =
                registration.dispose_ordered(if ready { previous_ready.clone() } else { None });
            if ready {
                previous_ready = Some(started);
            }
            futures.push(future);
        }
        let inner = self.inner.clone();
        let completion = Completion(Some(done));
        executor(&self.inner.executor).spawn(async move {
            let mut completion = completion;
            let results = futures_util::future::join_all(futures).await;
            let failures: Vec<_> = results.into_iter().filter_map(Result::err).collect();
            {
                let mut state = lock(&inner.state);
                state.registrations.clear();
                state.phase = "disposed";
            }
            completion.finish(aggregate(
                failures,
                "Failed to dispose telemetry observers.",
            ));
        });
        Box::pin(wait(receipt))
    }
}
impl TelemetryRegistration {
    pub fn name(&self) -> &JsString {
        &self.inner.name
    }
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }
    pub fn state(&self) -> &'static str {
        lock(&self.inner.state).phase
    }
    pub fn active(&self) -> bool {
        self.state() == "active"
    }
    pub fn in_flight(&self) -> usize {
        lock(&self.inner.state).inflight
    }
    pub fn stop(&self) {
        let mut state = lock(&self.inner.state);
        if state.phase != "active" {
            return;
        }
        state.phase = "draining";
        drop(state);
        if let Some(registry) = self.inner.registry.upgrade() {
            lock(&registry.state)
                .registrations
                .retain(|r| !Arc::ptr_eq(&r.inner, &self.inner));
        }
    }
    pub fn when_drained(&self) -> BoxFuture<()> {
        let mut state = lock(&self.inner.state);
        if state.inflight == 0 {
            return Box::pin(async {});
        }
        // A drain receipt belongs to the current nonzero period. Once that
        // period reaches zero, later observations cannot make it pending again.
        let receipt = state
            .drain
            .get_or_insert_with(|| watch::channel(false).0)
            .subscribe();
        Box::pin(wait_start(receipt))
    }
    fn observe(&self, observation: TelemetryObservation, dispatched: watch::Receiver<usize>) {
        {
            let mut state = lock(&self.inner.state);
            if state.phase != "active" {
                return;
            }
            state.inflight += 1;
            self.inner.count.send_replace(state.inflight);
        }
        let release = Release {
            registration: self.clone(),
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            self.inner.observer.observe(observation.clone())
        }));
        match result {
            Ok(TelemetryObserverResult::Ready(result)) => {
                if let Err(error) = result {
                    self.report(&observation, &error);
                }
                drop(release);
            }
            Ok(TelemetryObserverResult::Pending(mut future)) => {
                // A JS async callback executes its prefix during observe(),
                // but Promise settlement cannot interrupt this dispatch batch.
                let initial = catch_unwind(AssertUnwindSafe(|| future.as_mut().now_or_never()))
                    .unwrap_or_else(|_| {
                        Some(Err(TelemetryFailure("Telemetry observer panicked.".into())))
                    });
                let registration = self.clone();
                executor(&self.inner.executor).spawn(async move {
                    let _release = release;
                    wait_count(dispatched).await;
                    let result = match initial {
                        Some(result) => result,
                        None => AssertUnwindSafe(future)
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| {
                                Err(TelemetryFailure("Telemetry observer panicked.".into()))
                            }),
                    };
                    if let Err(error) = result {
                        registration.report(&observation, &error);
                    }
                });
            }
            Err(_) => {
                self.report(
                    &observation,
                    &TelemetryFailure("Telemetry observer panicked.".into()),
                );
                drop(release);
            }
        }
    }
    fn report(&self, observation: &TelemetryObservation, error: &TelemetryFailure) {
        if let Some(registry) = self.inner.registry.upgrade()
            && let Some(handler) = &registry.failure
        {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                handler(&self.inner.name, self.inner.generation, observation, error)
            }));
        }
    }
    pub fn dispose(&self) -> BoxFuture<Result<(), TelemetryFailure>> {
        self.dispose_ordered(None).0
    }
    fn dispose_ordered(
        &self,
        previous: Option<watch::Receiver<bool>>,
    ) -> (
        BoxFuture<Result<(), TelemetryFailure>>,
        watch::Receiver<bool>,
    ) {
        self.stop();
        let mut state = lock(&self.inner.state);
        if let Some(receipt) = &state.dispose {
            return (
                Box::pin(wait(receipt.clone())),
                state
                    .started
                    .as_ref()
                    .expect("dispose start receipt")
                    .clone(),
            );
        }
        let (done, receipt) = watch::channel(None);
        let (started, start_receipt) = watch::channel(false);
        state.dispose = Some(receipt.clone());
        state.started = Some(start_receipt.clone());
        drop(state);
        let inner = self.inner.clone();
        let completion = Completion(Some(done));
        let start_completion = StartCompletion(started);
        executor(&self.inner.executor).spawn(async move {
            let mut completion = completion;
            let start_completion = start_completion;
            wait_count(inner.count.subscribe()).await;
            if let Some(previous) = previous {
                wait_start(previous).await;
            }
            let invocation = catch_unwind(AssertUnwindSafe(|| {
                let mut future = match &inner.disposer {
                    Some(dispose) => dispose(inner.observer.clone()),
                    None => inner.observer.dispose(),
                };
                let initial = future.as_mut().now_or_never();
                (future, initial)
            }));
            // Publish only after the synchronous disposer prefix ran. This
            // preserves the same-wave reverse order without serializing drain.
            drop(start_completion);
            let result = match invocation {
                Ok((future, Some(result))) => {
                    drop(future);
                    result
                }
                Ok((future, None)) => AssertUnwindSafe(future)
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        Err(TelemetryFailure(
                            "Telemetry observer disposal panicked.".into(),
                        ))
                    }),
                Err(_) => Err(TelemetryFailure(
                    "Telemetry observer disposal panicked.".into(),
                )),
            };
            lock(&inner.state).phase = "disposed";
            completion.finish(result);
        });
        (Box::pin(wait(receipt)), start_receipt)
    }
}
struct StartCompletion(watch::Sender<bool>);
impl Drop for StartCompletion {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
async fn wait_start(mut receipt: watch::Receiver<bool>) {
    while !*receipt.borrow_and_update() {
        if receipt.changed().await.is_err() {
            break;
        }
    }
}
struct Release {
    registration: TelemetryRegistration,
}
impl Drop for Release {
    fn drop(&mut self) {
        let mut state = lock(&self.registration.inner.state);
        state.inflight -= 1;
        self.registration.inner.count.send_replace(state.inflight);
        if state.inflight == 0
            && let Some(drained) = state.drain.take()
        {
            drained.send_replace(true);
        }
    }
}
struct Completion(Option<watch::Sender<Option<Result<(), TelemetryFailure>>>>);
impl Completion {
    fn finish(&mut self, result: Result<(), TelemetryFailure>) {
        if let Some(sender) = self.0.take() {
            sender.send_replace(Some(result));
        }
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        self.finish(Err(TelemetryFailure(
            "Telemetry disposal worker terminated before completion.".into(),
        )));
    }
}
async fn wait(mut receipt: Receipt) -> Result<(), TelemetryFailure> {
    loop {
        if let Some(result) = receipt.borrow_and_update().clone() {
            return result;
        }
        if receipt.changed().await.is_err() {
            return Err(TelemetryFailure(
                "Telemetry disposal receipt closed.".into(),
            ));
        }
    }
}
async fn wait_count(mut receipt: watch::Receiver<usize>) {
    while *receipt.borrow_and_update() != 0 {
        if receipt.changed().await.is_err() {
            break;
        }
    }
}
fn normalize_name(name: &JsString) -> Result<JsString, TelemetryFailure> {
    let units = name.units();
    let whitespace = |u: &u16| {
        char::from_u32(u32::from(*u))
            .is_some_and(|c| crate::metadata::js_trim(&c.to_string()).is_empty())
    };
    let start = units
        .iter()
        .position(|u| !whitespace(u))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|u| !whitespace(u))
        .map_or(start, |i| i + 1);
    if start == end {
        Err(TelemetryFailure(
            "Telemetry observer name must not be empty.".into(),
        ))
    } else {
        Ok(JsString::from_utf16(units[start..end].to_vec()))
    }
}
fn active(state: &RegistryState, action: &str) -> Result<(), TelemetryFailure> {
    if state.phase == "active" {
        Ok(())
    } else {
        Err(TelemetryFailure(format!(
            "Cannot {action}; telemetry observer registry is {}.",
            state.phase
        )))
    }
}
fn missing(name: &JsString) -> TelemetryFailure {
    TelemetryFailure(format!(
        "Telemetry observer is not registered: {}",
        String::from_utf16_lossy(name.units())
    ))
}
fn aggregate(mut failures: Vec<TelemetryFailure>, message: &str) -> Result<(), TelemetryFailure> {
    match failures.len() {
        0 => Ok(()),
        1 => Err(failures.remove(0)),
        _ => Err(TelemetryFailure(format!(
            "{message} {}",
            failures
                .into_iter()
                .map(|f| f.0)
                .collect::<Vec<_>>()
                .join("; ")
        ))),
    }
}

pub struct ObservingTelemetryClient {
    base: Arc<dyn TelemetryClient>,
    observers: TelemetryObserverRegistry,
}
impl ObservingTelemetryClient {
    pub fn new(base: Arc<dyn TelemetryClient>, observers: TelemetryObserverRegistry) -> Arc<Self> {
        Arc::new(Self { base, observers })
    }
}
impl TelemetryClient for ObservingTelemetryClient {
    fn track(&self, name: &JsString, properties: &JsValue, context: &JsValue) {
        self.base.track(name, properties, context);
        self.observers.observe(TelemetryObservation::Track {
            event_name: name.clone(),
            properties: properties.clone(),
            context: context.clone(),
        });
    }
    fn track_feature_used(&self, input: &JsValue) {
        self.base.track_feature_used(input);
        self.observers
            .observe(TelemetryObservation::FeatureUsed(input.clone()));
    }
    fn track_feature_loop_stage(&self, input: &JsValue) {
        self.base.track_feature_loop_stage(input);
        self.observers
            .observe(TelemetryObservation::FeatureLoopStage(input.clone()));
    }
    fn track_error(&self, error: &TelemetryErrorValue, input: &JsValue) {
        self.base.track_error(error, input);
        self.observers.observe(TelemetryObservation::Error {
            error: error.clone(),
            input: input.clone(),
        });
    }
    fn set_enabled(&self, enabled: bool) {
        self.base.set_enabled(enabled);
    }
    fn flush(&self) -> BoxFuture<()> {
        self.base.flush()
    }
    fn shutdown(&self) -> BoxFuture<()> {
        self.base.shutdown()
    }
    fn snapshot(&self) -> JsValue {
        self.base.snapshot()
    }
    fn get_config(&self) -> JsValue {
        self.base.get_config()
    }
}

// A registry constructed without an executor binds once at its first async
// observation/disposal. Its generations then share that owner permanently.
fn executor(slot: &OnceLock<tokio::runtime::Handle>) -> &tokio::runtime::Handle {
    slot.get_or_init(tokio::runtime::Handle::current)
}
