use super::super::requests::Requests;
use crate::runtime::connection::{Call, RequestError};
use pilotdeck_protocol::{JsString, JsValue};

// Wire names are the WebSocket frame method names from the TS frames registry
// (frames.ts:47-48). `pilotdeck server` always wires `sopStatus` /
// `resumeSop` (createLocalGateway.ts:1072-1090): the StaffDeck control plane
// answers per project from `modules.sop`, and the public approval bridge
// (composition/publicApprovalBridge.ts) guards the human-approval paths.
pub(super) const METHODS: &[&str] = &["sop_status", "sop_resume"];
pub(super) const HANDLER: super::Handler = super::Handler::AfterAdmission(Requests::sop_request);

fn unavailable(message: &str) -> RequestError {
    RequestError {
        code: "CAPABILITY_UNAVAILABLE".into(),
        message: message.into(),
        details: None,
        validation: None,
    }
}

fn coded(code: &str, message: &str) -> RequestError {
    RequestError {
        code: code.into(),
        message: message.into(),
        details: None,
        validation: None,
    }
}

/// `failure(status, code)` (publicApprovalBridge.ts:20): message is the code,
/// and the WS boundary reports the status as `details.httpStatus`.
fn approval_failure(status: u16, code: &str) -> RequestError {
    RequestError {
        code: code.into(),
        message: code.into(),
        details: Some(Box::new(crate::metadata::object([(
            "httpStatus",
            JsValue::Number(f64::from(status)),
        )]))),
        validation: None,
    }
}

/// `StaffDeckSopControlPlane.store` without a `modules.sop` profile
/// (StaffDeckSopControlPlane.ts:49). The native config never carries the
/// module (the profile validation refuses `modules`), so every control-plane
/// call lands here.
fn sop_module_disabled() -> RequestError {
    coded(
        "SOP_MODULE_DISABLED",
        "The StaffDeck SOP module is not enabled for this project.",
    )
}

/// `nonempty` (publicApprovalBridge.ts:19).
fn nonempty(value: Option<&JsValue>) -> bool {
    match value {
        Some(JsValue::String(text)) => {
            !crate::metadata::js_trim(&String::from_utf16_lossy(text.units())).is_empty()
        }
        _ => false,
    }
}

fn own(params: &JsValue, key: &str) -> bool {
    params.get(key).is_some()
}

impl Requests {
    pub(in crate::native_process) fn sop_request(
        &self,
        method: String,
        params: JsValue,
    ) -> Call<'_, Result<JsValue, RequestError>> {
        Box::pin(async move {
            if method == "sop_status" {
                // createLocalGateway.ts:1072-1080.
                let authorization = super::input_property(&params, "approverAuthorization")?;
                if authorization
                    .as_ref()
                    .is_some_and(crate::metadata::js_truthy)
                {
                    return Err(self.approval_access(&params));
                }
                return Err(sop_module_disabled());
            }
            // InProcessGateway.ts:639-653.
            let session_key = super::input_property(&params, "sessionKey")?;
            if let Some(JsValue::String(key)) = &session_key
                && self.owner.has_active_turn(key)
            {
                return Err(coded(
                    "SESSION_BUSY",
                    "Cannot resume StaffDeck SOP while a turn is active.",
                ));
            }
            // createLocalGateway.ts:1081-1089.
            if own(&params, "authority") || own(&params, "subject") {
                return Err(approval_failure(400, "APPROVAL_AUTHORITY_OVERRIDE"));
            }
            if params
                .get("source")
                .and_then(JsValue::as_string)
                .is_some_and(|source| source.equals_str("human"))
            {
                // publicApprovalBridge.ts:66-69.
                let revision_ok = matches!(
                    params.get("expectedRevision"),
                    Some(JsValue::Number(n)) if n.fract() == 0.0 && *n >= 0.0 && *n <= 9_007_199_254_740_991.0
                );
                if !nonempty(params.get("waitId"))
                    || !nonempty(params.get("requestId"))
                    || !nonempty(params.get("message"))
                    || !revision_ok
                {
                    return Err(approval_failure(400, "APPROVAL_INPUT_INVALID"));
                }
                return Err(self.approval_access(&params));
            }
            Err(sop_module_disabled())
        })
    }

    /// `access(input)` of the public approval bridge
    /// (publicApprovalBridge.ts:31-42) up to the bearer check. Past it the
    /// bridge authenticates against StaffDeck (an external service the
    /// native process has no client for), and a project-scoped binding
    /// (`STAFFDECK_PROJECT_BINDING_ENABLED`) resolves through the same
    /// service; both stay unavailable.
    fn approval_access(&self, params: &JsValue) -> RequestError {
        let unported =
            || unavailable("StaffDeck SOP is not configured on this isolated native process.");
        let env = |key: &str| self.env.get(key).cloned().unwrap_or_default();
        if env("STAFFDECK_PROJECT_BINDING_ENABLED") == "true" {
            return unported();
        }
        let binding = [
            env("STAFFDECK_COPY_TENANT_ID"),
            env("STAFFDECK_COPY_TARGET_AGENT_ID"),
            env("STAFFDECK_COPY_PILOTDECK_USER_ID"),
        ];
        if !nonempty(params.get("sessionKey")) {
            return approval_failure(400, "APPROVAL_INPUT_INVALID");
        }
        if own(params, "authority") || own(params, "subject") {
            return approval_failure(400, "APPROVAL_AUTHORITY_OVERRIDE");
        }
        let mismatched = |key: &str, expected: &str| match params.get(key) {
            None | Some(JsValue::Undefined) => false,
            Some(JsValue::String(value)) => !value.equals_str(expected),
            Some(_) => true,
        };
        if mismatched("tenantId", &binding[0]) || mismatched("agentId", &binding[1]) {
            return approval_failure(403, "APPROVAL_SUBJECT_MISMATCH");
        }
        if !binding
            .iter()
            .all(|value| nonempty(Some(&JsValue::String(JsString::from(value.as_str())))))
        {
            return approval_failure(503, "APPROVAL_BINDING_UNAVAILABLE");
        }
        let bearer_ok = match params.get("approverAuthorization") {
            Some(JsValue::String(value)) => {
                let text = String::from_utf16_lossy(value.units());
                let units: Vec<u16> = text.encode_utf16().collect();
                let head = String::from_utf16_lossy(&units[..units.len().min(7)]);
                let rest = String::from_utf16_lossy(&units[units.len().min(7)..]);
                head.to_lowercase() == "bearer " && !crate::metadata::js_trim(&rest).is_empty()
            }
            _ => false,
        };
        if !bearer_ok {
            return approval_failure(401, "APPROVAL_AUTH_REQUIRED");
        }
        unported()
    }
}

