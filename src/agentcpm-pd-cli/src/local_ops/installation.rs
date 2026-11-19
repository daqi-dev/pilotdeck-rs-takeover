//! `installation.inspect` — the CLI-local installation identity report
//! (t021, 009 §4.1).
//!
//! Local by contract (`execution=local`, caller `local_os_owner`): the
//! report describes the *local installation* only and never mixes in the
//! remote service version (the handshake's `target.gatewayVersion` is the
//! other, separate projection). It never reads or writes `~/.pilotdeck`,
//! and an existing `pilotdeck` on PATH changes nothing in the report (the
//! compat shim's install refusal is the enforcement point there, not
//! inspection).
//!
//! The install root resolves: explicit `--installation-root` →
//! `AGENTCPM_PD_INSTALL_ROOT` → derived from the running executable (a
//! binary under `<root>/current/bin/` or `<root>/generations/<id>/bin/`) →
//! the default `~/.local/lib/agentcpm-pd`. The identity triple is the same
//! one `--version` prints (version + sourceSha + contract digest), and in
//! managed mode it is the current generation's manifest that speaks — the
//! installation, not the workspace, is what update flows switch.
//!
//! The gateway domain (`domains/installation.rs`) owns the heavier
//! install-root machinery; this handler deliberately reuses no gateway
//! types so it works in builds without `embedded-gateway`.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use agentcpm_pd_client::error::ClientError;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};

use crate::commands::{local_request_id, local_response, write_envelope_or_fail, write_error};
use crate::local_ops::LocalOpRegistry;
use crate::{GlobalOptions, OutputFormat};

/// Environment override for the install root (shared vocabulary with the
/// gateway domain's `installation.*` operations).
pub const INSTALL_ROOT_ENV: &str = "AGENTCPM_PD_INSTALL_ROOT";

/// Build-time source sha (40 hex) — the release build exports
/// `AGENTCPM_PD_SOURCE_SHA` from `git rev-parse HEAD`; a build without it
/// is honestly labeled `dev`.
pub const SOURCE_SHA_ENV: &str = "AGENTCPM_PD_SOURCE_SHA";

pub(super) fn register(reg: &mut LocalOpRegistry) {
    reg.op("installation.inspect", |globals, input, format, out| {
        Box::pin(inspect_operation(globals, input, format, out))
    });
}

async fn inspect_operation(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    match inspect(input.get("installationRoot").and_then(Value::as_str)) {
        Ok(result) => {
            write_envelope_or_fail(out, format, &local_response(local_request_id(), result))
        }
        Err(err) => write_error(out, format, &err),
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// The 009 §1 mode vocabulary. Only a managed installation (a resolvable
/// `current` generation) supports self-update; a source workspace build is
/// `development`, a container `container`. `desktop-bundled` is reserved
/// for the desktop host (D15) and is not produced by CLI-only builds.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InstallationMode {
    Source,
    Managed,
    Container,
}

impl InstallationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Managed => "managed",
            Self::Container => "container",
        }
    }

    pub fn update_supported(self) -> bool {
        matches!(self, Self::Managed)
    }

    pub fn reason_code(self) -> Option<&'static str> {
        match self {
            Self::Source => Some("development"),
            Self::Managed => None,
            Self::Container => Some("container"),
        }
    }
}

/// The running build's source sha (see [`SOURCE_SHA_ENV`]).
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

/// The running build's contract digest prefix — the same twelve characters
/// `--version` prints.
pub fn contract_digest12() -> String {
    agentcpm_pd_protocol::registry::OperationRegistry::global()
        .registry_digest()
        .chars()
        .take(12)
        .collect()
}

/// `<version>+<sourceSha12>.<contractDigest12>` (009 §1).
pub fn build_id(version: &str, source_sha: &str, contract_digest: &str) -> String {
    let short = |value: &str| -> String { value.chars().take(12).collect() };
    format!("{version}+{}.{}", short(source_sha), short(contract_digest))
}

