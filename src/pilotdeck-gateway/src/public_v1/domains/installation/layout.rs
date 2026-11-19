//! Install-root layout and local installation identity (t021, 009 §1/§3.1).
//!
//! The install root (where the program lives — suggested user-level
//! `~/.local/lib/agentcpm-pd`) is strictly separate from the data root
//! (`AGENTCPM_PD_HOME`, default `~/.agentcpm-pd`): generations never hold
//! user data, and the data root never holds binaries. Resolution order for
//! the install root: explicit per-call override → `AGENTCPM_PD_INSTALL_ROOT`
//! → derived from the running executable (a binary under
//! `<root>/current/bin/` or `<root>/generations/<id>/bin/`) → the default.
//!
//! Identity follows 009 §1: the triple (version, sourceSha, contractDigest)
//! with the derived buildId `<version>+<sourceSha12>.<contractDigest12>`.
//! `sourceSha` is embedded at build time (`AGENTCPM_PD_SOURCE_SHA` in the
//! release build environment, `git rev-parse HEAD`); a build without it is
//! honestly labeled `dev` — it never fabricates a fake commit id.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::release::{ReleaseManifest, read_generation_manifest};

/// Environment override for the install root (tests and managed wrappers).
pub const INSTALL_ROOT_ENV: &str = "AGENTCPM_PD_INSTALL_ROOT";

/// Build-time source sha (40 hex). The release build exports it from
/// `git rev-parse HEAD`; dev builds run without it and report `dev`.
pub const SOURCE_SHA_ENV: &str = "AGENTCPM_PD_SOURCE_SHA";

/// The restart handoff: a successor gateway process waits for this env
/// variable's pid to exit before claiming the instance lock (009 §7).
pub const RESTART_HANDOFF_ENV: &str = "AGENTCPM_PD_GATEWAY_RESTART_HANDOFF";

// ---------------------------------------------------------------------------
// Path helpers (009 §3.1)
// ---------------------------------------------------------------------------

pub fn generations_dir(root: &Path) -> PathBuf {
    root.join("generations")
}

pub fn generation_dir(root: &Path, build_id: &str) -> PathBuf {
    generations_dir(root).join(build_id)
}

pub fn current_link(root: &Path) -> PathBuf {
    root.join("current")
}

pub fn staging_dir(root: &Path) -> PathBuf {
    root.join("staging")
}

pub fn state_dir(root: &Path) -> PathBuf {
    root.join("state")
}

pub fn update_lock_dir(root: &Path) -> PathBuf {
    state_dir(root).join("update.lock")
}

pub fn last_update_path(root: &Path) -> PathBuf {
    state_dir(root).join("last-update.json")
}

/// `state/update.lock` is a *directory* (mkdir semantics, like the baseline's
/// `.git/pilotdeck-update.lock`); this file inside it names the owning
/// operation so `installation.inspect` can report it.
fn update_lock_operation_file(root: &Path) -> PathBuf {
    update_lock_dir(root).join("operation-id")
}

/// The restart marker lives in the DATA root (it must survive the switch and
/// be readable by the successor process, which boots from the install root).
pub fn restart_pending_path(home: &Path) -> PathBuf {
    home.join("installation").join("restart-pending.json")
}

pub fn channel_config_path(home: &Path) -> PathBuf {
    home.join("installation").join("channel.json")
}

// ---------------------------------------------------------------------------
// Install-root resolution
// ---------------------------------------------------------------------------

pub fn default_install_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set; pass --installation-root explicitly".to_owned())?;
    Ok(home.join(".local").join("lib").join("agentcpm-pd"))
}

pub fn expand_tilde(raw: &str) -> Result<PathBuf, String> {
    if raw == "~" {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".to_owned())?;
        return Ok(home);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".to_owned())?;
        return Ok(home.join(rest));
    }
    Ok(PathBuf::from(raw))
}