#[cfg(test)]
mod tests {
    use crate::native_process::management_tests::setup;
    use crate::negative_assert_eq;
    use pilotdeck_protocol::JsValue;

    async fn teardown(requests: &crate::native_process::requests::Requests) {
        requests.owner.shutdown().await.expect("sop shutdown");
        requests
            .project
            .dispose()
            .await
            .expect("sop project dispose");
    }

    // Codes and `details.httpStatus` as the baseline answers them with no
    // `modules.sop` and no StaffDeck binding (bbx `sop-disabled`).
    #[tokio::test]
    async fn sop_methods_answer_the_baseline_errors() {
        let (_scratch, requests) = std::panic::catch_unwind(setup).expect("sop fixture setup");
        let object =
            |fields: &[(&'static str, JsValue)]| crate::metadata::object(fields.iter().cloned());
        let human = |extra: &[(&'static str, JsValue)]| {
            let mut fields = vec![
                ("sessionKey", JsValue::from("s1")),
                ("source", "human".into()),
                ("requestId", "r1".into()),
                ("waitId", "w1".into()),
                ("message", "ok".into()),
            ];
            fields.extend(extra.iter().cloned());
            crate::metadata::object(fields)
        };
        for (method, params, code, status) in [
            ("sop_status", object(&[]), "SOP_MODULE_DISABLED", None),
            (
                "sop_status",
                object(&[("sessionKey", "s1".into())]),
                "SOP_MODULE_DISABLED",
                None,
            ),
            (
                "sop_status",
                object(&[
                    ("sessionKey", "s1".into()),
                    ("approverAuthorization", "Bearer x".into()),
                ]),
                "APPROVAL_BINDING_UNAVAILABLE",
                Some(503.0),
            ),
            (
                "sop_status",
                object(&[("approverAuthorization", "Bearer x".into())]),
                "APPROVAL_INPUT_INVALID",
                Some(400.0),
            ),
            ("sop_resume", object(&[]), "SOP_MODULE_DISABLED", None),
            (
                "sop_resume",
                object(&[("sessionKey", "s1".into()), ("subject", "x".into())]),
                "APPROVAL_AUTHORITY_OVERRIDE",
                Some(400.0),
            ),
            (
                "sop_resume",
                human(&[]),
                "APPROVAL_INPUT_INVALID",
                Some(400.0),
            ),
            (
                "sop_resume",
                human(&[("expectedRevision", JsValue::Number(0.0))]),
                "APPROVAL_BINDING_UNAVAILABLE",
                Some(503.0),
            ),
        ] {
            let error = requests
                .sop_request(method.into(), params)
                .await
                .expect_err("sop answers an error");
            negative_assert_eq!(error.code.as_str(), code, "{method} code");
            let http = error
                .details
                .as_deref()
                .and_then(|details| details.get("httpStatus"))
                .and_then(JsValue::as_number);
            negative_assert_eq!(http, status, "{method} {code} httpStatus");
        }
        teardown(&requests).await;
    }

    // InProcessGateway.ts:641: an active turn refuses the resume before the
    // override guard.
    #[tokio::test]
    async fn sop_resume_refuses_an_active_turn_first() {
        let (_scratch, requests) = std::panic::catch_unwind(setup).expect("sop fixture setup");
        let session_key: pilotdeck_protocol::JsString = "s1".into();
        let run: pilotdeck_protocol::JsString = "run-1".into();
        let reservation = requests
            .owner
            .begin_turn(&session_key, &run)
            .expect("reserve active turn");
        let params = crate::metadata::object([
            ("sessionKey", "s1".into()),
            ("authority", "injected".into()),
        ]);
        let error = requests
            .sop_request("sop_resume".into(), params)
            .await
            .expect_err("busy");
        negative_assert_eq!(error.code.as_str(), "SESSION_BUSY", "busy code");
        requests
            .owner
            .end_turn(&session_key, Some(&run), Some(&reservation));
        teardown(&requests).await;
    }
}