// ---------------------------------------------------------------------------
// Install-root resolution (mirrors the gateway domain's layout module; the
// CLI crate cannot depend on gateway types without `embedded-gateway`)
// ---------------------------------------------------------------------------

pub fn default_install_root() -> Result<PathBuf, ContractError> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| unavailable("HOME is not set; pass --installation-root explicitly"))?;
    Ok(home.join(".local").join("lib").join("agentcpm-pd"))
}

fn unavailable(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn expand_tilde(raw: &str) -> Result<PathBuf, ContractError> {
    agentcpm_pd_client::config::expand_tilde_with_env(raw)
        .map_err(|error| unavailable(error.to_string()))
}

/// Derives the install root from the running executable (a binary under
/// `<root>/current/bin/` or `<root>/generations/<id>/bin/`).
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

/// Resolution with an injected environment (test seam).
pub fn resolve_install_root_with(
    explicit: Option<&str>,
    env_value: Option<&str>,
) -> Result<PathBuf, ContractError> {
    if let Some(raw) = explicit.filter(|value| !value.trim().is_empty()) {
        let trimmed = raw.trim();
        if !trimmed.starts_with('/') && !trimmed.starts_with('~') {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("installationRoot `{trimmed}` is not an absolute path or ~/ path"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        return expand_tilde(trimmed);
    }
    if let Some(raw) = env_value.filter(|value| !value.trim().is_empty()) {
        return expand_tilde(raw.trim());
    }
    if let Some(root) = derive_install_root_from_current_exe() {
        return Ok(root);
    }
    default_install_root()
}

pub fn resolve_install_root(explicit: Option<&str>) -> Result<PathBuf, ContractError> {
    let env_value =
        std::env::var_os(INSTALL_ROOT_ENV).map(|value| value.to_string_lossy().into_owned());
    resolve_install_root_with(explicit, env_value.as_deref())
}

// ---------------------------------------------------------------------------
// The projection (009 §4.1, field by field)
// ---------------------------------------------------------------------------

/// The minimal fields this projection needs from a generation's manifest.
struct GenerationIdentity {
    build_id: String,
    version: String,
}

/// Reads `<root>/current` → the generation's manifest identity. `None` when
/// the link or the manifest is absent/broken (a non-managed installation).
fn current_generation(root: &Path) -> Option<GenerationIdentity> {
    let current = root.join("current");
    let target = std::fs::read_link(&current).ok()?;
    let dir = if target.is_absolute() {
        target
    } else {
        current.parent()?.join(&target)
    };
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("manifest").join("release.json")).ok()?,
    )
    .ok()?;
    Some(GenerationIdentity {
        build_id: manifest.get("buildId")?.as_str()?.to_owned(),
        version: manifest.get("version")?.as_str()?.to_owned().to_owned(),
    })
}

/// The in-flight update's operation id, when the mkdir lock is held (the
/// lock directory holds an `operation-id` file written by the apply
/// acceptance).
fn active_update_operation_id(root: &Path) -> Option<String> {
    let lock = root.join("state").join("update.lock");
    if !lock.is_dir() {
        return None;
    }
    let content = std::fs::read_to_string(lock.join("operation-id")).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn platform_string() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("{os}/{arch}")
}

/// Builds the contract result for `installation.inspect`.
pub fn inspect(explicit_root: Option<&str>) -> Result<Value, ClientError> {
    let root = resolve_install_root(explicit_root).map_err(ClientError::Contract)?;

    // Managed: the current generation's manifest is the identity.
    if let Some(generation) = current_generation(&root) {
        return Ok(json!({
            "productName": "agentcpm-pd",
            "version": generation.version,
            "buildId": generation.build_id,
            "platform": platform_string(),
            "installationMode": InstallationMode::Managed.as_str(),
            "updateSupported": true,
            "reasonCode": Value::Null,
            "activeUpdateOperationId": active_update_operation_id(&root),
        }));
    }

    // Container marker, then the source-workspace default.
    let mode = if Path::new("/.dockerenv").exists() {
        InstallationMode::Container
    } else {
        InstallationMode::Source
    };
    let version = env!("CARGO_PKG_VERSION").to_owned();
    let source_sha = build_source_sha();
    let digest = contract_digest12();
    Ok(json!({
        "productName": "agentcpm-pd",
        "version": version,
        "buildId": build_id(&version, source_sha, &digest),
        "platform": platform_string(),
        "installationMode": mode.as_str(),
        "updateSupported": mode.update_supported(),
        "reasonCode": mode.reason_code(),
        "activeUpdateOperationId": active_update_operation_id(&root),
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "t021-cli-inspect-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_generation(root: &Path, build_id: &str, version: &str, tag: &str) {
        let dir = root.join("generations").join(build_id);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("manifest")).unwrap();
        std::fs::write(
            dir.join("manifest").join("release.json"),
            json!({
                "schemaVersion": 2, "product": "agentcpm-pd", "version": version,
                "buildId": build_id, "tag": tag
            })
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(format!("generations/{build_id}"), root.join("current"))
            .unwrap();
    }

    #[test]
    fn build_id_follows_the_canonical_form() {
        assert_eq!(
            build_id(
                "0.1.0",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "bbbbbbbbbbbb"
            ),
            "0.1.0+aaaaaaaaaaaa.bbbbbbbbbbbb"
        );
    }

    #[test]
    fn resolution_rejects_relative_explicit_paths() {
        let error = resolve_install_root_with(Some("relative/root"), None).unwrap_err();
        assert_eq!(error.code, "INVALID_ARGUMENT");
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
        assert_eq!(
            install_root_of_exe(Path::new("/usr/local/bin/agentcpm-pd")),
            None
        );
    }

    #[test]
    fn source_identity_reports_all_fields_honestly() {
        let root = temp_dir("source");
        let result = inspect(Some(root.to_str().unwrap())).unwrap();
        assert_eq!(result["productName"], "agentcpm-pd");
        assert_eq!(result["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(result["installationMode"], "source");
        assert_eq!(result["updateSupported"], false);
        assert_eq!(result["reasonCode"], "development");
        assert_eq!(result["activeUpdateOperationId"], Value::Null);
        // The buildId carries the same digest family `--version` prints.
        assert!(
            result["buildId"]
                .as_str()
                .unwrap()
                .ends_with(&format!(".{}", contract_digest12()))
        );
        let platform = result["platform"].as_str().unwrap();
        assert!(
            platform.contains('/'),
            "platform is <os>/<arch>: {platform}"
        );
    }

    #[test]
    fn managed_identity_comes_from_the_current_generation() {
        let root = temp_dir("managed");
        write_generation(
            &root,
            "9.9.9+eeeeeeeeeeee.ffffffffffff",
            "9.9.9",
            "v2026.11.18",
        );
        let result = inspect(Some(root.to_str().unwrap())).unwrap();
        assert_eq!(result["installationMode"], "managed");
        assert_eq!(result["version"], "9.9.9");
        assert_eq!(result["buildId"], "9.9.9+eeeeeeeeeeee.ffffffffffff");
        assert_eq!(result["updateSupported"], true);
        assert_eq!(result["reasonCode"], Value::Null);

        // A held update lock surfaces as the active operation id.
        let lock = root.join("state").join("update.lock");
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(lock.join("operation-id"), "op-live-1").unwrap();
        let result = inspect(Some(root.to_str().unwrap())).unwrap();
        assert_eq!(result["activeUpdateOperationId"], "op-live-1");
    }

    #[test]
    fn broken_current_link_falls_back_to_the_source_identity() {
        let root = temp_dir("broken");
        // A dangling current link is not a managed installation.
        #[cfg(unix)]
        std::os::unix::fs::symlink("generations/nonexistent", root.join("current")).unwrap();
        let result = inspect(Some(root.to_str().unwrap())).unwrap();
        assert_eq!(result["installationMode"], "source");
    }
}
