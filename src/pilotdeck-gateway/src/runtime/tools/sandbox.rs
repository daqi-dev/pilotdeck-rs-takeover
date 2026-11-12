//! `agent.sandboxMode`: the per-invocation file-effect policy of the native
//! execution world (baseline `tool/execution-world/SandboxPort.ts`,
//! `NodeSandboxPort.ts`, `SandboxedFsPort.ts`, `SandboxedShellPort.ts`,
//! `SandboxedCodeRuntimePort.ts`, `SandboxedDetachedShellPort.ts`, composed
//! by `ExecutionWorldBundle.ts:134-216`).
//!
//! - `danger-full-access` (the default) wraps nothing.
//! - `read-only` / `workspace-write` route every process the execution world
//!   starts (`bash`, the `execute_code` interpreter and its probe, background
//!   tasks) through macOS Seatbelt (`/usr/bin/sandbox-exec -p <profile> --
//!   <argv>`). The profile allows everything except file writes; writes are
//!   allowed only to `/dev/null` and, under `workspace-write`, below the
//!   workspace (the command's cwd), the execution root, `/tmp` and the
//!   process temp directory. Network and reads are not restricted.
//! - Any other platform, or a host whose `sandbox-exec` probe fails, fails
//!   closed: a constrained mode never silently runs unconfined
//!   (`SandboxUnavailableError`).
//! - The write tools (`write_file`, `edit_file`, `edit_notebook`) run in the
//!   gateway process, so the policy is a path fence instead: `read-only`
//!   refuses every write; `workspace-write` refuses a target outside the
//!   root the path was resolved against (lexically, like the baseline).
use super::ToolError;
use crate::runtime::project::SandboxMode;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::path::{Path, PathBuf};

/// `NodeSandboxPort`'s fixed Seatbelt executable.
pub const SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";

/// `SandboxUnavailableError.message` (SandboxPort.ts:56-60).
pub fn unavailable_message(mode: SandboxMode) -> String {
    format!(
        "Sandbox mode {} is requested but no enforcing sandbox provider is available.",
        mode.as_str()
    )
}

/// One exact argv after `SandboxPort.prepare`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedCommand {
    pub executable: String,
    pub args: Vec<String>,
}

/// The selected mode of one execution world. `Copy`, so every consumer
/// carries its own handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sandbox {
    mode: SandboxMode,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new(SandboxMode::DangerFullAccess)
    }
}

impl Sandbox {
    pub fn new(mode: SandboxMode) -> Self {
        Self { mode }
    }
    pub fn mode(self) -> SandboxMode {
        self.mode
    }
    /// `useSandbox`: a constrained mode routes the providers through the
    /// sandbox (ExecutionWorldBundle.ts:137).
    pub fn confines(self) -> bool {
        self.mode != SandboxMode::DangerFullAccess
    }

    /// `NodeSandboxPort.prepare` for `{ mode, workspaceRoot, executionRoot }`.
    /// `Err` is the `SandboxUnavailableError` message.
    pub fn prepare(
        self,
        executable: &str,
        args: Vec<String>,
        workspace_root: &Path,
        execution_root: Option<&Path>,
    ) -> Result<PreparedCommand, String> {
        if !self.confines() {
            return Ok(PreparedCommand {
                executable: executable.to_owned(),
                args,
            });
        }
        if !seatbelt_available() {
            return Err(unavailable_message(self.mode));
        }
        let mut wrapped = vec![
            "-p".to_owned(),
            seatbelt_profile(self.mode, workspace_root, execution_root),
            "--".to_owned(),
            executable.to_owned(),
        ];
        wrapped.extend(args);
        Ok(PreparedCommand {
            executable: SEATBELT_EXECUTABLE.to_owned(),
            args: wrapped,
        })
    }

    /// `SandboxedFsPort.writeText` with the write tools' `workspaceRoot`
    /// (the root the path was resolved against): `read-only` refuses every
    /// write; `workspace-write` refuses a target lexically outside the root
    /// (`checkedSessionPath`, no symlink resolution — as in the baseline).
    pub fn check_write(
        self,
        file_path: &JsString,
        workspace_root: &JsString,
    ) -> Result<(), ToolError> {
        match self.mode {
            SandboxMode::DangerFullAccess => Ok(()),
            SandboxMode::ReadOnly => Err(denied(file_path, self.mode, None)),
            SandboxMode::WorkspaceWrite => {
                let root = resolve_js(workspace_root);
                let target = resolve_js(file_path);
                let relative = super::builtin::relative(&root, &target);
                let text = String::from_utf16_lossy(relative.units());
                if !text.is_empty() && (text.starts_with("..") || text.starts_with('/')) {
                    return Err(denied(
                        file_path,
                        SandboxMode::WorkspaceWrite,
                        Some("the path is outside the session workspace"),
                    ));
                }
                Ok(())
            }
        }
    }
}

