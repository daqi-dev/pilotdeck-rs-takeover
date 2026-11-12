//! GatewayTelemetryBundle.ts:26-66: observe all clients, own only a collector
//! constructed here, and drain observers before shutting that collector down.
use super::{
    BoxFuture, ObservingTelemetryClient, RuntimeContextOptions, TelemetryClient,
    TelemetryCollector, TelemetryCollectorOptions, TelemetryEnv, TelemetryFailure,
    TelemetryObserverRegistry, TelemetryRuntimeContext, lock, resolve_runtime_context,
};
use crate::runtime::model::http::ModelHttpTransport;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

pub struct GatewayTelemetryBundleOptions {
    pub env: TelemetryEnv,
    pub pilot_home: PathBuf,
    pub cwd: PathBuf,
    pub context: Option<TelemetryRuntimeContext>,
    /// A supplied client remains owned by its caller.
    pub telemetry: Option<Arc<dyn TelemetryClient>>,
    pub transport: Option<Arc<dyn ModelHttpTransport>>,
    pub enabled: Option<bool>,
}
impl GatewayTelemetryBundleOptions {
    pub fn new(env: TelemetryEnv, pilot_home: PathBuf, cwd: PathBuf) -> Self {
        Self {
            env,
            pilot_home,
            cwd,
            context: None,
            telemetry: None,
            transport: None,
            enabled: None,
        }
    }
}
type Outcome = Option<Result<(), TelemetryFailure>>;
pub struct GatewayTelemetryBundle {
    runtime: tokio::runtime::Handle,
    base: Arc<dyn TelemetryClient>,
    client: Arc<dyn TelemetryClient>,
    observers: TelemetryObserverRegistry,
    owns_base: bool,
    dispose: Mutex<Option<watch::Receiver<Outcome>>>,
}
impl GatewayTelemetryBundle {
    pub fn new(options: GatewayTelemetryBundleOptions) -> Result<Arc<Self>, TelemetryFailure> {
        let owns_base = options.telemetry.is_none();
        let base: Arc<dyn TelemetryClient> = match options.telemetry {
            Some(client) => client,
            None => {
                let context = match options.context {
                    Some(context) => context,
                    None => resolve_runtime_context(&RuntimeContextOptions::from_process(
                        options.env.clone(),
                        Some(options.pilot_home.clone()),
                        options.cwd,
                    ))?,
                };
                let mut collector =
                    TelemetryCollectorOptions::new(options.env, options.pilot_home, context);
                collector.enabled = options.enabled;
                collector.transport = options.transport;
                TelemetryCollector::new(collector)
            }
        };
        let observers = TelemetryObserverRegistry::default();
        let client = ObservingTelemetryClient::new(base.clone(), observers.clone());
        Ok(Arc::new(Self {
            runtime: tokio::runtime::Handle::current(),
            base,
            client,
            observers,
            owns_base,
            dispose: Mutex::new(None),
        }))
    }
    pub fn client(&self) -> Arc<dyn TelemetryClient> {
        self.client.clone()
    }
    pub fn observers(&self) -> TelemetryObserverRegistry {
        self.observers.clone()
    }
    pub fn dispose(&self) -> BoxFuture<Result<(), TelemetryFailure>> {
        let mut current = lock(&self.dispose);
        if let Some(receipt) = &*current {
            return Box::pin(wait(receipt.clone()));
        }
        let (done, receipt) = watch::channel(None);
        *current = Some(receipt.clone());
        drop(current);
        let observers = self.observers.dispose();
        let base = self.base.clone();
        let owns_base = self.owns_base;
        let completion = Completion(Some(done));
        self.runtime.spawn(async move {
            let mut completion = completion;
            let result = observers.await;
            if owns_base {
                base.shutdown().await;
            }
            completion.finish(result);
        });
        Box::pin(wait(receipt))
    }
}
struct Completion(Option<watch::Sender<Outcome>>);
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
            "Gateway telemetry disposal worker terminated before completion.".into(),
        )));
    }
}
async fn wait(mut receipt: watch::Receiver<Outcome>) -> Result<(), TelemetryFailure> {
    loop {
        if let Some(result) = receipt.borrow_and_update().clone() {
            return result;
        }
        if receipt.changed().await.is_err() {
            return Err(TelemetryFailure(
                "Gateway telemetry disposal receipt closed.".into(),
            ));
        }
    }
}
