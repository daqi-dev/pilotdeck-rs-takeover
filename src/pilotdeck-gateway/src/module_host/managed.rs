//! Explicit managed module-model routing; this does not infer identity from
//! the socket uid, session key, agent ID alone, or the main Gateway principal.
use super::{
    HostBody, HostEvent, HostFailure, HostResponse, PublicHostPrincipal, PublicHostProvider,
    PublicHostResolver,
};
use bytes::Bytes;
use pilotdeck_protocol::{JsString, JsValue};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::mpsc;

/// Original validated module-host identity, without dropping any field.
/// Transport validation alone does not authenticate the asserted actor: the
/// installed authority must verify the original installation/ownership mapping.
#[derive(Clone, PartialEq, Eq)]
pub struct ManagedModulePrincipal {
    pub pilot_deck_user_id: JsString,
    pub tenant_id: JsString,
    pub actor_user_id: JsString,
    pub agent_id: JsString,
}
impl From<&PublicHostPrincipal> for ManagedModulePrincipal {
    fn from(p: &PublicHostPrincipal) -> Self {
        Self {
            pilot_deck_user_id: p.pilot_deck_user_id.clone(),
            tenant_id: p.tenant_id.clone(),
            actor_user_id: p.actor_user_id.clone(),
            agent_id: p.agent_id.clone(),
        }
    }
}
pub type ManagedModuleEvent = HostEvent;
pub enum ManagedModuleBody {
    Value(JsValue),
    Bytes(Bytes),
    Stream(mpsc::Receiver<Result<ManagedModuleEvent, String>>),
}
pub struct ManagedModuleResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ManagedModuleBody,
    pub raw_body: Option<String>,
}
pub struct ManagedModuleFailure {
    pub status: Option<u16>,
    pub code: Option<String>,
    pub message: Option<String>,
}

/// A trusted composition root supplies an authenticated principal→Kernel
/// session/owner mapping. It must reject unavailable or ambiguous mappings;
/// it must not dispatch to the ambient model implementation as a fallback.
/// For a stream, dropping the returned receiver must cancel only that stream's
/// local operation and retain its resource receipt until real convergence;
/// it does not prove that the remote provider stopped. Error/output secrets
/// must be filtered by the same admitted registry as the actual model scope.
pub trait ManagedModuleModel: Send + Sync {
    fn call<'a>(
        &'a self,
        principal: &'a ManagedModulePrincipal,
        operation: &'a str,
        input: &'a JsValue,
    ) -> Pin<
        Box<dyn Future<Output = Result<ManagedModuleResponse, ManagedModuleFailure>> + Send + 'a>,
    >;
}

pub(crate) struct ManagedResolver {
    pub(crate) legacy: Arc<dyn PublicHostResolver>,
    pub(crate) model: Arc<dyn ManagedModuleModel>,
}
struct Provider {
    base: Arc<dyn PublicHostProvider>,
    model: Arc<dyn ManagedModuleModel>,
    principal: ManagedModulePrincipal,
}