/// Derives the install root from the running executable: a binary at
/// `<root>/current/bin/<name>` (the OS resolves the `current` symlink before
/// we see the path) or `<root>/generations/<buildId>/bin/<name>`.
pub fn derive_install_root_from_current_exe() -> Option<PathBuf> {
    install_root_of_exe(&std::env::current_exe().ok()?)
}

fn install_root_of_exe(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let holder = bin.parent()?; // `current` or `<buildId>`
    if holder.file_name()? == "current" {
        return holder.parent().map(Path::to_path_buf);
    }
    if holder.parent()?.file_name()? == "generations" {
        return holder.parent()?.parent().map(Path::to_path_buf);
    }
    None
}

/// Resolution with an injected environment (test seam; production callers use
/// [`resolve_install_root`]): explicit → env → exe-derived → default.
pub fn resolve_install_root_with(
    explicit: Option<&str>,
    env_value: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(raw) = explicit.filter(|value| !value.trim().is_empty()) {
        let trimmed = raw.trim();
        // Only absolute or `~/` paths — a relative root would silently
        // depend on the caller's cwd (mirrors the CLI-side local_ops
        // resolution, which rejects the same input).
        if trimmed.starts_with('/') || trimmed.starts_with('~') {
            return expand_tilde(trimmed);
        }
        return Err(format!(
            "installationRoot `{trimmed}` is not an absolute path or ~/ path"
        ));
    }
    if let Some(raw) = env_value.filter(|value| !value.trim().is_empty()) {
        return expand_tilde(raw.trim());
    }
    if let Some(root) = derive_install_root_from_current_exe() {
        return Ok(root);
    }
    default_install_root()
}

pub fn resolve_install_root(explicit: Option<&str>) -> Result<PathBuf, String> {
    let env_value =
        std::env::var_os(INSTALL_ROOT_ENV).map(|value| value.to_string_lossy().into_owned());
    resolve_install_root_with(explicit, env_value.as_deref())
}

// ---------------------------------------------------------------------------
// Local identity (009 §1 / §4.1)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallationMode {
    Source,
    Managed,
    Container,
    DesktopBundled,
}

impl InstallationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Managed => "managed",
            Self::Container => "container",
            Self::DesktopBundled => "desktop-bundled",
        }
    }

    /// The 009 §1 table: only a managed installation supports self-update.
    pub fn update_supported(self) -> bool {
        matches!(self, Self::Managed)
    }

    pub fn reason_code(self) -> Option<&'static str> {
        match self {
            Self::Source => Some("development"),
            Self::Managed => None,
            Self::Container => Some("container"),
            Self::DesktopBundled => Some("desktop"),
        }
    }
}

/// The running build's source sha: `AGENTCPM_PD_SOURCE_SHA` when the release
/// build embedded a 40-hex commit, otherwise the honest `dev` marker.
pub fn build_source_sha() -> &'static str {
    match option_env!("AGENTCPM_PD_SOURCE_SHA") {
        Some(sha)
            if sha.len() == 40
                && sha
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) =>
        {
            sha
        }
        _ => "dev",
    }
}

/// The running build's contract digest prefix (same source as `--version`).
pub fn contract_digest12() -> String {
    agentcpm_pd_protocol::registry::OperationRegistry::global()
        .registry_digest()
        .chars()
        .take(12)
        .collect()
}

#[derive(Clone, Debug)]
pub struct InstallationIdentity {
    pub product_name: String,
    pub version: String,
    pub build_id: String,
    pub platform: String,
    pub arch: String,
    pub installation_mode: InstallationMode,
    /// The current generation's manifest in managed mode (`None` otherwise).
    pub manifest: Option<ReleaseManifest>,
    /// The resolved install root this identity was read from.
    pub install_root: PathBuf,
}

impl InstallationIdentity {
    pub fn update_supported(&self) -> bool {
        self.installation_mode.update_supported()
    }

