//! Source-proven fork closure controls. These exercise the real recorder
//! validator and a fresh recorder continuation, without a provider or a home.
use super::*;
use crate::storage::{
    StoreResult, TranscriptKey, TranscriptRead, TranscriptStore, recorder::Recorder,
};
use pilotdeck_protocol::parse_json;
use std::sync::{Arc, Mutex};

fn json(text: &str) -> JsValue {
    parse_json(text).unwrap()
}

fn entry(kind: &str, sequence: u64, turn: &str, payload: &str) -> JsValue {
    let value = json(payload);
    let value = with(&value, "type", kind.into());
    let value = with(&value, "sessionId", "source".into());
    let value = with(&value, "turnId", turn.into());
    let value = with(&value, "sequence", JsValue::Number(sequence as f64));
    let value = with(
        &value,
        "entryId",
        JsValue::String(format!("entry-{sequence}").into()),
    );
    with(
        &value,
        "parentEntryId",
        if sequence == 1 {
            JsValue::Null
        } else {
            JsValue::String(format!("entry-{}", sequence - 1).into())
        },
    )
}

fn text_turn() -> Vec<JsValue> {
    vec![
        entry("turn_started", 1, "turn-1", "{}"),
        entry(
            "accepted_input",
            2,
            "turn-1",
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"question"}]}]}"#,
        ),
        entry("step_started", 3, "turn-1", r#"{"step":1}"#),
        entry("model_request", 4, "turn-1", r#"{"step":1,"request":{}}"#),
        entry(
            "assistant_message",
            5,
            "turn-1",
            r#"{"message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}"#,
        ),
        entry(
            "step_completed",
            6,
            "turn-1",
            r#"{"step":1,"outcome":"completed"}"#,
        ),
        entry(
            "turn_result",
            7,
            "turn-1",
            r#"{"result":{"type":"success","usage":{},"permissionDenials":[],"sessionId":"source","turnId":"turn-1","finalMessage":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}}"#,
        ),
    ]
}

fn fork(entries: &[JsValue], id: &str, resume: bool) -> Result<Vec<JsValue>, ReplacementError> {
    let point = find_fork_point(entries, &id.into(), resume)?;
    closed_fork_prefix(entries, &point)
}

fn assert_chain(entries: &[JsValue]) {
    let mut previous = &JsValue::Null;
    for entry in entries {
        assert_eq!(field(entry, "parentEntryId"), previous);
        previous = field(entry, "entryId");
    }
}

#[derive(Default)]
struct Memory(Mutex<Vec<JsValue>>);
impl TranscriptStore for Memory {
    fn append(&self, _: &TranscriptKey, entry: &JsValue) -> StoreResult<()> {
        self.0.lock().unwrap().push(entry.clone());
        Ok(())
    }
    fn read(&self, _: &TranscriptKey, _: Option<u64>) -> StoreResult<TranscriptRead> {
        Ok(TranscriptRead {
            entries: self.0.lock().unwrap().clone(),
            diagnostics: vec![],
        })
    }
}

#[test]
fn assistant_fork_closes_source_step_and_fresh_recorder_continues() {
    let source = text_turn();
    let point = find_fork_point(&source, &"entry-5".into(), false).unwrap();
    let old = source
        .iter()
        .filter(|entry| should_preserve(entry, &point))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        validate_fork_entries(&old)
            .unwrap_err()
            .message
            .contains("step 1 open")
    );
    let closed = closed_fork_prefix(&source, &point).unwrap();
    assert_eq!(closed, source);
    assert_chain(&closed);
    let store = Arc::new(Memory::default());
    let recorder = Recorder::new(
        store.clone(),
        TranscriptKey {
            project_root: PathBuf::new(),
            pilot_home: PathBuf::new(),
            session_id: "child".into(),
            transcript_path: PathBuf::new(),
        },
    );
    recorder.restore(&closed).unwrap();
    recorder
        .start_turn(&"child".into(), &"next".into())
        .unwrap();
    recorder
        .record_model_request(&"child".into(), &"next".into(), json("{}"))
        .unwrap();
    recorder
        .complete_turn(
            &"child".into(),
            &"next".into(),
            json(r#"{"type":"success","usage":{},"permissionDenials":[],"turnId":"next"}"#),
        )
        .unwrap();
    let mut continued = closed;
    continued.extend(store.0.lock().unwrap().clone());
    validate_fork_entries(&continued).unwrap();
    assert_chain(&continued);
    assert_eq!(source, text_turn());
}

