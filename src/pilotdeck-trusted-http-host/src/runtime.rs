//! Explicit trusted composition surface. No ambient credentials or allow-all
//! policy are synthesized here; callers must supply an admitted project lease,
//! approved provider snapshot, registered secrets and exact origin policy.
use crate::{
    headers::{JobFacts, JobHeaders, OriginCheck},
    prepared::TrustedProjectLease,
    pull::PullTransport,
    scope::ScopeManager,
};
use agentcpm_core::guard::SecretRegistry;
use agentcpm_host::{
    ManagedModelTransport, TrustedModelBinding, TrustedModelBindingResolver, TrustedModelServices,
};
use agentcpm_iface::env::http_pull::{
    HttpFailure, HttpFailureKind, HttpFailurePhase, HttpPreparationIssuer, HttpPullContext,
};
use pilotdeck_http_transport::transport::ProxySettings;
use pilotdeck_protocol::JsValue;
use std::sync::Arc;

/// Exact network origins approved by trusted product policy for this job.
/// No wildcard or default allow is provided.
#[derive(Clone)]
pub struct ApprovedOrigins {
    origins: Vec<url::Origin>,
}
impl ApprovedOrigins {
    pub fn new(origins: impl IntoIterator<Item = String>) -> Result<Self, HttpFailure> {
        let mut approved = Vec::new();
        for value in origins {
            let url = url::Url::parse(&value).map_err(|_| denied("invalid approved origin"))?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(denied("invalid approved origin"));
            }
            let origin = url.origin();
            if !approved.contains(&origin) {
                approved.push(origin);
            }
        }
        if approved.is_empty() {
            return Err(denied("approved origins are required"));
        }
        Ok(Self { origins: approved })
    }
    fn check(&self, uri: &hyper::Uri) -> Result<(), HttpFailure> {
        let parsed =
            url::Url::parse(&uri.to_string()).map_err(|_| denied("unapproved model origin"))?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || !self.origins.contains(&parsed.origin())
        {
            return Err(denied("unapproved model origin"));
        }
        Ok(())
    }
}

/// Constructed only at the trusted product composition boundary. `lease` must
/// own the exact admitted project generation. `approved_wire_model` is the
/// separately approved model after applying that snapshot's extraBody rules;
/// None explicitly approves absence, not an implicit allow-all. Google uses the
/// approved canonical model in its original endpoint rule instead.
pub struct TrustedJobSnapshot {
    pub context: HttpPullContext,
    pub binding: TrustedModelBinding,
    pub private_provider: JsValue,
    pub provider_view: JsValue,
    pub canonical_model: JsValue,
    pub approved_wire_model: Option<JsValue>,
    pub secrets: Arc<SecretRegistry>,
    pub lease: Arc<dyn TrustedProjectLease>,
    pub approved_origins: ApprovedOrigins,
}