    pub fn reason_code(&self) -> Option<&'static str> {
        self.installation_mode.reason_code()
    }

    /// The source-build identity triple (version/sourceSha/contractDigest),
    /// mirroring the `--version` output and the handshake build field.
    fn source_identity(install_root: PathBuf) -> Self {
        let version = env!("CARGO_PKG_VERSION").to_owned();
        let source_sha = build_source_sha();
        let contract_digest = contract_digest12();
        let (platform, arch) = super::release::current_platform_pair();
        Self {
            product_name: super::release::PRODUCT_NAME.to_owned(),
            build_id: super::release::build_id(&version, source_sha, &contract_digest),
            version,
            platform: platform.to_owned(),
            arch: arch.to_owned(),
            installation_mode: InstallationMode::Source,
            manifest: None,
            install_root,
        }
    }
}

/// Reads the installation identity at `root` (009 §4.1):
/// a resolvable `current` generation makes the installation `managed`
/// (identity = the generation's manifest); a container filesystem marker
/// makes it `container`; anything else is a source workspace build.
pub fn installation_identity_at(root: &Path) -> InstallationIdentity {
    if let Some((_build_id, manifest)) = current_generation(root) {
        let (platform, arch) = super::release::current_platform_pair();
        return InstallationIdentity {
            product_name: super::release::PRODUCT_NAME.to_owned(),
            version: manifest.version.clone(),
            build_id: manifest.build_id.clone(),
            platform: platform.to_owned(),
            arch: arch.to_owned(),
            installation_mode: InstallationMode::Managed,
            manifest: Some(manifest),
            install_root: root.to_path_buf(),
        };
    }
    if Path::new("/.dockerenv").exists() {
        let mut identity = InstallationIdentity::source_identity(root.to_path_buf());
        identity.installation_mode = InstallationMode::Container;
        return identity;
    }
    InstallationIdentity::source_identity(root.to_path_buf())
}

/// The `current` generation, when the link resolves and its manifest parses.
pub fn current_generation(root: &Path) -> Option<(String, ReleaseManifest)> {
    let current = current_link(root);
    let target = fs::read_link(&current).ok()?;
    let dir = if target.is_absolute() {
        target
    } else {
        current.parent()?.join(&target)
    };
    let canonical = dir.canonicalize().ok()?;
    let build_id = canonical.file_name()?.to_string_lossy().into_owned();
    let manifest = read_generation_manifest(&canonical).ok()?;
    Some((build_id, manifest))
}

/// One retained generation directory with its manifest.
#[derive(Clone, Debug)]
pub struct GenerationEntry {
    pub build_id: String,
    pub manifest: ReleaseManifest,
    pub dir: PathBuf,
}

pub fn list_generations(root: &Path) -> Vec<GenerationEntry> {
    let mut entries = Vec::new();
    let Ok(children) = fs::read_dir(generations_dir(root)) else {
        return entries;
    };
    for child in children.flatten() {
        let dir = child.path();
        if !dir.is_dir() {
            continue;
        }
        let build_id = child.file_name().to_string_lossy().into_owned();
        if let Ok(manifest) = read_generation_manifest(&dir) {
            entries.push(GenerationEntry {
                build_id,
                manifest,
                dir,
            });
        }
    }
    entries
}

// ---------------------------------------------------------------------------
// The update lock (009 §3.3 stage 1; mkdir semantics like the baseline)
// ---------------------------------------------------------------------------

/// The update lock: a directory whose creation is the atomic claim. The
/// guard removes it on drop, so a panic-free failure path always releases;
/// a killed process leaves it behind (diagnose's `installation.generations`
/// check reports the residue; cleanup stays manual by design). The owner
/// file inside names the holding operation for `installation.inspect`.
pub struct UpdateLock {
    dir: PathBuf,
}

