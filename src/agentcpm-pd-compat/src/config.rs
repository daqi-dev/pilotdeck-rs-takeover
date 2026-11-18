//! Explicit shim configuration, stored under `$AGENTCPM_PD_HOME/compat/`.
//!
//! The shim never defaults to a real home directory: when
//! `AGENTCPM_PD_HOME` is unset or empty, loading fails with
//! `configuration_required` (the `context` entry semantics), and tests
//! inject an isolated temporary home instead of touching `~/.pilotdeck` or
//! `~/.agentcpm-pd`.
//!
//! Schema (v2, finalized by the WIRE wave with the 27-entry mapping data):
//! - `defaultTarget` — target profile passed as `--target` to the CLI when
//!   an entry resolved no legacy flags (`None` keeps the CLI's own default,
//!   the local instance under `AGENTCPM_PD_HOME`);
//! - `legacyTargets` — explicit mappings of legacy endpoint parameters
//!   (`--gateway-url`, `--project`, `--channel`) onto a new target profile;
//! - `defaultProjectId` — project for entries that need a project without a
//!   legacy `--project` flag (`sessions`, `cron create`);
//! - `promptTargetId` / `promptSessionId` — the explicit default session
//!   policy for the prompt-style entries (`default-argv`, `default-stdin`),
//!   required (no implicit session is ever created).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Environment variable selecting the (isolated) agentcpm-pd home.
pub const HOME_ENV: &str = "AGENTCPM_PD_HOME";
/// Subdirectory of the home holding shim-owned state.
pub const COMPAT_DIR: &str = "compat";
/// Config file name inside `compat/`.
pub const CONFIG_FILE: &str = "config.json";

/// Current config schema version.
pub const SCHEMA_VERSION: u32 = 2;

/// Explicit legacy-endpoint → new-target mapping.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyTargetMapping {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub target: String,
}

/// Shim configuration document.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatConfig {
    pub schema_version: u32,
    /// Default target profile for business entries (`--target` value);
    /// `None` keeps the CLI's own default (local instance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_target: Option<String>,
    /// Explicit legacy endpoint mappings (no implicit fallback).
    #[serde(default)]
    pub legacy_targets: Vec<LegacyTargetMapping>,
    /// Default project for project-scoped entries without a legacy
    /// `--project` flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project_id: Option<String>,
    /// Explicit default session policy for the prompt-style entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_session_id: Option<String>,
}

impl Default for CompatConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            default_target: None,
            legacy_targets: Vec::new(),
            default_project_id: None,
            prompt_target_id: None,
            prompt_session_id: None,
        }
    }
}

/// Config loading failure. `HomeNotSet` is the `configuration_required`
/// contract behavior: no implicit default home is ever used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    HomeNotSet,
    Io(String),
    Parse(String),
}

impl ConfigError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::HomeNotSet => "configuration_required",
            Self::Io(_) | Self::Parse(_) => "compat_config_invalid",
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HomeNotSet => write!(
                f,
                "{HOME_ENV} is not set; the shim never defaults to a real home directory (error: configuration_required)"
            ),
            Self::Io(m) => write!(f, "compat config I/O failure: {m}"),
            Self::Parse(m) => write!(f, "compat config is not a valid v1 document: {m}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Resolves the shim home from the environment value. `None` or empty means
/// `configuration_required` — by design there is no fallback home.
pub fn resolve_home(env_value: Option<&OsStr>) -> Result<PathBuf, ConfigError> {
    match env_value.filter(|v| !v.is_empty()) {
        Some(v) => Ok(PathBuf::from(v)),
        None => Err(ConfigError::HomeNotSet),
    }
}

/// Path of the config file under an explicit home.
pub fn config_path(home: &Path) -> PathBuf {
    home.join(COMPAT_DIR).join(CONFIG_FILE)
}

/// Loads the config; `Ok(None)` when no config file exists yet (a fresh
/// install). A present-but-invalid document is an error, not a silent
/// default.
pub fn load(home: &Path) -> Result<Option<CompatConfig>, ConfigError> {
    let path = config_path(home);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ConfigError::Io(format!("{}: {e}", path.display()))),
    };
    let config: CompatConfig =
        serde_json::from_str(&raw).map_err(|e| ConfigError::Parse(e.to_string()))?;
    if config.schema_version != SCHEMA_VERSION {
        return Err(ConfigError::Parse(format!(
            "schema version {} is not supported (expected {SCHEMA_VERSION})",
            config.schema_version
        )));
    }
    Ok(Some(config))
}

