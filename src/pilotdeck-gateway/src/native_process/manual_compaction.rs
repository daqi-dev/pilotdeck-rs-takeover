//! `GatewayManualCompactionCoordinator`'s router port
//! (`GatewayManualCompactionCoordinator.ts:8-13`) over the session owner:
//! `/compact` reaches `SessionRouter.compact` with the command deadline as
//! the maintenance abort.
use crate::runtime::{
    session::SessionOwner,
    turn::{
        admission::{CompactionAbort, SubmitCompactionRouter},
        submit::{SubmitError, SubmitFuture},
    },
};
use pilotdeck_protocol::{JsString, JsValue};

pub(super) struct OwnerCompaction {
    pub owner: SessionOwner,
}

impl SubmitCompactionRouter for OwnerCompaction {
    fn compact<'a>(
        &'a self,
        session: &'a JsString,
        turn: &'a JsString,
        abort: &'a CompactionAbort,
    ) -> SubmitFuture<'a, Result<JsValue, SubmitError>> {
        Box::pin(async move {
            let cancel = crate::host::Cancellation::default();
            // `AbortSignal.any([maintenanceSignal, options.abortSignal])`.
            let bridge = {
                let (cancel, abort) = (cancel.clone(), abort.clone());
                tokio::spawn(async move {
                    let reason = abort.aborted().await;
                    cancel.cancel(Some(reason));
                })
            };
            let result = self.owner.compact(session, turn.clone(), cancel).await;
            bridge.abort();
            result.map_err(|error| SubmitError::new("manual_compaction_failed", error.message))
        })
    }
}