impl UpdateLock {
    /// Claims the update lock. `Err(kind == AlreadyExists)` means another
    /// update is in flight (lockBusy).
    pub fn acquire(root: &Path) -> Result<Self, std::io::Error> {
        fs::create_dir_all(state_dir(root))?;
        let dir = update_lock_dir(root);
        fs::create_dir(&dir)?;
        Ok(Self { dir })
    }

    /// Records the owning operation id once the acceptance assigned one.
    pub fn set_owner(&self, operation_id: &str) {
        let _ = fs::write(self.dir.join("operation-id"), operation_id);
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.dir.join("operation-id"));
        let _ = fs::remove_dir(&self.dir);
    }
}

/// The in-flight update's operation id, when the lock is held.
pub fn active_update_operation_id(root: &Path) -> Option<String> {
    if !update_lock_dir(root).is_dir() {
        return None;
    }
    let content = fs::read_to_string(update_lock_operation_file(root)).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

// ---------------------------------------------------------------------------
// Channel configuration (009 §3.2: recorded in the data root)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelConfig {
    /// channel name (`stable`/`preview`) → base URL of the channel service.
    pub channels: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct ChannelConfigFile {
    #[serde(default)]
    channels: BTreeMap<String, String>,
}

impl ChannelConfig {
    pub fn url_for(&self, channel: &str) -> Option<&str> {
        self.channels.get(channel).map(String::as_str)
    }
}

/// Loads the channel configuration from `<home>/installation/channel.json`.
/// A missing file is an empty configuration (not an error); a malformed file
/// is — an operator's broken config must surface, not silently reset.
pub fn load_channel_config(home: &Path) -> Result<ChannelConfig, String> {
    let path = channel_config_path(home);
    if !path.exists() {
        return Ok(ChannelConfig::default());
    }
    let text = fs::read_to_string(&path).map_err(|error| format!("read {path:?}: {error}"))?;
    let file: ChannelConfigFile =
        serde_json::from_str(&text).map_err(|error| format!("parse {path:?}: {error}"))?;
    for (name, url) in &file.channels {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!(
                "channel `{name}` url `{url}` is not an http(s) URL"
            ));
        }
    }
    Ok(ChannelConfig {
        channels: file.channels,
    })
}