/// Legacy endpoint flags as parsed from old SDK options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LegacyFlags {
    pub gateway_url: Option<String>,
    pub project: Option<String>,
    pub channel: Option<String>,
}

impl LegacyFlags {
    /// Whether any legacy endpoint flag was provided (a lookup must happen).
    pub fn any(&self) -> bool {
        self.gateway_url.is_some() || self.project.is_some() || self.channel.is_some()
    }
}

impl CompatConfig {
    /// Resolves the target profile for one entry invocation:
    /// - legacy flags present → the first `legacyTargets` row whose
    ///   specified fields all equal the provided flags matches (unspecified
    ///   row fields are wildcards); no match → `None` (the caller reports
    ///   `configuration_required` — the shim never guesses a target);
    /// - no legacy flags → `Some(None)` carrying `default_target` (or the
    ///   CLI's own default when that is unset too).
    ///
    /// `--auth-token` never participates: credentials live in the new
    /// target profiles, not in shim config (nothing is stored, forwarded,
    /// or logged).
    pub fn resolve_target(&self, flags: &LegacyFlags) -> Result<Option<String>, NoLegacyMatch> {
        if flags.any() {
            for row in &self.legacy_targets {
                let url_ok = row
                    .gateway_url
                    .as_ref()
                    .is_none_or(|v| flags.gateway_url.as_ref() == Some(v));
                let project_ok = row
                    .project
                    .as_ref()
                    .is_none_or(|v| flags.project.as_ref() == Some(v));
                let channel_ok = row
                    .channel
                    .as_ref()
                    .is_none_or(|v| flags.channel.as_ref() == Some(v));
                if url_ok && project_ok && channel_ok {
                    return Ok(Some(row.target.clone()));
                }
            }
            Err(NoLegacyMatch)
        } else {
            Ok(self.default_target.clone())
        }
    }
}

/// No `legacyTargets` row matched the provided legacy flags (the caller
/// reports `configuration_required` — the shim never guesses a target).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoLegacyMatch;

