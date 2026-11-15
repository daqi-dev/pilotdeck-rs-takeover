//! Candidate pending the Kernel project-reservation public contract.
//! The snapshot policy has no default: production assembly must supply one
//! derived from the exact lease, turn and approved per-session catalog.
use super::project_model_owner::{ProjectModelLease, ProjectModelOwner, ProjectTurnBorrow};
use crate::runtime::model::http::ManagedGatewayServices;
use agentcpm_core::guard::SecretRegistry;
use agentcpm_host::{
    TrustedModelBindingResolver, TrustedProjectExecutionSource, TrustedProjectReservation,
};
use agentcpm_iface::{
    env::http_pull::{HttpFailure, HttpFailureKind, HttpFailurePhase, HttpPullContext},
    model_session::{ConfigRevision, ModelBinding, ModelExecutionContext},
};
use pilotdeck_model_iface::{ModelJobServices, ModelJobServicesProvider};
use pilotdeck_model_runtime::http::StreamOptions;
use pilotdeck_protocol::JsValue;
use pilotdeck_trusted_http_host::{
    ApprovedOrigins, TrustedHttpRuntime, TrustedJobSnapshot, TrustedProjectLease,
    validate_safe_model_config,
};
use std::sync::Arc;

/// Order matters: the actual project fork is returned before the Host pending
/// reservation, even on early errors. The scope owns this until its receipt.
struct ReservedProjectLease {
    turn: ProjectTurnBorrow,
    _lease: ProjectModelLease,
    reservation: TrustedProjectReservation,
}
impl TrustedProjectLease for ReservedProjectLease {}

/// All three references originate from the same retained, revalidated Host
/// reservation. Public getters expose immutable facts; callers cannot assemble
/// a different owner/source/turn combination.
pub struct ProjectJobOwner<'a> {
    source: &'a TrustedProjectExecutionSource,
    owner: &'a ProjectModelOwner,
    turn: &'a ProjectTurnBorrow,
}
impl ProjectJobOwner<'_> {
    pub fn source(&self) -> &TrustedProjectExecutionSource {
        self.source
    }
    pub fn owner(&self) -> &ProjectModelOwner {
        self.owner
    }
    pub fn turn(&self) -> &ProjectTurnBorrow {
        self.turn
    }
}

/// Trusted product implementation must derive every field from this exact
/// lease's immutable project snapshot and its already-admitted turn; it must
/// include proxy and custom secrets, and has no ambient fallback.
pub trait ProjectJobPolicy: Send + Sync {
    fn supports_binding(&self, binding: &ModelBinding) -> bool;
    fn derive(
        &self,
        lease: &ProjectModelLease,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        ownership: &ProjectJobOwner<'_>,
    ) -> Result<ProjectJobFacts, HttpFailure>;
}

pub struct ProjectJobFacts {
    pub private_provider: JsValue,
    pub provider_view: JsValue,
    pub safe_config: JsValue,
    pub canonical_model: JsValue,
    pub approved_wire_model: Option<JsValue>,
    pub secrets: Arc<SecretRegistry>,
    pub origins: ApprovedOrigins,
    pub options: StreamOptions,
}

/// This authority is supplied by the authenticated product session/turn. Its
/// inventory must explicitly include all approved secrets, including custom
/// headers and secrets embedded in extraBody; discovery by scanning unlabelled
/// project values is not a substitute. Proxy material is added by the runtime.
pub trait ProjectTurnAuthority: Send + Sync {
    fn supports_binding(&self, binding: &ModelBinding) -> bool;
    fn check(
        &self,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        ownership: &ProjectJobOwner<'_>,
    ) -> Result<(), HttpFailure>;
    /// Explicitly approve the effective route derived from the immutable
    /// provider snapshot, including any extraBody model override.
    fn approve_route(
        &self,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        ownership: &ProjectJobOwner<'_>,
        provider: &JsValue,
        effective_wire_model: Option<&JsValue>,
    ) -> Result<(), HttpFailure>;
    fn secret_inventory(
        &self,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        ownership: &ProjectJobOwner<'_>,
        provider: &JsValue,
    ) -> Result<Vec<String>, HttpFailure>;
    fn origins(
        &self,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        ownership: &ProjectJobOwner<'_>,
        provider: &JsValue,
    ) -> Result<ApprovedOrigins, HttpFailure>;
    fn options(
        &self,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        ownership: &ProjectJobOwner<'_>,
    ) -> Result<StreamOptions, HttpFailure>;
}