/// Writes the channel configuration (0600 directory, 0644 file — it holds no
/// secrets; tests and operators use this to point at a channel).
pub fn save_channel_config(home: &Path, config: &ChannelConfig) -> Result<(), String> {
    let path = channel_config_path(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("mkdir {parent:?}: {error}"))?;
    }
    let file = ChannelConfigFile {
        channels: config.channels.clone(),
    };
    let text =
        serde_json::to_string_pretty(&file).map_err(|error| format!("serialize: {error}"))?;
    fs::write(&path, text).map_err(|error| format!("write {path:?}: {error}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// last-update.json (009 §3.1 / §3.3 stage 7)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LastUpdateRecord {
    pub result: String,
    pub release_id: String,
    pub previous_build_id: Option<String>,
    pub new_build_id: String,
    pub retained_generations: Vec<String>,
    pub finished_at: String,
}

pub fn write_last_update(root: &Path, record: &LastUpdateRecord) -> Result<(), String> {
    let path = last_update_path(root);
    fs::create_dir_all(state_dir(root)).map_err(|error| format!("mkdir state: {error}"))?;
    let text =
        serde_json::to_string_pretty(record).map_err(|error| format!("serialize: {error}"))?;
    fs::write(&path, text).map_err(|error| format!("write {path:?}: {error}"))?;
    Ok(())
}

pub fn read_last_update(root: &Path) -> Option<LastUpdateRecord> {
    let text = fs::read_to_string(last_update_path(root)).ok()?;
    serde_json::from_str(&text).ok()
}

// ---------------------------------------------------------------------------
// The atomic switch + generation retention (009 §3.1/§3.3)
// ---------------------------------------------------------------------------

/// Points `current` at `generations/<buildId>` atomically: a fresh temp
/// symlink renamed over `current` (same-directory rename, POSIX atomic).
pub fn switch_current(root: &Path, build_id: &str) -> Result<(), String> {
    let current = current_link(root);
    let tmp = root.join(format!(".current.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    #[cfg(unix)]
    let link_result = std::os::unix::fs::symlink(format!("generations/{build_id}"), &tmp);
    #[cfg(not(unix))]
    let link_result: Result<(), std::io::Error> = Err(std::io::Error::other(
        "installation switching requires a unix platform",
    ));
    link_result.map_err(|error| format!("stage the current symlink: {error}"))?;
    fs::rename(&tmp, &current).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("switch current: {error}")
    })
}

/// Prunes old generations, keeping `current` plus the newest others so that
/// at most `keep` generations total survive (009 §3.1: 默认保留最近 2 代).
/// Ordering comes from each generation's manifest `buildTime` (deterministic,
/// unlike filesystem mtimes). The current target is never removed. Returns
/// the pruned build ids.
pub fn prune_generations(root: &Path, keep: usize) -> Vec<String> {
    let Some((current_build_id, _)) = current_generation(root) else {
        return Vec::new();
    };
    let mut others: Vec<(chrono::DateTime<chrono::Utc>, String, PathBuf)> = list_generations(root)
        .into_iter()
        .filter(|entry| entry.build_id != current_build_id)
        .map(|entry| {
            let when = chrono::DateTime::parse_from_rfc3339(&entry.manifest.build_time)
                .map(|parsed| parsed.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::DateTime::<chrono::Utc>::MIN_UTC);
            (when, entry.build_id, entry.dir)
        })
        .collect();
    others.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut pruned = Vec::new();
    for (_when, build_id, dir) in others.into_iter().skip(keep.saturating_sub(1)) {
        if fs::remove_dir_all(&dir).is_ok() {
            pruned.push(build_id);
        }
    }
    pruned
}

// ---------------------------------------------------------------------------
// The restart handoff marker (009 §7)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RestartPending {
    /// The `installation.restart` operation waiting for this restart
    /// (`None` for the apply `whenIdle` origin, which owns its completion).
    pub operation_id: Option<String>,
    pub expected_build_id: String,
    /// `installation.restart` or `installation.apply:whenIdle`.
    pub origin: String,
    pub data_root: String,
    pub install_root: String,
    pub previous_pid: u32,
    pub started_at: String,
}

pub fn write_restart_pending(home: &Path, pending: &RestartPending) -> Result<(), String> {
    let path = restart_pending_path(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("mkdir {parent:?}: {error}"))?;
    }
    let text =
        serde_json::to_string_pretty(pending).map_err(|error| format!("serialize: {error}"))?;
    fs::write(&path, text).map_err(|error| format!("write {path:?}: {error}"))?;
    Ok(())
}

