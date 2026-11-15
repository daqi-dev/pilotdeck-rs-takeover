//! Isolated test composition, not a production ProjectSession lease supplier.
//! The real Host resolver and guarded pull are mandatory in this fixture.
use agentcpm_core::guard::SecretRegistry;
use agentcpm_host::{
    ManagedModelTransport, TrustedModelBindingResolver, TrustedModelCatalogEntry,
    TrustedModelServices, TrustedModelSetup,
};
use agentcpm_iface::{
    env::http_pull::{
        HttpFailure, HttpFailureKind, HttpFailurePhase, HttpPullContext, HttpPullTransport,
        HttpRequestPreparation,
    },
    model_session::{
        ConfigRevision, ModelBinding, ModelExecutionContext, ModelSession, ModelSessionConfig,
        ModelSessionFactory, SessionCapabilities,
    },
    types::{Error, ErrorCategory},
};
use pilotdeck_model_core::codec::normalize_error::JsError;
use pilotdeck_model_iface::{
    ModelJobServices, ModelJobServicesProvider, SharedPilotDeckModelFactory,
};
use pilotdeck_model_runtime::{
    driver_transport::HostOperationError,
    headers::{HeaderMaterializeResult, ModelHeaderService, RequestMode},
    services::ModelHostServices,
};
use pilotdeck_protocol::{JsString, JsValue, parse_json};
use pilotdeck_trusted_http_host::{
    ApprovedOrigins, JobHeaders, TrustedHttpRuntime, TrustedJobSnapshot, TrustedProjectLease,
};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

