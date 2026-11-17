//! Isolated instance home discovery (`AGENTCPM_PD_HOME` -> `~/.agentcpm-pd`),
//! `0700`/`0600` credential/config persistence, and strict D8 target resolution.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agentcpm_pd_protocol::errors::{
    CODE_FORBIDDEN, CODE_INVALID_ARGUMENT, ContractError, EffectStatus, RetrySpec,
};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::ClientError;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn expand_tilde_with_env(raw: &str) -> Result<PathBuf, ClientError> {
    if raw == "~" {
        return env_home();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(env_home()?.join(rest));
    }
    Ok(PathBuf::from(raw))
}

fn env_home() -> Result<PathBuf, ClientError> {
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ClientError::config("HOME environment variable is not set"))?;
    Ok(PathBuf::from(home))
}

/// Resolves `AGENTCPM_PD_HOME` (`explicit` -> env `AGENTCPM_PD_HOME` -> `$HOME/.agentcpm-pd`).
/// Explicitly isolated from `~/.pilotdeck`.
pub fn resolve_pd_home(explicit: Option<&Path>) -> Result<PathBuf, ClientError> {
    if let Some(p) = explicit {
        return if let Some(s) = p.to_str() {
            expand_tilde_with_env(s)
        } else {
            Ok(p.to_path_buf())
        };
    }
    if let Some(env_val) = std::env::var_os("AGENTCPM_PD_HOME").filter(|v| !v.is_empty()) {
        let raw = env_val.to_string_lossy();
        return expand_tilde_with_env(&raw);
    }
    Ok(env_home()?.join(".agentcpm-pd"))
}

pub fn local_metadata_path(home: &Path) -> PathBuf {
    home.join("local.json")
}

pub fn local_token_path(home: &Path) -> PathBuf {
    home.join("local.token")
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("config.json")
}

pub fn credentials_path(home: &Path) -> PathBuf {
    home.join("credentials.json")
}

pub fn ready_path(home: &Path) -> PathBuf {
    home.join("run").join("ready.json")
}

pub fn daemon_lock_path(home: &Path) -> PathBuf {
    home.join("run").join("daemon.lock")
}

pub fn daemon_pid_path(home: &Path) -> PathBuf {
    home.join("run").join("daemon.pid")
}

pub fn idempotency_ledger_path(home: &Path) -> PathBuf {
    home.join("client").join("idempotency.json")
}

pub fn idempotency_lock_path(home: &Path) -> PathBuf {
    home.join("client").join("idempotency.lock")
}

pub fn sqlite_state_path(home: &Path) -> PathBuf {
    home.join("v1").join("state.sqlite")
}

