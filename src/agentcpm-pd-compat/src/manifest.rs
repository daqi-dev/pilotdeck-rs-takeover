//! Builtin compat manifest: the authoritative procedural table of the 27
//! legacy `pilotdeck` entries (slugs from `cli.case.compat.*`), their
//! dispositions, and the contract version.
//!
//! Division of responsibility with `mappings/*.json`: this module holds
//! *procedural* facts (which entry exists, how it is disposed, which module
//! owns it, which legacy argv triggers it); the JSON resources hold per-entry
//! *mapping data* (argv patterns, old semantics, target operation sequence,
//! projections). The two are joined on slug and validated for consistency at
//! load time (see [`crate::mappings`]).
//!
//! All data here is read-only and verifiable without starting a gateway or
//! reading any home directory; the shim exposes it through
//! `pilotdeck compat manifest`.

use serde_json::{Map, Value};

/// Contract version the shim adapts onto (decision D11 / `007` v0.7.1).
pub const CONTRACT_VERSION: &str = "agentcpm-pd 007 v0.7.1";

/// Disposition of a legacy entry (`cli.case.compat.*` expectation). The
/// lowercase wire values are the mapping JSON schema (`supported`,
/// `adapted`, `rejected`); anything else fails mapping loads loudly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Disposition {
    /// Directly supported by public operations.
    Supported,
    /// Supported with accepted differences (explicit target mapping, projections).
    Adapted,
    /// Rejected locally with an explicit error code.
    Rejected,
}

impl Disposition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Adapted => "adapted",
            Self::Rejected => "rejected",
        }
    }
}

/// Entry module that owns the runtime behavior of a legacy entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handler {
    Help,
    Context,
    DefaultArgv,
    DefaultStdin,
    Run,
    Resume,
    Sessions,
    Settings,
    Server,
    Cron,
    Setup,
    ChatSearch,
    Skills,
    Update,
    UsageErrors,
    SidecarStdio,
    SidecarTcp,
    SidecarFactory,
    GatewayBin,
}

/// One row of the builtin manifest.
pub struct ManifestEntry {
    /// `cli.case.compat.*` slug (join key with `mappings/*.json`).
    pub slug: &'static str,
    pub disposition: Disposition,
    pub handler: Handler,
    /// Legacy argv syntax that triggers the entry (from the case ledger).
    pub argv_patterns: &'static [&'static str],
}

