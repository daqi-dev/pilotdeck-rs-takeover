use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("pilotdeck-staffdeck-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn json(value: &str) -> JsValue {
    pilotdeck_protocol::parse_json(value).unwrap()
}
fn env() -> Env {
    Env::from([
        ("STAFFDECK_COPY_TENANT_ID".into(), "t".into()),
        ("STAFFDECK_COPY_ACTOR_USER_ID".into(), "u".into()),
        ("STAFFDECK_COPY_PILOTDECK_USER_ID".into(), "owner".into()),
        ("STAFFDECK_COPY_TARGET_AGENT_ID".into(), "a".into()),
        (
            "STAFFDECK_FORMAL_API_ORIGIN".into(),
            "http://127.0.0.1:1".into(),
        ),
        ("STAFFDECK_SOP_MANAGEMENT_CREDENTIAL_ID".into(), "c".into()),
        (
            "STAFFDECK_COPY_USER_TOKEN".into(),
            "actor-test-token".into(),
        ),
        (
            "STAFFDECK_SOP_MANAGEMENT_API_KEY".into(),
            "abcdefghijklmnopqrst-full-test-key".into(),
        ),
    ])
}
fn principal() -> PublicHostPrincipal {
    PublicHostPrincipal {
        tenant_id: "t".into(),
        actor_user_id: "u".into(),
        pilot_deck_user_id: "owner".into(),
        agent_id: "a".into(),
    }
}
#[test]
fn project_selection_requires_the_complete_identity_and_unique_active_binding() {
    let scratch = Scratch::new();
    let mut env = env();
    env.insert("STAFFDECK_PROJECT_BINDING_ENABLED".into(), "true".into());
    std::fs::create_dir_all(scratch.0.join("staffdeck")).unwrap();
    let row = r#"{"projectRoot":"/project-a","agentId":"a","tenantId":"t","origin":"http://127.0.0.1:1","status":"bound"}"#;
    let write = |rows: &str| {
        std::fs::write(
            scratch.0.join("staffdeck/project-bindings.json"),
            format!(r#"{{"version":1,"installationId":"i","projects":[{rows}]}}"#),
        )
        .unwrap()
    };
    write(row);
    assert_eq!(
        project_for_principal(&scratch.0, &env, &principal()).unwrap(),
        Some(PathBuf::from("/project-a"))
    );
    let mut wrong = principal();
    wrong.actor_user_id = "different".into();
    assert_eq!(
        project_for_principal(&scratch.0, &env, &wrong)
            .unwrap_err()
            .code
            .as_deref(),
        Some("PROJECT_BINDING_PRINCIPAL_MISMATCH")
    );
    write(&format!("{row},{row}"));
    assert_eq!(
        project_for_principal(&scratch.0, &env, &principal())
            .unwrap_err()
            .code
            .as_deref(),
        Some("PROJECT_BINDING_NOT_FOUND")
    );
    write("");
    assert!(project_for_principal(&scratch.0, &env, &principal()).is_err());
}
#[test]
fn original_admission_rejects_conflicting_or_user_forged_metadata() {
    let admission = r#"{"tenantId":"t","agentId":"a","actorUserId":"u","pilotDeckUserId":"owner","projectKey":"/p","credentialId":"c","staffDeckOrigin":"http://localhost"}"#;
    let record = |turn: &str, value: &str| {
        json(&format!(
            r#"{{"type":"session_metadata","sessionId":"s","turnId":"{turn}","metadata":{{"staffDeckAdmission":{value}}}}}"#
        ))
    };
    assert!(original_admission(&[record("host-admission", admission)], &"s".into()).is_some());
    assert!(original_admission(&[record("ordinary-turn", admission)], &"s".into()).is_none());
    assert!(
        original_admission(
            &[
                record("host-admission", admission),
                record("host-admission", &admission.replace("\"c\"", "\"other\""))
            ],
            &"s".into()
        )
        .is_none()
    );
    assert!(
        original_admission(
            &[record("host-admission", admission)],
            &"other-session".into()
        )
        .is_none()
    );
}
async fn mock(responses: Vec<(u16, String)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut input = Vec::new();
            let mut buf = [0u8; 2048];
            loop {
                let len = socket.read(&mut buf).await.unwrap();
                assert!(len > 0);
                input.extend_from_slice(&buf[..len]);
                if input.windows(4).any(|v| v == b"\r\n\r\n") {
                    break;
                }
            }
            requests.push(String::from_utf8(input).unwrap());
            socket.write_all(format!("HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    (origin, task)
}
#[tokio::test]
async fn admission_validates_actor_credential_and_exact_target_using_distinct_bearers() {
    let (origin, task) = mock(vec![
        (200, r#"{"id":"u","tenant_id":"t","source":"web","disabled":false}"#.into()),
        (200, r#"[{"id":"c","user_id":"u","status":"active","access":"user_full_access","key_prefix":"abcdefghijklmnopqrst…"}]"#.into()),
        (200, r#"{"id":"a","status":"active","is_overall":false}"#.into()),
    ]).await;
    let mut env = env();
    env.insert("STAFFDECK_FORMAL_API_ORIGIN".into(), origin.clone());
    let result = authenticate_admission(&env, Path::new("/p"), &"owner".into(), None)
        .await
        .unwrap();
    assert_eq!(text(&result, "credentialId").as_deref(), Some("c"));
    assert_eq!(
        text(&result, "staffDeckOrigin").as_deref(),
        Some(origin.as_str())
    );
    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].contains("GET /api/auth/me HTTP/1.1"));
    assert!(requests[1].contains("GET /api/auth/me/api-credentials HTTP/1.1"));
    assert!(requests[2].contains("GET /api/v1/agents/a HTTP/1.1"));
    assert!(requests[0].contains("authorization: Bearer actor-test-token"));
    assert!(requests[2].contains("authorization: Bearer abcdefghijklmnopqrst-full-test-key"));
    let serialized = pilotdeck_protocol::stringify_json(&result)
        .unwrap()
        .unwrap();
    assert!(!serialized.contains("test-token"));
    assert!(!serialized.contains("test-key"));
}
#[tokio::test]
async fn revoked_admission_stops_before_target_and_approver_uses_own_bearer() {
    let (origin, task) = mock(vec![
        (200, r#"{"id":"u","tenant_id":"t","source":"web"}"#.into()),
        (200, r#"[{"id":"c","user_id":"u","status":"revoked","access":"user_full_access","key_prefix":"abcdefghijklmnopqrst…"}]"#.into()),
    ]).await;
    let mut env = env();
    env.insert("STAFFDECK_FORMAL_API_ORIGIN".into(), origin);
    assert!(
        authenticate_admission(&env, Path::new("/p"), &"owner".into(), None)
            .await
            .is_none()
    );
    assert_eq!(task.await.unwrap().len(), 2);
    let (origin, task) = mock(vec![(
        200,
        r#"{"id":"reviewer","tenant_id":"t","source":"web","role":"member"}"#.into(),
    )])
    .await;
    env.insert("STAFFDECK_FORMAL_API_ORIGIN".into(), origin);
    env.insert("STAFFDECK_APPROVAL_USER_ID".into(), "reviewer".into());
    let result = FixedApprovalAuthenticator(env)
        .authenticate(&"reviewer-test-token".into())
        .await
        .unwrap();
    assert_eq!(result.user_id, JsString::from("reviewer"));
    let requests = task.await.unwrap();
    assert!(requests[0].contains("authorization: Bearer reviewer-test-token"));
    assert!(!requests[0].contains("actor-test-token"));
}
#[tokio::test]
async fn approver_rejection_keeps_status_and_never_accepts_foreign_tenant() {
    for (status, body, expected) in [
        (401, "{}", "APPROVAL_AUTH_REJECTED"),
        (
            200,
            r#"{"id":"reviewer","tenant_id":"foreign","source":"web","role":"admin"}"#,
            "APPROVAL_SUBJECT_MISMATCH",
        ),
    ] {
        let (origin, task) = mock(vec![(status, body.into())]).await;
        let mut env = env();
        env.insert("STAFFDECK_FORMAL_API_ORIGIN".into(), origin);
        env.insert("STAFFDECK_APPROVAL_USER_ID".into(), "reviewer".into());
        let error = FixedApprovalAuthenticator(env)
            .authenticate(&"reviewer-test-token".into())
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, Some(JsValue::from(expected)));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn transcript_filename_alias_does_not_bypass_original_catalog() {
    let scratch = Scratch::new();
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let chat = crate::session_chat_dir(&scratch.0, &project, &project.to_string_lossy());
    std::fs::create_dir_all(&chat).unwrap();
    let admission = r#"{"tenantId":"t","agentId":"a","actorUserId":"u","pilotDeckUserId":"owner","projectKey":"/p","credentialId":"c","staffDeckOrigin":"http://localhost"}"#;
    let record = format!(
        r#"{{"type":"session_metadata","metadata":{{"staffDeckAdmission":{admission}}},"sessionId":"s/a","turnId":"host-admission","sequence":1,"createdAt":"2026-01-01T00:00:00.000Z","entryId":"e","parentEntryId":null}}"#
    );
    std::fs::write(chat.join("s_a.jsonl"), record + "\n").unwrap();
    assert!(!catalog_has_session(&scratch.0, &project, &"s/a".into()).await);
    assert!(catalog_has_session(&scratch.0, &project, &"s_a".into()).await);
}
