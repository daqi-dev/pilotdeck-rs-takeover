//! The loader-table channels Rust does not implement, as the baseline runs
//! them: their SDK is not installed (telegram, discord, slack, matrix,
//! dingtalk, email) or their credentials are absent (mattermost, signal,
//! whatsapp, homeassistant), so `start` logs one line through
//! `logger.error` and returns a no-op handle while the gateway keeps
//! running. An option of the wrong type that makes the constructor or the
//! first lines of `start` throw is logged the baseline's way too. A
//! configuration the baseline would actually connect with has no Rust
//! implementation (`state/takeover.md` 人已定 12): it starts as the same
//! stand-in, logging `loader::rust_unavailable_line` instead. The same
//! stand-in serves the `ChannelAdapterBundle` keys `qq` / `wecom`
//! (`native_process::channels`).

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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
    start: Start,
    /// The line was already logged by [`ChannelAdapter::start_prelude`]; the
    /// next `start` does not log it again.
    prelude_logged: AtomicBool,
}

/// What the baseline's `start` does.
enum Start {
    /// Logs this line at this logger level and returns a no-op handle.
    Line(&'static str, String),
    /// Rejects with a `TypeError` with this message.
    TypeError(&'static str),
}

impl UnavailableChannel {
    pub fn new(key: &str, line: &'static str) -> Self {
        Self::logging(key, "error", line.to_string())
    }
    /// A channel whose `start` logs `line` at logger `level` (`info`,
    /// `warn`, `error`) and returns a no-op handle. Also used for the
    /// `ChannelAdapterBundle` keys `qq` / `wecom`
    /// (`loader::bundle_unavailable_start`).
    pub fn logging(key: &str, level: &'static str, line: String) -> Self {
        Self {
            key: ChannelKey::new(key),
            start: Start::Line(level, line),
            prelude_logged: AtomicBool::new(false),
        }
    }
    /// A channel whose `start` rejects with a `TypeError` carrying `message`.
    pub fn failing(key: &str, message: &'static str) -> Self {
        Self {
            key: ChannelKey::new(key),
            start: Start::TypeError(message),
            prelude_logged: AtomicBool::new(false),
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
        let logged = self.prelude_logged.swap(false, Ordering::SeqCst);
        let start = match &self.start {
            Start::Line(level, line) => Ok((!logged).then(|| (*level, line.clone()))),
            Start::TypeError(message) => Err(*message),
        };
        Box::pin(async move {
            match start {
                Ok(line) => {
                    if let (Some(log), Some((level, line))) = (deps.log, line) {
                        log(level, &line);
                    }
                    Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>)
                }
                Err(message) => Err(ChannelError::error(message)),
            }
        })
    }
    /// The baseline's `start` here never suspends before it logs (or throws),
    /// so the line comes out during background startup, in startup order.
    fn start_prelude(&self, deps: &ChannelStartDeps) -> bool {
        if let Start::Line(level, line) = &self.start {
            if let Some(log) = &deps.log {
                log(level, line);
            }
            self.prelude_logged.store(true, Ordering::SeqCst);
        }
        true
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
                    Some(UnavailableStart::Logged { level, line }) => {
                        Ok(Arc::new(UnavailableChannel::logging(key, level, line))
                            as Arc<dyn ChannelAdapter>)
                    }
                    Some(UnavailableStart::StartTypeError(message)) => {
                        Ok(Arc::new(UnavailableChannel::failing(key, message))
                            as Arc<dyn ChannelAdapter>)
                    }
                    // `${e}` of the constructor's TypeError.
                    Some(UnavailableStart::ConstructorTypeError(message)) => {
                        Err(ChannelError::Plain(format!("TypeError: {message}")))
                    }
                    // The baseline would connect: Rust has no implementation.
                    Some(UnavailableStart::Unsupported) => {
                        Ok(Arc::new(UnavailableChannel::logging(
                            key,
                            "error",
                            super::loader::rust_unavailable_line(key),
                        )) as Arc<dyn ChannelAdapter>)
                    }
                    // Every key in `UNAVAILABLE_LOADER_KEYS` is classified.
                    None => Err(ChannelError::error(format!(
                        "channel module not registered: {key}"
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