/// The builtin 27-entry table. Order matches the S4 plan §2 listing.
pub static ENTRIES: &[ManifestEntry] = &[
    ManifestEntry {
        slug: "entry-help",
        disposition: Disposition::Supported,
        handler: Handler::Help,
        argv_patterns: &["help", "--help", "-h"],
    },
    ManifestEntry {
        slug: "context",
        disposition: Disposition::Adapted,
        handler: Handler::Context,
        argv_patterns: &[
            "SDK --gateway-url/--auth-token/--project/--channel/--model/--permission-mode; legacy env/data roots",
        ],
    },
    ManifestEntry {
        slug: "default-argv",
        disposition: Disposition::Adapted,
        handler: Handler::DefaultArgv,
        argv_patterns: &["<argv-as-prompt> (incl. unknown top-level words)"],
    },
    ManifestEntry {
        slug: "default-stdin",
        disposition: Disposition::Supported,
        handler: Handler::DefaultStdin,
        argv_patterns: &["(empty argv; stdin line loop)"],
    },
    ManifestEntry {
        slug: "sdk-run",
        disposition: Disposition::Supported,
        handler: Handler::Run,
        argv_patterns: &[
            "run <prompt> [--json] [--stream-json] [SDK options]",
            "(no prompt: read stdin)",
        ],
    },
    ManifestEntry {
        slug: "sdk-resume",
        disposition: Disposition::Supported,
        handler: Handler::Resume,
        argv_patterns: &["resume <session-id> <prompt> [SDK options]"],
    },
    ManifestEntry {
        slug: "sessions-list",
        disposition: Disposition::Supported,
        handler: Handler::Sessions,
        argv_patterns: &["sessions [list] [--project <key>]"],
    },
    ManifestEntry {
        slug: "sessions-messages",
        disposition: Disposition::Adapted,
        handler: Handler::Sessions,
        argv_patterns: &["sessions messages <session-id> [--project <key>]"],
    },
    ManifestEntry {
        slug: "settings-get",
        disposition: Disposition::Adapted,
        handler: Handler::Settings,
        argv_patterns: &["settings [get]"],
    },
    ManifestEntry {
        slug: "settings-set",
        disposition: Disposition::Adapted,
        handler: Handler::Settings,
        argv_patterns: &["settings set <json>", "settings set --json <payload>"],
    },
    ManifestEntry {
        slug: "server",
        disposition: Disposition::Adapted,
        handler: Handler::Server,
        argv_patterns: &["server [--port N] [--host H]"],
    },
    ManifestEntry {
        slug: "cron-list",
        disposition: Disposition::Supported,
        handler: Handler::Cron,
        argv_patterns: &["cron list [--history] [--limit N]"],
    },
    ManifestEntry {
        slug: "cron-create",
        disposition: Disposition::Supported,
        handler: Handler::Cron,
        argv_patterns: &[
            "cron create --session ID --message TEXT (--once ISO | --cron EXPR) [--channel C] [--project P] [--timezone Z]",
        ],
    },
    ManifestEntry {
        slug: "cron-delete",
        disposition: Disposition::Supported,
        handler: Handler::Cron,
        argv_patterns: &["cron delete <taskId> [--stop-running]"],
    },
    ManifestEntry {
        slug: "cron-stop",
        disposition: Disposition::Adapted,
        handler: Handler::Cron,
        argv_patterns: &["cron stop <taskId>", "cron stop --run <runId>"],
    },
    ManifestEntry {
        slug: "setup-menu",
        disposition: Disposition::Adapted,
        handler: Handler::Setup,
        argv_patterns: &["gateway setup (no/unknown platform)"],
    },
    ManifestEntry {
        slug: "setup-feishu",
        disposition: Disposition::Adapted,
        handler: Handler::Setup,
        argv_patterns: &["gateway setup feishu|lark"],
    },
    ManifestEntry {
        slug: "setup-weixin",
        disposition: Disposition::Adapted,
        handler: Handler::Setup,
        argv_patterns: &["gateway setup weixin|wechat"],
    },
    ManifestEntry {
        slug: "setup-wecom",
        disposition: Disposition::Adapted,
        handler: Handler::Setup,
        argv_patterns: &["gateway setup wecom|work-weixin|workwechat"],
    },
    ManifestEntry {
        slug: "chat-search",
        disposition: Disposition::Adapted,
        handler: Handler::ChatSearch,
        argv_patterns: &["chat search <keyword> [filters]"],
    },
    ManifestEntry {
        slug: "skills-migrate",
        disposition: Disposition::Adapted,
        handler: Handler::Skills,
        argv_patterns: &["skills migrate [options]"],
    },
    ManifestEntry {
        slug: "update",
        disposition: Disposition::Rejected,
        handler: Handler::Update,
        argv_patterns: &["update [argv forwarded to the Web updater]"],
    },
    ManifestEntry {
        slug: "usage-errors",
        disposition: Disposition::Adapted,
        handler: Handler::UsageErrors,
        argv_patterns: &[
            "missing args / unknown subcommand in a known group",
            "unknown top-level words",
            "--version",
        ],
    },
    ManifestEntry {
        slug: "sidecar-stdio",
        disposition: Disposition::Rejected,
        handler: Handler::SidecarStdio,
        argv_patterns: &["pilotdeck-agent-loop-sidecar [--stdio]"],
    },
    ManifestEntry {
        slug: "sidecar-tcp",
        disposition: Disposition::Rejected,
        handler: Handler::SidecarTcp,
        argv_patterns: &["pilotdeck-agent-loop-sidecar + PILOTDECK_AGENT_LOOP_TCP_PORT/HOST"],
    },
    ManifestEntry {
        slug: "sidecar-factory",
        disposition: Disposition::Rejected,
        handler: Handler::SidecarFactory,
        argv_patterns: &["pilotdeck-agent-loop-sidecar + PILOTDECK_AGENT_LOOP_FACTORY=<JS module>"],
    },
    ManifestEntry {
        slug: "rust-gateway-bin",
        disposition: Disposition::Rejected,
        handler: Handler::GatewayBin,
        argv_patterns: &[
            "pilotdeck-gateway [--native ...]",
            "PILOTDECK_GATEWAY_MODE=proxy",
        ],
    },
];

/// Read-only view over the builtin table.
#[derive(Clone, Copy)]
pub struct Manifest {
    entries: &'static [ManifestEntry],
}

impl Manifest {
    pub fn builtin() -> Self {
        Self { entries: ENTRIES }
    }