pub fn read_restart_pending(home: &Path) -> Option<RestartPending> {
    let text = fs::read_to_string(restart_pending_path(home)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn remove_restart_pending(home: &Path) {
    let _ = fs::remove_file(restart_pending_path(home));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::domains::installation::release::build_id;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "t021-layout-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes one generation whose manifest is internally consistent
    /// (009 §1: buildId is derived from version + sourceSha +
    /// contractDigest, and §2.3: `assets` is never empty) so
    /// `read_generation_manifest` accepts it. Returns `(dir, buildId)`.
    fn write_generation_at(
        root: &Path,
        source_sha: &str,
        tag: &str,
        version: &str,
        build_time: &str,
    ) -> (PathBuf, String) {
        let build_id = build_id(version, source_sha, "bbbbbbbbbbbb");
        let dir = generation_dir(root, &build_id);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("manifest")).unwrap();
        fs::write(
            dir.join("manifest").join("release.json"),
            serde_json::json!({
                "schemaVersion": 2, "product": "agentcpm-pd", "version": version,
                "buildId": build_id, "tag": tag, "date": "2026-11-18",
                "buildTime": build_time,
                "sourceSha": source_sha,
                "contractDigest": "bbbbbbbbbbbb",
                "assets": [{
                    "name": format!("agentcpm-pd-{version}-darwin-arm64.tar.gz"),
                    "size": 1000, "sha256": "c".repeat(64),
                    "sha512": "d".repeat(86) + "==",
                    "platform": "darwin", "arch": "arm64", "kind": "archive",
                }]
            })
            .to_string(),
        )
        .unwrap();
        (dir, build_id)
    }

    fn write_generation(
        root: &Path,
        source_sha: &str,
        tag: &str,
        version: &str,
    ) -> (PathBuf, String) {
        write_generation_at(root, source_sha, tag, version, "2026-11-18T00:00:00Z")
    }

    #[test]
    fn install_root_resolution_prefers_explicit_then_env() {
        let explicit = "/tmp/some-root";
        assert_eq!(
            resolve_install_root_with(Some(explicit), Some("/tmp/env-root")).unwrap(),
            PathBuf::from(explicit)
        );
        assert_eq!(
            resolve_install_root_with(None, Some("/tmp/env-root")).unwrap(),
            PathBuf::from("/tmp/env-root")
        );
        // A relative explicit path is rejected (it would silently depend on cwd).
        assert!(resolve_install_root_with(Some("relative/root"), None).is_err());
    }

    #[test]
    fn exe_derivation_recognizes_both_managed_layouts() {
        let root = PathBuf::from("/opt/agentcpm-pd");
        assert_eq!(
            install_root_of_exe(&root.join("current").join("bin").join("agentcpm-pd")),
            Some(root.clone())
        );
        assert_eq!(
            install_root_of_exe(
                &root
                    .join("generations")
                    .join("0.1.0+aaaa.bbbb")
                    .join("bin")
                    .join("agentcpm-pd")
            ),
            Some(root)
        );
        // A random bin directory is not an installation.
        assert_eq!(
            install_root_of_exe(Path::new("/usr/local/bin/agentcpm-pd")),
            None
        );
        assert_eq!(
            install_root_of_exe(Path::new("/workspace/agentcpm-pd")),
            None
        );
    }

    #[test]
    fn identity_is_source_without_a_current_generation() {
        let root = temp_dir("identity-source");
        let identity = installation_identity_at(&root);
        assert_eq!(identity.installation_mode, InstallationMode::Source);
        assert!(!identity.update_supported());
        assert_eq!(identity.reason_code(), Some("development"));
        assert_eq!(identity.product_name, "agentcpm-pd");
        assert!(
            identity
                .build_id
                .starts_with(&format!("{}+", env!("CARGO_PKG_VERSION")))
        );
    }

    #[test]
    fn identity_is_managed_with_a_current_generation() {
        let root = temp_dir("identity-managed");
        let (_dir, build_id) = write_generation(
            &root,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "v2026.11.18",
            "0.1.1",
        );
        switch_current(&root, &build_id).unwrap();
        let identity = installation_identity_at(&root);
        assert_eq!(identity.installation_mode, InstallationMode::Managed);
        assert!(identity.update_supported());
        assert_eq!(identity.reason_code(), None);
        assert_eq!(identity.version, "0.1.1");
        assert_eq!(identity.build_id, build_id);
    }

    #[test]
    fn update_lock_is_exclusive_and_self_releasing() {
        let root = temp_dir("lock");
        let first = UpdateLock::acquire(&root).unwrap();
        first.set_owner("op-1");
        assert_eq!(active_update_operation_id(&root).as_deref(), Some("op-1"));
        let second = UpdateLock::acquire(&root);
        assert_eq!(
            second.err().map(|error| error.kind()),
            Some(std::io::ErrorKind::AlreadyExists)
        );
        drop(first);
        assert_eq!(active_update_operation_id(&root), None);
        // Re-acquirable after release.
        assert!(UpdateLock::acquire(&root).is_ok());
    }

    #[test]
    fn channel_config_round_trip_and_missing_file_is_empty() {
        let home = temp_dir("channel");
        assert_eq!(
            load_channel_config(&home).unwrap(),
            ChannelConfig::default()
        );
        let mut config = ChannelConfig::default();
        config
            .channels
            .insert("stable".to_owned(), "http://127.0.0.1:1/".to_owned());
        save_channel_config(&home, &config).unwrap();
        let loaded = load_channel_config(&home).unwrap();
        assert_eq!(loaded.url_for("stable"), Some("http://127.0.0.1:1/"));
        assert_eq!(loaded.url_for("preview"), None);
        // A malformed file surfaces as an error, never as an empty config.
        fs::write(channel_config_path(&home), "{not json").unwrap();
        assert!(load_channel_config(&home).is_err());
    }

    #[test]
    fn switch_and_prune_keep_current_and_recent() {
        let root = temp_dir("prune");
        // Each generation carries its own sourceSha so the buildIds differ;
        // the fixture derives each buildId from its own fields (009 §1).
        let (_old_dir, old) = write_generation_at(
            &root,
            &"1".repeat(40),
            "v2026.10.01",
            "0.1.0",
            "2026-10-01T00:00:00Z",
        );
        let (_mid_dir, mid) = write_generation_at(
            &root,
            &"2".repeat(40),
            "v2026.11.01",
            "0.1.1",
            "2026-11-01T00:00:00Z",
        );
        let (_new_dir, new) = write_generation_at(
            &root,
            &"3".repeat(40),
            "v2026.11.18",
            "0.1.2",
            "2026-11-18T00:00:00Z",
        );
        switch_current(&root, &new).unwrap();

        // Three generations, keep 2: the oldest is pruned.
        let pruned = prune_generations(&root, 2);
        assert_eq!(pruned, vec![old]);
        assert!(generation_dir(&root, &mid).is_dir());

        // current always survives; a 4th generation pushes mid out too.
        let (_four_dir, four) = write_generation_at(
            &root,
            &"4".repeat(40),
            "v2026.11.19",
            "0.1.3",
            "2026-11-19T00:00:00Z",
        );
        switch_current(&root, &four).unwrap();
        let pruned = prune_generations(&root, 2);
        assert_eq!(pruned, vec![mid]);
        assert!(generation_dir(&root, &four).is_dir());
        assert!(generation_dir(&root, &new).is_dir());
    }

    #[test]
    fn last_update_and_restart_pending_round_trip() {
        let root = temp_dir("records");
        let record = LastUpdateRecord {
            result: "succeeded".to_owned(),
            release_id: "v2026.11.18".to_owned(),
            previous_build_id: Some("0.1.1+aaaaaaaaaaaa.bbbbbbbbbbbb".to_owned()),
            new_build_id: "0.1.2+newnewnewnew.bbbbbbbbbbbb".to_owned(),
            retained_generations: vec!["0.1.1+aaaaaaaaaaaa.bbbbbbbbbbbb".to_owned()],
            finished_at: "2026-11-18T00:00:00Z".to_owned(),
        };
        write_last_update(&root, &record).unwrap();
        assert_eq!(read_last_update(&root).as_ref(), Some(&record));

        let pending = RestartPending {
            operation_id: Some("op-restart-1".to_owned()),
            expected_build_id: "0.1.2+newnewnewnew.bbbbbbbbbbbb".to_owned(),
            origin: "installation.restart".to_owned(),
            data_root: root.display().to_string(),
            install_root: root.display().to_string(),
            previous_pid: 42,
            started_at: "2026-11-18T00:00:00Z".to_owned(),
        };
        write_restart_pending(&root, &pending).unwrap();
        assert_eq!(read_restart_pending(&root).as_ref(), Some(&pending));
        remove_restart_pending(&root);
        assert!(read_restart_pending(&root).is_none());
    }
}
