//! Restricted original-project model authority. The Host's actual session
//! catalog and scoped reservation remain the admission authority.
use super::managed_project_jobs::{ProjectJobOwner, ProjectTurnAuthority, known_provider_secrets};
use agentcpm_iface::{
    env::http_pull::{HttpFailure, HttpFailureKind, HttpFailurePhase},
    model_session::{ModelBinding, ModelExecutionContext},
};
use pilotdeck_model_runtime::http::StreamOptions;
use pilotdeck_protocol::{JsString, JsValue};
use pilotdeck_trusted_http_host::ApprovedOrigins;

/// No global project registry, credential or path; every job is checked using
/// its already-reserved original project generation and turn facts.
pub struct OriginalProjectTurnAuthority;
fn unsupported() -> HttpFailure {
    HttpFailure::new(
        HttpFailurePhase::Scope,
        HttpFailureKind::Unsupported,
        "project model configuration requires an explicit secret inventory or original router path",
    )
}
fn denied() -> HttpFailure {
    HttpFailure::new(
        HttpFailurePhase::Scope,
        HttpFailureKind::Denied,
        "project model policy rejected the binding",
    )
}
fn provider_and_model(binding: &ModelBinding) -> Result<(JsString, JsString), HttpFailure> {
    let provider = binding
        .endpoint_ref
        .as_ref()
        .filter(|s| !s.is_empty())
        .ok_or_else(denied)?;
    if binding.model.is_empty() || !binding.extensions.is_empty() {
        return Err(denied());
    }
    Ok((provider.as_str().into(), binding.model.as_str().into()))
}
fn known_auth_header(name: &JsString) -> bool {
    [
        "authorization",
        "proxy-authorization",
        "cookie",
        "x-api-key",
        "x-goog-api-key",
    ]
    .iter()
    .any(|candidate| {
        name.to_utf8()
            .is_ok_and(|value| value.eq_ignore_ascii_case(candidate))
    })
}
fn safe_inventory(config: &JsValue) -> Result<Vec<String>, HttpFailure> {
    let providers = config
        .get("providers")
        .and_then(JsValue::as_object)
        .ok_or_else(denied)?;
    let mut values = Vec::new();
    for (_, provider) in providers.own_entries() {
        // Provider URLs are included in the ordinary driver view. Userinfo,
        // query and fragment may contain credentials; without an explicitly
        // labelled inventory they are outside this first supported subset.
        let raw_url = provider
            .get("url")
            .and_then(JsValue::as_string)
            .ok_or_else(unsupported)?
            .to_utf8()
            .map_err(|_| unsupported())?;
        if !raw_url.is_empty() {
            let url = url::Url::parse(&raw_url).map_err(|_| unsupported())?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(unsupported());
            }
        } else if !provider
            .get("protocol")
            .and_then(JsValue::as_string)
            .is_some_and(|v| v.equals_str("google"))
        {
            return Err(unsupported());
        }
        match provider.get("headers") {
            None | Some(JsValue::Undefined | JsValue::Null) => {}
            Some(JsValue::Object(headers)) => {
                if headers
                    .own_entries()
                    .into_iter()
                    .any(|(key, value)| !known_auth_header(key) || value.as_string().is_none())
                {
                    return Err(unsupported());
                }
            }
            _ => return Err(unsupported()),
        }
        match provider.get("extraBody") {
            None | Some(JsValue::Undefined | JsValue::Null) => {}
            Some(JsValue::Object(extra)) if extra.entries().is_empty() => {}
            _ => return Err(unsupported()),
        }
        known_provider_secrets(provider, &mut values);
    }
    Ok(values)
}
fn approved_origins(
    provider: &JsValue,
    binding: &ModelBinding,
) -> Result<ApprovedOrigins, HttpFailure> {
    let endpoints = if provider
        .get("protocol")
        .and_then(JsValue::as_string)
        .is_some_and(|value| value.equals_str("google"))
    {
        vec![
            pilotdeck_model_core::codec::providers::google::endpoint(
                provider,
                &JsValue::String(binding.model.as_str().into()),
            )
            .map_err(|_| unsupported())?,
        ]
    } else {
        pilotdeck_model_core::codec::providers::chat_endpoint_candidates(provider)
    };
    let mut origins = Vec::new();
    for endpoint in endpoints {
        let url = url::Url::parse(&endpoint).map_err(|_| unsupported())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(unsupported());
        }
        let origin = url.origin().ascii_serialization();
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    ApprovedOrigins::new(origins)
}
impl ProjectTurnAuthority for OriginalProjectTurnAuthority {
    // This is only an effect-free capability/shape predicate. Host session
    // catalog admission and current scoped reservation authorize every job.
    fn supports_binding(&self, binding: &ModelBinding) -> bool {
        provider_and_model(binding).is_ok()
    }
    fn check(
        &self,
        _: &ModelExecutionContext,
        binding: &ModelBinding,
        owner: &ProjectJobOwner<'_>,
    ) -> Result<(), HttpFailure> {
        let (provider, model) = provider_and_model(binding)?;
        let router = owner.lease().router().map_err(|_| denied())?;
        // This initial adapter does not run router decide/execute. Explicitly
        // reject enabled routing until that original path is connected.
        if router.config().enabled {
            return Err(unsupported());
        }
        router
            .validate_model_selection(
                &provider,
                &model,
                owner.turn().facts().model.managed_model_policy.as_ref(),
            )
            .map_err(|_| denied())
    }
    fn approve_route(
        &self,
        _: &ModelExecutionContext,
        binding: &ModelBinding,
        _: &ProjectJobOwner<'_>,
        _: &JsValue,
        effective_wire_model: Option<&JsValue>,
    ) -> Result<(), HttpFailure> {
        if effective_wire_model.and_then(JsValue::as_string)
            != Some(&JsString::from(binding.model.as_str()))
        {
            return Err(unsupported());
        }
        Ok(())
    }
    fn secret_inventory(
        &self,
        _: &ModelExecutionContext,
        _: &ModelBinding,
        owner: &ProjectJobOwner<'_>,
        _: &JsValue,
    ) -> Result<Vec<String>, HttpFailure> {
        let config = owner.lease().private_model_config().map_err(|_| denied())?;
        safe_inventory(&config)
    }
    fn origins(
        &self,
        _: &ModelExecutionContext,
        binding: &ModelBinding,
        _: &ProjectJobOwner<'_>,
        provider: &JsValue,
    ) -> Result<ApprovedOrigins, HttpFailure> {
        approved_origins(provider, binding)
    }
    fn options(
        &self,
        _: &ModelExecutionContext,
        _: &ModelBinding,
        _: &ProjectJobOwner<'_>,
    ) -> Result<StreamOptions, HttpFailure> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::parse_json;
    #[test]
    fn source_snapshot_registers_all_known_keys_but_rejects_unmarked_custom_material() {
        let good = parse_json(r#"{"providers":{"a":{"url":"https://a.example/","apiKey":"alpha","headers":{"Authorization":"Bearer beta"}},"b":{"url":"https://b.example/","apiKey":"gamma","headers":{}}}}"#).unwrap();
        let secrets = safe_inventory(&good).unwrap();
        assert!(secrets.iter().any(|s| s == "alpha"));
        assert!(secrets.iter().any(|s| s == "Bearer beta"));
        assert!(secrets.iter().any(|s| s == "gamma"));
        for raw in [
            r#"{"providers":{"a":{"url":"https://a.example/","headers":{"x-custom":"x"}}}}"#,
            r#"{"providers":{"a":{"url":"https://a.example/","extraBody":{"token":"x"}}}}"#,
            r#"{"providers":{"a":{"url":"https://a.example/","extraBody":"unmarked-secret"}}}"#,
            r#"{"providers":{"a":{"url":"https://a.example/","headers":"unmarked-secret"}}}"#,
        ] {
            let failure = safe_inventory(&parse_json(raw).unwrap()).unwrap_err();
            assert_eq!(failure.kind, HttpFailureKind::Unsupported);
        }
    }
    #[test]
    fn origins_come_from_original_provider_endpoint_rules() {
        let binding = ModelBinding::new("m".into(), Some("p".into()), vec![]);
        for raw in [
            r#"{"protocol":"openai","url":"http://127.0.0.1:9010/v1"}"#,
            r#"{"protocol":"google","url":""}"#,
        ] {
            assert!(approved_origins(&parse_json(raw).unwrap(), &binding).is_ok());
        }
        for raw in [
            r#"{"protocol":"openai","url":"http://user:pass@127.0.0.1:9010/v1"}"#,
            r#"{"protocol":"openai","url":"file:///tmp/model"}"#,
        ] {
            assert!(approved_origins(&parse_json(raw).unwrap(), &binding).is_err());
        }
    }
    #[test]
    fn shape_check_cannot_grant_an_empty_or_extended_binding() {
        let authority = OriginalProjectTurnAuthority;
        assert!(authority.supports_binding(&ModelBinding::new(
            "m".into(),
            Some("p".into()),
            vec![]
        )));
        assert!(!authority.supports_binding(&ModelBinding::new(
            "".into(),
            Some("p".into()),
            vec![]
        )));
        assert!(!authority.supports_binding(&ModelBinding::new("m".into(), None, vec![])));
        assert!(!authority.supports_binding(&ModelBinding::new(
            "m".into(),
            Some("p".into()),
            vec![("x".into(), "y".into())]
        )));
    }
}
