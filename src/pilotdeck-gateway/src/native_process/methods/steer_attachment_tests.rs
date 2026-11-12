use super::*;
use crate::runtime::turn::gateway::GatewayRequests;
use pilotdeck_protocol::parse_json;
use std::{fs, path::PathBuf};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("steer-attachments-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("project")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        fs::write(
            root.join("external/registered.txt"),
            "REGISTERED-STEER-CONTENT\n",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            root.join("external/registered.txt"),
            root.join("project/link.txt"),
        )
        .unwrap();
        #[cfg(not(unix))]
        fs::copy(
            root.join("external/registered.txt"),
            root.join("project/link.txt"),
        )
        .unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn substitute(value: &JsValue, root: &str) -> JsValue {
    match value {
        JsValue::String(value) => JsValue::String(
            String::from_utf16_lossy(value.units())
                .replace("<fixture>", root)
                .into(),
        ),
        JsValue::Array(items) => {
            JsValue::Array(items.iter().map(|v| substitute(v, root)).collect())
        }
        JsValue::Object(fields) => {
            let mut result = pilotdeck_protocol::JsObject::new();
            for (key, value) in fields.own_entries() {
                result.insert(key.clone(), substitute(value, root));
            }
            JsValue::Object(result)
        }
        _ => value.clone(),
    }
}
fn array(value: &JsValue) -> &[JsValue] {
    match value {
        JsValue::Array(items) => items,
        _ => panic!("array"),
    }
}
#[tokio::test]
async fn attachment_projection_matches_actual_frozen_steer_oracle() {
    let fixture = Scratch::new();
    let oracle = parse_json(include_str!(
        "../../../tests/fixtures/steer-attachments-native/oracle.json"
    ))
    .unwrap();
    let oracle = substitute(&oracle, &fixture.0.to_string_lossy());
    let mut checked = 0;
    for case in array(oracle.get("records").unwrap()) {
        let name = case.get("name").unwrap().as_string().unwrap();
        if name.equals_str("no-active-invalid") || name.equals_str("wrong-run-invalid") {
            continue;
        }
        #[cfg(not(unix))]
        if name.equals_str("path-registered") {
            continue;
        }
        let input = case.get("input").unwrap();
        let message = input.get("message").cloned().unwrap_or(JsValue::Undefined);
        let result = match steer_attachments(input.get("attachments")) {
            Err(error) => Err(error),
            Ok(attachments) => {
                prepare_steer_content(fixture.0.join("project"), message, attachments).await
            }
        };
        if let Some(error) = case.get("error") {
            assert_eq!(
                &result.unwrap_err().message,
                error.get("message").unwrap().as_string().unwrap(),
                "{name:?}"
            );
        } else {
            let (content, allowed) = result.unwrap();
            let calls = array(case.get("calls").unwrap());
            assert_eq!(calls.len(), 1);
            let item = calls[0].get("item").unwrap();
            // The captured JSON omits object properties whose value is real
            // JS undefined; compare the same wire serialization, preserving
            // key order and UTF-16 rather than inventing null for that case.
            assert_eq!(
                pilotdeck_protocol::stringify_json(&content).unwrap(),
                pilotdeck_protocol::stringify_json(
                    item.get("message").unwrap().get("content").unwrap()
                )
                .unwrap(),
                "{name:?}"
            );
            assert_eq!(
                JsValue::Array(allowed.into_iter().map(JsValue::String).collect()),
                *item.get("allowedReadFiles").unwrap(),
                "{name:?}"
            );
        }
        checked += 1;
    }
    #[cfg(unix)]
    assert_eq!(checked, 24);
    #[cfg(not(unix))]
    assert_eq!(checked, 23);
}

#[tokio::test]
async fn real_request_enqueues_attachment_content_and_registered_read_authority() {
    let fixture = Scratch::new();
    let (_scratch, requests) = crate::native_process::management_tests::setup();
    let _reservation = requests
        .owner
        .begin_turn(&"session".into(), &"run".into())
        .unwrap();
    let lease = requests.project.acquire(None).unwrap();
    let resource = lease
        .runtime()
        .resources()
        .get::<crate::runtime::project::ProjectResources>()
        .unwrap()
        .context()
        .resource(crate::runtime::project::Resource::InteractionSteer)
        .unwrap();
    let steer = resource
        .as_any()
        .downcast_ref::<crate::runtime::project::interaction_steer::InteractionSteer>()
        .unwrap()
        .clone();
    let _turn = steer
        .bind_session("session".into(), None)
        .unwrap()
        .start_turn(
            "turn".into(),
            "run".into(),
            vec![],
            crate::runtime::hooks::Cancellation::new(),
            &mut crate::host::modules::ModuleRouter::default(),
        )
        .unwrap();
    let path = fixture.0.join("project/link.txt");
    let input = object([
        ("sessionKey", "session".into()),
        ("runId", "run".into()),
        ("itemId", "item".into()),
        ("message", "steered".into()),
        (
            "attachments",
            JsValue::Array(vec![object([
                ("type", "text".into()),
                ("path", path.to_string_lossy().as_ref().into()),
                ("metadata", object([("channelKey", "fixture".into())])),
            ])]),
        ),
        (
            "uploadedAttachments",
            JsValue::Array(vec![object([("uploadId", "never-acquired".into())])]),
        ),
    ]);
    let result = requests.request("steer_turn".into(), input).await.unwrap();
    assert_eq!(result.get("accepted"), Some(&JsValue::Bool(true)));
    let info = crate::runtime::project::interaction_steer::active_turn_for(
        &requests.project,
        &"session".into(),
        Some(&"run".into()),
    )
    .unwrap();
    let queued = info.mailbox.drain(&"turn".into());
    assert_eq!(queued.len(), 1);
    let content = array(queued[0].message.get("content").unwrap());
    assert!(content.iter().any(|block| {
        block
            .get("text")
            .and_then(JsValue::as_string)
            .is_some_and(|text| {
                String::from_utf16_lossy(text.units()).contains("REGISTERED-STEER-CONTENT")
            })
    }));
    let allowed = queued[0].allowed_read_files.as_ref().unwrap();
    assert!(allowed.contains(&JsString::from(path.to_string_lossy().as_ref())));
    #[cfg(unix)]
    assert!(
        allowed.contains(&JsString::from(
            fixture
                .0
                .join("external/registered.txt")
                .to_string_lossy()
                .as_ref()
        ))
    );
    requests.owner.shutdown().await.unwrap();
    requests.project.dispose().await.unwrap();
}
