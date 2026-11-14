//! Offline recovery gate: use the public production recorder on exact JSONL.
//! No gateway, provider, tool, writable store, or real home is constructed.
use pilotdeck_gateway::storage::{
    LedgerError, StoreResult, TranscriptKey, TranscriptRead, TranscriptStore, read_transcript,
    recorder::{Recorder, RecorderError},
};
use pilotdeck_protocol::{JsValue, parse_json};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};

struct NoIoStore;

impl TranscriptStore for NoIoStore {
    fn append(&self, _: &TranscriptKey, _: &JsValue) -> StoreResult<()> {
        Err(LedgerError::Invalid(
            "offline restore prohibits append".into(),
        ))
    }

    fn read(&self, _: &TranscriptKey, _: Option<u64>) -> StoreResult<TranscriptRead> {
        Err(LedgerError::Invalid(
            "offline restore prohibits store read".into(),
        ))
    }
}

fn restore(entries: &[JsValue]) -> Result<(), RecorderError> {
    let key = TranscriptKey {
        project_root: PathBuf::new(),
        pilot_home: PathBuf::new(),
        session_id: "offline-restore-validation".into(),
        transcript_path: PathBuf::new(),
    };
    Recorder::new(Arc::new(NoIoStore), key).restore(entries)
}

fn fixture() -> Vec<JsValue> {
    [
        json!({"type":"turn_started"}),
        json!({"type":"accepted_input","messages":[{"role":"user","content":[{"type":"text","text":"synthetic input"}]}]}),
        json!({"type":"step_started","step":1}),
        json!({"type":"model_request","step":1,"request":{"provider":"synthetic","model":"synthetic","messages":[]}}),
        json!({"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":"synthetic answer"}]}}),
        json!({"type":"step_completed","step":1,"outcome":"completed"}),
        json!({"type":"turn_result","result":{"type":"success","sessionId":"offline-session","turnId":"offline-turn","stopReason":"completed","usage":{},"permissionDenials":[],"turns":1,"startedAt":"2026-11-14T00:00:00.000Z","completedAt":"2026-11-14T00:00:00.000Z"}}),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, mut value)| {
        value["sequence"] = json!(index + 1);
        value["entryId"] = json!(format!("offline-entry-{}", index + 1));
        value["parentEntryId"] = if index == 0 {
            serde_json::Value::Null
        } else {
            json!(format!("offline-entry-{index}"))
        };
        value["sessionId"] = json!("offline-session");
        value["turnId"] = json!("offline-turn");
        value["createdAt"] = json!("2026-11-14T00:00:00.000Z");
        parse_json(&value.to_string()).expect("synthetic fixture must encode")
    })
    .collect()
}

fn positive_and_negative_controls() {
    let entries = fixture();
    assert!(restore(&entries).is_ok(), "complete text turn must restore");
    // Keep original sequence/parent links exactly as the production fork did.
    // A gap is legal; the open-step lifecycle must be the reason for rejection.
    let missing_step = entries
        .into_iter()
        .filter(|entry| {
            !entry
                .get("type")
                .and_then(JsValue::as_string)
                .is_some_and(|kind| kind.equals_str("step_completed"))
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(
            restore(&missing_step),
            Err(RecorderError::Domain {
                code: "turn_has_open_step",
                ..
            })
        ),
        "missing completion marker must fail with turn_has_open_step"
    );
}

#[test]
fn complete_turn_and_missing_step_controls() {
    positive_and_negative_controls();
}

fn reject(phase: &str, class: &str, code: &str) -> ! {
    // Never display RecorderError.message, input contents, IDs, or input path.
    println!(
        "RECORDER_RESTORE {}",
        json!({"ok":false,"phase":phase,"class":class,"code":code})
    );
    panic!("offline recorder restore rejected candidate; see structural result");
}

#[test]
#[ignore = "requires explicit PILOTDECK_RESTORE_JSONL absolute candidate path"]
fn candidate_jsonl_restores() {
    // Every external candidate also proves this binary catches the known defect.
    positive_and_negative_controls();
    let Some(input) = std::env::var_os("PILOTDECK_RESTORE_JSONL") else {
        reject("input", "environment", "missing_input");
    };
    let path = PathBuf::from(input);
    if !path.is_absolute() {
        reject("input", "environment", "absolute_path_required");
    }
    let read = match read_transcript(&path) {
        Ok(read) => read,
        Err(_) => reject("read", "io", "transcript_read_failed"),
    };
    // The production reader recovers partial logs. A recovery candidate gate
    // must not certify only its surviving subset or a missing/oversized input.
    if !read.diagnostics.is_empty() {
        reject("read", "diagnostic", "transcript_has_diagnostics");
    }
    if read.entries.is_empty() {
        reject("read", "shape", "empty_candidate");
    }
    match restore(&read.entries) {
        Ok(()) => println!(
            "RECORDER_RESTORE {}",
            json!({"ok":true,"entries":read.entries.len(),"controls":"passed"})
        ),
        Err(RecorderError::Domain { code, .. }) => reject("restore", "domain", code),
        Err(RecorderError::Envelope { code, .. }) => reject("restore", "envelope", code),
        Err(_) => reject("restore", "recorder", "restore_failed"),
    }
}
