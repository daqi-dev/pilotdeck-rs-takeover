use super::*;
use agentcpm_iface::env::http_pull::HttpNetworkCause;

#[test]
fn managed_network_outer_text_is_fetch_failed_but_real_cause_is_retained() {
    for phase in [
        HttpFailurePhase::Dns,
        HttpFailurePhase::Connect,
        HttpFailurePhase::Tls,
        HttpFailurePhase::ResponseHead,
    ] {
        for code in [None, Some("NEW_VENDOR_雪_CODE"), Some("ETIMEDOUT")] {
            let original = HttpFailure {
                phase,
                kind: HttpFailureKind::Network,
                detail: "internal endpoint diagnostic".into(),
                network_cause: Some(HttpNetworkCause {
                    code: code.map(str::to_owned),
                    message: "real redacted IO detail".into(),
                }),
            };
            let wrapped = std::io::Error::other(original.clone());
            let mapped = map_exchange_error(ExchangeError::Dial(Box::new(wrapped)));
            assert_eq!(mapped.phase, phase);
            assert_eq!(mapped.kind, HttpFailureKind::Network);
            assert_eq!(mapped.detail, "fetch failed");
            assert_eq!(mapped.network_cause, original.network_cause);
        }
    }
}

#[test]
fn host_scope_failures_are_not_relabelled_from_their_cause_or_text() {
    for phase in [HttpFailurePhase::Scope, HttpFailurePhase::Connect] {
        for kind in [
            HttpFailureKind::Deadline,
            HttpFailureKind::Denied,
            HttpFailureKind::Cancelled,
            HttpFailureKind::Resource,
        ] {
            let original = HttpFailure {
                phase,
                kind,
                detail: "ETIMEDOUT fetch failed is not authority".into(),
                network_cause: Some(HttpNetworkCause {
                    code: Some("ETIMEDOUT".into()),
                    message: "known text cannot change phase or kind".into(),
                }),
            };
            let mapped = map_exchange_error(ExchangeError::Network(Box::new(original.clone())));
            assert_eq!(mapped, original);
        }
    }
}

#[test]
fn native_encoding_failure_retains_its_original_outer_message() {
    let original = pilotdeck_model_runtime::TransportFailure {
        cause_code: Some("ORIGINAL_ENCODING_CODE"),
        message: "original encoding rejection".into(),
    };
    let mapped = map_exchange_error(ExchangeError::Encoding(original));
    assert_eq!(mapped.phase, HttpFailurePhase::ResponseHead);
    assert_eq!(mapped.kind, HttpFailureKind::Network);
    assert_eq!(mapped.detail, "original encoding rejection");
    assert_eq!(
        mapped.network_cause.unwrap().code.as_deref(),
        Some("ORIGINAL_ENCODING_CODE")
    );
}