/// `path.resolve(value)` against the gateway's working directory.
fn resolve_js(value: &JsString) -> JsString {
    if super::builtin::is_absolute(value) {
        return super::builtin::normalize(value);
    }
    let process = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    super::builtin::join(&JsString::from(process.to_string_lossy().as_ref()), value)
}

/// `sandboxDenied` (SandboxedFsPort.ts:123-129).
fn denied(file_path: &JsString, mode: SandboxMode, detail: Option<&str>) -> ToolError {
    let mut message: Vec<u16> = "Writing to ".encode_utf16().collect();
    message.extend_from_slice(file_path.units());
    let tail = match detail {
        Some(detail) => format!(
            " is denied by the {} sandbox policy: {detail}",
            mode.as_str()
        ),
        None => format!(" is denied by the {} sandbox policy.", mode.as_str()),
    };
    message.extend(tail.encode_utf16());
    let mut details = JsObject::new();
    details.insert("sandboxMode".into(), JsValue::String(mode.as_str().into()));
    details.insert("filePath".into(), JsValue::String(file_path.clone()));
    ToolError {
        code: "permission_denied".into(),
        message: JsString::from_utf16(message),
        details: Some(details),
    }
}

/// `seatbeltProfileArgs(policy)[1]` (NodeSandboxPort.ts:63-75).
pub fn seatbelt_profile(
    mode: SandboxMode,
    workspace_root: &Path,
    execution_root: Option<&Path>,
) -> String {
    let mut forms = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        "(deny file-write*)".to_owned(),
        format!("(allow file-write* (literal {}))", sbpl_string("/dev/null")),
    ];
    let roots = writable_roots(mode, workspace_root, execution_root);
    if !roots.is_empty() {
        let subpaths: Vec<String> = roots
            .iter()
            .map(|root| format!("(subpath {})", sbpl_string(root)))
            .collect();
        forms.push(format!("(allow file-write* {})", subpaths.join(" ")));
    }
    forms.join(" ")
}

/// `getWritableRoots` (NodeSandboxPort.ts:90-98): workspace, execution root,
/// `/tmp` and `os.tmpdir()`, each canonicalized, first occurrence kept.
fn writable_roots(
    mode: SandboxMode,
    workspace_root: &Path,
    execution_root: Option<&Path>,
) -> Vec<String> {
    if mode != SandboxMode::WorkspaceWrite {
        return Vec::new();
    }
    let tmp = crate::node_io::node_tmpdir(|key| std::env::var_os(key));
    let mut candidates: Vec<&Path> = vec![workspace_root];
    candidates.extend(execution_root);
    candidates.push(Path::new("/tmp"));
    candidates.push(Path::new(&tmp));
    let mut roots: Vec<String> = Vec::new();
    for candidate in candidates {
        if candidate.as_os_str().is_empty() {
            continue;
        }
        let canonical = canonical_path(candidate);
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    roots
}

/// `realpathSync.native(resolve(value))`, the resolved path when that fails.
fn canonical_path(value: &Path) -> String {
    let absolute = resolve_js(&JsString::from(value.to_string_lossy().as_ref()));
    let absolute = String::from_utf16_lossy(absolute.units());
    match std::fs::canonicalize(&absolute) {
        Ok(real) => real.to_string_lossy().into_owned(),
        Err(_) => absolute,
    }
}

/// `sbplString`: a double-quoted SBPL string with `\` and `"` escaped.
fn sbpl_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `platform === "darwin" && isSeatbeltAvailable()`: the executable exists
/// and confines `/usr/bin/true` under the read-only profile within 5 s.
/// Probed once per process (the baseline probes once per execution world).
fn seatbelt_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| cfg!(target_os = "macos") && probe_seatbelt(SEATBELT_EXECUTABLE))
}

