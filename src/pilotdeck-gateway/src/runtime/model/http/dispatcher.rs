//! The process-wide fetch dispatcher (`src/cli/proxy.ts`).
//!
//! The baseline routes every `fetch` in the process (model and title
//! requests, memory LLM calls, `web_fetch` / `web_search`, HTTP hooks, MCP
//! HTTP transports, telemetry) through undici's global dispatcher, and swaps
//! that dispatcher as a whole: `installGlobalProxy` at startup
//! (`pilotdeck.ts:28` from the environment, then `pilotdeck.ts:48-50` with
//! `proxy.url` from `pilotdeck.yaml`) and `reinstallGlobalProxy` on a config
//! hot reload that touches `proxy` (`pilotdeck.ts:148-151`). A swap installs
//! a new agent with its own connection pool; requests already in flight finish
//! on the old one, and nothing reuses the old pool afterwards.
//!
//! Here the installed state is one [`Dispatcher`] (a generation number and the
//! proxy, if any). Every proxy-aware client is a [`DispatchedClient`]: on each
//! request it compares the generation it was built for with the current one
//! and, after a swap, builds a fresh hyper client (fresh pool) for the new
//! dispatcher — the same "new agent, old pool abandoned" shape.
//!
//! Before anything is installed (library embedders and unit tests; the
//! `pilotdeck` binary installs at startup) the dispatcher is derived from the
//! environment alone, as the transports did before this state existed.
use std::sync::{Arc, Mutex, OnceLock};

use super::{PROXY_ENV_KEYS, ProxyConfig};

/// Where an installed proxy came from (`ProxyInstallSource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Env,
    Config,
}

/// `dispatcherState` (proxy.ts:11-14, 49).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Direct,
    Proxy {
        source: Source,
        url: String,
    },
    /// A proxy was asked for but its URL is unusable: requests that would go
    /// through it fail instead of going direct (see [`apply_proxy`]).
    Refusing,
}

/// One installed dispatcher: what connections opened from now on use.
#[derive(Debug)]
pub(crate) struct Dispatcher {
    pub(crate) generation: u64,
    pub(crate) proxy: Option<Arc<ProxyConfig>>,
}

struct State {
    current: Option<Arc<Dispatcher>>,
    mode: Option<Mode>,
    generations: u64,
}

static STATE: Mutex<State> = Mutex::new(State {
    current: None,
    mode: None,
    generations: 0,
});
static ENV_FALLBACK: OnceLock<Arc<Dispatcher>> = OnceLock::new();

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The dispatcher new connections use right now.
pub(crate) fn current() -> Arc<Dispatcher> {
    if let Some(current) = state().current.clone() {
        return current;
    }
    ENV_FALLBACK
        .get_or_init(|| {
            Arc::new(Dispatcher {
                generation: 0,
                proxy: ProxyConfig::from_settings(None).map(Arc::new),
            })
        })
        .clone()
}

fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// `getProxyUrl` (proxy.ts:20-28): the first non-empty of the five variables.
fn env_proxy_url(lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    PROXY_ENV_KEYS
        .iter()
        .take(5)
        .find_map(|&key| lookup(key).filter(|value| !value.is_empty()))
}

/// `installGlobalProxy(explicitUrl)` (proxy.ts:52-84). `None` is the
/// top-level call that reads the environment; `Some(url)` is the server's
/// `proxy.url`, which yields to a proxy already installed from the
/// environment. `lookup` reads the environment (`process.env`).
pub fn install_global_proxy(explicit_url: Option<&str>, lookup: &dyn Fn(&str) -> Option<String>) {
    let url = match explicit_url {
        Some(url) => Some(url.to_owned()),
        None => env_proxy_url(lookup),
    };
    let Some(url) = url.filter(|url| !url.is_empty()) else {
        apply_direct(false);
        return;
    };
    // `explicitUrl ? "config" : "env"`.
    let source = if explicit_url.is_some_and(|url| !url.is_empty()) {
        Source::Config
    } else {
        Source::Env
    };
    match &state().mode {
        Some(Mode::Proxy {
            source: Source::Env,
            ..
        }) if source == Source::Config => return,
        Some(Mode::Proxy {
            source: installed,
            url: installed_url,
        }) if *installed == source && *installed_url == url => return,
        _ => {}
    }
    apply_proxy(&url, source, None, lookup, false);
}

