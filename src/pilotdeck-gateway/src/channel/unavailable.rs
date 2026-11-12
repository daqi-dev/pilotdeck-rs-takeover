//! The loader-table channels Rust does not implement, as the baseline runs
//! them: their SDK is not installed (telegram, discord, slack, matrix,
//! dingtalk, email) or their credentials are absent (mattermost, signal,
//! whatsapp, homeassistant), so `start` logs one line through
//! `logger.error` and returns a no-op handle while the gateway keeps
//! running. A configuration the baseline would actually connect with is
//! refused before startup (`loader::reject_unmigrated_channels`).

use std::sync::Arc;

use pilotdeck_protocol::JsObject;

use super::loader::{ChannelLoaderMounts, EnvLookupFn, UnavailableStart, unavailable_start};
use super::protocol::{
    BoxFuture, ChannelAdapter, ChannelError, ChannelHandle, ChannelKey, ChannelStartDeps,
};

/// The loader keys this module mounts.
pub const UNAVAILABLE_LOADER_KEYS: &[&str] = &[
    "telegram",
    "discord",
    "slack",
    "matrix",
    "mattermost",
    "signal",
    "whatsapp",
    "dingtalk",
    "email",
    "homeassistant",
];

pub struct UnavailableChannel {
    key: ChannelKey,
    line: &'static str,
}

impl UnavailableChannel {
    pub fn new(key: &str, line: &'static str) -> Self {
        Self {
            key: ChannelKey::new(key),
            line,
        }
    }
}

struct NoopHandle;
impl ChannelHandle for NoopHandle {
    fn stop(&self, _reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
}

impl ChannelAdapter for UnavailableChannel {
    fn channel_key(&self) -> &ChannelKey {
        &self.key
    }
    fn start(
        &self,
        deps: ChannelStartDeps,
    ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
        let line = self.line;
        Box::pin(async move {
            if let Some(log) = deps.log {
                log("error", line);
            }
            Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>)
        })
    }
}

/// Mounts every key in [`UNAVAILABLE_LOADER_KEYS`], reading the environment
/// through `env` the way each baseline constructor reads `process.env`.
pub fn register_with_env(mounts: &mut ChannelLoaderMounts, env: Arc<EnvLookupFn>) {
    for &key in UNAVAILABLE_LOADER_KEYS {
        let env = env.clone();
        mounts.register(
            key,
            Arc::new(move |options: &JsObject| {
                match unavailable_start(key, options, env.as_ref()) {
                    Some(UnavailableStart::Line(line)) => {
                        Ok(Arc::new(UnavailableChannel::new(key, line)) as Arc<dyn ChannelAdapter>)
                    }
                    // Startup refuses these configurations before any channel
                    // is constructed; reaching here means that gate was
                    // bypassed.
                    _ => Err(ChannelError::error(format!(
                        "channel {key} {}",
                        super::loader::UNMIGRATED_REASON
                    ))),
                }
            }),
        );
    }
}

/// [`register_with_env`] over the process environment.
pub fn register(mounts: &mut ChannelLoaderMounts) {
    register_with_env(mounts, Arc::new(super::loader::process_env));
}
