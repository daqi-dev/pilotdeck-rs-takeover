//! The release-package model and verification rules (t021, 009 §2).
//!
//! The *release artifact* of this product is defined as "the products built
//! from this source tree, plus their manifest and verification rules"
//! (AGENTS.md red line: the build never downloads precompiled binaries). This
//! module owns the consumer side of that definition:
//!
//! * `release.json` (schemaVersion 2) — parse + full consumer validation.
//!   The field set and every per-field rule follow the 009 design, which in
//!   turn carries the TS baseline's consumer checks (`releaseService.js`
//!   tag regex / sha checks, `create-release-manifest.mjs` platform/arch
//!   normalization). New agentcpm-pd identity fields (`product`, `buildId`,
//!   `sourceSha`, `contractDigest`) are validated for internal consistency:
//!   `buildId` must equal `<version>+<sourceSha12>.<contractDigest12>`.
//! * `SHA256SUMS.txt` — GNU `sha256sum` line format (`<hex>  <path>`),
//!   covering `bin/*` and `manifest/*` but never itself.
//! * the `tar.gz` package — a strict read-only ustar extractor (regular
//!   files and directories only; symlinks, hardlinks, and pax/GNU
//!   extensions are integrity failures, never silently honored) plus the
//!   matching ustar writer the packaging tooling and the test suites use.
//!
//! Nothing here touches the network, the store, or the install root; callers
//! map the `Err(String)` reasons onto contract error codes.

use std::path::PathBuf;

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};

/// The product name every manifest must carry (007: the new product line).
pub const PRODUCT_NAME: &str = "agentcpm-pd";

/// The release manifest schema this build consumes. 2, not the baseline's
/// 1: the field set differs and the old updater only reads `=== 1`
/// (009 §2.3, open question §10.2).
pub const MANIFEST_SCHEMA_VERSION: u64 = 2;

/// Baseline release tag shape (`releaseService.js:4`).
pub fn release_tag_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^v\d{4}\.\d{2}\.\d{2}(?:-r[1-9]\d*)?$").expect("valid release tag regex")
    })
}

fn hex_lower_64_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| Regex::new("^[0-9a-f]{64}$").expect("sha256 pattern"))
}

fn sha512_base64_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| Regex::new("^[A-Za-z0-9+/]{86}==$").expect("sha512 pattern"))
}

/// `^[0-9a-f]{40}$` for `sourceSha`.
fn source_sha_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| Regex::new("^[0-9a-f]{40}$").expect("source sha pattern"))
}

/// `^[0-9a-f]{12}$` for the digests.
fn digest12_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| Regex::new("^[0-9a-f]{12}$").expect("digest12 pattern"))
}

/// Package-internal path safety: no empty, absolute, or traversal names.
fn safe_relative_path(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && name
            .split('/')
            .all(|component| !component.is_empty() || name.ends_with('/'))
        && name
            .split('/')
            .all(|component| component != "." && component != "..")
}

// ---------------------------------------------------------------------------
// buildId (009 §1): <version>+<sourceSha12>.<contractDigest12>
// ---------------------------------------------------------------------------

/// The canonical build identity: `<version>+<sourceSha12>.<contractDigest12>`
/// (009 §1). `sourceSha`/`contractDigest` are truncated to their first 12 hex
/// characters, mirroring the `--version` output's contract digest.
pub fn build_id(version: &str, source_sha: &str, contract_digest: &str) -> String {
    let short = |value: &str| -> String { value.chars().take(12).collect() };
    format!("{version}+{}.{}", short(source_sha), short(contract_digest))
}

// ---------------------------------------------------------------------------
// Release manifest
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseAsset {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub sha512: String,
    pub platform: String,
    pub arch: String,
    #[serde(default = "default_asset_kind")]
    pub kind: String,
}

fn default_asset_kind() -> String {
    "archive".to_owned()
}

