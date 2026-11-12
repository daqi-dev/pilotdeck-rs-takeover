use super::*;
use crate::{
    host::modules::tool_context::{ToolFileState, ToolRuntimeContext},
    user_dialog::{ChannelOptions, DialogRuntime, GatewayUserDialogBus},
};
use std::sync::{
    Mutex,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};

fn context() -> ToolRunContext {
    ToolRunContext::new(Arc::new(ToolRuntimeContext {
        fields: JsObject::new(),
        files: ToolFileState {
            read_file_state: Arc::default(),
            write_snapshots: Arc::default(),
            allowed_read_files: vec![],
        },
        abort_signal: None,
        model: None,
        elicitation: None,
        user_dialog: None,
        audit_recorder: None,
        file_history: None,
        file_update_notifier: None,
        now: None,
        progress: None,
        subagent: None,
        plan_todo: None,
        goal: None,
        plan_directory: None,
    }))
}

struct Fixture {
    bus: Arc<Mutex<GatewayUserDialogBus>>,
    channel: GatewayUserDialogChannel,
    events: Arc<Mutex<Vec<JsValue>>>,
    now: Arc<AtomicI64>,
    polls: Arc<AtomicUsize>,
}
impl Fixture {
    fn new(bus: Option<Arc<Mutex<GatewayUserDialogBus>>>) -> Self {
        let bus = bus.unwrap_or_else(|| {
            Arc::new(Mutex::new(GatewayUserDialogBus::connected(
                DialogRuntime::default(),
            )))
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let capture = events.clone();
        let mut options = ChannelOptions::new(
            "s",
            bus.clone(),
            Arc::new(move |event| {
                capture.lock().unwrap().push(event);
                Ok(())
            }),
        );
        // Deliberately permit two channel generations to reuse public IDs.
        options.uuid = Arc::new(|| "same-request".into());
        let now = Arc::new(AtomicI64::new(0));
        let clock = now.clone();
        options.now_ms = Arc::new(move || clock.load(Ordering::SeqCst));
        let polls = Arc::new(AtomicUsize::new(0));
        let count = polls.clone();
        options.take_stored_answer = Some(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }));
        Self {
            bus,
            channel: GatewayUserDialogChannel::new(options),
            events,
            now,
            polls,
        }
    }
    fn tick(&self) {
        self.now.fetch_add(100, Ordering::SeqCst);
        self.channel.tick();
    }
    fn pending(&self) -> usize {
        self.bus.lock().unwrap().pending_count(&"s".into())
    }
    fn cancellations(&self) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.get("type") == Some(&JsValue::from("user_dialog_cancelled")))
            .count()
    }
}

fn input(kind: Kind) -> JsValue {
    let mut input = object([("prompt", "fixture question".into())]);
    match kind {
        Kind::Choice => {
            input.insert(
                "options".into(),
                JsValue::Array(vec![
                    JsValue::Object(object([("value", "a".into())])),
                    JsValue::Object(object([("value", "b".into())])),
                ]),
            );
        }
        Kind::Form => {
            input.insert(
                "schema".into(),
                JsValue::Object(object([
                    ("type", "object".into()),
                    ("properties", JsValue::Object(JsObject::new())),
                ])),
            );
        }
        Kind::Input | Kind::Confirmation => {}
    }
    JsValue::Object(input)
}

async fn pending(future: &mut ToolFuture<ToolOutput>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn h1_dropped_native_dialog_tools_cancel_pending_and_stop_polling() {
    for kind in [Kind::Input, Kind::Choice, Kind::Confirmation, Kind::Form] {
        let fixture = Fixture::new(None);
        let tool = RequestUserTool::new(kind, fixture.channel.clone());
        let mut future = tool.execute(input(kind), context());
        pending(&mut future).await;
        assert_eq!(fixture.pending(), 1, "{}", kind.name());
        let initial = fixture.polls.load(Ordering::SeqCst);
        assert!(initial > 0);
        fixture.tick();
        assert_eq!(fixture.polls.load(Ordering::SeqCst), initial + 1);
        drop(future);
        assert_eq!(fixture.pending(), 0, "{}", kind.name());
        assert_eq!(fixture.cancellations(), 1, "{}", kind.name());
        fixture.tick();
        assert_eq!(fixture.polls.load(Ordering::SeqCst), initial + 1);
    }
}

#[tokio::test]
async fn h1_old_dialog_tool_drop_preserves_same_key_successor_and_its_polling() {
    let old = Fixture::new(None);
    let old_tool = RequestUserTool::new(Kind::Input, old.channel.clone());
    let mut old_future = old_tool.execute(input(Kind::Input), context());
    pending(&mut old_future).await;
    assert_eq!(old.pending(), 1);
    // A distinct channel replaces the bus entry without touching the original
    // channel's request table or abort callback. This reaches the real owned
    // generation check when the old tool future is dropped.
    let newer = Fixture::new(Some(old.bus.clone()));
    let new_tool = RequestUserTool::new(Kind::Input, newer.channel.clone());
    let mut new_future = new_tool.execute(input(Kind::Input), context());
    pending(&mut new_future).await;
    let old_polls = old.polls.load(Ordering::SeqCst);
    let new_polls = newer.polls.load(Ordering::SeqCst);
    drop(old_future);
    assert_eq!(newer.pending(), 1);
    assert_eq!(newer.cancellations(), 0);
    old.tick();
    newer.tick();
    assert_eq!(old.polls.load(Ordering::SeqCst), old_polls);
    assert_eq!(newer.polls.load(Ordering::SeqCst), new_polls + 1);

    let (consumed, events) = {
        let mut bus = newer.bus.lock().unwrap();
        let consumed = bus
            .consume(&"s".into(), &"same-request".into(), "answered")
            .unwrap();
        (consumed, bus.drain_events())
    };
    if let Some(settle) = &consumed.entry.on_settled {
        settle().unwrap();
    }
    events.deliver();
    (consumed.entry.resolve)(DialogAnswer::Answered("new answer".into()));
    let output = new_future.await.unwrap();
    assert_eq!(
        output.data.as_ref().and_then(|data| data.get("answer")),
        Some(&JsValue::from("new answer"))
    );
    assert_eq!(newer.pending(), 0);
    assert_eq!(newer.cancellations(), 0);
    newer.tick();
    assert_eq!(newer.polls.load(Ordering::SeqCst), new_polls + 1);
}
