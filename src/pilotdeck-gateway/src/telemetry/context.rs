use super::{TelemetryEnv, TelemetryFailure, lock};
use pilotdeck_protocol::{JsString, JsValue};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct RuntimeContextOptions {
    pub env: TelemetryEnv,
    pub pilot_home: Option<PathBuf>,
    pub cwd: PathBuf,
    pub executable: PathBuf,
    pub entrypoint: Option<PathBuf>,
    pub platform: String,
    pub arch: String,
    pub package_version: Option<String>,
}
impl RuntimeContextOptions {
    pub fn from_process(env: TelemetryEnv, pilot_home: Option<PathBuf>, cwd: PathBuf) -> Self {
        let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("unknown-exec"));
        Self {
            env,
            pilot_home,
            cwd,
            entrypoint: Some(executable.clone()),
            executable,
            platform: match std::env::consts::OS {
                "macos" => "darwin",
                "windows" => "win32",
                other => other,
            }
            .into(),
            arch: match std::env::consts::ARCH {
                "aarch64" => "arm64",
                "x86_64" => "x64",
                "x86" => "ia32",
                other => other,
            }
            .into(),
            package_version: Some(env!("CARGO_PKG_VERSION").into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelemetryRuntimeContext {
    pub installation_id: JsString,
    pub instance_id: JsString,
    pub deployment_mode: JsString,
    pub commit_hash: JsString,
    pub app_version: JsString,
    pub platform: JsString,
}
impl TelemetryRuntimeContext {
    pub fn to_value(&self) -> JsValue {
        super::object([
            (
                "installationId",
                JsValue::String(self.installation_id.clone()),
            ),
            ("instanceId", JsValue::String(self.instance_id.clone())),
            (
                "deploymentMode",
                JsValue::String(self.deployment_mode.clone()),
            ),
            ("commitHash", JsValue::String(self.commit_hash.clone())),
            ("appVersion", JsValue::String(self.app_version.clone())),
            ("platform", JsValue::String(self.platform.clone())),
        ])
    }
}

/// Node crypto.update replaces unpaired UTF-16 before UTF-8 hashing.
pub fn hash_telemetry_id(value: &JsString) -> JsString {
    hash(&String::from_utf16_lossy(value.units())).into()
}
fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..24].into()
}
pub(super) fn resolve(cwd: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            part => out.push(part.as_os_str()),
        }
    }
    out
}
fn first_env<'a>(env: &'a TelemetryEnv, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|key| env.get(*key).map(String::as_str))
}
fn git(options: &RuntimeContextOptions, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(&options.cwd)
        .env_clear()
        .envs(&options.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            _ => {
                #[cfg(unix)]
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
                }
                #[cfg(not(unix))]
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn resolve_runtime_context(
    options: &RuntimeContextOptions,
) -> Result<TelemetryRuntimeContext, TelemetryFailure> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, TelemetryRuntimeContext>>> = OnceLock::new();
    let env = &options.env;
    let home = options
        .pilot_home
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| env.get("PILOT_HOME").cloned());
    let commit = first_env(
        env,
        &["COMMIT_HASH", "GIT_COMMIT", "PILOTDECK_GIT_SHA", "GIT_SHA"],
    );
    let version = first_env(env, &["PILOTDECK_VERSION", "npm_package_version"]);
    let key = [
        home.as_deref().unwrap_or("default"),
        commit.unwrap_or(""),
        version.unwrap_or(""),
        &options.executable.to_string_lossy(),
        &options
            .entrypoint
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    ]
    .join("|");
    let cache = CACHE.get_or_init(Default::default);
    let mut cache = lock(cache);
    if let Some(value) = cache.get(&key) {
        return Ok(value.clone());
    }
    let executable = options.executable.to_string_lossy().to_lowercase();
    let entry = options
        .entrypoint
        .as_ref()
        .map(|p| p.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let docker = env.get("DOCKER_CONTAINER").is_some_and(|v| v == "1")
        || env
            .get("KUBERNETES_SERVICE_HOST")
            .is_some_and(|v| !v.is_empty())
        || Path::new("/.dockerenv").exists()
        || fs::read_to_string("/proc/1/cgroup").is_ok_and(|s| {
            let s = s.to_lowercase();
            ["docker", "containerd", "kubepods", "podman"]
                .iter()
                .any(|v| s.contains(v))
        });
    let mode = if docker {
        "docker"
    } else if executable.contains("pilotdeck.app") || executable.ends_with("pilotdeck.exe") {
        "desktop_installer"
    } else if git(options, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true") {
        "source"
    } else if entry.contains("/.local/bin/pilotdeck") || entry.contains("/usr/local/bin/pilotdeck")
    {
        "curl_installer"
    } else if env
        .get("npm_config_user_agent")
        .is_some_and(|v| !v.is_empty())
        || entry.contains("node_modules")
    {
        "npm_binary"
    } else {
        "unknown"
    };
    let pilot_home = home.as_ref().map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(
            env.get("HOME")
                .map(String::as_str)
                .unwrap_or("unknown-home"),
        )
        .join(".pilotdeck")
    });
    let token_path = resolve(&options.cwd, &pilot_home).join("server-token");
    let token = if token_path.exists() {
        Some(
            String::from_utf8_lossy(
                &fs::read(&token_path).map_err(|e| TelemetryFailure(e.to_string()))?,
            )
            .into_owned(),
        )
    } else {
        None
    };
    let installation = token
        .as_deref()
        .map(crate::metadata::js_trim)
        .filter(|v| !v.is_empty())
        .map(hash)
        .unwrap_or_else(|| {
            hash(&format!(
                "{}:{}:{}",
                options.platform,
                options.arch,
                env.get("HOME")
                    .map(String::as_str)
                    .unwrap_or("unknown-home")
            ))
        });
    let seed = format!(
        "{}|{}|{}|{}",
        resolve(
            &options.cwd,
            Path::new(home.as_deref().unwrap_or("unknown-pilot-home"))
        )
        .display(),
        resolve(&options.cwd, &options.executable).display(),
        resolve(
            &options.cwd,
            options
                .entrypoint
                .as_deref()
                .unwrap_or(Path::new("unknown-entry"))
        )
        .display(),
        mode
    );
    let context = TelemetryRuntimeContext {
        installation_id: installation.into(),
        instance_id: hash(&seed).into(),
        deployment_mode: mode.into(),
        commit_hash: commit
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .or_else(|| git(options, &["rev-parse", "--short", "HEAD"]).filter(|v| !v.is_empty()))
            .unwrap_or_else(|| "unknown".into())
            .into(),
        app_version: version
            .filter(|v| !v.is_empty())
            .or(options.package_version.as_deref())
            .unwrap_or("0.0.0")
            .into(),
        platform: options.platform.clone().into(),
    };
    cache.insert(key, context.clone());
    Ok(context)
}