/// Holds the unique trusted issuer and scope pools for one resident assembly.
pub struct TrustedHttpRuntime {
    scopes: Arc<ScopeManager>,
    issuer: Arc<HttpPreparationIssuer>,
    bindings: TrustedModelBindingResolver,
}
impl TrustedHttpRuntime {
    /// Construct a registry from the caller's explicit complete secret
    /// inventory plus the exact proxy configuration frozen by this runtime.
    /// This does not discover unknown custom secrets or certify completeness.
    pub fn secret_registry(&self, values: Vec<String>) -> Arc<SecretRegistry> {
        self.scopes.secret_registry(values)
    }
    pub fn new(services: TrustedModelServices, proxy: Option<ProxySettings>) -> Self {
        Self {
            scopes: ScopeManager::new(services.egress, proxy),
            issuer: Arc::new(HttpPreparationIssuer::new()),
            bindings: services.bindings,
        }
    }
    /// Pass only this port through the Host's guarded model factory path.
    pub fn managed_transport(&self) -> ManagedModelTransport {
        ManagedModelTransport {
            transport: Arc::new(PullTransport {
                scopes: self.scopes.clone(),
                issuer: self.issuer.clone(),
                bindings: self.bindings.clone(),
            }),
            lifecycle: self.scopes.clone(),
        }
    }
    /// Registers the revocable job header service before giving it to a driver.
    /// The product assembly owns the authenticity of the lease and snapshot;
    /// the Host resolver independently verifies the original job binding.
    pub fn job_headers(
        &self,
        snapshot: TrustedJobSnapshot,
    ) -> Result<Arc<JobHeaders>, HttpFailure> {
        // Track even secret checks before scope creation, so an empty close
        // cannot report release while this trusted preparation is in flight.
        let reservation = self.scopes.reserve_creation(&snapshot.context)?;
        let validation = if !self.scopes.proxy_secrets_covered(&snapshot.secrets) {
            Err(denied(
                "proxy credentials are absent from the secret registry",
            ))
        } else if !view_is_secret_free(&snapshot.provider_view, &snapshot.secrets) {
            Err(denied("provider view contains registered secrets"))
        } else if !provider_secrets_covered(&snapshot.private_provider, &snapshot.secrets) {
            Err(denied(
                "provider credentials are absent from the secret registry",
            ))
        } else {
            Ok(())
        };
        if let Err(error) = validation {
            // Drop the actual snapshot/lease before releasing its creation
            // fence; empty cleanup cannot race ahead of this destructor.
            drop(snapshot);
            drop(reservation);
            return Err(error);
        }
        let scope_reservation = reservation.fork();
        let origins = snapshot.approved_origins;
        let origin: OriginCheck = Arc::new(move |uri| origins.check(uri));
        let result = JobHeaders::new(
            snapshot.context,
            snapshot.binding,
            self.bindings.clone(),
            JobFacts {
                private_provider: snapshot.private_provider,
                provider_view: snapshot.provider_view,
                canonical_model: snapshot.canonical_model,
                approved_wire_model: snapshot.approved_wire_model,
                registry: snapshot.secrets,
                lease: snapshot.lease,
                origin,
            },
            &self.scopes,
            self.issuer.clone(),
            scope_reservation,
        );
        // On failure all moved JobFacts have been dropped by the callee.
        drop(reservation);
        result
    }
}
fn denied(detail: &str) -> HttpFailure {
    HttpFailure::new(HttpFailurePhase::Scope, HttpFailureKind::Denied, detail)
}

/// Check the complete ordinary-model configuration against the exact
/// registry for this admitted project, including keys and prototypes. This
/// does not establish that the registry itself covers all sensitive material.
pub fn validate_safe_model_config(
    value: &JsValue,
    registry: &SecretRegistry,
) -> Result<(), HttpFailure> {
    if view_is_secret_free(value, registry) {
        Ok(())
    } else {
        Err(denied("model configuration contains registered secrets"))
    }
}

/// A secret-free view must remain safe even through prototypes, object keys
/// and primitive formatting, not only through its ordinary JSON projection.
fn view_is_secret_free(value: &JsValue, registry: &SecretRegistry) -> bool {
    let safe = |text: &str| registry.redact(text) == text;
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            JsValue::String(text) if !safe(&String::from_utf16_lossy(text.units())) => {
                return false;
            }
            JsValue::Number(number)
                if !safe(&pilotdeck_model_core::codec::js::number_string(*number)) =>
            {
                return false;
            }
            JsValue::Array(values) => pending.extend(values),
            JsValue::Object(object) => {
                if object.native_tag().is_some_and(|tag| !safe(tag)) {
                    return false;
                }
                if let Some(prototype) = object.prototype() {
                    pending.push(prototype);
                }
                for (key, value) in object.entries() {
                    if !safe(&String::from_utf16_lossy(key.units())) {
                        return false;
                    }
                    pending.push(value);
                }
            }
            _ => {}
        }
    }
    true
}