impl PublicHostResolver for ManagedResolver {
    fn resolve(
        &self,
        principal: &PublicHostPrincipal,
    ) -> Result<Arc<dyn PublicHostProvider>, HostFailure> {
        Ok(Arc::new(Provider {
            base: self.legacy.resolve(principal)?,
            model: self.model.clone(),
            principal: principal.into(),
        }))
    }
}
impl PublicHostProvider for Provider {
    fn operations(&self) -> Vec<String> {
        self.base.operations()
    }
    fn call(
        &self,
        operation: &str,
        input: &JsValue,
        principal: &PublicHostPrincipal,
    ) -> Pin<Box<dyn Future<Output = Result<HostResponse, HostFailure>> + Send + '_>> {
        let operation = operation.to_owned();
        let input = input.clone();
        let principal = ManagedModulePrincipal::from(principal);
        Box::pin(async move {
            if principal != self.principal {
                return Err(HostFailure {
                    status: Some(403),
                    code: Some("PUBLIC_HOST_PRINCIPAL_MISMATCH".into()),
                    message: Some("Module principal changed during resolution.".into()),
                });
            }
            if matches!(
                operation.as_str(),
                "list_model_catalog" | "model_prepare" | "model_stream"
            ) {
                let response = self
                    .model
                    .call(&principal, &operation, &input)
                    .await
                    .map_err(|error| HostFailure {
                        status: error.status,
                        code: error.code,
                        message: error.message,
                    })?;
                let body = match response.body {
                    ManagedModuleBody::Value(value) => HostBody::Value(value),
                    ManagedModuleBody::Bytes(value) => HostBody::Bytes(value),
                    ManagedModuleBody::Stream(source) => HostBody::Stream(source),
                };
                return Ok(HostResponse {
                    status: response.status,
                    headers: response.headers,
                    body,
                    raw_body: response.raw_body,
                });
            }
            let original = PublicHostPrincipal {
                pilot_deck_user_id: principal.pilot_deck_user_id,
                tenant_id: principal.tenant_id,
                actor_user_id: principal.actor_user_id,
                agent_id: principal.agent_id,
            };
            self.base.call(&operation, &input, &original).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct Base(Arc<AtomicUsize>);
    impl PublicHostProvider for Base {
        fn operations(&self) -> Vec<String> {
            vec!["model_stream".into(), "file_parse".into()]
        }
        fn call(
            &self,
            _: &str,
            _: &JsValue,
            _: &PublicHostPrincipal,
        ) -> Pin<Box<dyn Future<Output = Result<HostResponse, HostFailure>> + Send + '_>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(HostResponse {
                    status: 200,
                    headers: vec![],
                    body: HostBody::Value(JsValue::Null),
                    raw_body: None,
                })
            })
        }
    }
    struct Legacy(Arc<AtomicUsize>);
    impl PublicHostResolver for Legacy {
        fn resolve(
            &self,
            _: &PublicHostPrincipal,
        ) -> Result<Arc<dyn PublicHostProvider>, HostFailure> {
            Ok(Arc::new(Base(self.0.clone())))
        }
    }
    struct Deny(Mutex<Vec<ManagedModulePrincipal>>);
    impl ManagedModuleModel for Deny {
        fn call<'a>(
            &'a self,
            principal: &'a ManagedModulePrincipal,
            _: &'a str,
            _: &'a JsValue,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<ManagedModuleResponse, ManagedModuleFailure>>
                    + Send
                    + 'a,
            >,
        > {
            self.0.lock().unwrap().push(principal.clone());
            Box::pin(async {
                Err(ManagedModuleFailure {
                    status: Some(403),
                    code: Some("UNMAPPED".into()),
                    message: None,
                })
            })
        }
    }
    fn principal(actor: &str) -> PublicHostPrincipal {
        PublicHostPrincipal {
            pilot_deck_user_id: "user".into(),
            tenant_id: "tenant".into(),
            actor_user_id: actor.into(),
            agent_id: "agent".into(),
        }
    }
    #[tokio::test]
    async fn managed_model_rejects_unmapped_and_never_uses_ambient_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let managed = Arc::new(Deny(Mutex::new(Vec::new())));
        let resolver = ManagedResolver {
            legacy: Arc::new(Legacy(calls.clone())),
            model: managed.clone(),
        };
        let first = principal("actor");
        let provider = resolver.resolve(&first).unwrap();
        assert!(
            provider
                .call("model_stream", &JsValue::Null, &first)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            provider
                .call("model_stream", &JsValue::Null, &principal("other"))
                .await
                .is_err()
        );
        for field in 0..3 {
            let mut changed = principal("actor");
            match field {
                0 => changed.pilot_deck_user_id = "other".into(),
                1 => changed.tenant_id = "other".into(),
                _ => changed.agent_id = "other".into(),
            }
            assert!(
                provider
                    .call("model_stream", &JsValue::Null, &changed)
                    .await
                    .is_err()
            );
        }
        assert!(
            provider
                .call("file_parse", &JsValue::Null, &first)
                .await
                .is_ok()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(managed.0.lock().unwrap().len(), 1);
        let observed = managed.0.lock().unwrap();
        assert!(observed[0].pilot_deck_user_id.equals_str("user"));
        assert!(observed[0].tenant_id.equals_str("tenant"));
        assert!(observed[0].actor_user_id.equals_str("actor"));
        assert!(observed[0].agent_id.equals_str("agent"));
    }
}