/// Builds jobs from the admitted lease's normalized model snapshot. The
/// caller must supply a real authenticated turn authority; there is no default.
pub struct SnapshotJobPolicy {
    pub runtime: Arc<TrustedHttpRuntime>,
    pub authority: Arc<dyn ProjectTurnAuthority>,
}

impl ProjectJobPolicy for SnapshotJobPolicy {
    fn supports_binding(&self, binding: &ModelBinding) -> bool {
        self.authority.supports_binding(binding)
    }

    fn derive(
        &self,
        lease: &ProjectModelLease,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        ownership: &ProjectJobOwner<'_>,
    ) -> Result<ProjectJobFacts, HttpFailure> {
        self.authority.check(context, binding, ownership)?;
        let config = lease.private_model_config().map_err(|_| denied())?;
        let provider = snapshot_provider(&config, binding)?;
        let mut inventory = self
            .authority
            .secret_inventory(context, binding, ownership, &provider)?;
        known_provider_secrets(&provider, &mut inventory);
        let secrets = self.runtime.secret_registry(inventory);
        let origins = self
            .authority
            .origins(context, binding, ownership, &provider)?;
        let options = self.authority.options(context, binding, ownership)?;
        let canonical_model = JsValue::String(binding.model.clone().into());
        let approved_wire_model = snapshot_wire_model(&provider, &canonical_model);
        self.authority.approve_route(
            context,
            binding,
            ownership,
            &provider,
            approved_wire_model.as_ref(),
        )?;
        Ok(ProjectJobFacts {
            provider_view: crate::runtime::model::http::provider_view(&provider),
            safe_config: crate::runtime::model::http::config_view(&config),
            private_provider: provider,
            canonical_model,
            approved_wire_model,
            secrets,
            origins,
            options,
        })
    }
}

fn snapshot_provider(config: &JsValue, binding: &ModelBinding) -> Result<JsValue, HttpFailure> {
    let endpoint = binding
        .endpoint_ref
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(denied)?;
    let provider = config
        .get("providers")
        .and_then(|value| value.get(endpoint))
        .cloned()
        .ok_or_else(denied)?;
    if provider
        .get("models")
        .and_then(|value| value.get(&binding.model))
        .is_none()
        || !binding.extensions.is_empty()
    {
        return Err(denied());
    }
    Ok(provider)
}

fn snapshot_wire_model(provider: &JsValue, canonical_model: &JsValue) -> Option<JsValue> {
    let mut input = pilotdeck_protocol::JsObject::new();
    input.insert("model".into(), canonical_model.clone());
    let body = pilotdeck_model_core::request::final_body(&JsValue::Object(input), provider);
    body.get("model").cloned()
}

/// Gather only structurally identified credential fields. Arbitrary custom
/// header/body secrets still require the authority's explicit inventory.
fn known_provider_secrets(provider: &JsValue, values: &mut Vec<String>) {
    let convert = |value: &JsValue| {
        pilotdeck_model_core::codec::js::to_string_units(value)
            .ok()
            .map(|units| String::from_utf16_lossy(&units))
    };
    if let Some(key) = provider.get("apiKey")
        && !matches!(key, JsValue::Undefined | JsValue::Null)
        && let Some(key) = convert(key)
    {
        let trimmed = pilotdeck_model_core::codec::metadata::js_trim(&key);
        if !trimmed.is_empty() {
            values.push(trimmed.to_owned());
        }
    }
    if let Some(headers) = provider.get("headers").and_then(JsValue::as_object) {
        for (name, value) in headers.own_entries() {
            let name = String::from_utf16_lossy(name.units());
            if [
                "authorization",
                "proxy-authorization",
                "cookie",
                "x-api-key",
                "x-goog-api-key",
            ]
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
                && !matches!(value, JsValue::Undefined | JsValue::Null)
                && let Some(value) = convert(value)
                && !value.is_empty()
            {
                values.push(value);
            }
        }
    }
}

pub struct ProjectJobs {
    pub(crate) runtime: Arc<TrustedHttpRuntime>,
    pub(crate) resolver: TrustedModelBindingResolver,
    pub(crate) policy: Arc<dyn ProjectJobPolicy>,
}

impl ProjectJobs {
    pub fn new(
        runtime: Arc<TrustedHttpRuntime>,
        resolver: TrustedModelBindingResolver,
        policy: Arc<dyn ProjectJobPolicy>,
    ) -> Self {
        Self {
            runtime,
            resolver,
            policy,
        }
    }
}