/// Saves the config, creating `compat/` as needed.
pub fn save(home: &Path, config: &CompatConfig) -> Result<(), ConfigError> {
    let dir = home.join(COMPAT_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ConfigError::Io(format!("{}: {e}", dir.display())))?;
    let path = config_path(home);
    let raw = serde_json::to_string_pretty(config)
        .map_err(|e| ConfigError::Io(format!("serialize: {e}")))?;
    std::fs::write(&path, raw).map_err(|e| ConfigError::Io(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::ffi::OsString;

    #[test]
    fn missing_home_is_configuration_required() {
        let err = resolve_home(None).unwrap_err();
        assert_eq!(err, ConfigError::HomeNotSet);
        assert_eq!(err.code(), "configuration_required");
        assert!(err.to_string().contains(HOME_ENV));
        // Empty is treated as unset.
        assert_eq!(
            resolve_home(Some(OsStr::new(""))).unwrap_err(),
            ConfigError::HomeNotSet
        );
        let home: OsString = "/tmp/agentcpm-pd-home".into();
        assert_eq!(
            resolve_home(Some(home.as_os_str())).unwrap(),
            PathBuf::from("/tmp/agentcpm-pd-home")
        );
    }

    #[test]
    fn config_path_is_compat_config_json_under_home() {
        let path = config_path(Path::new("/h"));
        assert_eq!(path, PathBuf::from("/h/compat/config.json"));
    }

    #[test]
    fn load_returns_none_for_fresh_home_and_round_trips() {
        let home = TempDir::new("config");
        assert_eq!(load(home.path()).unwrap(), None);

        let config = CompatConfig {
            default_target: Some("default".to_owned()),
            legacy_targets: vec![LegacyTargetMapping {
                gateway_url: Some("http://old:7000".to_owned()),
                project: Some("proj".to_owned()),
                channel: None,
                target: "t1".to_owned(),
            }],
            default_project_id: Some("shim-p-1".to_owned()),
            prompt_target_id: Some("target-1".to_owned()),
            prompt_session_id: Some("shim-s-1".to_owned()),
            ..CompatConfig::default()
        };
        save(home.path(), &config).unwrap();
        assert_eq!(load(home.path()).unwrap().as_ref(), Some(&config));

        // The document is written under compat/ with the schema version.
        let raw = std::fs::read_to_string(config_path(home.path())).unwrap();
        assert!(raw.contains("\"schemaVersion\": 2"));
        assert!(raw.contains("\"gatewayUrl\""));
        assert!(raw.contains("\"promptTargetId\""));
    }

    #[test]
    fn target_resolution_matches_legacy_rows_and_never_guesses() {
        let config = CompatConfig {
            default_target: Some("fallback-profile".to_owned()),
            legacy_targets: vec![
                LegacyTargetMapping {
                    gateway_url: Some("ws://old:18789/ws".to_owned()),
                    project: None,
                    channel: None,
                    target: "old-gw".to_owned(),
                },
                LegacyTargetMapping {
                    gateway_url: None,
                    project: Some("proj-2".to_owned()),
                    channel: Some("feishu".to_owned()),
                    target: "proj2-feishu".to_owned(),
                },
            ],
            ..CompatConfig::default()
        };

        // Exact legacy match.
        let flags = LegacyFlags {
            gateway_url: Some("ws://old:18789/ws".to_owned()),
            project: None,
            channel: None,
        };
        assert_eq!(config.resolve_target(&flags), Ok(Some("old-gw".to_owned())));
        // Row with unspecified fields acts as a wildcard for those fields.
        let flags = LegacyFlags {
            gateway_url: Some("ws://old:18789/ws".to_owned()),
            project: Some("anything".to_owned()),
            channel: None,
        };
        assert_eq!(config.resolve_target(&flags), Ok(Some("old-gw".to_owned())));
        // Second row matches on project+channel.
        let flags = LegacyFlags {
            gateway_url: None,
            project: Some("proj-2".to_owned()),
            channel: Some("feishu".to_owned()),
        };
        assert_eq!(
            config.resolve_target(&flags),
            Ok(Some("proj2-feishu".to_owned()))
        );
        // Unmatched legacy flags must fail, never fall through to default.
        let flags = LegacyFlags {
            gateway_url: Some("ws://unknown:1/ws".to_owned()),
            project: None,
            channel: None,
        };
        assert_eq!(config.resolve_target(&flags), Err(NoLegacyMatch));
        // No legacy flags: default target (or CLI default when unset).
        assert_eq!(
            config.resolve_target(&LegacyFlags::default()),
            Ok(Some("fallback-profile".to_owned()))
        );
        let bare = CompatConfig::default();
        assert_eq!(bare.resolve_target(&LegacyFlags::default()), Ok(None));
    }

    #[test]
    fn invalid_documents_fail_loudly() {
        let home = TempDir::new("config-invalid");
        std::fs::create_dir_all(home.path().join(COMPAT_DIR)).unwrap();
        std::fs::write(config_path(home.path()), "not json").unwrap();
        assert!(matches!(load(home.path()), Err(ConfigError::Parse(_))));

        let future = serde_json::to_string_pretty(&CompatConfig {
            schema_version: 99,
            ..CompatConfig::default()
        })
        .unwrap();
        std::fs::write(config_path(home.path()), future).unwrap();
        match load(home.path()) {
            Err(ConfigError::Parse(m)) => assert!(m.contains("not supported")),
            other => panic!("expected version mismatch, got {other:?}"),
        }
    }

    #[test]
    fn default_config_serializes_minimally() {
        let json = serde_json::to_value(CompatConfig::default()).unwrap();
        assert_eq!(json["schemaVersion"], 2);
        assert!(json.get("defaultTarget").is_none());
        assert!(json.get("defaultProjectId").is_none());
        assert!(json.get("promptTargetId").is_none());
        assert!(json.get("promptSessionId").is_none());
        assert_eq!(json["legacyTargets"].as_array().map(Vec::len), Some(0));
    }
}