/// `reinstallGlobalProxy(proxy?.url, proxy?.noProxy)` (proxy.ts:92-102), the
/// config hot-reload path: no URL removes the proxy (and says so), otherwise
/// the config URL replaces whatever is installed, environment included, with
/// its `noProxy` appended.
pub fn reinstall_global_proxy(url: Option<&str>, extra_no_proxy: Option<&str>) {
    match url.filter(|url| !url.is_empty()) {
        None => apply_direct(true),
        Some(url) => apply_proxy(url, Source::Config, extra_no_proxy, &process_env, true),
    }
}

fn install(state: &mut State, mode: Mode, proxy: Option<ProxyConfig>) {
    state.generations += 1;
    state.current = Some(Arc::new(Dispatcher {
        generation: state.generations,
        proxy: proxy.map(Arc::new),
    }));
    state.mode = Some(mode);
}

/// `applyDirectDispatcher(logRemoval)` (proxy.ts:108-120).
fn apply_direct(log_removal: bool) {
    install(&mut state(), Mode::Direct, None);
    if log_removal {
        println!("[proxy] Global fetch proxy removed");
    }
}

/// `applyGlobalProxy` (proxy.ts:122-147): a URL undici's agent rejects logs
/// the same warning as the baseline. What happens next deliberately differs
/// from the baseline, which always leaves the installed dispatcher as it was
/// (direct when nothing was installed): registered difference
/// A6.5-MCP-PROXY-FALLBACK-001-R1/R2 (human-approved) — a configured proxy
/// must not silently turn into direct traffic.
/// - Startup (`reload == false`): a structurally invalid URL or unsupported
///   protocol installs a refusing dispatcher; a URL that is fine except for
///   its userinfo keeps the baseline (dispatcher untouched, i.e. direct). A
///   refusing dispatcher does not block the later `proxy.url` install, as the
///   baseline's failed env install does not.
/// - Reload (`reload == true`): a working proxy already installed stays (as in
///   the baseline); otherwise (direct, or already refusing) the dispatcher
///   becomes refusing. A later valid URL or a removed `proxy` section recovers.
fn apply_proxy(
    url: &str,
    source: Source,
    extra_no_proxy: Option<&str>,
    lookup: &dyn Fn(&str) -> Option<String>,
    reload: bool,
) {
    let extra = extra_no_proxy.filter(|value| !value.is_empty());
    match ProxyConfig::for_install(url, extra, lookup) {
        Ok(proxy) => {
            let line = format!(
                "[proxy] Global fetch proxy \u{2192} {url} (noProxy: {})",
                proxy.no_proxy()
            );
            install(
                &mut state(),
                Mode::Proxy {
                    source,
                    url: url.to_owned(),
                },
                Some(proxy),
            );
            println!("{line}");
        }
        Err(error) => {
            eprintln!(
                "[proxy] Failed to install global proxy ({url}): {}",
                error.message()
            );
            let refuse = if reload {
                current()
                    .proxy
                    .as_ref()
                    .is_none_or(|proxy| proxy.is_refusing())
            } else {
                matches!(error, super::ProxyParseError::Configuration(_))
            };
            if refuse {
                install(
                    &mut state(),
                    Mode::Refusing,
                    Some(ProxyConfig::refusing(extra, lookup)),
                );
            }
        }
    }
}

/// A hyper client that follows the installed dispatcher: built for one
/// generation, rebuilt (with a fresh pool) once another dispatcher is
/// installed. A pinned client keeps one proxy for its whole life (test seams
/// and explicit per-client proxy settings).
pub(crate) struct DispatchedClient<C> {
    build: Box<dyn Fn(Option<Arc<ProxyConfig>>) -> C + Send + Sync>,
    pinned: Option<Option<Arc<ProxyConfig>>>,
    slot: Mutex<Option<Built<C>>>,
}

/// A client and the generation and proxy it was built for.
type Built<C> = (u64, C, Option<Arc<ProxyConfig>>);

impl<C: Clone> DispatchedClient<C> {
    pub(crate) fn global(
        build: impl Fn(Option<Arc<ProxyConfig>>) -> C + Send + Sync + 'static,
    ) -> Self {
        Self {
            build: Box::new(build),
            pinned: None,
            slot: Mutex::new(None),
        }
    }

    pub(crate) fn pinned(
        proxy: Option<ProxyConfig>,
        build: impl Fn(Option<Arc<ProxyConfig>>) -> C + Send + Sync + 'static,
    ) -> Self {
        Self {
            build: Box::new(build),
            pinned: Some(proxy.map(Arc::new)),
            slot: Mutex::new(None),
        }
    }

