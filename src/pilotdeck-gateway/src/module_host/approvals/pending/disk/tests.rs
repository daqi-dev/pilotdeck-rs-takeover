use super::*;
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("pdrs-sop-disk-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join("sessions")).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn store(root: &Path) -> DiskSopStore {
    let root = root.to_string_lossy().into_owned();
    DiskSopStore::new(Arc::new(move |_| Some(root.clone())))
}
fn seed(root: &Path, id: &str) {
    let value = format!(
        r#"{{"schemaVersion":3,"sessionId":"{id}","bundle":{{"sops":[{{"id":"sop","version":"1","content":{{"nodes":[]}}}}]}},"state":{{"status":"waiting_external_task","selected_skill_id":"sop","active_step_id":"node","slots_json":{{"before":true}}}},"revision":2,"wait":{{"id":"wait","kind":"external_task","createdAt":"2026-01-01T00:00:00.000Z"}},"resumeRequests":{{}},"updatedAt":"2026-01-01T00:00:00.000Z"}}"#
    );
    fs::write(state_path(&root.join("sessions"), &id.into()), value).unwrap();
}
fn input(id: &str, request: &str) -> StoreResumeInput {
    StoreResumeInput {
        session_key: id.into(),
        project_key: None,
        request_id: request.into(),
        wait_id: "wait".into(),
        message: "done".into(),
        expected_revision: Some(2.0),
        slot_updates: Some(crate::metadata::object([("after", JsValue::Bool(true))])),
        source: ResumeSource::ExternalTask,
        authority: None,
    }
}
#[tokio::test]
async fn disk_resume_survives_restart_replays_receipt_and_preserves_authority_revision() {
    let scratch = Scratch::new();
    seed(&scratch.0, "s/a");
    let original = store(&scratch.0).resume(&input("s/a", "r")).await.unwrap();
    assert_eq!(original.get("revision"), Some(&JsValue::Number(3.0)));
    let after_restart = store(&scratch.0).resume(&input("s/a", "r")).await.unwrap();
    assert_eq!(after_restart.get("duplicate"), Some(&JsValue::Bool(true)));
    let status_input = StoreStatusInput {
        session_key: "s/a".into(),
        project_key: None,
    };
    let authority = store(&scratch.0)
        .authority(&status_input, 3.0)
        .await
        .unwrap();
    assert_eq!(authority.get("sopId"), Some(&JsValue::from("sop")));
    let stale = store(&scratch.0)
        .authority(&status_input, 2.0)
        .await
        .unwrap_err();
    assert_eq!(stale.code, Some(JsValue::from("SOP_REVISION_CONFLICT")));
    assert!(
        store(&scratch.0)
            .status(&StoreStatusInput {
                session_key: "s_a".into(),
                project_key: None
            })
            .await
            .unwrap()
            .is_none()
    );
    let saved = read_state(&state_path(&scratch.0.join("sessions"), &"s/a".into()))
        .unwrap()
        .unwrap();
    assert!(saved.get("wait").is_none());
    assert_eq!(
        saved
            .get("state")
            .unwrap()
            .get("slots_json")
            .unwrap()
            .get("before"),
        Some(&JsValue::Bool(true))
    );
    assert_eq!(
        saved
            .get("state")
            .unwrap()
            .get("slots_json")
            .unwrap()
            .get("after"),
        Some(&JsValue::Bool(true))
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_store_instances_serialize_the_same_wait() {
    let scratch = Scratch::new();
    seed(&scratch.0, "s");
    let a = store(&scratch.0);
    let b = store(&scratch.0);
    let first = input("s", "r1");
    let second = input("s", "r2");
    let (a, b) = tokio::join!(a.resume(&first), b.resume(&second));
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let error = a.err().or(b.err()).unwrap();
    assert_eq!(error.code, Some(JsValue::from("SOP_NOT_WAITING")));
}
#[tokio::test]
async fn corruption_wrong_session_and_inactive_state_never_produce_authority() {
    let scratch = Scratch::new();
    seed(&scratch.0, "s");
    let query = StoreStatusInput {
        session_key: "s".into(),
        project_key: None,
    };
    assert_eq!(
        store(&scratch.0)
            .authority(&query, 2.0)
            .await
            .unwrap_err()
            .code,
        Some(JsValue::from("SOP_AUTHORITY_STATE_INACTIVE"))
    );
    let path = state_path(&scratch.0.join("sessions"), &"s".into());
    fs::write(&path, "{}").unwrap();
    assert!(store(&scratch.0).status(&query).await.is_err());
    fs::write(&path, "{").unwrap();
    assert!(store(&scratch.0).resume(&input("s", "r")).await.is_err());
}

#[tokio::test]
async fn resume_retains_only_valid_reply_delivery() {
    let valid = pilotdeck_protocol::parse_json(r#"{"turnId":"t","phase":"pending","result":{"status":"completed","replyFragment":"done","slotUpdates":{},"events":[]},"committedAt":"2026-01-01T00:00:00.000Z"}"#).unwrap();
    for delivery in [crate::metadata::object([]), valid.clone()] {
        let scratch = Scratch::new();
        seed(&scratch.0, "s");
        let path = state_path(&scratch.0.join("sessions"), &"s".into());
        let mut raw = read_state(&path).unwrap().unwrap();
        let JsValue::Object(ref mut fields) = raw else {
            unreachable!()
        };
        fields.insert("replyDelivery".into(), delivery.clone());
        fs::write(
            &path,
            pilotdeck_protocol::stringify_json(&raw).unwrap().unwrap(),
        )
        .unwrap();
        store(&scratch.0).resume(&input("s", "r")).await.unwrap();
        let persisted = read_state(&path).unwrap().unwrap();
        if delivery == valid {
            assert_eq!(persisted.get("replyDelivery"), Some(&valid));
        } else {
            assert!(persisted.get("replyDelivery").is_none());
        }
    }
}
