//! The server's subsystem hot reload: `pilotdeck server` subscribes to the
//! config store (src/cli/pilotdeck.ts:136-191) and, for every published
//! reload whose `changedPaths` touch `alwaysOn` or `cron`, reloads the
//! changed automation families through the bundle (`automation.reload`,
//! ProjectAutomationBundle.ts:191-257) and prints
//! `[pilotdeck] Subsystem hot-reload complete: always-on=started, cron=stopped`
//! (only the changed families, always-on first). A rejected reload prints
//! `[pilotdeck] subsystem reload failed: <message>` on stderr.
//!
//! The listener itself only classifies the paths and queues the work: TS
//! chains every reload on one promise (`reloadChain`), so reloads run one at
//! a time in publication order — here one task drains a queue.
//!
//! A reload with a `changedPaths` entry starting with `adapters.` rebuilds and
//! reconciles the channels (`handleAdapterHotReload`, pilotdeck.ts:153-160,
//! 198-205 → `NativeChannels::reload`) and prints `[pilotdeck] Adapter
//! hot-reload complete: <key>=started, …, <key>=stopped` when anything was
//! started or stopped; a rejected reload prints `[pilotdeck] adapter
//! hot-reload failed: <message>` on stderr.
//!
//! A reload whose `changedPaths` touch `proxy` reinstalls the process-wide
//! fetch dispatcher from the new `proxy` section (see `reinstall_proxy`).
//!
//! The other branches of the same TS listener:
//! - `telemetry.*` → `telemetry.setEnabled(...)`: no visible output (the
//!   sender only starts or stops its flush timer, sender.ts:43-60), and this
//!   embedding does not upload telemetry, so there is nothing to switch.
use std::sync::Arc;

use pilotdeck_protocol::{JsString, JsValue};

use super::Published;
use super::automation::{
    ProjectAutomationBundle, ProjectAutomationConfig, ProjectAutomationReloadInput,
};
use crate::channel::protocol::ReconcileResult;
use crate::config::store::{ConfigStore, ReloadEvent, Subscription};
use crate::native_process::channels::{NativeChannels, console_log_line};
use crate::runtime::always_on::AlwaysOnConfig;
use crate::runtime::model::http::{ProxySettings, dispatcher};
use crate::switch::CronStopHandle;

/// Which listener branches one change set takes (pilotdeck.ts:138-147).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Branches {
    pub always_on: bool,
    pub cron: bool,
}

/// `p.startsWith("<name>.") || p === "<name>"`.
fn touches(paths: &[JsString], name: &str) -> bool {
    let exact: Vec<u16> = name.encode_utf16().collect();
    let mut prefix = exact.clone();
    prefix.push(u16::from(b'.'));
    paths.iter().any(|path| {
        let units = path.units();
        units == exact.as_slice() || units.starts_with(&prefix)
    })
}

pub(super) fn classify(paths: &[JsString]) -> Branches {
    Branches {
        always_on: touches(paths, "alwaysOn"),
        cron: touches(paths, "cron"),
    }
}

/// The automation half of a snapshot (`event.nextSnapshot.config`), read
/// the way the assembly reads the startup sections.
fn automation_config(snapshot: &JsValue) -> ProjectAutomationConfig {
    let config = snapshot.get("config");
    let section = |name: &str| config.and_then(|config| config.get(name)).cloned();
    ProjectAutomationConfig {
        always_on: section("alwaysOn")
            .as_ref()
            .map(AlwaysOnConfig::from_section),
        cron: section("cron"),
    }
}

/// `handleSubsystemReload` (pilotdeck.ts:173-188): the line names only the
/// changed families, always-on first.
pub(super) fn complete_line(branches: Branches, always_on: bool, cron: bool) -> String {
    let state = |started: bool| if started { "started" } else { "stopped" };
    let mut parts = Vec::new();
    if branches.always_on {
        parts.push(format!("always-on={}", state(always_on)));
    }
    if branches.cron {
        parts.push(format!("cron={}", state(cron)));
    }
    format!(
        "[pilotdeck] Subsystem hot-reload complete: {}",
        parts.join(", ")
    )
}