    pub fn entries(&self) -> &'static [ManifestEntry] {
        self.entries
    }

    pub fn find(&self, slug: &str) -> Option<&'static ManifestEntry> {
        self.entries.iter().find(|e| e.slug == slug)
    }

    pub fn contains(&self, slug: &str) -> bool {
        self.find(slug).is_some()
    }

    /// Read-only verifiable JSON summary, annotated with which slugs have
    /// mapping data loaded. Consumed by `pilotdeck compat manifest` and the
    /// help entry.
    pub fn summary_json(&self, loaded: &crate::mappings::Mappings) -> String {
        let mut root = Map::new();
        root.insert("shim".into(), Value::from(crate::SHIM_NAME));
        root.insert("shimVersion".into(), Value::from(env!("CARGO_PKG_VERSION")));
        root.insert("contractVersion".into(), Value::from(CONTRACT_VERSION));
        root.insert("entryCount".into(), Value::from(self.entries.len()));
        root.insert("mappedCount".into(), Value::from(loaded.entries().count()));
        let entries: Vec<Value> = self
            .entries
            .iter()
            .map(|e| {
                let mut o = Map::new();
                o.insert("slug".into(), Value::from(e.slug));
                o.insert("disposition".into(), Value::from(e.disposition.as_str()));
                o.insert("mapped".into(), Value::from(loaded.find(e.slug).is_some()));
                o.insert(
                    "oldArgv".into(),
                    Value::from(
                        e.argv_patterns
                            .iter()
                            .map(|p| Value::from(*p))
                            .collect::<Vec<_>>(),
                    ),
                );
                Value::Object(o)
            })
            .collect();
        root.insert("entries".into(), Value::Array(entries));
        Value::Object(root).to_string()
    }
}

impl Handler {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::Context => "context",
            Self::DefaultArgv => "default-argv",
            Self::DefaultStdin => "default-stdin",
            Self::Run => "run",
            Self::Resume => "resume",
            Self::Sessions => "sessions",
            Self::Settings => "settings",
            Self::Server => "server",
            Self::Cron => "cron",
            Self::Setup => "setup",
            Self::ChatSearch => "chat_search",
            Self::Skills => "skills",
            Self::Update => "update",
            Self::UsageErrors => "usage_errors",
            Self::SidecarStdio => "sidecar-stdio",
            Self::SidecarTcp => "sidecar-tcp",
            Self::SidecarFactory => "sidecar-factory",
            Self::GatewayBin => "rust-gateway-bin",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn manifest_lists_exactly_27_unique_slugs() {
        assert_eq!(ENTRIES.len(), 27);
        let slugs: HashSet<&str> = ENTRIES.iter().map(|e| e.slug).collect();
        assert_eq!(slugs.len(), ENTRIES.len(), "duplicate slugs in manifest");
    }

    #[test]
    fn disposition_counts_match_the_approved_decision() {
        let supported = ENTRIES
            .iter()
            .filter(|e| e.disposition == Disposition::Supported)
            .count();
        let adapted = ENTRIES
            .iter()
            .filter(|e| e.disposition == Disposition::Adapted)
            .count();
        let rejected = ENTRIES
            .iter()
            .filter(|e| e.disposition == Disposition::Rejected)
            .count();
        assert_eq!(supported, 8, "supported entries (plan §2)");
        assert_eq!(adapted, 14, "adapted entries (plan §2)");
        assert_eq!(rejected, 5, "rejected entries (plan §2)");
    }

    #[test]
    fn contract_version_matches_the_case_ledger() {
        assert_eq!(CONTRACT_VERSION, "agentcpm-pd 007 v0.7.1");
    }

    #[test]
    fn find_and_contains_work_on_join_key() {
        let m = Manifest::builtin();
        assert!(m.contains("sdk-run"));
        assert_eq!(
            m.find("update").map(|e| e.disposition),
            Some(Disposition::Rejected)
        );
        assert!(!m.contains("no-such-slug"));
    }

    #[test]
    fn every_entry_has_at_least_one_argv_pattern() {
        assert!(ENTRIES.iter().all(|e| !e.argv_patterns.is_empty()));
    }

    #[test]
    fn summary_json_is_stable_and_complete() {
        let m = Manifest::builtin();
        let loaded = crate::mappings::Mappings::load().expect("placeholder mappings load");
        let value: Value =
            serde_json::from_str(&m.summary_json(&loaded)).expect("summary is valid JSON");
        assert_eq!(value["contractVersion"], CONTRACT_VERSION);
        assert_eq!(value["entryCount"], 27);
        assert_eq!(value["entries"].as_array().map(Vec::len), Some(27));
        let slugs: HashSet<&str> = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs.len(), 27);
    }
}
