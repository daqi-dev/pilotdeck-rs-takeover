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

pub struct TestLease(
    pub Arc<AtomicUsize>,
    pub Option<agentcpm_host::TrustedProjectReservation>,
);
impl TrustedProjectLease for TestLease {}
impl Drop for TestLease {
    fn drop(&mut self) {
        drop(self.1.take());
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
    project_mode: bool,
    guarded: Arc<dyn HttpPullTransport>,
    project_checks: Arc<AtomicUsize>,
    raw_starts: Arc<AtomicUsize>,
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
        let reservation = if self.project_mode {
            let reservation = self.resolver.reserve_project(&scope)?;
            let owner = reservation
                .owner::<TestProjectOwner>()
                .expect("bound fixture owner");
            let source = reservation
                .source()
                .expect("positive control needs a real Native Scheduler source");
            assert_eq!(source.turn(), 7);
            assert_eq!(source.input_idempotency_key(), "fixture-7");
            assert_eq!(source.generation(), owner.generation);
            assert!(!source.call_ref().is_empty() && !source.input_ref().is_empty());
            reservation.revalidate()?;
            Some(reservation)
        } else {
            None
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
            context: scope.clone(),
            binding: resolved,
            private_provider: self.private_provider.clone(),
            provider_view: self.provider.clone(),
            canonical_model: JsValue::String("m".into()),
            approved_wire_model: Some(JsValue::String("m".into())),
            secrets: Arc::new(SecretRegistry::new([(
                "fixture/key".into(),
                "fixture-secret".into(),
            )])),
            lease: Arc::new(TestLease(self.drops.clone(), reservation)),
            approved_origins: ApprovedOrigins::new([self.origin.clone()])?,
        })?;
        if self.project_mode {
            // Use this exact job's genuine issuer, materialization and
            // endpoint. The valid start is deliberately never polled: it
            // establishes raw acceptance without making an extra HTTP call.
            let body = parse_json(r#"{"model":"m","messages":[],"stream":true}"#).unwrap();
            let prepared = headers
                .materialize(&self.provider, &body, RequestMode::Stream)
                .expect("genuine job materialization");
            let endpoint =
                pilotdeck_model_core::codec::providers::chat_endpoint_candidates(&self.provider)
                    .into_iter()
                    .next()
                    .expect("provider endpoint");
            let projection = prepared
                .validate_url(&endpoint)
                .expect("genuine prepared endpoint");
            let body_bytes = pilotdeck_protocol::stringify_json(&body)
                .unwrap()
                .unwrap()
                .into_bytes();
            let request = agentcpm_iface::env::http_pull::HttpPullRequest {
                method: "POST".into(),
                url: projection.url.to_string(),
                headers: projection
                    .headers
                    .iter()
                    .map(|(name, value)| (name.as_str().to_string(), value.as_bytes().to_vec()))
                    .collect(),
                body: body_bytes,
                caller_follows_redirects: true,
                preparation: Some(projection.preparation),
            };
            let before = self.raw_starts.load(Ordering::SeqCst);
            let valid = self
                .guarded
                .start(request.clone(), scope.clone())
                .expect("genuine scoped preparation reaches raw adapter");
            assert_eq!(self.raw_starts.load(Ordering::SeqCst), before + 1);
            drop(valid.response);
            valid.control.stop_local();
            drop(valid.control);
            for variant in 0..2 {
                let mut forged = scope.clone();
                if variant == 0 {
                    forged.cancel = Default::default();
                } else {
                    forged.job_id.push_str("-forged");
                }
                let failure = self
                    .resolver
                    .resolve(&forged)
                    .err()
                    .expect("forged context resolved");
                assert_eq!(failure.kind, HttpFailureKind::Denied);
                let failure = self
                    .resolver
                    .reserve_project(&forged)
                    .err()
                    .expect("forged context reserved project");
                assert_eq!(failure.kind, HttpFailureKind::Denied);
                let before = self.raw_starts.load(Ordering::SeqCst);
                let failure = self
                    .guarded
                    .start(request.clone(), forged)
                    .err()
                    .expect("forged context started guarded transport");
                assert_eq!(failure.kind, HttpFailureKind::Denied);
                assert_eq!(
                    self.raw_starts.load(Ordering::SeqCst),
                    before,
                    "forgery reached raw transport"
                );
                self.project_checks.fetch_add(3, Ordering::SeqCst);
            }
        }
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
    project_mode: bool,
    projects: OnceLock<agentcpm_host::TrustedProjectSessions>,
    pub project_checks: Arc<AtomicUsize>,
    pub raw_starts: Arc<AtomicUsize>,
}
impl TestSetup {
    pub fn new(
        origin: String,
        executor: tokio::runtime::Handle,
        drops: Arc<AtomicUsize>,
        trace: crate::trace::Trace,
        project_mode: bool,
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
            project_mode,
            projects: OnceLock::new(),
            project_checks: Arc::new(AtomicUsize::new(0)),
            raw_starts: Arc::new(AtomicUsize::new(0)),
        }
    }
}
pub struct TestProjectOwner {
    revoked: std::sync::atomic::AtomicBool,
    generation: u32,
}
impl agentcpm_host::TrustedProjectOwner for TestProjectOwner {
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
    fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
    }
    fn is_released(&self) -> bool {
        self.revoked.load(Ordering::SeqCst)
    }
}
impl TestSetup {
    pub fn bind_created(&self, reference: &str) -> agentcpm_host::TrustedProjectSession {
        let projects = self
            .projects
            .get()
            .expect("trusted project services installed");
        let creation = projects
            .claim_created(reference)
            .expect("authenticated creation reference");
        let generation = creation.generation();
        projects
            .bind_owner(
                creation,
                Arc::new(TestProjectOwner {
                    revoked: std::sync::atomic::AtomicBool::new(false),
                    generation,
                }),
                fixture_catalog(),
            )
            .expect("bind fixture owner and catalog")
    }
}
fn setup_error(message: &'static str) -> Error {
    Error::new(ErrorCategory::Internal, -32603, message)
}
struct CountRaw {
    inner: Arc<dyn HttpPullTransport>,
    starts: Arc<AtomicUsize>,
}
impl HttpPullTransport for CountRaw {
    fn start(
        &self,
        request: agentcpm_iface::env::http_pull::HttpPullRequest,
        context: HttpPullContext,
    ) -> Result<agentcpm_iface::env::http_pull::HttpExchange, HttpFailure> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.inner.start(request, context)
    }
    fn close_scope(
        &self,
        context: HttpPullContext,
    ) -> Result<Arc<dyn agentcpm_iface::env::http_pull::HttpScopeCleanup>, HttpFailure> {
        self.inner.close_scope(context)
    }
}
fn fixture_catalog() -> Vec<TrustedModelCatalogEntry> {
    vec![TrustedModelCatalogEntry {
        binding: ModelBinding::new("m".into(), Some("p".into()), vec![]),
        capabilities: vec!["text".into()],
    }]
}
impl TrustedModelSetup for TestSetup {
    fn uses_project_sessions(&self) -> bool {
        self.project_mode
    }
    fn catalog(&self) -> Result<Vec<TrustedModelCatalogEntry>, Error> {
        if self.project_mode {
            Ok(Vec::new())
        } else {
            Ok(fixture_catalog())
        }
    }
    fn transport(&self, services: TrustedModelServices) -> Result<ManagedModelTransport, Error> {
        self.projects
            .set(services.project_sessions())
            .map_err(|_| setup_error("test projects already installed"))?;
        self.resolver
            .set(services.bindings.clone())
            .map_err(|_| setup_error("test resolver already installed"))?;
        let runtime = Arc::new(TrustedHttpRuntime::new(services, None));
        let mut port = runtime.managed_transport();
        if self.project_mode {
            port.transport = Arc::new(CountRaw {
                inner: port.transport,
                starts: self.raw_starts.clone(),
            });
        }
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
        let guarded = transport.clone();
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
                project_mode: self.project_mode,
                guarded,
                project_checks: self.project_checks.clone(),
                raw_starts: self.raw_starts.clone(),
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