/// The `proxyChanged` branch (pilotdeck.ts:148-151):
/// `reinstallGlobalProxy(proxy?.url, proxy?.noProxy)` on the next
/// snapshot's normalized `proxy` section, before the automation branches and
/// outside their queue (TS does not chain it on `reloadChain`).
fn reinstall_proxy(snapshot: &JsValue) {
    let settings = snapshot
        .get("config")
        .and_then(|config| config.get("proxy"))
        .and_then(ProxySettings::from_config);
    dispatcher::reinstall_global_proxy(
        settings.as_ref().map(|settings| settings.url.as_str()),
        settings
            .as_ref()
            .and_then(|settings| settings.no_proxy.as_deref()),
    );
}

enum Job {
    Subsystem {
        branches: Branches,
        snapshot: Arc<JsValue>,
    },
    Adapters {
        snapshot: Arc<JsValue>,
    },
}

/// `adapterChanged` (pilotdeck.ts:146): some path starts with `adapters.` —
/// a whole `adapters` section added or removed (path `adapters`) does not
/// count.
pub(super) fn adapters_changed(paths: &[JsString]) -> bool {
    let prefix: Vec<u16> = "adapters.".encode_utf16().collect();
    paths.iter().any(|path| path.units().starts_with(&prefix))
}

/// `handleAdapterHotReload` (pilotdeck.ts:198-205): the line lists
/// `<key>=started` for every reconciled channel, then `<key>=stopped`; no
/// line when both are empty.
pub(super) fn adapter_complete_line(result: &ReconcileResult) -> Option<String> {
    let parts: Vec<String> = result
        .started
        .iter()
        .map(|key| format!("{key}=started"))
        .chain(result.stopped.iter().map(|key| format!("{key}=stopped")))
        .collect();
    (!parts.is_empty()).then(|| {
        format!(
            "[pilotdeck] Adapter hot-reload complete: {}",
            parts.join(", ")
        )
    })
}

