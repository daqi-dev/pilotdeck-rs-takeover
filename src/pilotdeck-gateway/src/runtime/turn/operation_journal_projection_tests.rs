//! Same Live projection used by external journal settlement, not a second projector.
use super::*;

fn projection_turn() -> LiveTurnStream {
    let integrity = Arc::new(StreamIntegrity::new());
    *integrity.module_id.lock().unwrap() = Some("new-peer-instance".into());
    let mut turn = stream(
        Box::pin(std::future::pending()),
        mpsc::channel(1).1,
        integrity,
    );
    turn.timeline = Some(Arc::new(Mutex::new(
        super::super::super::timeline::TurnTimeline::new("turn".into()),
    )));
    turn
}
fn unprojected() -> JsValue {
    parse_json(r#"{"result":{"type":"success","sessionId":"session","turnId":"turn","finalMessage":{"role":"assistant","content":[{"type":"text","text":"external final"}]}},"messages":[{"role":"assistant","content":[{"type":"text","text":"external final"}]}],"seedState":{}}"#).unwrap()
}
#[test]
fn journal_capture_is_exact_live_projected_payload_including_seed() {
    let turn = projection_turn();
    let original = unprojected();
    let mut captured = None;
    let event = turn
        .completed_event_projected(&original, true, false, Some(&mut captured))
        .unwrap();
    let projected = captured.unwrap();
    assert_eq!(projected.get("result"), event.get("result"));
    assert_eq!(projected.get("messages"), event.get("messages"));
    assert_eq!(projected.get("seedState"), original.get("seedState"));
    assert_eq!(
        projected
            .get("result")
            .unwrap()
            .get("finalMessage")
            .unwrap()
            .get("metadata")
            .unwrap()
            .get("moduleId"),
        Some(&"new-peer-instance".into())
    );
    assert_ne!(
        projected.get("result"),
        original.get("result"),
        "journal must not receive the raw pre-projection terminal"
    );
}
#[test]
fn recovered_terminal_keeps_original_module_and_block_coordinates_exactly() {
    let turn = projection_turn();
    let original = unprojected();
    let mut captured = None;
    turn.completed_event_projected(&original, true, false, Some(&mut captured))
        .unwrap();
    let projected = captured.unwrap();
    *turn.integrity.module_id.lock().unwrap() = Some("replacement-peer-instance".into());
    let mut recovered_capture = None;
    let event = turn
        .finish_with_projection(
            Ok(ExecuteResult::RecoveredTerminal(projected.clone())),
            Some(&mut recovered_capture),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered_capture,
        Some(projected.clone()),
        "recovery must not re-mark messages or reallocate timeline coordinates"
    );
    assert_eq!(event.get("result"), projected.get("result"));
    assert_eq!(event.get("messages"), projected.get("messages"));
}
#[test]
fn recovered_terminal_cannot_bypass_session_turn_identity() {
    let turn = projection_turn();
    let mut wrong = unprojected().as_object().unwrap().clone();
    let mut result = wrong.get("result").unwrap().as_object().unwrap().clone();
    result.insert("turnId".into(), "foreign-turn".into());
    wrong.insert("result".into(), JsValue::Object(result));
    let mut captured = None;
    let error = turn
        .finish_with_projection(
            Ok(ExecuteResult::RecoveredTerminal(JsValue::Object(wrong))),
            Some(&mut captured),
        )
        .unwrap_err();
    assert!(error.message.contains("different session or turn"));
    assert!(captured.is_none());
}