/// Verify recognized credential fields before an error can cross the trust
/// boundary. Trusted assembly must also register any custom secret fields.
fn provider_secrets_covered(provider: &JsValue, registry: &SecretRegistry) -> bool {
    let fully_covered =
        |text: &str| text.is_empty() || (text != "***" && registry.redact(text) == "***");
    if let Some(key) = provider.get("apiKey")
        && !matches!(key, JsValue::Null | JsValue::Undefined)
    {
        let google = provider
            .get("protocol")
            .and_then(JsValue::as_string)
            .is_some_and(|v| v.equals_str("google"));
        let units = if google {
            pilotdeck_model_core::codec::js::to_string_units_checked(key).ok()
        } else {
            pilotdeck_model_core::codec::js::to_string_units(key).ok()
        };
        // A coercion error remains the original later materialization
        // error, rather than being moved into trusted setup.
        if units.is_some_and(|units| {
            let text = String::from_utf16_lossy(&units);
            !fully_covered(pilotdeck_model_core::codec::metadata::js_trim(&text))
        }) {
            return false;
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
                && !matches!(value, JsValue::Null | JsValue::Undefined)
                && let Ok(units) = pilotdeck_model_core::codec::js::to_string_units(value)
            {
                let text = String::from_utf16_lossy(&units);
                let whole = fully_covered(&text);
                // Auth scheme labels are not secret; require the entire
                // credential token to be covered, not merely a substring.
                let allows_scheme = name.eq_ignore_ascii_case("authorization")
                    || name.eq_ignore_ascii_case("proxy-authorization");
                let scheme_token = allows_scheme
                    && text
                        .split_once(' ')
                        .filter(|(scheme, _)| {
                            scheme.eq_ignore_ascii_case("bearer")
                                || scheme.eq_ignore_ascii_case("basic")
                        })
                        .is_some_and(|(_, token)| !token.is_empty() && fully_covered(token));
                if !whole && !scheme_token {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn full_safe_config_rejects_registered_secrets_outside_selected_provider() {
        let registry =
            agentcpm_core::guard::SecretRegistry::new([("fixture".into(), "hidden-value".into())]);
        let bad = pilotdeck_protocol::parse_json(
            r#"{"providers":{"safe":{},"other":{"extraBody":{"field":"hidden-value"}}}}"#,
        )
        .unwrap();
        let safe =
            pilotdeck_protocol::parse_json(r#"{"providers":{"safe":{},"other":{}}}"#).unwrap();
        assert!(super::validate_safe_model_config(&bad, &registry).is_err());
        assert!(super::validate_safe_model_config(&safe, &registry).is_ok());
    }
    use super::*;
    #[test]
    fn origin_policy_is_explicit_and_exact() {
        assert!(ApprovedOrigins::new(Vec::<String>::new()).is_err());
        let policy = ApprovedOrigins::new(["https://api.test".into()]).unwrap();
        assert!(
            policy
                .check(&"https://api.test/v1".parse().unwrap())
                .is_ok()
        );
        assert!(
            policy
                .check(&"http://api.test/v1".parse().unwrap())
                .is_err()
        );
        assert!(
            policy
                .check(&"https://other.test/v1".parse().unwrap())
                .is_err()
        );
        assert!(ApprovedOrigins::new(["https://user:secret@api.test".into()]).is_err());
    }
    #[test]
    fn provider_auth_values_must_be_registered() {
        let provider = pilotdeck_protocol::parse_json(r#"{"protocol":"openai","apiKey":"synthetic-key","headers":{"Authorization":"Bearer other-secret"}}"#).unwrap();
        let empty = SecretRegistry::new(Vec::<(String, String)>::new());
        assert!(!provider_secrets_covered(&provider, &empty));
        let registry = SecretRegistry::new([
            ("a/key".into(), "synthetic-key".into()),
            ("a/header".into(), "other-secret".into()),
        ]);
        assert!(provider_secrets_covered(&provider, &registry));
    }
    #[test]
    fn public_provider_view_cannot_contain_a_registered_secret() {
        let registry = SecretRegistry::new([("test/key".into(), "synthetic-key".into())]);
        let bad =
            pilotdeck_protocol::parse_json(r#"{"headers":{"X-Example":"synthetic-key"}}"#).unwrap();
        let safe = pilotdeck_protocol::parse_json(r#"{"headers":{"X-Example":"***"}}"#).unwrap();
        assert!(!view_is_secret_free(&bad, &registry));
        assert!(view_is_secret_free(&safe, &registry));
    }
    #[test]
    fn partial_secret_matches_do_not_prove_full_coverage() {
        let provider =
            pilotdeck_protocol::parse_json(r#"{"protocol":"openai","apiKey":"abcdef"}"#).unwrap();
        let prefix = SecretRegistry::new([("test/key".into(), "abc".into())]);
        let suffix = SecretRegistry::new([("test/key".into(), "def".into())]);
        let full = SecretRegistry::new([("test/key".into(), "abcdef".into())]);
        assert!(!provider_secrets_covered(&provider, &prefix));
        assert!(!provider_secrets_covered(&provider, &suffix));
        assert!(provider_secrets_covered(&provider, &full));
    }
    #[test]
    fn scheme_exception_does_not_apply_to_api_key_or_cookie() {
        let registry = SecretRegistry::new([("test/key".into(), "abcdef".into())]);
        for name in ["x-api-key", "cookie"] {
            let mut headers = pilotdeck_protocol::JsObject::new();
            headers.insert(name.into(), "Bearer abcdef".into());
            let mut provider = pilotdeck_protocol::JsObject::new();
            provider.insert("headers".into(), JsValue::Object(headers));
            assert!(!provider_secrets_covered(
                &JsValue::Object(provider),
                &registry
            ));
        }
    }
}