/// Subscribe the server's listener. Must be called after the automation
/// bundle started (the TS subscription is made after `bootstrap.run`) and
/// from inside the gateway runtime, which runs the reload queue. The switch
/// handle is consulted so that a reload arriving after the stop machine
/// already stopped the cron manager does not start a new one behind it.
/// Adapter reloads share the queue (TS chains both on `reloadChain`, the
/// adapter one first); a reload that reaches a closing channel server fails
/// with its error like TS's.
pub(super) fn subscribe(
    store: &ConfigStore,
    automation: Arc<ProjectAutomationBundle>,
    switch_cron: Published<CronStopHandle>,
    channels: Arc<NativeChannels>,
) -> Subscription {
    let (queue, mut jobs) = tokio::sync::mpsc::unbounded_channel::<Job>();
    tokio::spawn(async move {
        while let Some(job) = jobs.recv().await {
            match job {
                Job::Adapters { snapshot } => {
                    let config = snapshot
                        .get("config")
                        .cloned()
                        .unwrap_or_else(|| JsValue::Object(Default::default()));
                    match channels.reload(&config).await {
                        Ok(result) => {
                            if let Some(line) = adapter_complete_line(&result) {
                                console_log_line(&mut std::io::stdout(), &line);
                            }
                        }
                        Err(error) => console_log_line(
                            &mut std::io::stderr(),
                            &format!("[pilotdeck] adapter hot-reload failed: {}", error.message()),
                        ),
                    }
                }
                Job::Subsystem { branches, snapshot } => {
                    if switch_cron.get().is_some_and(|handle| handle.is_stopped()) {
                        continue;
                    }
                    let input = ProjectAutomationReloadInput {
                        config: automation_config(&snapshot),
                        always_on_changed: branches.always_on,
                        cron_changed: branches.cron,
                    };
                    match automation.reload(input).await {
                        Ok(status) => {
                            println!("{}", complete_line(branches, status.always_on, status.cron))
                        }
                        // `console.warn(`... ${err.message}`)`: the message alone,
                        // not the aggregate's causes.
                        Err(error) => {
                            eprintln!("[pilotdeck] subsystem reload failed: {}", error.message)
                        }
                    }
                }
            }
        }
    });
    store.subscribe(Arc::new(move |event: &ReloadEvent| {
        if touches(&event.changed_paths, "proxy") {
            reinstall_proxy(&event.next_snapshot);
        }
        if adapters_changed(&event.changed_paths) {
            let _ = queue.send(Job::Adapters {
                snapshot: event.next_snapshot.clone(),
            });
        }
        let branches = classify(&event.changed_paths);
        if !branches.always_on && !branches.cron {
            return;
        }
        let _ = queue.send(Job::Subsystem {
            branches,
            snapshot: event.next_snapshot.clone(),
        });
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(items: &[&str]) -> Vec<JsString> {
        items.iter().map(|item| JsString::from(*item)).collect()
    }

    #[test]
    fn classification_matches_the_listener_prefixes() {
        assert_eq!(
            classify(&paths(&["cron"])),
            Branches {
                always_on: false,
                cron: true
            }
        );
        assert_eq!(
            classify(&paths(&["agent.maxOutputTokens", "cron.timezone"])),
            Branches {
                always_on: false,
                cron: true
            }
        );
        assert_eq!(
            classify(&paths(&["alwaysOn.projects./x.enabled"])),
            Branches {
                always_on: true,
                cron: false
            }
        );
        // Neither a longer key nor an unrelated path counts.
        assert_eq!(
            classify(&paths(&["cronx", "alwaysOnly", "agent"])),
            Branches::default()
        );
    }

    // Measured on the baseline (bbx `hot-reload-adapters`): a whole section
    // added or removed reports the path `adapters` and does not reload.
    #[test]
    fn only_paths_under_adapters_reload_the_channels() {
        assert!(adapters_changed(&paths(&["adapters.telegram.token"])));
        assert!(adapters_changed(&paths(&["agent", "adapters.qq"])));
        assert!(!adapters_changed(&paths(&["adapters"])));
        assert!(!adapters_changed(&paths(&["adaptersx.a", "cron"])));
    }

    #[test]
    fn the_adapter_line_lists_started_then_stopped() {
        use crate::channel::protocol::ChannelKey;
        let keys = |names: &[&str]| names.iter().map(|name| ChannelKey::new(*name)).collect();
        assert_eq!(adapter_complete_line(&ReconcileResult::default()), None);
        assert_eq!(
            adapter_complete_line(&ReconcileResult {
                started: keys(&["qq", "wecom", "matrix"]),
                stopped: keys(&["discord"]),
            })
            .as_deref(),
            Some(
                "[pilotdeck] Adapter hot-reload complete: qq=started, wecom=started, matrix=started, discord=stopped"
            )
        );
        assert_eq!(
            adapter_complete_line(&ReconcileResult {
                started: Vec::new(),
                stopped: keys(&["telegram"]),
            })
            .as_deref(),
            Some("[pilotdeck] Adapter hot-reload complete: telegram=stopped")
        );
    }

    #[test]
    fn the_complete_line_names_only_the_changed_families() {
        assert_eq!(
            complete_line(
                Branches {
                    always_on: true,
                    cron: true
                },
                false,
                true
            ),
            "[pilotdeck] Subsystem hot-reload complete: always-on=stopped, cron=started"
        );
        assert_eq!(
            complete_line(
                Branches {
                    always_on: false,
                    cron: true
                },
                true,
                false
            ),
            "[pilotdeck] Subsystem hot-reload complete: cron=stopped"
        );
    }
}
