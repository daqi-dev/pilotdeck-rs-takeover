use super::*;
fn event(kind: &str) -> JsValue {
    let mut fields = JsObject::new();
    fields.insert("type".into(), kind.into());
    JsValue::Object(fields)
}
fn delivered(source: &Arc<NativeLoopActivity>, value: JsValue) {
    let admission = source.stage(value);
    let delivery = source.claim().unwrap();
    admission.admit();
    delivery.commit();
}
#[test]
fn occurrences_require_admission_and_delivery_in_either_order() {
    let source = Arc::new(NativeLoopActivity::default());
    let first = source.stage(event("same"));
    let first_delivery = source.claim().unwrap();
    first_delivery.commit();
    let second = source.stage(event("same"));
    let second_delivery = source.claim().unwrap();
    second.admit();
    second_delivery.commit();
    assert!(
        source.0.lock().unwrap().committed.is_empty(),
        "later receipt cannot overtake pending occurrence"
    );
    first.admit();
    assert_eq!(source.finish(), vec![event("same"), event("same")]);
}
#[test]
fn dropped_admission_delivery_and_unwind_discard_without_shifting_successors() {
    let source = Arc::new(NativeLoopActivity::default());
    drop(source.stage(event("unaccepted")));
    let rejected = source.stage(event("rejected"));
    let receipt = source.claim().unwrap();
    rejected.admit();
    drop(receipt);
    let source2 = source.clone();
    let _ = std::panic::catch_unwind(move || {
        let _admission = source2.stage(event("unwind"));
        panic!("fixture");
    });
    delivered(&source, event("accepted_after_cancel"));
    assert_eq!(source.finish(), vec![event("accepted_after_cancel")]);
}
#[test]
fn same_event_kind_from_host_never_enters_owned_loop_buffer() {
    let source = Arc::new(NativeLoopActivity::default());
    let host_only = event("tool_progress");
    source.emit_owned(event("tool_progress"));
    source.drain_owned();
    delivered(&source, event("assistant_message"));
    let actual = source.finish();
    assert_eq!(actual, vec![host_only, event("assistant_message")]);
    // There is one owned progress occurrence, even if the presentation stream
    // contains any number of equal host-origin payloads.
    assert_eq!(actual.len(), 2);
}
#[test]
fn sixty_four_real_events_keep_final_assistant_and_reject_true_sixty_fifth() {
    let source = Arc::new(NativeLoopActivity::default());
    for _ in 0..63 {
        delivered(&source, event("model_event"));
    }
    delivered(&source, event("assistant_message"));
    delivered(&source, event("turn_completed"));
    let actual = source.finish();
    assert_eq!(actual.len(), 64);
    assert_eq!(actual[63], event("assistant_message"));
}
#[test]
fn owned_notices_only_count_at_loop_drains_and_do_not_flush_at_terminal() {
    let source = Arc::new(NativeLoopActivity::default());
    source.emit_owned(event("instructions_loaded"));
    delivered(&source, event("model_request_started"));
    source.emit_owned(event("tool_progress"));
    source.drain_owned();
    delivered(&source, event("tool_result"));
    source.emit_owned(event("instructions_loaded"));
    delivered(&source, event("turn_completed"));
    assert_eq!(
        source.finish(),
        vec![
            event("model_request_started"),
            event("instructions_loaded"),
            event("tool_progress"),
            event("tool_result"),
            event("turn_completed")
        ]
    );
}
#[test]
fn concurrent_child_channels_and_terminal_cleanup_never_cross_turns() {
    let a = Arc::new(NativeLoopActivity::default());
    let b = Arc::new(NativeLoopActivity::default());
    let aa = a.clone();
    let bb = b.clone();
    let first = std::thread::spawn(move || delivered(&aa, event("a")));
    let second = std::thread::spawn(move || delivered(&bb, event("b")));
    first.join().unwrap();
    second.join().unwrap();
    let late = a.stage(event("late"));
    let late_receipt = a.claim().unwrap();
    assert_eq!(a.finish(), vec![event("a")]);
    late.admit();
    late_receipt.commit();
    assert!(a.finish().is_empty());
    assert_eq!(b.finish(), vec![event("b")]);
}

#[test]
fn drained_owned_activity_survives_cancel_without_another_wire_event() {
    let source = Arc::new(NativeLoopActivity::default());
    source.emit_owned(event("instructions_loaded"));
    source.emit_owned(event("tool_progress"));
    source.drain_owned();
    source.emit_owned(event("never_yielded"));
    assert_eq!(
        source.finish(),
        vec![event("instructions_loaded"), event("tool_progress")]
    );
}
#[test]
fn drained_owned_activity_is_independent_of_rejected_next_wire_delivery() {
    let source = Arc::new(NativeLoopActivity::default());
    let first = source.stage(event("preceding"));
    let first_delivery = source.claim().unwrap();
    source.emit_owned(event("instructions_loaded"));
    source.drain_owned();
    let rejected = source.stage(event("next_wire"));
    let rejected_delivery = source.claim().unwrap();
    rejected.admit();
    drop(rejected_delivery);
    assert!(
        source.0.lock().unwrap().committed.is_empty(),
        "prior occurrence still fences ordering"
    );
    first.admit();
    first_delivery.commit();
    assert_eq!(
        source.finish(),
        vec![event("preceding"), event("instructions_loaded")]
    );
}

#[test]
fn finish_discards_pending_wire_but_preserves_later_independently_delivered_owned() {
    let source = Arc::new(NativeLoopActivity::default());
    let pending = source.stage(event("undelivered_wire"));
    let pending_delivery = source.claim().unwrap();
    pending.admit();
    source.emit_owned(event("instructions_loaded"));
    source.drain_owned();
    assert!(source.0.lock().unwrap().committed.is_empty());
    assert_eq!(source.finish(), vec![event("instructions_loaded")]);
    // A callback completing after close cannot resurrect the discarded wire.
    pending_delivery.commit();
    assert!(source.finish().is_empty());
}
