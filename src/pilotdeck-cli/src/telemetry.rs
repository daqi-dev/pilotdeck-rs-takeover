//! Resolve the real collector context once, before native Gateway assembly.
use super::{Failure, paths::Env};
use pilotdeck_gateway::telemetry::{
    RuntimeContextOptions, TelemetryRuntimeContext, resolve_runtime_context,
};
use std::path::Path;

pub fn runtime_context(
    env: &Env,
    cwd: &Path,
    pilot_home: &Path,
) -> Result<TelemetryRuntimeContext, Failure> {
    resolve_runtime_context(&RuntimeContextOptions::from_process(
        env.strings().into_iter().collect(),
        Some(pilot_home.to_owned()),
        cwd.to_owned(),
    ))
    .map_err(|error| Failure::Error(error.to_string()))
}
