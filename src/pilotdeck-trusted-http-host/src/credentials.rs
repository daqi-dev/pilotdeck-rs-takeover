//! Trusted assembly reuses the same pure ordered-header algorithms as Gateway.
//! Callers must not expose credential-bearing results outside the trusted host.
pub use pilotdeck_model_core::codec::providers::google::headers_with_credentials as google_headers;
pub use pilotdeck_model_core::codec::providers::provider_headers_with_credentials as provider_headers;
pub use pilotdeck_model_core::codec::providers::request_headers_with_credentials;