fn probe_seatbelt(executable: &str) -> bool {
    if !Path::new(executable).exists() {
        return false;
    }
    let profile = seatbelt_profile(SandboxMode::ReadOnly, Path::new("/"), None);
    let Ok(mut child) = std::process::Command::new(executable)
        .args(["-p", &profile, "--", "/usr/bin/true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code() == Some(0),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danger_full_access_returns_the_exact_argv() {
        let prepared = Sandbox::default()
            .prepare(
                "/bin/sh",
                vec!["-c".into(), "x".into()],
                Path::new("/w"),
                None,
            )
            .unwrap();
        assert_eq!(
            prepared,
            PreparedCommand {
                executable: "/bin/sh".into(),
                args: vec!["-c".into(), "x".into()]
            }
        );
    }

    #[test]
    fn read_only_profile_denies_every_write_but_dev_null() {
        assert_eq!(
            seatbelt_profile(SandboxMode::ReadOnly, Path::new("/w"), None),
            "(version 1) (allow default) (deny file-write*) (allow file-write* (literal \"/dev/null\"))"
        );
    }

    #[test]
    fn workspace_write_profile_lists_canonical_unique_roots() {
        let dir = std::env::temp_dir().join(format!("sbx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let profile = seatbelt_profile(SandboxMode::WorkspaceWrite, &dir, Some(&dir));
        let real = std::fs::canonicalize(&dir).unwrap();
        let real_tmp = std::fs::canonicalize("/tmp").unwrap();
        let expected_head = format!(
            "(version 1) (allow default) (deny file-write*) (allow file-write* (literal \"/dev/null\")) (allow file-write* (subpath \"{}\") (subpath \"{}\")",
            real.display(),
            real_tmp.display()
        );
        assert!(profile.starts_with(&expected_head), "{profile}");
        // The workspace appears once even though it is also the execution root.
        assert_eq!(
            profile.matches(&format!("\"{}\"", real.display())).count(),
            1
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sbpl_strings_escape_quotes_and_backslashes() {
        assert_eq!(sbpl_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn write_fence_follows_the_mode() {
        let file = JsString::from("/w/a.txt");
        let root = JsString::from("/w");
        assert!(Sandbox::default().check_write(&file, &root).is_ok());
        let error = Sandbox::new(SandboxMode::ReadOnly)
            .check_write(&file, &root)
            .unwrap_err();
        assert_eq!(error.code, "permission_denied");
        assert_eq!(
            String::from_utf16_lossy(error.message.units()),
            "Writing to /w/a.txt is denied by the read-only sandbox policy."
        );
        let write = Sandbox::new(SandboxMode::WorkspaceWrite);
        assert!(write.check_write(&file, &root).is_ok());
        assert!(
            write
                .check_write(&JsString::from("/w/sub/../b"), &root)
                .is_ok()
        );
        let error = write
            .check_write(&JsString::from("/w/../x"), &root)
            .unwrap_err();
        assert_eq!(
            String::from_utf16_lossy(error.message.units()),
            "Writing to /w/../x is denied by the workspace-write sandbox policy: the path is outside the session workspace"
        );
        // `relative.startsWith("..")` also refuses a sibling-looking name.
        assert!(write.check_write(&JsString::from("/w/..x"), &root).is_err());
        assert!(write.check_write(&JsString::from("/tmp/x"), &root).is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn constrained_modes_fail_closed_off_macos() {
        for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
            assert_eq!(
                Sandbox::new(mode).prepare("/bin/sh", vec![], Path::new("/w"), None),
                Err(unavailable_message(mode))
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_confines_a_shell_write() {
        let dir = std::env::temp_dir().join(format!("sbx-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("f.txt");
        let script = format!("echo x > '{}'", target.display());
        let run = |mode| {
            let prepared = Sandbox::new(mode)
                .prepare("/bin/sh", vec!["-c".into(), script.clone()], &dir, None)
                .unwrap();
            std::process::Command::new(&prepared.executable)
                .args(&prepared.args)
                .output()
                .unwrap()
        };
        let output = run(SandboxMode::ReadOnly);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Operation not permitted"));
        assert!(!target.exists());
        assert!(run(SandboxMode::WorkspaceWrite).status.success());
        assert!(target.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The bbx scenarios run below `/tmp`, which `workspace-write` keeps
    /// writable; this covers a target outside every writable root.
    #[cfg(target_os = "macos")]
    #[test]
    fn workspace_write_confines_writes_outside_the_writable_roots() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target")
            .join(format!("sbx-outside-{}", std::process::id()));
        let workspace = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let script = format!(
            "echo in > inside.txt && (echo out > '{0}/a.txt'; sh -c \"echo n > '{0}/b.txt'\"; ln -s '{0}' lnk; echo l > lnk/c.txt; echo up > ../up.txt); exit 0",
            outside.display()
        );
        let prepared = Sandbox::new(SandboxMode::WorkspaceWrite)
            .prepare("/bin/sh", vec!["-c".into(), script], &workspace, None)
            .unwrap();
        let output = std::process::Command::new(&prepared.executable)
            .args(&prepared.args)
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(workspace.join("inside.txt").exists());
        for escaped in ["a.txt", "b.txt", "c.txt"] {
            assert!(!outside.join(escaped).exists(), "{escaped} escaped");
        }
        assert!(!base.join("up.txt").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