impl ReleaseAsset {
    pub fn matches(&self, platform: &str, arch: &str) -> bool {
        self.platform == platform && self.arch == arch
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseManifest {
    pub schema_version: u64,
    pub product: String,
    pub version: String,
    pub build_id: String,
    pub tag: String,
    pub date: String,
    pub build_time: String,
    pub source_sha: String,
    pub contract_digest: String,
    #[serde(default)]
    pub compat_manifest_digest: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

/// The current machine's `(platform, arch)` in manifest vocabulary: the
/// baseline's normalization (`create-release-manifest.mjs:66-67`,
/// aarch64→arm64 / x86_64→x64) applied to `std::env::consts`.
pub fn current_platform_pair() -> (&'static str, &'static str) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    (os, arch)
}

/// Parses and fully validates one channel `release.json` document. Every
/// rejection carries a reason precise enough for an operator to act on.
pub fn parse_release_manifest(bytes: &[u8]) -> Result<ReleaseManifest, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("release.json is not valid JSON: {error}"))?;
    let manifest: ReleaseManifest = serde_json::from_value(value.clone())
        .map_err(|error| format!("release.json does not match the manifest shape: {error}"))?;

    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(format!(
            "manifest schemaVersion {} is not supported (expected {MANIFEST_SCHEMA_VERSION})",
            manifest.schema_version
        ));
    }
    if manifest.product != PRODUCT_NAME {
        return Err(format!(
            "manifest product `{}` is not {PRODUCT_NAME}",
            manifest.product
        ));
    }
    if manifest.version.is_empty() || manifest.version.contains(char::is_whitespace) {
        return Err("manifest version must be a non-empty version string".to_owned());
    }
    if !source_sha_pattern().is_match(&manifest.source_sha) {
        return Err("manifest sourceSha must be 40 lowercase hex characters".to_owned());
    }
    if !digest12_pattern().is_match(&manifest.contract_digest) {
        return Err("manifest contractDigest must be 12 lowercase hex characters".to_owned());
    }
    if let Some(compat) = &manifest.compat_manifest_digest
        && !digest12_pattern().is_match(compat)
    {
        return Err("manifest compatManifestDigest must be 12 lowercase hex characters".to_owned());
    }
    let derived = build_id(
        &manifest.version,
        &manifest.source_sha,
        &manifest.contract_digest,
    );
    if manifest.build_id != derived {
        return Err(format!(
            "manifest buildId `{}` does not match its own fields (expected `{derived}`)",
            manifest.build_id
        ));
    }
    if !release_tag_pattern().is_match(&manifest.tag) {
        return Err(format!(
            "manifest tag `{}` is not a release tag",
            manifest.tag
        ));
    }
    if !Regex::new(r"^\d{4}-\d{2}-\d{2}$")
        .expect("date pattern")
        .is_match(&manifest.date)
    {
        return Err(format!(
            "manifest date `{}` is not YYYY-MM-DD",
            manifest.date
        ));
    }
    if chrono::DateTime::parse_from_rfc3339(&manifest.build_time).is_err() {
        return Err(format!(
            "manifest buildTime `{}` is not an RFC 3339 timestamp",
            manifest.build_time
        ));
    }
    if manifest.assets.is_empty() {
        return Err("manifest assets must not be empty".to_owned());
    }
    let mut seen_platforms = std::collections::BTreeSet::new();
    for asset in &manifest.assets {
        if !safe_relative_path(&asset.name) || asset.name.contains('/') {
            return Err(format!(
                "asset name `{}` is not a plain file name",
                asset.name
            ));
        }
        if !hex_lower_64_pattern().is_match(&asset.sha256) {
            return Err(format!(
                "asset `{}` sha256 must be 64 lowercase hex characters",
                asset.name
            ));
        }
        if !sha512_base64_pattern().is_match(&asset.sha512) {
            return Err(format!(
                "asset `{}` sha512 must be base64 (86 chars + '==')",
                asset.name
            ));
        }
        if asset.size == 0 {
            return Err(format!("asset `{}` size must be positive", asset.name));
        }
        if asset.kind != "archive" {
            return Err(format!(
                "asset `{}` kind `{}` is not supported (expected `archive`)",
                asset.name, asset.kind
            ));
        }
        if !matches!(asset.platform.as_str(), "darwin" | "linux") {
            return Err(format!(
                "asset `{}` platform `{}` is not darwin/linux",
                asset.name, asset.platform
            ));
        }
        if !matches!(asset.arch.as_str(), "arm64" | "x64") {
            return Err(format!(
                "asset `{}` arch `{}` is not arm64/x64",
                asset.name, asset.arch
            ));
        }
        if !seen_platforms.insert((asset.platform.clone(), asset.arch.clone())) {
            return Err(format!(
                "manifest lists more than one asset for {}/{}",
                asset.platform, asset.arch
            ));
        }
    }
    Ok(manifest)
}

/// Serializes a manifest back to bytes (the packaging tooling writes
/// `manifest/release.json` with exactly this shape).
pub fn serialize_release_manifest(manifest: &ReleaseManifest) -> Result<Vec<u8>, String> {
    let value =
        serde_json::to_value(manifest).map_err(|error| format!("serialize manifest: {error}"))?;
    serde_json::to_vec_pretty(&value).map_err(|error| format!("render manifest: {error}"))
}

// ---------------------------------------------------------------------------
// SHA256SUMS.txt
// ---------------------------------------------------------------------------