pub struct TestLease(pub Arc<AtomicUsize>);
impl TrustedProjectLease for TestLease {}
impl Drop for TestLease {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct JobHost(Arc<JobHeaders>);
impl ModelHeaderService for JobHost {
    type Rejection = HttpFailure;
    type Preparation = HttpRequestPreparation;
    fn materialize(
        &self,
        provider: &JsValue,
        body: &JsValue,
        mode: RequestMode,
    ) -> HeaderMaterializeResult<Self::Rejection, Self::Preparation> {
        self.0.materialize(provider, body, mode)
    }
}
impl ModelHostServices for JobHost {
    fn dump_request_if_enabled(
        &self,
        _: &JsValue,
        _: &JsValue,
    ) -> Result<(), HostOperationError<JsError, HttpFailure>> {
        Ok(())
    }
    fn parse_retry_after_date(&self, _: &JsString) -> Result<Option<f64>, JsString> {
        Ok(None)
    }
    fn format_epoch_iso(&self, _: i64) -> String {
        "1970-01-01T00:00:00.000Z".into()
    }
    fn streaming_duration_limit(&self) -> Option<String> {
        None
    }
    fn diagnostic(&self, _: &str) {}
}
struct Jobs {
    runtime: Arc<TrustedHttpRuntime>,
    resolver: TrustedModelBindingResolver,
    provider: JsValue,
    private_provider: JsValue,
    config: JsValue,
    origin: String,
    drops: Arc<AtomicUsize>,
    pub calls: Arc<AtomicUsize>,
}
impl ModelJobServicesProvider for Jobs {
    fn supports_binding(&self, binding: &ModelBinding) -> bool {
        binding.model == "m"
            && binding.endpoint_ref.as_deref() == Some("p")
            && binding.extensions.is_empty()
    }
    fn for_job(
        &self,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        revision: ConfigRevision,
    ) -> Result<ModelJobServices, HttpFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let scope = HttpPullContext {
            session_id: context.session_id().into(),
            model_session_id: context.model_session_id().into(),
            job_id: context.job_id().into(),
            principal: context.principal().into(),
            deadline: context.deadline(),
            cancel: context.cancel(),
        };
        let resolved = self.resolver.resolve(&scope)?;
        if resolved.binding != *binding || resolved.config_revision != revision {
            return Err(HttpFailure::new(
                HttpFailurePhase::Scope,
                HttpFailureKind::Denied,
                "fixture binding mismatch",
            ));
        }
        let headers = self.runtime.job_headers(TrustedJobSnapshot {
            context: scope,
            binding: resolved,
            private_provider: self.private_provider.clone(),
            provider_view: self.provider.clone(),
            canonical_model: JsValue::String("m".into()),
            approved_wire_model: Some(JsValue::String("m".into())),
            secrets: Arc::new(SecretRegistry::new([(
                "fixture/key".into(),
                "fixture-secret".into(),
            )])),
            lease: Arc::new(TestLease(self.drops.clone())),
            approved_origins: ApprovedOrigins::new([self.origin.clone()])?,
        })?;
        Ok(ModelJobServices {
            safe_config: self.config.clone(),
            host: Arc::new(JobHost(headers)),
            options: Default::default(),
        })
    }
}
/// `executor` belongs to the fixture and outlives Host shutdown; it must not
/// come from caller JSON. No ambient credential or ProjectSession is implied.
pub struct TestSetup {
    pub origin: String,
    pub trace: crate::trace::Trace,
    pub executor: tokio::runtime::Handle,
    pub drops: Arc<AtomicUsize>,
    pub calls: Arc<AtomicUsize>,
    pub opened: Arc<Mutex<Vec<Arc<dyn ModelSession>>>>,
    runtime: OnceLock<Arc<TrustedHttpRuntime>>,
    resolver: OnceLock<TrustedModelBindingResolver>,
}
impl TestSetup {
    pub fn new(
        origin: String,
        executor: tokio::runtime::Handle,
        drops: Arc<AtomicUsize>,
        trace: crate::trace::Trace,
    ) -> Self {
        Self {
            origin,
            trace,
            executor,
            drops,
            calls: Arc::new(AtomicUsize::new(0)),
            opened: Arc::new(Mutex::new(Vec::new())),
            runtime: OnceLock::new(),
            resolver: OnceLock::new(),
        }
    }
}
fn setup_error(message: &'static str) -> Error {
    Error::new(ErrorCategory::Internal, -32603, message)
}
impl TrustedModelSetup for TestSetup {
    fn catalog(&self) -> Result<Vec<TrustedModelCatalogEntry>, Error> {
        Ok(vec![TrustedModelCatalogEntry {
            binding: ModelBinding::new("m".into(), Some("p".into()), vec![]),
            capabilities: vec!["text".into()],
        }])
    }
    fn transport(&self, services: TrustedModelServices) -> Result<ManagedModelTransport, Error> {
        self.resolver
            .set(services.bindings.clone())
            .map_err(|_| setup_error("test resolver already installed"))?;
        let runtime = Arc::new(TrustedHttpRuntime::new(services, None));
        let port = runtime.managed_transport();
        self.runtime
            .set(runtime)
            .map_err(|_| setup_error("test runtime already installed"))?;
        Ok(port)
    }
    fn factory(
        &self,
        transport: Arc<dyn HttpPullTransport>,
    ) -> Result<Arc<dyn ModelSessionFactory>, Error> {
        let runtime = self
            .runtime
            .get()
            .cloned()
            .ok_or_else(|| setup_error("missing trusted runtime"))?;
        let resolver = self
            .resolver
            .get()
            .cloned()
            .ok_or_else(|| setup_error("missing trusted resolver"))?;
        let config = parse_json(&format!(r#"{{"providers":{{"p":{{"id":"p","protocol":"openai","url":"{}","models":{{"m":{{"id":"m","capabilities":{{"supportsStreaming":true,"supportsSystemPrompt":true}},"multimodal":{{"input":["text"]}}}}}}}}}}}}"#, self.origin)).map_err(|_| setup_error("invalid test config"))?;
        let provider = config
            .get("providers")
            .and_then(|providers| providers.get("p"))
            .cloned()
            .ok_or_else(|| setup_error("missing provider"))?;
        let mut private_provider = provider.clone();
        if let JsValue::Object(fields) = &mut private_provider {
            fields.insert("apiKey".into(), "fixture-secret".into());
        }
        let transport: Arc<dyn HttpPullTransport> =
            if std::env::var("PILOTDECK_FIXTURE_TRACE_PULL").as_deref() == Ok("1") {
                Arc::new(crate::trace::Port {
                    inner: transport,
                    trace: self.trace.clone(),
                })
            } else {
                transport
            };
        let inner = SharedPilotDeckModelFactory {
            jobs: Arc::new(Jobs {
                runtime,
                resolver,
                provider,
                private_provider,
                config,
                origin: self.origin.clone(),
                drops: self.drops.clone(),
                calls: self.calls.clone(),
            }),
            transport,
            executor: self.executor.clone(),
        };
        Ok(Arc::new(RecordedFactory {
            inner,
            opened: self.opened.clone(),
        }))
    }
}

struct RecordedFactory {
    inner: SharedPilotDeckModelFactory,
    opened: Arc<Mutex<Vec<Arc<dyn ModelSession>>>>,
}
impl ModelSessionFactory for RecordedFactory {
    fn capabilities(&self) -> SessionCapabilities {
        self.inner.capabilities()
    }
    fn supported_profiles(&self) -> Vec<String> {
        self.inner.supported_profiles()
    }
    fn open(
        &self,
        config: ModelSessionConfig,
    ) -> agentcpm_iface::BoxFuture<'_, Result<Arc<dyn ModelSession>, Error>> {
        Box::pin(async move {
            let session = self.inner.open(config).await?;
            self.opened.lock().unwrap().push(session.clone());
            Ok(session)
        })
    }
}