/// Creates a directory (and ancestors) and enforces `0700` on unix.
pub fn ensure_secure_dir(path: &Path) -> Result<(), ClientError> {
    fs::create_dir_all(path).map_err(|e| {
        ClientError::config(format!(
            "failed to create directory {}: {e}",
            path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| {
            ClientError::config(format!(
                "failed to chmod 0700 directory {}: {e}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

/// Atomically writes a file with restrictive permissions (`0600` on unix when `secret` is true).
pub fn atomic_write_file(path: &Path, bytes: &[u8], secret: bool) -> Result<(), ClientError> {
    let parent = path
        .parent()
        .ok_or_else(|| ClientError::config(format!("path {} has no parent", path.display())))?;
    ensure_secure_dir(parent)?;

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        ".tmp.{}.{}.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id(),
        seq
    );
    let tmp_path = parent.join(tmp_name);

    let mode = if secret { 0o600 } else { 0o644 };
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }

    let mut f = opts.open(&tmp_path).map_err(|e| {
        ClientError::config(format!(
            "failed to create temp file {}: {e}",
            tmp_path.display()
        ))
    })?;
    f.write_all(bytes).map_err(|e| {
        ClientError::config(format!(
            "failed to write temp file {}: {e}",
            tmp_path.display()
        ))
    })?;
    f.sync_all().map_err(|e| {
        ClientError::config(format!(
            "failed to sync temp file {}: {e}",
            tmp_path.display()
        ))
    })?;
    drop(f);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode));
    }

    fs::rename(&tmp_path, path).map_err(|e| {
        ClientError::config(format!(
            "failed to rename {} -> {}: {e}",
            tmp_path.display(),
            path.display()
        ))
    })?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalInstanceMetadata {
    pub instance_id: String,
    pub owner_id: String,
    pub product: String,
    pub protocol_version: String,
    pub node_id: String,
    pub initialized_at: String,
    pub initialization_revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyFile {
    pub pid: u32,
    pub instance_id: String,
    pub generation: String,
    pub protocol_version: String,
    pub ws_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_url: Option<String>,
    pub started_at: String,
    pub server_revision: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Local,
    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetProfile {
    pub id: String,
    pub kind: TargetKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_target: Option<String>,
    #[serde(default)]
    pub targets: BTreeMap<String, TargetProfile>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsFile {
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedTarget {
    Local {
        profile_name: String,
        home: PathBuf,
        metadata: LocalInstanceMetadata,
        token: String,
    },
    Remote {
        profile_name: String,
        ws_url: String,
        expected_instance_id: Option<String>,
        token: Option<String>,
    },
}

impl ResolvedTarget {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    pub fn expected_instance_id(&self) -> Option<&str> {
        match self {
            Self::Local { metadata, .. } => Some(&metadata.instance_id),
            Self::Remote {
                expected_instance_id,
                ..
            } => expected_instance_id.as_deref(),
        }
    }
}

pub fn load_client_config(home: &Path) -> Result<ClientConfig, ClientError> {
    let path = config_path(home);
    if !path.exists() {
        return Ok(ClientConfig::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| ClientError::config(format!("failed to read {}: {e}", path.display())))?;
    serde_json::from_str(&raw)
        .map_err(|e| ClientError::config(format!("malformed {}: {e}", path.display())))
}

pub fn save_client_config(home: &Path, cfg: &ClientConfig) -> Result<(), ClientError> {
    let bytes = serde_json::to_vec_pretty(cfg)
        .map_err(|e| ClientError::config(format!("serialize config.json: {e}")))?;
    atomic_write_file(&config_path(home), &bytes, true)
}

pub fn load_credentials(home: &Path) -> Result<CredentialsFile, ClientError> {
    let path = credentials_path(home);
    if !path.exists() {
        return Ok(CredentialsFile::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| ClientError::config(format!("failed to read {}: {e}", path.display())))?;
    serde_json::from_str(&raw)
        .map_err(|e| ClientError::config(format!("malformed {}: {e}", path.display())))
}

pub fn save_credentials(home: &Path, creds: &CredentialsFile) -> Result<(), ClientError> {
    let bytes = serde_json::to_vec_pretty(creds)
        .map_err(|e| ClientError::config(format!("serialize credentials.json: {e}")))?;
    atomic_write_file(&credentials_path(home), &bytes, true)
}

pub fn load_local_metadata(home: &Path) -> Result<LocalInstanceMetadata, ClientError> {
    let path = local_metadata_path(home);
    if !path.exists() {
        return Err(ClientError::not_initialized(format!(
            "local instance is not initialized at {} (run `agentcpm-pd instance init`)",
            home.display()
        )));
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| ClientError::config(format!("failed to read {}: {e}", path.display())))?;
    serde_json::from_str(&raw)
        .map_err(|e| ClientError::config(format!("malformed {}: {e}", path.display())))
}

pub fn load_local_token(home: &Path) -> Result<String, ClientError> {
    let path = local_token_path(home);
    if !path.exists() {
        return Err(ClientError::not_initialized(format!(
            "missing local token at {} (run `agentcpm-pd instance init`)",
            path.display()
        )));
    }
    let tok = fs::read_to_string(&path)
        .map_err(|e| ClientError::config(format!("failed to read {}: {e}", path.display())))?
        .trim()
        .to_owned();
    if tok.is_empty() {
        return Err(ClientError::config(format!(
            "local token file {} is empty",
            path.display()
        )));
    }
    Ok(tok)
}

pub fn load_ready_file(home: &Path) -> Result<Option<ReadyFile>, ClientError> {
    let path = ready_path(home);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| ClientError::config(format!("failed to read {}: {e}", path.display())))?;
    let parsed: ReadyFile = serde_json::from_str(&raw)
        .map_err(|e| ClientError::config(format!("malformed {}: {e}", path.display())))?;
    Ok(Some(parsed))
}

/// Checks D7 security rule: `ws://` is restricted to loopback hosts (`127.0.0.1`, `localhost`, `::1`);
/// remote hosts MUST use `wss://`.
pub fn normalize_and_validate_ws_url(raw: &str) -> Result<String, ClientError> {
    let url = Url::parse(raw).map_err(|e| {
        ClientError::Contract(ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("invalid endpoint URL `{raw}`: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ))
    })?;

    match url.scheme() {
        "wss" => {}
        "ws" => {
            let host = url.host_str().unwrap_or("");
            let is_loopback = host == "127.0.0.1"
                || host.eq_ignore_ascii_case("localhost")
                || host == "::1"
                || host == "[::1]";
            if !is_loopback {
                return Err(ClientError::Contract(ContractError::new(
                    CODE_FORBIDDEN,
                    format!(
                        "plain ws:// endpoint `{raw}` is forbidden for non-loopback host `{host}`; use wss:// or an SSH tunnel"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )));
            }
        }
        other => {
            return Err(ClientError::Contract(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("unsupported URL scheme `{other}` in `{raw}`; expected ws:// or wss://"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }
    }

    let mut normalized = url;
    if normalized.path().is_empty() || normalized.path() == "/" {
        normalized.set_path("/v1/ws");
    }
    Ok(normalized.to_string())
}

/// Resolves target strictly per D8:
/// - Named remote target or explicit URL NEVER falls back to local.
/// - Configured `defaultTarget` remote NEVER falls back to local.
/// - Local target NEVER silently falls back to `~/.pilotdeck`.
pub fn resolve_target(
    home: &Path,
    explicit_target: Option<&str>,
) -> Result<ResolvedTarget, ClientError> {
    let cfg = load_client_config(home)?;
    let creds = load_credentials(home)?;

    let selected = match explicit_target {
        Some(t) if !t.trim().is_empty() => t.trim().to_owned(),
        _ => cfg
            .default_target
            .clone()
            .unwrap_or_else(|| "local".to_owned()),
    };

    if selected.starts_with("ws://") || selected.starts_with("wss://") {
        let ws_url = normalize_and_validate_ws_url(&selected)?;
        return Ok(ResolvedTarget::Remote {
            profile_name: selected.clone(),
            ws_url,
            expected_instance_id: None,
            token: creds.tokens.get(&selected).cloned(),
        });
    }

    if let Some(profile) = cfg.targets.get(&selected) {
        match profile.kind {
            TargetKind::Remote => {
                let raw_url = profile.endpoint_url.as_deref().ok_or_else(|| {
                    ClientError::config(format!(
                        "remote target `{selected}` is missing endpointUrl"
                    ))
                })?;
                let ws_url = normalize_and_validate_ws_url(raw_url)?;
                let token_key = profile.credential_ref.as_deref().unwrap_or(&profile.id);
                let token = creds
                    .tokens
                    .get(token_key)
                    .or_else(|| creds.tokens.get(&selected))
                    .cloned();
                return Ok(ResolvedTarget::Remote {
                    profile_name: selected,
                    ws_url,
                    expected_instance_id: profile.expected_instance_id.clone(),
                    token,
                });
            }
            TargetKind::Local => {
                let target_home = match profile.data_root.as_deref() {
                    Some(dr) => expand_tilde_with_env(dr)?,
                    None => home.to_path_buf(),
                };
                let metadata = load_local_metadata(&target_home)?;
                let token = load_local_token(&target_home)?;
                if let Some(expected) = &profile.expected_instance_id
                    && expected != &metadata.instance_id
                {
                    return Err(ClientError::InstanceChanged {
                        expected: expected.clone(),
                        actual: metadata.instance_id,
                    });
                }
                return Ok(ResolvedTarget::Local {
                    profile_name: selected,
                    home: target_home,
                    metadata,
                    token,
                });
            }
        }
    }

    if selected == "local" {
        let metadata = load_local_metadata(home)?;
        let token = load_local_token(home)?;
        return Ok(ResolvedTarget::Local {
            profile_name: "local".to_owned(),
            home: home.to_path_buf(),
            metadata,
            token,
        });
    }

    Err(ClientError::config(format!(
        "target profile `{selected}` not found in {}; remote profiles never fall back to local",
        config_path(home).display()
    )))
}
