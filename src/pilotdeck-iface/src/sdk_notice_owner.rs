//! Composition seam for product events and the host's real lifecycle owner.
//! This type is not installed until a public lifecycle backend is available.
use crate::{
    preparation_effects::{NoticeOwner, PendingNotice},
    product_event_queue::ProductEventQueue,
};
use pilotdeck_agent::{
    ports::{CallControl, PortResult},
    turn::TurnInput,
};
use pilotdeck_protocol::JsObject;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Host-owned implementation must enforce binding, generation, prepare ACK,
/// cleanup grant and retire semantics; it dispatches actual hooks, not events.
pub trait LifecycleOwner: Send + Sync {
    fn advertised(&self) -> bool;
    fn start(
        &self,
        event: &'static str,
        detail: JsObject,
        control: &CallControl,
    ) -> PortResult<Box<dyn PendingNotice>>;
    fn retire(&self);
}
pub trait NoticeAdmission: Send + Sync {
    fn instructions_loaded(&self, input: &TurnInput, control: &CallControl) -> PortResult<()>;
    fn pre_model(&self, control: &CallControl) -> PortResult<()>;
    /// Reconcile original accepted work before the terminal event is published.
    fn before_turn_completed(&self, control: &CallControl) -> PortResult<()>;
    /// Revalidate the actual call/source/grant immediately before each queued
    /// SDK invocation; an earlier enqueue is not authorization.
    fn event_dispatch(&self, control: &CallControl) -> PortResult<()>;
}
/// Source-owned activity, separate from the Controller presentation event.
pub trait OwnedActivity: Send + Sync {
    fn emit(&self, event: &JsObject);
    fn drain(&self);
    fn stage_event(&self, event: &JsObject) -> Box<dyn ActivityAdmission>;
}
pub trait ActivityAdmission: Send {
    fn admit(self: Box<Self>);
}
pub struct SdkNoticeOwner {
    admission: Arc<dyn NoticeAdmission>,
    event_retirement: Arc<AtomicBool>,
    lifecycle: Arc<dyn LifecycleOwner>,
    activity: Arc<dyn OwnedActivity>,
    events: Mutex<ProductEventQueue>,
    event_advertised: bool,
}
impl SdkNoticeOwner {
    /// Must be called and used from a blocking worker, never a Tokio runtime
    /// worker; the SDK runtime must keep driving independently.
    pub fn new(
        admission: Arc<dyn NoticeAdmission>,
        lifecycle: Arc<dyn LifecycleOwner>,
        activity: Arc<dyn OwnedActivity>,
        events: ProductEventQueue,
        event_advertised: bool,
    ) -> Self {
        let event_retirement = events.retirement_signal();
        Self {
            admission,
            event_retirement,
            lifecycle,
            activity,
            events: Mutex::new(events),
            event_advertised,
        }
    }
}
impl NoticeOwner for SdkNoticeOwner {
    fn authorize_instructions_loaded(
        &self,
        input: &TurnInput,
        control: &CallControl,
    ) -> PortResult<()> {
        self.admission.instructions_loaded(input, control)
    }
    fn authorize_pre_model(&self, control: &CallControl) -> PortResult<()> {
        self.admission.pre_model(control)
    }
    fn lifecycle_advertised(&self) -> bool {
        self.lifecycle.advertised()
    }
    fn event_advertised(&self) -> bool {
        self.event_advertised
    }
    fn emit_owned_activity(&self, event: &JsObject) {
        self.activity.emit(event)
    }
    fn start_lifecycle(
        &self,
        event: &'static str,
        detail: JsObject,
        control: &CallControl,
    ) -> PortResult<Box<dyn PendingNotice>> {
        self.lifecycle.start(event, detail, control)
    }
    fn enqueue_event(&self, event: JsObject, control: &CallControl) -> PortResult<()> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .enqueue(event, control)
    }
    fn finish_events(&self, control: &CallControl) -> PortResult<()> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .finish(control)
    }
    fn retire(&self) {
        self.event_retirement.store(true, Ordering::Release);
        self.lifecycle.retire();
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retire();
    }
}