    /// The client for the dispatcher in force now, and that dispatcher's
    /// proxy.
    pub(crate) fn get(&self) -> (C, Option<Arc<ProxyConfig>>) {
        let (generation, proxy) = match &self.pinned {
            Some(proxy) => (u64::MAX, proxy.clone()),
            None => {
                let current = current();
                (current.generation, current.proxy.clone())
            }
        };
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((built_for, client, proxy)) = slot.as_ref()
            && *built_for == generation
        {
            return (client.clone(), proxy.clone());
        }
        let client = (self.build)(proxy.clone());
        *slot = Some((generation, client.clone(), proxy.clone()));
        (client, proxy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn proxy_url() -> Option<String> {
        current()
            .proxy
            .as_ref()
            .and_then(|proxy| proxy.proxy.as_ref())
            .map(|url| url.to_string())
    }

    // One test owns the process-wide state (nextest runs each test in its
    // own process; the sequence below is the server's own call order).
    #[test]
    fn installs_follow_the_baseline_precedence() {
        // Startup with an env proxy, then the config proxy yields to it.
        install_global_proxy(None, &env(&[("HTTPS_PROXY", "http://127.0.0.1:2")]));
        let first = current().generation;
        assert_eq!(proxy_url().as_deref(), Some("http://127.0.0.1:2/"));
        install_global_proxy(Some("http://127.0.0.1:1"), &env(&[]));
        assert_eq!(current().generation, first);
        // A reload replaces it with the config URL and its noProxy.
        reinstall_global_proxy(Some("http://127.0.0.1:3"), Some("a.test"));
        assert_eq!(proxy_url().as_deref(), Some("http://127.0.0.1:3/"));
        assert!(
            current()
                .proxy
                .as_ref()
                .unwrap()
                .no_proxy()
                .ends_with("a.test,127.0.0.1,localhost")
        );
        // A URL undici rejects leaves the installed dispatcher alone.
        let before = current().generation;
        reinstall_global_proxy(Some("not a url"), None);
        assert_eq!(current().generation, before);
        assert_eq!(proxy_url().as_deref(), Some("http://127.0.0.1:3/"));
        // Removal goes direct.
        reinstall_global_proxy(None, None);
        assert!(current().proxy.is_none());
        assert!(current().generation > before);
        // A-R1: an unusable URL with nothing installed refuses (the baseline
        // stays direct), and a later valid URL recovers.
        reinstall_global_proxy(Some("http://127.0.0.1:4/sub"), None);
        assert!(current().proxy.as_ref().is_some_and(|p| p.is_refusing()));
        reinstall_global_proxy(Some("http://u:%zz@127.0.0.1:4"), None);
        assert!(current().proxy.as_ref().is_some_and(|p| p.is_refusing()));
        reinstall_global_proxy(Some("http://127.0.0.1:5"), None);
        assert_eq!(proxy_url().as_deref(), Some("http://127.0.0.1:5/"));
    }

    #[test]
    fn startup_structural_errors_refuse_and_userinfo_errors_go_direct() {
        // A structurally invalid env URL refuses, but does not block the
        // server's `proxy.url` (the baseline's failed env install does not).
        install_global_proxy(None, &env(&[("HTTPS_PROXY", "not a url")]));
        let refusing = current();
        assert!(refusing.proxy.as_ref().is_some_and(|p| p.is_refusing()));
        let loopback: hyper::Uri = "http://127.0.0.1:1/".parse().unwrap();
        let remote: hyper::Uri = "http://[::1]:1/".parse().unwrap();
        assert!(!refusing.proxy.as_ref().unwrap().should_proxy(&loopback));
        assert!(refusing.proxy.as_ref().unwrap().should_proxy(&remote));
        install_global_proxy(Some("http://127.0.0.1:6"), &env(&[]));
        assert_eq!(proxy_url().as_deref(), Some("http://127.0.0.1:6/"));
    }

    #[test]
    fn startup_userinfo_only_errors_stay_direct() {
        install_global_proxy(None, &env(&[]));
        install_global_proxy(Some("http://u:%zz@127.0.0.1:7"), &env(&[]));
        assert!(current().proxy.is_none());
    }

    #[test]
    fn a_client_is_rebuilt_only_after_a_swap() {
        let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = builds.clone();
        let client = DispatchedClient::global(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        });
        install_global_proxy(None, &env(&[]));
        assert_eq!(client.get().0, 0);
        assert_eq!(client.get().0, 0);
        reinstall_global_proxy(Some("http://127.0.0.1:9"), None);
        let (built, proxy) = client.get();
        assert_eq!(built, 1);
        assert!(proxy.is_some());
        assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