#[test]
fn assistant_fork_second_generation_keeps_exact_body_and_closure() {
    let source = text_turn();
    let first = fork(&source, "entry-5", false)
        .unwrap()
        .iter()
        .map(|entry| retarget_entry(entry, &"child".into()).unwrap())
        .collect::<Vec<_>>();
    let second = fork(&first, "entry-5", false).unwrap();
    assert_eq!(second.len(), source.len());
    assert_eq!(field(&second[4], "message"), field(&first[4], "message"));
    assert_chain(&second);
    validate_fork_entries(&second).unwrap();
}

#[test]
fn assistant_fork_keeps_completed_tool_steps_and_terminal_artifact_references() {
    let mut source = text_turn();
    let final_message = source[4].clone();
    source.truncate(4);
    source.push(entry(
        "tool_call",
        5,
        "turn-1",
        r#"{"step":1,"call":{"id":"read-1","name":"read_file"}}"#,
    ));
    source.push(entry(
        "tool_result",
        6,
        "turn-1",
        r#"{"step":1,"result":{"toolCallId":"read-1","content":"read result"}}"#,
    ));
    source.push(entry("tool_result_message", 7, "turn-1", r#"{"message":{"role":"tool","content":[{"type":"tool_result","toolCallId":"read-1","content":"read result"}]}}"#));
    source.push(entry(
        "step_completed",
        8,
        "turn-1",
        r#"{"step":1,"outcome":"completed"}"#,
    ));
    source.push(entry("step_started", 9, "turn-1", r#"{"step":2}"#));
    source.push(entry(
        "model_request",
        10,
        "turn-1",
        r#"{"step":2,"request":{}}"#,
    ));
    source.push(entry("assistant_message", 11, "turn-1", "{}"));
    source[10] = with(
        &source[10],
        "message",
        field(&final_message, "message").clone(),
    );
    source.push(entry(
        "step_completed",
        12,
        "turn-1",
        r#"{"step":2,"outcome":"completed"}"#,
    ));
    source.push(entry(
        "file_artifacts",
        13,
        "turn-1",
        r#"{"artifacts":[{"id":"artifact-1","name":"artifact.txt","path":"artifact.txt","operation":"created","source":"tool","status":"complete","size":1,"sha256":"digest","createdAt":"2026-11-14T00:00:00.000Z"}]}"#,
    ));
    source.push(entry("turn_result", 14, "turn-1", r#"{"result":{"type":"success","usage":{},"permissionDenials":[],"finalMessage":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}}"#));
    validate_fork_entries(&source).unwrap();
    let closed = fork(&source, "entry-11", false).unwrap();
    assert_eq!(closed, source);
    assert_chain(&closed);
}

#[test]
fn middle_assistant_rejects_later_step_and_response_without_mutating_source() {
    let mut source = text_turn();
    source.pop();
    source.push(entry("step_started", 7, "turn-1", r#"{"step":2}"#));
    source.push(entry(
        "model_request",
        8,
        "turn-1",
        r#"{"step":2,"request":{}}"#,
    ));
    source.push(entry(
        "assistant_message",
        9,
        "turn-1",
        r#"{"message":{"role":"assistant","content":[{"type":"text","text":"later answer"}]}}"#,
    ));
    source.push(entry(
        "step_completed",
        10,
        "turn-1",
        r#"{"step":2,"outcome":"completed"}"#,
    ));
    source.push(entry(
        "turn_result",
        11,
        "turn-1",
        r#"{"result":{"type":"success","usage":{},"permissionDenials":[]}}"#,
    ));
    let before = source.clone();
    validate_fork_entries(&source).unwrap();
    let error = fork(&source, "entry-5", false).unwrap_err();
    assert_eq!(error.code, Some("fork_cutoff_not_closed"));
    assert!(error.message.contains("step_started"));
    assert_eq!(source, before);
}

#[test]
fn assistant_fork_rejects_missing_terminal_even_when_restore_accepts_eof() {
    let source = text_turn()[..5].to_vec();
    validate_fork_entries(&source).unwrap();
    assert_eq!(
        fork(&source, "entry-5", false).unwrap_err().code,
        Some("fork_cutoff_not_closed")
    );
}

#[test]
fn assistant_fork_rejects_a_future_tool_result_needed_to_close_the_step() {
    let base = text_turn();
    let mut source = base[..4].to_vec();
    source.push(entry(
        "tool_call",
        5,
        "turn-1",
        r#"{"step":1,"call":{"id":"pending-tool"}}"#,
    ));
    source.push(entry("assistant_message", 6, "turn-1", "{}"));
    source[5] = with(&source[5], "message", field(&base[4], "message").clone());
    source.push(entry(
        "tool_result",
        7,
        "turn-1",
        r#"{"step":1,"result":{"toolCallId":"pending-tool"}}"#,
    ));
    source.push(entry(
        "step_completed",
        8,
        "turn-1",
        r#"{"step":1,"outcome":"completed"}"#,
    ));
    source.push(entry(
        "turn_result",
        9,
        "turn-1",
        r#"{"result":{"type":"success","usage":{},"permissionDenials":[]}}"#,
    ));
    validate_fork_entries(&source).unwrap();
    assert!(
        fork(&source, "entry-6", false)
            .unwrap_err()
            .message
            .contains("tool_result")
    );
}

#[test]
fn assistant_fork_rejects_future_lifecycle_closures_and_compaction_content() {
    for kind in [
        "question_completed",
        "permission_completed",
        "compaction_completed",
        "control_boundary",
        "tool_result",
        "inbox_mutation",
    ] {
        let mut source = text_turn();
        source.insert(5, entry(kind, 6, "turn-1", "{}"));
        assert_eq!(
            fork(&source, "entry-5", false).unwrap_err().code,
            Some("fork_cutoff_not_closed"),
            "{kind}"
        );
    }
}

#[test]
fn assistant_fork_rejects_future_final_message_without_later_message_event() {
    let mut source = text_turn();
    source[6] = with(
        &source[6],
        "result",
        json(
            r#"{"type":"success","usage":{},"permissionDenials":[],"finalMessage":{"role":"assistant","content":[{"type":"text","text":"future response"}]}}"#,
        ),
    );
    assert!(
        fork(&source, "entry-5", false)
            .unwrap_err()
            .message
            .contains("response beyond")
    );
}

#[test]
fn accepted_input_resume_preserves_baseline_prefix_semantics() {
    let source = text_turn();
    let prefill = fork(&source, "entry-2", false).unwrap();
    assert_eq!(prefill, source[..1]);
    let resume = fork(&source, "entry-2", true).unwrap();
    assert_eq!(resume, source[..2]);
    assert_chain(&resume);
    validate_fork_entries(&resume).unwrap();
    let store = Arc::new(Memory::default());
    let recorder = Recorder::new(
        store.clone(),
        TranscriptKey {
            project_root: PathBuf::new(),
            pilot_home: PathBuf::new(),
            session_id: "resume-child".into(),
            transcript_path: PathBuf::new(),
        },
    );
    recorder.restore(&resume).unwrap();
    recorder
        .start_turn(&"resume-child".into(), &"resumed-next".into())
        .unwrap();
    recorder
        .record_model_request(&"resume-child".into(), &"resumed-next".into(), json("{}"))
        .unwrap();
    recorder
        .complete_turn(
            &"resume-child".into(),
            &"resumed-next".into(),
            json(r#"{"type":"success","usage":{},"permissionDenials":[],"turnId":"resumed-next"}"#),
        )
        .unwrap();
    let mut continued = resume;
    continued.extend(store.0.lock().unwrap().clone());
    validate_fork_entries(&continued).unwrap();
}

#[test]
fn legacy_assistant_without_domain_lifecycle_remains_restorable() {
    let source = vec![
        entry("accepted_input", 1, "legacy", r#"{"messages":[]}"#),
        entry(
            "assistant_message",
            2,
            "legacy",
            r#"{"message":{"role":"assistant","content":[]}}"#,
        ),
    ];
    assert_eq!(fork(&source, "entry-2", false).unwrap(), source);
}

#[test]
fn accepted_resume_rejects_an_open_step_at_the_cutoff() {
    let mut source = text_turn();
    source.swap(1, 2);
    for (index, record) in source.iter_mut().enumerate() {
        *record = with(record, "sequence", JsValue::Number((index + 1) as f64));
        *record = with(
            record,
            "entryId",
            JsValue::String(format!("entry-{}", index + 1).into()),
        );
        *record = with(
            record,
            "parentEntryId",
            if index == 0 {
                JsValue::Null
            } else {
                JsValue::String(format!("entry-{index}").into())
            },
        );
    }
    assert_eq!(
        fork(&source, "entry-3", true).unwrap_err().code,
        Some("fork_cutoff_not_closed")
    );
}

struct IsolatedFork {
    root: PathBuf,
    options: Options,
}
impl IsolatedFork {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("pilotdeck-fork-lifecycle-{}", uuid::Uuid::new_v4()));
        let home = root.join("pilot-home");
        let project = root.join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let options = Options {
            home: home.clone(),
            project: project.clone(),
            project_root: project,
            listen: "127.0.0.1:0".parse().unwrap(),
            token_file: root.join("unused-token"),
            sidecar: root.join("unused-sidecar"),
            sidecar_args: vec![],
            drain_budget: std::time::Duration::from_secs(1),
            storage_root: None,
            os_home: Some(root.join("os-home")),
            builtin_skills_root: None,
            builtin_plugins_root: None,
            in_process_cli: false,
            telemetry: None,
            telemetry_context: None,
            telemetry_reload: false,
        };
        Self { root, options }
    }
    fn path(&self, session: &JsString) -> PathBuf {
        let project = &self.options.project;
        crate::session_chat_dir(&self.options.home, project, &project.to_string_lossy()).join(
            format!("{}.jsonl", crate::storage::sanitize_session_id(session)),
        )
    }
}
impl Drop for IsolatedFork {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn product_node_fork_writes_restorable_history_and_fresh_next_turn() {
    // Each case calls the actual product fork/storage function, then reopens
    // its resulting file with the production store and performs a new turn.
    // A provider, gateway service, process environment, or credential is never
    // used. Web gateway/SDK transport acceptance remains an integration check.
    for (id, resume) in [("entry-5", false), ("entry-2", false), ("entry-2", true)] {
        let fixture = IsolatedFork::new();
        let source = text_turn();
        let source_path = fixture.path(&"source".into());
        std::fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        let mut original = String::new();
        for record in &source {
            original.push_str(&pilotdeck_protocol::stringify_json(record).unwrap().unwrap());
            original.push('\n');
        }
        std::fs::write(&source_path, original.as_bytes()).unwrap();
        let request = json(&format!(
            r#"{{"sessionKey":"source","fromEntryId":"{id}","resumeAt":{resume}}}"#
        ));
        let result = fork_session_with_adapter(&fixture.options, None, &request).unwrap();
        let session = field(&result, "newSessionKey").as_string().unwrap().clone();
        let path = fixture.path(&session);
        let copied = crate::storage::read_transcript(&path).unwrap().entries;
        assert_chain(&copied);
        if resume {
            assert!(
                !copied
                    .iter()
                    .any(|record| is(field(record, "type"), "turn_result"))
            );
            assert!(
                !copied
                    .iter()
                    .any(|record| is(field(record, "type"), "assistant_message"))
            );
        }
        let recorder = Recorder::new(
            Arc::new(crate::storage::JsonlTranscriptStore::new()),
            TranscriptKey {
                project_root: fixture.options.project.clone(),
                pilot_home: fixture.options.home.clone(),
                session_id: session.clone(),
                transcript_path: path.clone(),
            },
        );
        recorder.restore(&copied).unwrap();
        recorder.start_turn(&session, &"new-turn".into()).unwrap();
        recorder
            .writer()
            .record_accepted_input(
                &session,
                &"new-turn".into(),
                vec![json(
                    r#"{"role":"user","content":[{"type":"text","text":"continue"}]}"#,
                )],
                None,
            )
            .unwrap();
        recorder
            .record_model_request(&session, &"new-turn".into(), json("{}"))
            .unwrap();
        recorder
            .complete_turn(
                &session,
                &"new-turn".into(),
                json(r#"{"type":"success","usage":{},"permissionDenials":[],"turnId":"new-turn"}"#),
            )
            .unwrap();
        let continued = crate::storage::read_transcript(&path).unwrap().entries;
        validate_fork_entries(&continued).unwrap();
        assert_chain(&continued);
        assert_eq!(std::fs::read(&source_path).unwrap(), original.as_bytes());
        assert_eq!(
            continued
                .iter()
                .filter(|record| is(field(record, "type"), "turn_result")
                    && is(field(record, "turnId"), "new-turn"))
                .count(),
            1
        );
    }
}

#[test]
fn completed_pending_interaction_tail_cannot_cross_assistant_cutoff() {
    for (started, completed, start, finish) in [
        (
            "question_started",
            "question_completed",
            r#"{"operationId":"q-1","toolCallId":"ask-1","toolName":"ask_user_question","questionCount":1}"#,
            r#"{"operationId":"q-1","questionCount":1}"#,
        ),
        (
            "permission_started",
            "permission_completed",
            r#"{"operationId":"p-1","toolCallId":"write-1","toolName":"write_file","mode":"ask","step":1}"#,
            r#"{"operationId":"p-1","toolCallId":"write-1","toolName":"write_file","mode":"ask","step":1,"decision":"allow","reason":"user"}"#,
        ),
    ] {
        let base = text_turn();
        let mut source = base[..4].to_vec();
        source.push(entry(started, 5, "turn-1", start));
        source.push(entry("assistant_message", 6, "turn-1", "{}"));
        source[5] = with(&source[5], "message", field(&base[4], "message").clone());
        source.push(entry(completed, 7, "turn-1", finish));
        source.push(entry(
            "step_completed",
            8,
            "turn-1",
            r#"{"step":1,"outcome":"completed"}"#,
        ));
        source.push(entry(
            "turn_result",
            9,
            "turn-1",
            r#"{"result":{"type":"success","usage":{},"permissionDenials":[]}}"#,
        ));
        validate_fork_entries(&source).unwrap();
        assert!(
            fork(&source, "entry-6", false)
                .unwrap_err()
                .message
                .contains(completed)
        );
    }
}

#[test]
fn accepted_resume_rejects_pending_question_without_an_open_step() {
    let source = vec![
        entry("turn_started", 1, "turn-1", "{}"),
        entry(
            "question_started",
            2,
            "turn-1",
            r#"{"operationId":"q-1","toolCallId":"ask-1","toolName":"ask_user_question","questionCount":1}"#,
        ),
        entry("accepted_input", 3, "turn-1", r#"{"messages":[]}"#),
    ];
    validate_fork_entries(&source).unwrap();
    assert!(
        fork(&source, "entry-3", true)
            .unwrap_err()
            .message
            .contains("pending question")
    );
}

#[test]
fn fork_parent_repair_preserves_retained_dag_links_and_optional_fields() {
    let mut source = vec![
        entry("accepted_input", 1, "legacy", r#"{"messages":[]}"#),
        entry(
            "assistant_message",
            2,
            "legacy",
            r#"{"message":{"role":"assistant","content":[]}}"#,
        ),
        entry("note", 3, "legacy", "{}"),
        entry(
            "turn_result",
            4,
            "legacy",
            r#"{"result":{"type":"success","usage":{},"permissionDenials":[]}}"#,
        ),
    ];
    source[1] = with(&source[1], "parentEntryId", JsValue::Null);
    let copied = fork(&source, "entry-2", false).unwrap();
    assert_eq!(field(&copied[1], "parentEntryId"), &JsValue::Null);
    assert_eq!(
        field(&copied[2], "parentEntryId"),
        &JsValue::from("entry-2")
    );
    source[1] = with(&source[1], "parentEntryId", JsValue::Undefined);
    let copied = fork(&source, "entry-2", false).unwrap();
    assert_eq!(field(&copied[1], "parentEntryId"), &JsValue::Undefined);
}

#[test]
fn fork_rejects_an_unknown_source_parent_before_repairing_any_link() {
    let mut source = text_turn();
    source[4] = with(&source[4], "parentEntryId", "unknown-source-parent".into());
    assert_eq!(
        fork(&source, "entry-5", false).unwrap_err().code,
        Some("fork_invalid_source_chain")
    );
}

#[test]
fn assistant_fork_rejects_semantics_after_terminal_before_late_artifact() {
    let mut source = text_turn();
    source.push(entry(
        "assistant_message",
        8,
        "turn-1",
        r#"{"message":{"role":"assistant","content":[]}}"#,
    ));
    source.push(entry("file_artifacts", 9, "turn-1", r#"{"artifacts":[]}"#));
    assert!(
        fork(&source, "entry-5", false)
            .unwrap_err()
            .message
            .contains("assistant_message")
    );
}

#[test]
fn second_generation_media_reference_matches_retargeted_final_message() {
    let mut source = text_turn();
    let message = json(
        r#"{"role":"assistant","content":[{"type":"media_reference","path":"/isolated/source/tool-results/image.png","mimeType":"image/png","sha256":"same-digest"}]}"#,
    );
    source[4] = with(&source[4], "message", message.clone());
    source[6] = with(
        &source[6],
        "result",
        with(field(&source[6], "result"), "finalMessage", message),
    );
    let first = fork(&source, "entry-5", false)
        .unwrap()
        .iter()
        .map(|record| {
            let marked = retarget_entry(record, &"child".into()).unwrap();
            node_retarget(
                &marked,
                "source",
                "child",
                Path::new("/isolated/source"),
                Path::new("/isolated/child"),
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let second = fork(&first, "entry-5", false).unwrap();
    assert_eq!(
        field(field(&second[6], "result"), "finalMessage"),
        &fork_message_identity(field(&second[4], "message"))
    );
    assert_eq!(
        field(
            &array(field(field(&second[4], "message"), "content"))[0],
            "path"
        ),
        &JsValue::from("/isolated/child/tool-results/image.png")
    );
}

#[test]
fn assistant_fork_keeps_source_terminal_artifacts_recorded_after_result() {
    let mut source = text_turn();
    source.push(entry("file_artifacts", 8, "turn-1", r#"{"artifacts":[{"id":"late-1","name":"output.txt","path":"output.txt","operation":"created","source":"workspace_diff","status":"complete","size":1,"sha256":"digest","createdAt":"2026-11-14T00:00:00.000Z"}]}"#));
    assert_eq!(fork(&source, "entry-5", false).unwrap(), source);
}