impl ModelJobServicesProvider for ProjectJobs {
    fn supports_binding(&self, binding: &ModelBinding) -> bool {
        self.policy.supports_binding(binding)
    }

    fn for_job(
        &self,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        revision: ConfigRevision,
    ) -> Result<ModelJobServices, HttpFailure> {
        let scope = HttpPullContext {
            session_id: context.session_id().into(),
            model_session_id: context.model_session_id().into(),
            job_id: context.job_id().into(),
            principal: context.principal().into(),
            deadline: context.deadline(),
            cancel: context.cancel(),
        };
        let reservation = self.resolver.reserve_project(&scope)?;
        let bound = reservation.binding().clone();
        if bound.binding != *binding || bound.config_revision != revision {
            return Err(denied());
        }
        let owner = reservation
            .owner::<ProjectModelOwner>()
            .ok_or_else(denied)?;
        let turn = owner
            .borrow_model_turn(&reservation, context)
            .map_err(|_| denied())?;
        let token = owner.token().map_err(|_| denied())?;
        let child = token.checkout(owner).map_err(|_| denied())?;
        // The reservation still accounts for the child on every later exit.
        let lease = Arc::new(ReservedProjectLease {
            turn,
            _lease: child,
            reservation,
        });
        // The registry is not yet available on derivation failure. Keep the
        // typed decision, but never forward a policy error's potentially
        // secret-bearing detail or cause to the ordinary factory.
        let source = lease.reservation.source().ok_or_else(denied)?;
        let owner = lease
            .reservation
            .owner::<ProjectModelOwner>()
            .ok_or_else(denied)?;
        let ownership = ProjectJobOwner {
            source,
            owner,
            turn: &lease.turn,
        };
        let facts = self
            .policy
            .derive(&lease._lease, context, binding, &ownership)
            .map_err(|error| {
                HttpFailure::new(error.phase, error.kind, "project model preparation failed")
            })?;
        validate_safe_model_config(&facts.safe_config, &facts.secrets)?;
        lease.reservation.revalidate()?;
        let headers = self.runtime.job_headers(TrustedJobSnapshot {
            context: scope,
            binding: bound,
            private_provider: facts.private_provider,
            provider_view: facts.provider_view,
            canonical_model: facts.canonical_model,
            approved_wire_model: facts.approved_wire_model,
            secrets: facts.secrets,
            lease: lease.clone(),
            approved_origins: facts.origins,
        })?;
        // Failure here is cleaned via the guarded factory's close_scope path;
        // the scope retains its reservation and secret facts until convergence.
        lease.reservation.revalidate()?;
        Ok(ModelJobServices {
            safe_config: facts.safe_config,
            host: Arc::new(ManagedGatewayServices { headers }),
            options: facts.options,
        })
    }
}

fn denied() -> HttpFailure {
    HttpFailure::new(
        HttpFailurePhase::Scope,
        HttpFailureKind::Denied,
        "project model binding is unavailable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inventory_registers_known_credentials_without_guessing_custom_secrets() {
        let provider = pilotdeck_protocol::parse_json(r#"{"apiKey":" key-value ","headers":{"Authorization":"Bearer credential","x-custom":"ordinary"}}"#).unwrap();
        let mut values = vec!["explicit-custom-secret".to_string()];
        known_provider_secrets(&provider, &mut values);
        assert!(values.iter().any(|value| value == "key-value"));
        assert!(values.iter().any(|value| value == "Bearer credential"));
        assert!(values.iter().any(|value| value == "explicit-custom-secret"));
        assert!(!values.iter().any(|value| value == "ordinary"));
    }

    #[test]
    fn snapshot_selects_exact_provider_and_preserves_extra_body_wire_route() {
        let config = pilotdeck_protocol::parse_json(r#"{"providers":{"a":{"models":{"same":{}},"extraBody":{"model":"override"}},"b":{"models":{"same":{}}}}}"#).unwrap();
        let a = ModelBinding::new("same".into(), Some("a".into()), vec![]);
        let b = ModelBinding::new("same".into(), Some("b".into()), vec![]);
        let missing = ModelBinding::new("other".into(), Some("a".into()), vec![]);
        let pa = snapshot_provider(&config, &a).unwrap();
        let pb = snapshot_provider(&config, &b).unwrap();
        assert!(snapshot_provider(&config, &missing).is_err());
        let canonical = JsValue::String("same".into());
        assert_eq!(
            snapshot_wire_model(&pa, &canonical),
            Some(JsValue::String("override".into()))
        );
        assert_eq!(snapshot_wire_model(&pb, &canonical), Some(canonical));
    }
}
