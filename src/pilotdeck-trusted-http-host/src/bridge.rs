//! Trusted Kernel egress adapter for one host-minted job/generation scope.
use agentcpm_host::TrustedEgressPool;
use pilotdeck_http_transport::transport::{BoxError, ProxyIo, ScopedDial};
use std::{future::Future, pin::Pin, time::Duration};

pub(crate) struct ScopeDialer(pub(crate) TrustedEgressPool);
impl ScopedDial for ScopeDialer {
    fn preserve_error_source(&self) -> bool {
        true
    }
    fn authorize(&self, logical: hyper::Uri, physical_endpoint: String) -> Result<(), BoxError> {
        self.0
            .authorize(&logical.to_string(), &physical_endpoint)
            .map_err(|error| Box::new(error) as BoxError)
    }
    fn connect(
        &self,
        logical: hyper::Uri,
        physical_endpoint: String,
        _proxy: bool,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn ProxyIo>, BoxError>> + Send>> {
        // The bridge starts this budget after physical DNS resolves, matching
        // HttpConnector's TCP-phase timeout. ProxyConnect separately retains
        // the existing 10-second deadline for the whole proxy tunnel.
        let connect = self.0.connect(
            &logical.to_string(),
            &physical_endpoint,
            Duration::from_secs(10),
        );
        Box::pin(async move {
            connect
                .await
                .map(|stream| Box::new(stream) as Box<dyn ProxyIo>)
                .map_err(|error| Box::new(error) as BoxError)
        })
    }
}
