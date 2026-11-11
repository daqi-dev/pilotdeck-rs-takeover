use crate::user_dialog::GatewayUserDialogBus;
use std::sync::{Arc, Mutex};

pub(super) fn build_user_dialog_bus() -> Arc<Mutex<GatewayUserDialogBus>> {
    // #1445: the production bus is *live* even though no renderer change
    // observer is attached. Availability is method presence, not observer
    // wiring: TS `GatewayWsConnection.ts:481-490` gates on
    // `this.options.gateway.listUserDialogs` (always defined by
    // `InProcessGateway.ts:2710/2734/2765`), and an empty bus returns
    // `{dialogs: []}`. Coupling `is_connected()` to `on_change` made the whole
    // family unreachable in native mode.
    //
    // The observer argument stays `None`: forwarding `user_dialog_changed` is a
    // separate renderer-broadcast gap (the process now has a connection set,
    // `ConnectionBroadcast`, but only the refresh bundle's `config_changed`
    // is bound to it). Tracked separately; it must not gate availability.
    Arc::new(Mutex::new(GatewayUserDialogBus::connected(
        crate::user_dialog::DialogRuntime::default(),
    )))
}

/// `onUserDialogChange(change)` → `broadcastNotification("user_dialog_changed",
/// change)` (createLocalGateway.ts:1053-1058).
pub(super) fn bind_broadcast(
    bus: &Arc<Mutex<GatewayUserDialogBus>>,
    broadcast: Arc<crate::runtime::connection::ConnectionBroadcast>,
) {
    bus.lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_observer(Arc::new(move |change| {
            broadcast.broadcast("user_dialog_changed", &change);
            Ok(())
        }));
}

/// One session's `GatewayUserDialogChannel` (ProjectSessionRuntimeBundle.ts:
/// 638-650): requests are published on the shared bus, emitted into the
/// session's active turn stream and journaled next to the transcript
/// (`createGatewayUserDialogJournal(storage)`). No host dialog store is
/// configured, as in `pilotdeck server`.
pub(super) fn session_channel(
    session: pilotdeck_protocol::JsString,
    bus: Arc<Mutex<GatewayUserDialogBus>>,
    emit: super::super::permission_prompt::GatewayEmitSlot,
    transcript: &std::path::Path,
) -> crate::user_dialog::GatewayUserDialogChannel {
    use crate::user_dialog::{
        ChannelOptions, GatewayUserDialogChannel, JournalOptions, UserDialogJournal,
        create_gateway_user_dialog_journal,
    };
    let emit_session = session.clone();
    let mut options = ChannelOptions::new(
        session,
        bus,
        Arc::new(move |event| {
            if let Some(emit) = emit.get() {
                emit(&emit_session, event);
            }
            Ok(())
        }),
    );
    options.journal =
        create_gateway_user_dialog_journal(Some(transcript), JournalOptions::default())
            .map(|journal| Arc::new(journal) as Arc<dyn UserDialogJournal>);
    GatewayUserDialogChannel::new(options)
}

#[cfg(test)]
mod tests {
    use super::build_user_dialog_bus;

    /// #1445 negative control: if production reverts to
    /// `GatewayUserDialogBus::new(runtime, None)` (observer-coupled
    /// connectivity) this test turns red, restoring the unreachable family.
    #[test]
    fn production_bus_is_connected_without_an_observer() {
        let bus = build_user_dialog_bus();
        assert!(
            bus.lock().unwrap_or_else(|e| e.into_inner()).is_connected(),
            "#1445 production assembly must build a connected bus"
        );
    }
}
