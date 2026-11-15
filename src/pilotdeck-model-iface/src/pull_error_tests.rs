use super::*;
use agentcpm_iface::env::http_pull::HttpNetworkCause;

fn failure(phase: HttpFailurePhase, kind: HttpFailureKind, code: Option<&str>) -> HttpFailure {
    HttpFailure {
        phase,
        kind,
        detail: "fetch failed".into(),
        network_cause: Some(HttpNetworkCause {
            code: code.map(str::to_owned),
            message: "redacted connector detail".into(),
        }),
    }
}

#[test]
fn real_network_phases_keep_owned_unknown_and_absent_codes() {
    for phase in [
        HttpFailurePhase::Dns,
        HttpFailurePhase::Connect,
        HttpFailurePhase::Tls,
        HttpFailurePhase::ResponseHead,
    ] {
        for code in [None, Some("ECONNRESET"), Some("NEW_VENDOR_雪_CODE")] {
            let original = failure(phase, HttpFailureKind::Network, code);
            match head_failure(original.clone()) {
                HostOperationError::Original(mapped) => {
                    assert_eq!(mapped.cause_code.as_deref(), code);
                    assert_eq!(mapped.message, original.detail);
                    if code.is_some() {
                        assert!(matches!(mapped.cause_code, Some(Cow::Owned(_))));
                    }
                }
                _ => panic!("a real network source must not become a host refusal"),
            }
        }
        let mut missing_cause = failure(phase, HttpFailureKind::Network, None);
        missing_cause.network_cause = None;
        assert!(
            matches!(head_failure(missing_cause), HostOperationError::Original(error) if error.cause_code.is_none())
        );
    }
    let mut encoding = failure(
        HttpFailurePhase::ResponseHead,
        HttpFailureKind::Network,
        None,
    );
    encoding.detail = "original encoding rejection".into();
    assert!(
        matches!(head_failure(encoding), HostOperationError::Original(error) if error.message == "original encoding rejection")
    );
}

#[test]
fn phase_and_kind_refusals_are_preserved_without_code_or_message_guessing() {
    for phase in [
        HttpFailurePhase::Scope,
        HttpFailurePhase::Request,
        HttpFailurePhase::ResponseBody,
    ] {
        let original = failure(phase, HttpFailureKind::Network, Some("ECONNRESET"));
        assert!(
            matches!(head_failure(original.clone()), HostOperationError::Rejected(error) if error == original)
        );
    }
    for phase in [
        HttpFailurePhase::Scope,
        HttpFailurePhase::Dns,
        HttpFailurePhase::Connect,
        HttpFailurePhase::Tls,
        HttpFailurePhase::ResponseHead,
    ] {
        for kind in [
            HttpFailureKind::Cancelled,
            HttpFailureKind::Deadline,
            HttpFailureKind::Denied,
            HttpFailureKind::Unavailable,
            HttpFailureKind::Resource,
            HttpFailureKind::Unsupported,
        ] {
            let original = failure(phase, kind, Some("ETIMEDOUT"));
            assert!(
                matches!(head_failure(original.clone()), HostOperationError::Rejected(error) if error == original)
            );
        }
    }
}

#[test]
fn body_decode_requires_its_own_phase_and_cause_while_resource_stays_rejected() {
    let mut decoded = failure(
        HttpFailurePhase::ResponseBody,
        HttpFailureKind::Decode,
        None,
    );
    decoded.detail = "terminated".into();
    decoded.network_cause.as_mut().unwrap().message = "dynamic redacted decoder cause".into();
    assert!(
        matches!(body_failure(decoded.clone()), HostOperationError::Original(DriverBodyFailure::Decode(Cow::Owned(reason))) if reason == "dynamic redacted decoder cause")
    );
    decoded.network_cause = None;
    assert!(
        matches!(body_failure(decoded.clone()), HostOperationError::Rejected(error) if error == decoded)
    );
    let network = failure(
        HttpFailurePhase::ResponseBody,
        HttpFailureKind::Network,
        None,
    );
    assert!(matches!(
        body_failure(network),
        HostOperationError::Original(DriverBodyFailure::Terminated)
    ));
    for phase in [
        HttpFailurePhase::Scope,
        HttpFailurePhase::Connect,
        HttpFailurePhase::ResponseHead,
    ] {
        let original = failure(phase, HttpFailureKind::Decode, None);
        assert!(
            matches!(body_failure(original.clone()), HostOperationError::Rejected(error) if error == original)
        );
    }
    let mut resource = failure(
        HttpFailurePhase::ResponseBody,
        HttpFailureKind::Resource,
        None,
    );
    resource.detail = pilotdeck_model_runtime::transport::STRING_TOO_LONG.into();
    assert!(
        matches!(body_failure(resource.clone()), HostOperationError::Rejected(error) if error == resource)
    );
}

#[test]
fn explicit_body_limit_requires_the_body_phase_and_does_not_relabel_resource() {
    let body = failure(
        HttpFailurePhase::ResponseBody,
        HttpFailureKind::ResponseBodyTooLarge,
        None,
    );
    assert!(matches!(
        body_failure(body),
        HostOperationError::Original(DriverBodyFailure::StringTooLong)
    ));
    for phase in [
        HttpFailurePhase::Scope,
        HttpFailurePhase::Request,
        HttpFailurePhase::ResponseHead,
    ] {
        let original = failure(phase, HttpFailureKind::ResponseBodyTooLarge, None);
        assert!(
            matches!(body_failure(original.clone()), HostOperationError::Rejected(error) if error == original)
        );
    }
    let original = failure(
        HttpFailurePhase::ResponseBody,
        HttpFailureKind::Resource,
        None,
    );
    assert!(
        matches!(body_failure(original.clone()), HostOperationError::Rejected(error) if error == original)
    );
}
