//! The baseline's global proxy install (src/cli/proxy.ts
//! `installGlobalProxy`): `pilotdeck.ts:28` runs it for every command before
//! `main()`, from the environment; `pilotdeck server` runs it a second time
//! with `proxy.url` from `pilotdeck.yaml` (pilotdeck.ts:48-50), which only
//! takes effect when no environment proxy was installed.
//!
//! The state lives in the gateway's process-wide dispatcher
//! (`pilotdeck_gateway::runtime::model::http::dispatcher`), which every
//! outgoing HTTP client of the process follows and which the server's config
//! hot reload replaces; it also prints the `[proxy]` lines.
use super::paths::Env;
use pilotdeck_gateway::runtime::model::http::dispatcher;

/// The top-level `await installGlobalProxy()` (pilotdeck.ts:28).
pub fn install_from_env(env: &Env) {
    dispatcher::install_global_proxy(None, &|key| env.string(key));
}

/// `installGlobalProxy(snapshot.config.proxy.url)` in the server branch: an
/// environment proxy already installed keeps precedence (proxy.ts:64-70).
pub fn install_from_config(env: &Env, url: &str) {
    dispatcher::install_global_proxy(Some(url), &|key| env.string(key));
}
