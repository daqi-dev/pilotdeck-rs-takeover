//! Native Loop entry for a root that has installed all real production adapters.
pub use crate::production_bindings::{
    DefaultProductionAdapters, REQUIRED_PRODUCTION_HOST_FEATURES,
};
use crate::{
    native_loop_launch::NativeLoopConfiguration,
    original::OriginalPayloadAdmission,
    production_bindings::{ProductionAdapters, ProductionBindings},
};
use agentcpm_scheduler_protocol::{Failure, Result};
use std::sync::Arc;

/// Produces the actual public Native Loop export. The caller is the trusted
/// product composition root, responsible for real installed adapters and
/// negotiated host profiles; startup JSON does not provide those authorities.
pub async fn serve(
    config: NativeLoopConfiguration,
    adapters: Arc<dyn ProductionAdapters>,
) -> Result<()> {
    adapters
        .validate_startup(&config.controller, &config.controller_revision)
        .map_err(|_| {
            Failure::external(
                "PILOTDECK_NATIVE_CONFIG",
                "controller installation mismatch",
            )
        })?;
    let bindings = ProductionBindings::new(
        config.controller,
        config.controller_revision,
        config.contexts,
        adapters,
    )
    .map_err(|_| {
        Failure::external(
            "PILOTDECK_NATIVE_CONFIG",
            "complete Context installation required",
        )
    })?;
    let admission = Arc::new(OriginalPayloadAdmission::new(
        config.process_cwd.into(),
        Arc::new(bindings),
    ));
    agentcpm_scheduler_protocol::provider::serve_with_required_host_features(
        agentcpm_scheduler_protocol::provider::Export::Loop(Arc::new(
            crate::NativeFactory::new(admission)
                .with_settlement_data()
                .with_scoped_steps(),
        )),
        REQUIRED_PRODUCTION_HOST_FEATURES,
    )
    .await
}