/// Parses a `SHA256SUMS.txt` document: one `<sha256>  <relative path>` per
/// line (GNU two-space format, `create-release-manifest.mjs:38-40`).
pub fn parse_sha256sums(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches(['\r']);
        if line.is_empty() {
            continue;
        }
        let (digest, path) = line
            .split_once("  ")
            .ok_or_else(|| format!("SHA256SUMS line is not `<sha256>  <path>`: `{line}`"))?;
        if !hex_lower_64_pattern().is_match(digest) {
            return Err(format!(
                "SHA256SUMS digest `{digest}` is not 64 lowercase hex"
            ));
        }
        if !safe_relative_path(path) {
            return Err(format!(
                "SHA256SUMS path `{path}` is not a safe relative path"
            ));
        }
        entries.push((digest.to_owned(), path.to_owned()));
    }
    if entries.is_empty() {
        return Err("SHA256SUMS.txt is empty".to_owned());
    }
    Ok(entries)
}

/// Renders a `SHA256SUMS.txt` document from `(relative path, absolute file)`
/// pairs, hashing the files on the spot.
pub fn render_sha256sums(files: &[(String, PathBuf)]) -> Result<String, String> {
    let mut lines = Vec::new();
    for (relative, path) in files {
        if !safe_relative_path(relative) || relative == "SHA256SUMS.txt" {
            return Err(format!("refusing to sum unsafe path `{relative}`"));
        }
        let digest = file_sha256(path)?;
        lines.push(format!("{digest}  {relative}"));
    }
    lines.sort();
    Ok(lines.join("\n") + "\n")
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ---------------------------------------------------------------------------
// ustar (read-only extractor + writer for the tooling)
// ---------------------------------------------------------------------------

/// A file staged into a release package: its package-relative path, its
/// source path on this machine, and the permission bits it carries in the
/// archive (0755 for executables, 0644 for data).
pub struct PackageFile {
    pub relative_path: String,
    pub source: PathBuf,
    pub mode: u32,
}

const TAR_BLOCK: usize = 512;
const MAX_TAR_ENTRIES: usize = 2048;

/// Writes a `.tar.gz` release package from the staged files. Only regular
/// files are emitted (leading directories are implied by the names); names
/// longer than the ustar prefix/name split allow are rejected.
pub fn write_release_package(archive_path: &Path, files: &[PackageFile]) -> Result<(), String> {
    let parent = archive_path
        .parent()
        .ok_or_else(|| format!("archive path {} has no parent", archive_path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
    let file = fs::File::create(archive_path)
        .map_err(|error| format!("create {}: {error}", archive_path.display()))?;
    let mut tar = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for entry in files {
        write_ustar_entry(&mut tar, entry, now)?;
    }
    // Two zero blocks terminate the archive (POSIX ustar).
    tar.write_all(&[0u8; TAR_BLOCK])
        .and_then(|_| tar.write_all(&[0u8; TAR_BLOCK]))
        .map_err(|error| format!("write tar terminator: {error}"))?;
    tar.finish()
        .map_err(|error| format!("finish gzip stream: {error}"))?;
    Ok(())
}

fn write_ustar_entry<W: Write>(out: &mut W, entry: &PackageFile, now: u64) -> Result<(), String> {
    let (prefix, name) = split_ustar_name(&entry.relative_path)?;
    let metadata = fs::metadata(&entry.source)
        .map_err(|error| format!("stat {}: {error}", entry.source.display()))?;
    let size = metadata.len();
    let mut header = [0u8; TAR_BLOCK];
    write_ustar_field(&mut header, 0, &name)?;
    write_ustar_octal(&mut header, 100, 8, u64::from(entry.mode & 0o7777))?;
    write_ustar_octal(&mut header, 108, 8, 0)?; // uid
    write_ustar_octal(&mut header, 116, 8, 0)?; // gid
    write_ustar_octal(&mut header, 124, 12, size)?;
    write_ustar_octal(&mut header, 136, 12, now)?;
    header[148..156].fill(b' ');
    header[156] = b'0'; // regular file
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    write_ustar_field(&mut header, 265, "root")?; // uname
    write_ustar_field(&mut header, 297, "root")?; // gname
    if !prefix.is_empty() {
        write_ustar_field(&mut header, 345, &prefix)?;
    }
    let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    let checksum_field = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(checksum_field.as_bytes());

    out.write_all(&header)
        .map_err(|error| format!("write tar header: {error}"))?;
    let mut source = fs::File::open(&entry.source)
        .map_err(|error| format!("open {}: {error}", entry.source.display()))?;
    std::io::copy(&mut source, out).map_err(|error| format!("copy into tar: {error}"))?;
    let padding = (TAR_BLOCK - (size as usize % TAR_BLOCK)) % TAR_BLOCK;
    if padding > 0 {
        out.write_all(&vec![0u8; padding])
            .map_err(|error| format!("write tar padding: {error}"))?;
    }
    Ok(())
}

/// Splits a package-relative path into ustar `(prefix, name)` fields.
fn split_ustar_name(path: &str) -> Result<(String, String), String> {
    if !safe_relative_path(path) || path.ends_with('/') {
        return Err(format!(
            "package path `{path}` is not a safe relative file path"
        ));
    }
    if path.len() <= 100 {
        return Ok((String::new(), path.to_owned()));
    }
    // Find a '/' split so name ≤ 100 and prefix ≤ 155 bytes.
    let bytes = path.as_bytes();
    for split in (0..bytes.len()).rev() {
        if bytes[split] != b'/' {
            continue;
        }
        let (candidate_prefix, candidate_name) = path.split_at(split);
        let candidate_name = &candidate_name[1..];
        if candidate_prefix.len() <= 155
            && candidate_name.len() <= 100
            && !candidate_name.is_empty()
        {
            return Ok((candidate_prefix.to_owned(), candidate_name.to_owned()));
        }
    }
    Err(format!(
        "package path `{path}` does not fit the ustar name fields"
    ))
}

fn write_ustar_field(
    header: &mut [u8; TAR_BLOCK],
    offset: usize,
    value: &str,
) -> Result<(), String> {
    let bytes = value.as_bytes();
    if offset + bytes.len() + 1 > TAR_BLOCK {
        return Err(format!("tar field `{value}` overflows its slot"));
    }
    header[offset..offset + bytes.len()].copy_from_slice(bytes);
    header[offset + bytes.len()] = 0;
    Ok(())
}

fn write_ustar_octal(
    header: &mut [u8; TAR_BLOCK],
    offset: usize,
    width: usize,
    value: u64,
) -> Result<(), String> {
    // `<width-1>` octal digits + NUL, the classic fixed-width tar encoding.
    let text = format!("{:0width$o}", value, width = width - 1);
    let bytes = text.as_bytes();
    if bytes.len() != width - 1 {
        return Err(format!("value {value} overflows its tar field"));
    }
    header[offset..offset + width - 1].copy_from_slice(bytes);
    header[offset + width - 1] = 0;
    Ok(())
}

/// Extracts a `.tar.gz` package into `dest_dir`, returning the extracted
/// regular files as `(relative path, size)`. The extractor is deliberately
/// strict: it accepts only ustar regular files and directories with safe
/// relative names, applies the archived permission bits, and rejects
/// symlinks / hardlinks / pax headers — an untrusted package cannot smuggle
/// a traversal path or a link out of the destination.
pub fn unpack_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<Vec<(String, u64)>, String> {
    let file = fs::File::open(archive_path)
        .map_err(|error| format!("open archive {}: {error}", archive_path.display()))?;
    let mut tar = flate2::read::GzDecoder::new(file);
    let mut extracted = Vec::new();
    let mut header = [0u8; TAR_BLOCK];
    loop {
        tar.read_exact(&mut header)
            .map_err(|error| format!("read tar header: {error}"))?;
        if header.iter().all(|byte| *byte == 0) {
            break; // terminator block
        }
        let name = cstr_field(&header[0..100]);
        let mode = parse_octal(&header[100..108])?;
        let size = parse_octal(&header[124..136])?;
        let stored_checksum = parse_octal(&header[148..156])?;
        let typeflag = header[156];
        let prefix = cstr_field(&header[345..500]);
        if header[257..262] != *b"ustar" {
            return Err(format!(
                "archive entry `{name}` is not ustar (magic {:?})",
                String::from_utf8_lossy(&header[257..263])
            ));
        }
        let computed: u32 = header
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                if (148..156).contains(&index) {
                    u32::from(b' ')
                } else {
                    u32::from(*byte)
                }
            })
            .sum();
        if u64::from(computed) != stored_checksum {
            return Err(format!("archive entry `{name}` has a bad header checksum"));
        }
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if !safe_relative_path(&relative) {
            return Err(format!(
                "archive entry `{relative}` is not a safe relative path"
            ));
        }
        let target = dest_dir.join(&relative);
        match typeflag {
            b'0' | 0 => {
                let mut payload = vec![0u8; size as usize];
                tar.read_exact(&mut payload)
                    .map_err(|error| format!("read body of `{relative}`: {error}"))?;
                skip_padding(&mut tar, size)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
                }
                let mut out = fs::File::create(&target)
                    .map_err(|error| format!("create {}: {error}", target.display()))?;
                out.write_all(&payload)
                    .map_err(|error| format!("write {}: {error}", target.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(
                        &target,
                        fs::Permissions::from_mode((mode & 0o777) as u32),
                    );
                }
                extracted.push((relative, size));
            }
            b'5' => {
                fs::create_dir_all(&target)
                    .map_err(|error| format!("mkdir {}: {error}", target.display()))?;
            }
            other => {
                return Err(format!(
                    "archive entry `{relative}` has unsupported type flag {other:#x} \
                     (only regular files and directories are accepted)"
                ));
            }
        }
        if extracted.len() >= MAX_TAR_ENTRIES {
            return Err(format!("archive exceeds {MAX_TAR_ENTRIES} entries"));
        }
    }
    if extracted.is_empty() {
        return Err("archive contains no files".to_owned());
    }
    Ok(extracted)
}

fn skip_padding<R: Read>(reader: &mut R, size: u64) -> Result<(), String> {
    let padding = (TAR_BLOCK - (size as usize % TAR_BLOCK)) % TAR_BLOCK;
    if padding > 0 {
        let mut sink = vec![0u8; padding];
        reader
            .read_exact(&mut sink)
            .map_err(|error| format!("read tar padding: {error}"))?;
    }
    Ok(())
}

fn cstr_field(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn parse_octal(bytes: &[u8]) -> Result<u64, String> {
    let text: String = cstr_field(bytes)
        .trim_matches([' ', '\0'])
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(&text, 8).map_err(|_| format!("tar field `{text}` is not octal"))
}

// ---------------------------------------------------------------------------
// Generation verification (apply stage 4; diagnose installation.manifest)
// ---------------------------------------------------------------------------

/// Verifies one unpacked generation directory against its expected manifest
/// (009 §3.3 stage 4 / §6.1):
///
/// * `bin/agentcpm-pd` exists, is a regular file, and is executable;
/// * `manifest/release.json` parses and matches the expected identity
///   (buildId, tag, version, sourceSha, contractDigest);
/// * `SHA256SUMS.txt` covers `bin/agentcpm-pd` and `manifest/release.json`
///   (plus `bin/pilotdeck` when present), every listed digest matches the
///   file on disk, and no regular file under `bin/` or `manifest/` is
///   missing from the list (nothing unlisted ships);
/// * `bin/pilotdeck`, when present, is executable.
///
/// The "two binaries from one build" rule (009 §6.1) is enforced by the
/// package-internal consistency here — both binaries ship in the same
/// archive under the one verified manifest — because apply never executes
/// downloaded binaries to interrogate their embedded version.
pub fn verify_generation(dir: &Path, expected: &ReleaseManifest) -> Result<(), String> {
    let main_binary = dir.join("bin").join("agentcpm-pd");
    let meta = fs::metadata(&main_binary).map_err(|error| format!("bin/agentcpm-pd: {error}"))?;
    if !meta.is_file() {
        return Err("bin/agentcpm-pd is not a regular file".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err("bin/agentcpm-pd is not executable".to_owned());
        }
    }

    let manifest_path = dir.join("manifest").join("release.json");
    let manifest_bytes =
        fs::read(&manifest_path).map_err(|error| format!("manifest/release.json: {error}"))?;
    let manifest = parse_release_manifest(&manifest_bytes)?;
    if manifest.build_id != expected.build_id
        || manifest.tag != expected.tag
        || manifest.version != expected.version
        || manifest.source_sha != expected.source_sha
        || manifest.contract_digest != expected.contract_digest
    {
        return Err(format!(
            "package manifest identity ({}/{}) does not match the expected release ({}/{})",
            manifest.tag, manifest.build_id, expected.tag, expected.build_id
        ));
    }

    let sums_path = dir.join("SHA256SUMS.txt");
    let sums_text =
        fs::read_to_string(&sums_path).map_err(|error| format!("SHA256SUMS.txt: {error}"))?;
    let entries = parse_sha256sums(&sums_text)?;
    let mut listed = std::collections::BTreeMap::new();
    for (digest, path) in &entries {
        if listed.insert(path.clone(), digest.clone()).is_some() {
            return Err(format!("SHA256SUMS lists `{path}` twice"));
        }
    }
    let required = ["bin/agentcpm-pd", "manifest/release.json"];
    for required_path in required {
        if !listed.contains_key(required_path) {
            return Err(format!("SHA256SUMS does not cover `{required_path}`"));
        }
    }
    let shim = dir.join("bin").join("pilotdeck");
    let shim_present = shim.exists();
    if shim_present && !listed.contains_key("bin/pilotdeck") {
        return Err("SHA256SUMS does not cover the bundled bin/pilotdeck".to_owned());
    }

    for (digest, relative) in &entries {
        let path = dir.join(relative);
        let actual = file_sha256(&path).map_err(|error| format!("{relative}: {error}"))?;
        if &actual != digest {
            return Err(format!(
                "`{relative}` digest {actual} does not match SHA256SUMS entry {digest}"
            ));
        }
    }

    // No unlisted regular files under bin/ or manifest/.
    for base in ["bin", "manifest"] {
        let base_dir = dir.join(base);
        if !base_dir.is_dir() {
            continue;
        }
        let mut stack = vec![base_dir];
        while let Some(current) = stack.pop() {
            let entries = fs::read_dir(&current)
                .map_err(|error| format!("scan {}: {error}", current.display()))?;
            for entry in entries {
                let entry =
                    entry.map_err(|error| format!("scan {}: {error}", current.display()))?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let relative = path
                    .strip_prefix(dir)
                    .map_err(|error| format!("strip {}: {error}", path.display()))?
                    .to_string_lossy()
                    .replace('\\', "/");
                if !listed.contains_key(relative.as_str()) {
                    return Err(format!(
                        "package ships unlisted file `{relative}` (not covered by SHA256SUMS)"
                    ));
                }
            }
        }
    }

    if shim_present {
        let shim_meta = fs::metadata(&shim).map_err(|error| format!("bin/pilotdeck: {error}"))?;
        if !shim_meta.is_file() {
            return Err("bin/pilotdeck is not a regular file".to_owned());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if shim_meta.permissions().mode() & 0o111 == 0 {
                return Err("bin/pilotdeck is not executable".to_owned());
            }
        }
    }
    Ok(())
}

/// Reads a generation directory's manifest without the full consumer
/// validation (identity projection for `installation.inspect` / diagnose);
/// a structurally broken manifest surfaces as the error string.
pub fn read_generation_manifest(dir: &Path) -> Result<ReleaseManifest, String> {
    let bytes = fs::read(dir.join("manifest").join("release.json"))
        .map_err(|error| format!("manifest/release.json: {error}"))?;
    parse_release_manifest(&bytes)
}

/// The digest of the sha512 the packaging tool records: base64 of the raw
/// sha512 digest (standard alphabet, padded).
pub fn sha512_base64_of_file(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha512::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(BASE64.encode(hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "t021-release-{tag}-{}-{}",
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

    fn valid_manifest_json() -> Value {
        json_manifest("0.1.1", "v2026.11.18", "0.1.1+aaaaaaaaaaaa.bbbbbbbbbbbb")
    }

    fn json_manifest(version: &str, tag: &str, build_id: &str) -> Value {
        serde_json::json!({
            "schemaVersion": 2,
            "product": "agentcpm-pd",
            "version": version,
            "buildId": build_id,
            "tag": tag,
            "date": "2026-11-18",
            "buildTime": "2026-11-18T00:00:00Z",
            "sourceSha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "contractDigest": "bbbbbbbbbbbb",
            "assets": [{
                "name": format!("agentcpm-pd-{version}-darwin-arm64.tar.gz"),
                "size": 1000, "sha256": "c".repeat(64),
                "sha512": "d".repeat(86) + "==",
                "platform": "darwin", "arch": "arm64", "kind": "archive"
            }]
        })
    }

    fn parse(value: &Value) -> Result<ReleaseManifest, String> {
        parse_release_manifest(&serde_json::to_vec(value).unwrap())
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
    fn manifest_accepts_the_documented_shape() {
        let manifest = parse(&valid_manifest_json()).expect("valid");
        assert_eq!(manifest.tag, "v2026.11.18");
        assert_eq!(manifest.assets.len(), 1);
        assert!(manifest.assets[0].matches("darwin", "arm64"));
        // Release tag with a revision suffix also passes (baseline regex).
        let revisioned =
            json_manifest("0.1.1", "v2026.11.18-r3", "0.1.1+aaaaaaaaaaaa.bbbbbbbbbbbb");
        assert!(parse(&revisioned).is_ok());
    }

    #[test]
    fn manifest_rejects_schema_version_one_and_wrong_product() {
        let mut value = valid_manifest_json();
        value["schemaVersion"] = serde_json::json!(1);
        assert!(parse(&value).is_err());
        let mut value = valid_manifest_json();
        value["product"] = serde_json::json!("pilotdeck");
        assert!(parse(&value).is_err());
    }

    #[test]
    fn manifest_rejects_identity_inconsistency() {
        let mut value = valid_manifest_json();
        value["buildId"] = serde_json::json!("0.1.1+deadbeefdead.bbbbbbbbbbbb");
        let error = parse(&value).unwrap_err();
        assert!(error.contains("does not match its own fields"), "{error}");
    }

    #[test]
    fn manifest_rejects_bad_tag_date_and_digests() {
        let mut value = valid_manifest_json();
        value["tag"] = serde_json::json!("2026.11.18");
        assert!(parse(&value).is_err());
        let mut value = valid_manifest_json();
        value["date"] = serde_json::json!("18.11.2026");
        assert!(parse(&value).is_err());
        let mut value = valid_manifest_json();
        value["buildTime"] = serde_json::json!("yesterday");
        assert!(parse(&value).is_err());
        let mut value = valid_manifest_json();
        value["sourceSha"] = serde_json::json!("AAAA");
        assert!(parse(&value).is_err());
        let mut value = valid_manifest_json();
        value["contractDigest"] = serde_json::json!("BBBBBBBBBBBB");
        assert!(parse(&value).is_err());
    }

    #[test]
    fn manifest_rejects_asset_rule_violations() {
        let mut value = valid_manifest_json();
        value["assets"] = serde_json::json!([]);
        assert!(parse(&value).is_err());
        let mut value = valid_manifest_json();
        value["assets"][0]["sha256"] = serde_json::json!("C".repeat(64));
        assert!(parse(&value).is_err());
        let mut value = valid_manifest_json();
        value["assets"][0]["sha512"] = serde_json::json!("d".repeat(64));
        assert!(parse(&value).is_err());
        let mut value = valid_manifest_json();
        value["assets"][0]["size"] = serde_json::json!(0);
        assert!(parse(&value).is_err());
        let mut value = valid_manifest_json();
        value["assets"][0]["platform"] = serde_json::json!("windows");
        assert!(parse(&value).is_err());
        // Two assets for the same platform/arch is ambiguous.
        let mut value = valid_manifest_json();
        let mut clone = value["assets"][0].clone();
        clone["name"] = serde_json::json!("other-name.tar.gz");
        value["assets"].as_array_mut().unwrap().push(clone);
        assert!(parse(&value).is_err());
    }

    #[test]
    fn sha256sums_round_trip() {
        let dir = temp_dir("sums");
        let file = dir.join("bin").join("agentcpm-pd");
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(&file, b"binary-bytes").unwrap();
        let text = render_sha256sums(&[
            ("bin/agentcpm-pd".to_owned(), file.clone()),
            ("manifest/release.json".to_owned(), {
                fs::create_dir_all(dir.join("manifest")).unwrap();
                let path = dir.join("manifest").join("release.json");
                fs::write(&path, b"{}").unwrap();
                path
            }),
        ])
        .unwrap();
        let entries = parse_sha256sums(&text).unwrap();
        assert_eq!(entries.len(), 2);
        for (digest, relative) in &entries {
            let path = dir.join(relative);
            assert_eq!(&file_sha256(&path).unwrap(), digest);
        }
        assert!(parse_sha256sums("not-a-sum-line").is_err());
        assert!(parse_sha256sums("").is_err());
    }

    #[test]
    fn package_round_trip_preserves_bytes_and_modes() {
        let dir = temp_dir("roundtrip");
        let bin = dir.join("agentcpm-pd");
        fs::write(&bin, b"#!/bin/sh\necho main\n").unwrap();
        let sums = dir.join("release.json");
        fs::write(&sums, b"{}").unwrap();
        let archive = dir.join("pkg.tar.gz");
        write_release_package(
            &archive,
            &[
                PackageFile {
                    relative_path: "bin/agentcpm-pd".to_owned(),
                    source: bin.clone(),
                    mode: 0o755,
                },
                PackageFile {
                    relative_path: "manifest/release.json".to_owned(),
                    source: sums.clone(),
                    mode: 0o644,
                },
            ],
        )
        .unwrap();

        let out = dir.join("unpacked");
        fs::create_dir_all(&out).unwrap();
        let extracted = unpack_tar_gz(&archive, &out).unwrap();
        assert_eq!(extracted.len(), 2);
        assert_eq!(
            fs::read(out.join("bin").join("agentcpm-pd")).unwrap(),
            b"#!/bin/sh\necho main\n"
        );
        assert_eq!(
            fs::read(out.join("manifest").join("release.json")).unwrap(),
            b"{}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(out.join("bin").join("agentcpm-pd"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn unpack_rejects_traversal_and_links() {
        let dir = temp_dir("hostile");
        let archive = dir.join("hostile.tar.gz");
        // Hand-craft a tar with an absolute/traversal name.
        {
            use flate2::write::GzEncoder;
            let file = fs::File::create(&archive).unwrap();
            let mut tar = GzEncoder::new(file, flate2::Compression::default());
            let mut header = [0u8; 512];
            header[0..16].copy_from_slice(b"../evil.txt\0");
            write_ustar_octal(&mut header, 100, 8, 0o644).unwrap();
            write_ustar_octal(&mut header, 108, 8, 0).unwrap();
            write_ustar_octal(&mut header, 116, 8, 0).unwrap();
            write_ustar_octal(&mut header, 124, 12, 3).unwrap();
            write_ustar_octal(&mut header, 136, 12, 0).unwrap();
            header[148..156].fill(b' ');
            header[156] = b'0';
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
            header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
            tar.write_all(&header).unwrap();
            tar.write_all(b"bad").unwrap();
            tar.write_all(&[0u8; 509]).unwrap();
            tar.write_all(&[0u8; 512]).unwrap();
            tar.write_all(&[0u8; 512]).unwrap();
            tar.finish().unwrap();
        }
        let out = dir.join("out");
        fs::create_dir_all(&out).unwrap();
        let error = unpack_tar_gz(&archive, &out).unwrap_err();
        assert!(error.contains("not a safe relative path"), "{error}");
        assert!(!out.join("../evil.txt").exists() && !dir.join("evil.txt").exists());
    }

    #[test]
    fn unpack_rejects_symlink_entries() {
        let dir = temp_dir("symlink");
        let archive = dir.join("link.tar.gz");
        {
            use flate2::write::GzEncoder;
            let file = fs::File::create(&archive).unwrap();
            let mut tar = GzEncoder::new(file, flate2::Compression::default());
            let mut header = [0u8; 512];
            header[0..10].copy_from_slice(b"link-name\0");
            write_ustar_octal(&mut header, 100, 8, 0o777).unwrap();
            write_ustar_octal(&mut header, 108, 8, 0).unwrap();
            write_ustar_octal(&mut header, 116, 8, 0).unwrap();
            write_ustar_octal(&mut header, 124, 12, 0).unwrap();
            write_ustar_octal(&mut header, 136, 12, 0).unwrap();
            header[148..156].fill(b' ');
            header[156] = b'2'; // symlink
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
            header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
            tar.write_all(&header).unwrap();
            tar.write_all(&[0u8; 512]).unwrap();
            tar.write_all(&[0u8; 512]).unwrap();
            tar.finish().unwrap();
        }
        let out = dir.join("out");
        fs::create_dir_all(&out).unwrap();
        let error = unpack_tar_gz(&archive, &out).unwrap_err();
        assert!(error.contains("unsupported type flag"), "{error}");
    }

    #[test]
    fn verify_generation_accepts_a_good_package_and_rejects_tampering() {
        let dir = temp_dir("verify");
        let generation_dir = dir.join("generation");
        let (manifest, archive) = build_generation_fixture(&dir, &generation_dir);
        let out = dir.join("unpacked");
        fs::create_dir_all(&out).unwrap();
        unpack_tar_gz(&archive, &out).unwrap();
        verify_generation(&out, &manifest).expect("good package verifies");

        // Tamper with the binary: the sums check must fail.
        let tampered = dir.join("tampered");
        fs::create_dir_all(&tampered).unwrap();
        unpack_tar_gz(&archive, &tampered).unwrap();
        fs::write(tampered.join("bin").join("agentcpm-pd"), b"evil").unwrap();
        let error = verify_generation(&tampered, &manifest).unwrap_err();
        assert!(error.contains("does not match"), "{error}");

        // An unlisted extra file inside bin/ is rejected.
        let extra = dir.join("extra");
        fs::create_dir_all(&extra).unwrap();
        unpack_tar_gz(&archive, &extra).unwrap();
        fs::write(extra.join("bin").join("surprise"), b"x").unwrap();
        let error = verify_generation(&extra, &manifest).unwrap_err();
        assert!(error.contains("unlisted"), "{error}");
    }

    /// Builds one complete generation fixture: an on-disk layout + the
    /// manifest describing its archive.
    fn build_generation_fixture(dir: &Path, generation_dir: &Path) -> (ReleaseManifest, PathBuf) {
        let bin = generation_dir.join("bin").join("agentcpm-pd");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, b"#!/bin/sh\necho agentcpm-pd\n").unwrap();
        let manifest_json =
            json_manifest("0.1.1", "v2026.11.18", "0.1.1+aaaaaaaaaaaa.bbbbbbbbbbbb");
        let manifest_dir = generation_dir.join("manifest");
        fs::create_dir_all(&manifest_dir).unwrap();
        fs::write(manifest_dir.join("release.json"), manifest_json.to_string()).unwrap();

        let sums = render_sha256sums(&[
            ("bin/agentcpm-pd".to_owned(), bin.clone()),
            (
                "manifest/release.json".to_owned(),
                manifest_dir.join("release.json"),
            ),
        ])
        .unwrap();
        fs::write(generation_dir.join("SHA256SUMS.txt"), &sums).unwrap();

        let archive = dir.join("agentcpm-pd-0.1.1-darwin-arm64.tar.gz");
        write_release_package(
            &archive,
            &[
                PackageFile {
                    relative_path: "bin/agentcpm-pd".to_owned(),
                    source: bin,
                    mode: 0o755,
                },
                PackageFile {
                    relative_path: "manifest/release.json".to_owned(),
                    source: manifest_dir.join("release.json"),
                    mode: 0o644,
                },
                PackageFile {
                    relative_path: "SHA256SUMS.txt".to_owned(),
                    source: generation_dir.join("SHA256SUMS.txt"),
                    mode: 0o644,
                },
            ],
        )
        .unwrap();

        let manifest = parse(&manifest_json).unwrap();
        (manifest, archive)
    }
}
