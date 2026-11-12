use super::super::requests::Requests;
use crate::{
    module_host::{
        approvals::{
            self, ApprovalFailure, ApprovalPendingStore, ResumeSource, StoreResumeInput,
            StoreStatusInput,
        },
        runtime::RuntimeHost,
        staffdeck,
    },
    runtime::connection::{Call, RequestError},
};
use pilotdeck_protocol::{JsString, JsValue};
use std::sync::Arc;

pub(super) const METHODS: &[&str] = &["sop_status", "sop_resume"];
pub(super) const HANDLER: super::Handler = super::Handler::AfterAdmission(Requests::sop_request);
fn coded(code: &str, message: &str) -> RequestError {
    RequestError {
        code: code.into(),
        message: message.into(),
        details: None,
        validation: None,
    }
}
fn approval_failure(status: u16, code: &str) -> RequestError {
    RequestError {
        details: Some(Box::new(crate::metadata::object([(
            "httpStatus",
            JsValue::Number(f64::from(status)),
        )]))),
        ..coded(code, code)
    }
}
fn failed(error: ApprovalFailure, params: &JsValue) -> RequestError {
    let code = error
        .code
        .as_ref()
        .and_then(JsValue::as_string)
        .map(|v| String::from_utf16_lossy(v.units()))
        .unwrap_or_else(|| "gateway_request_failed".into());
    if let Some(status) = error.status {
        return approval_failure(status, &code);
    }
    let message = match code.as_str() {
        "SOP_SESSION_NOT_FOUND" => format!(
            "No StaffDeck SOP state exists for '{}'.",
            string(params, "sessionKey").to_utf8().unwrap_or_default()
        ),
        "SOP_RESUME_REQUEST_CONFLICT" => {
            "This request id already belongs to a different SOP resume payload.".into()
        }
        "SOP_APPROVAL_LEGACY_RECEIPT" => "Old receipt has no authenticated approval facts.".into(),
        _ => staffdeck::sop_error_message(&code).into(),
    };
    coded(&code, &message)
}
fn string(params: &JsValue, key: &str) -> JsString {
    params
        .get(key)
        .and_then(JsValue::as_string)
        .cloned()
        .unwrap_or_default()
}
impl Requests {
    pub(in crate::native_process) fn sop_request(
        &self,
        method: String,
        params: JsValue,
    ) -> Call<'_, Result<JsValue, RequestError>> {
        Box::pin(async move {
            let session = super::input_property(&params, "sessionKey")?
                .and_then(|v| v.as_string().cloned())
                .unwrap_or_default();
            let host = Arc::new(RuntimeHost {
                home: self.options.home.clone(),
                project: self.options.project.clone(),
                builtin_skills_root: self.options.builtin_skills_root.clone(),
                env: self.env.clone(),
                registry: Arc::clone(&self.project),
            });
            if method == "sop_status" {
                let authorization = super::input_property(&params, "approverAuthorization")?;
                if authorization
                    .as_ref()
                    .is_some_and(crate::metadata::js_truthy)
                {
                    return approvals::production_bridge(host)
                        .status(&params)
                        .await
                        .map_err(|e| failed(e, &params));
                }
                let snapshot = approvals::production_store(host)
                    .status(&StoreStatusInput {
                        session_key: session,
                        project_key: params.get("projectKey").cloned(),
                    })
                    .await
                    .map_err(|e| failed(e, &params))?;
                let Some(snapshot) = snapshot else {
                    return Ok(JsValue::Null);
                };
                if snapshot
                    .get("wait")
                    .and_then(|wait| wait.get("kind"))
                    .and_then(JsValue::as_string)
                    .is_some_and(|kind| kind.equals_str("external_task"))
                {
                    let mut result = pilotdeck_protocol::JsObject::new();
                    if let Some(fields) = snapshot.as_object() {
                        for (key, value) in fields.own_entries() {
                            if !key.equals_str("approval") && !key.equals_str("approvalError") {
                                result.insert(key.clone(), value.clone());
                            }
                        }
                    }
                    return Ok(JsValue::Object(result));
                }
                if !snapshot.get("wait").is_some_and(crate::metadata::js_truthy) {
                    return Ok(JsValue::Null);
                }
                return Err(approval_failure(401, "APPROVAL_AUTH_REQUIRED"));
            }
            if self.owner.has_active_turn(&session) {
                return Err(coded(
                    "SESSION_BUSY",
                    "Cannot resume StaffDeck SOP while a turn is active.",
                ));
            }
            if params.get("authority").is_some() || params.get("subject").is_some() {
                return Err(approval_failure(400, "APPROVAL_AUTHORITY_OVERRIDE"));
            }
            if string(&params, "source").equals_str("human") {
                return approvals::production_bridge(host)
                    .resume(&params)
                    .await
                    .map_err(|e| failed(e, &params));
            }
            approvals::production_store(host)
                .resume(&StoreResumeInput {
                    session_key: session,
                    project_key: params.get("projectKey").cloned(),
                    request_id: string(&params, "requestId"),
                    wait_id: string(&params, "waitId"),
                    message: string(&params, "message"),
                    source: if string(&params, "source").equals_str("external_task") {
                        ResumeSource::ExternalTask
                    } else {
                        ResumeSource::Invalid
                    },
                    expected_revision: params.get("expectedRevision").and_then(JsValue::as_number),
                    slot_updates: params.get("slotUpdates").cloned(),
                    authority: None,
                })
                .await
                .map_err(|e| failed(e, &params))
        })
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
