use super::*;
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("pdrs-lsp-fd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p.canonicalize().unwrap())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[tokio::test]
#[cfg(unix)]
async fn native_read_preserves_legitimate_inside_symlink_and_exact_limit() {
    use std::os::unix::fs::symlink;
    let s = Scratch::new();
    std::fs::write(s.0.join("real.ts"), "OWNED").unwrap();
    symlink("real.ts", s.0.join("alias.ts")).unwrap();
    let port = NativeLspExecutionPort::new(Default::default(), s.0.clone()).unwrap();
    assert_eq!(
        port.read_file_bounded(s.0.join("alias.ts"), 5, Cancellation::new())
            .await
            .unwrap(),
        b"OWNED"
    );
    assert!(
        port.read_file_bounded(s.0.join("alias.ts"), 4, Cancellation::new())
            .await
            .unwrap_err()
            .message
            .contains("exceeds 4 bytes")
    );
}
#[tokio::test]
#[cfg(unix)]
async fn real_barrier_rejects_source_symlink_swap_that_unprotected_read_leaks() {
    use std::os::unix::fs::symlink;
    let s = Scratch::new();
    let owned = s.0.join("workspace");
    std::fs::create_dir(&owned).unwrap();
    let file = owned.join("source.ts");
    let outside = s.0.join("canary.ts");
    std::fs::write(&file, b"OWNED").unwrap();
    std::fs::write(&outside, b"FAKE_OUTSIDE_CANARY").unwrap();
    let port = NativeLspExecutionPort::new(Default::default(), owned).unwrap();
    let swapped = file.clone();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let unsafe_seen = seen.clone();
    let result = port
        .read_with_barrier(
            file,
            100,
            Cancellation::new(),
            Some(Box::new(move || {
                std::fs::remove_file(&swapped).unwrap();
                symlink(&outside, &swapped).unwrap();
                *unsafe_seen.lock().unwrap() = std::fs::read(&swapped).unwrap();
            })),
        )
        .await;
    assert_eq!(*seen.lock().unwrap(), b"FAKE_OUTSIDE_CANARY");
    assert!(
        result.is_err(),
        "a pathname reopen mutation would return the observed canary"
    );
}
#[tokio::test]
#[cfg(unix)]
async fn original_root_descriptor_survives_rename_and_foreign_real_directory_replacement() {
    let s = Scratch::new();
    let owned = s.0.join("workspace");
    std::fs::create_dir(&owned).unwrap();
    let file = owned.join("source.ts");
    std::fs::write(&file, b"OWNED").unwrap();
    let port = NativeLspExecutionPort::new(Default::default(), owned.clone()).unwrap();
    let old = s.0.join("old-owned");
    let result = port
        .read_with_barrier(
            file,
            100,
            Cancellation::new(),
            Some(Box::new(move || {
                std::fs::rename(&owned, &old).unwrap();
                std::fs::create_dir(&owned).unwrap();
                std::fs::write(owned.join("source.ts"), b"FOREIGN_REPLACEMENT").unwrap();
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        result, b"OWNED",
        "must never reopen the replaced root pathname"
    );
}
#[tokio::test]
#[cfg(unix)]
async fn raced_fifo_is_nonblocking_and_rejected_as_nonregular() {
    use std::os::unix::fs::FileTypeExt;
    let s = Scratch::new();
    let file = s.0.join("source.ts");
    std::fs::write(&file, b"OWNED").unwrap();
    let port = NativeLspExecutionPort::new(Default::default(), s.0.clone()).unwrap();
    let swapped = file.clone();
    let operation = port.read_with_barrier(
        file,
        100,
        Cancellation::new(),
        Some(Box::new(move || {
            std::fs::remove_file(&swapped).unwrap();
            #[cfg(target_vendor = "apple")]
            assert!(
                std::process::Command::new("/usr/bin/mkfifo")
                    .args(["-m", "600"])
                    .arg(&swapped)
                    .status()
                    .expect("system mkfifo must create the owned fixture")
                    .success()
            );
            #[cfg(not(target_vendor = "apple"))]
            rustix::fs::mknodat(
                rustix::fs::CWD,
                &swapped,
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::from_raw_mode(0o600),
                0,
            )
            .unwrap();
            assert!(
                std::fs::symlink_metadata(&swapped)
                    .unwrap()
                    .file_type()
                    .is_fifo()
            );
        })),
    );
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), operation)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.message.contains("not a file"));
}
#[tokio::test]
#[cfg(unix)]
async fn cancellation_before_or_at_open_is_explicit_and_owned() {
    let s = Scratch::new();
    let file = s.0.join("source.ts");
    std::fs::write(&file, b"OWNED").unwrap();
    let port = NativeLspExecutionPort::new(Default::default(), s.0.clone()).unwrap();
    let cancel = Cancellation::new();
    cancel.cancel();
    assert_eq!(
        port.read_file_bounded(file.clone(), 100, cancel)
            .await
            .unwrap_err()
            .code,
        "ABORT_ERR"
    );
    let cancel = Cancellation::new();
    let fired = cancel.clone();
    assert_eq!(
        port.read_with_barrier(file, 100, cancel, Some(Box::new(move || fired.cancel())))
            .await
            .unwrap_err()
            .code,
        "ABORT_ERR"
    );
}

#[test]
fn fixed_number_witness_preserves_large_coordinates_and_json_number_spelling() {
    let oracle: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/lsp-number/oracle.json"
    ))
    .unwrap();
    for row in oracle["results"].as_array().unwrap() {
        assert_eq!(row["validation"]["ok"], true);
        let position: LspPosition =
            serde_json::from_value(row["query"]["position"].clone()).unwrap();
        assert_eq!(position.line, row["input"].as_f64().unwrap() - 1.0);
        assert_eq!(position.character, row["input"].as_f64().unwrap() - 1.0);
        assert_eq!(
            serde_json::to_string(&position).unwrap(),
            row["wirePosition"].as_str().unwrap()
        );
    }
}

#[cfg(unix)]
struct RacePort {
    native: Arc<NativeLspExecutionPort>,
    hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    unsafe_control: bool,
}
#[cfg(unix)]
impl LspExecutionPort for RacePort {
    fn stat(&self, path: PathBuf, cancel: Cancellation) -> LspFuture<LspFileStat> {
        self.native.stat(path, cancel)
    }
    fn realpath(&self, path: PathBuf, cancel: Cancellation) -> LspFuture<PathBuf> {
        self.native.realpath(path, cancel)
    }
    fn read_file(&self, path: PathBuf, cancel: Cancellation) -> LspFuture<Vec<u8>> {
        self.native.read_file(path, cancel)
    }
    fn prepare_subprocess(
        &self,
        command: LspCommand,
        cancel: Cancellation,
    ) -> LspFuture<LspCommand> {
        self.native.prepare_subprocess(command, cancel)
    }
    fn read_file_bounded(
        &self,
        path: PathBuf,
        max: u64,
        cancel: Cancellation,
    ) -> LspFuture<Vec<u8>> {
        let hook = self.hook.lock().unwrap().take();
        if self.unsafe_control {
            Box::pin(async move {
                let _ = tokio::fs::canonicalize(&path).await.map_err(io_error)?;
                if let Some(hook) = hook {
                    hook();
                }
                let bytes = tokio::fs::read(path).await.map_err(io_error)?;
                if bytes.len() as u64 > max {
                    return Err(LspError::new("LSP_MALFORMED_RESPONSE", "too large"));
                }
                Ok(bytes)
            })
        } else {
            self.native.read_with_barrier(path, max, cancel, hook)
        }
    }
}
#[tokio::test]
#[cfg(unix)]
async fn actual_stdio_didopen_preserves_inside_symlink_and_never_leaks_raced_canary() {
    use super::super::stdio::{StdioLspConfig, StdioLspProvider};
    use std::os::unix::fs::symlink;
    const SERVER: &str = r#"import sys,json,os
wire=sys.argv[1]
with open(wire,'a') as f: f.write(json.dumps({'pid':os.getpid()})+'\n')
while True:
 headers={}
 while True:
  line=sys.stdin.buffer.readline()
  if not line: sys.exit(0)
  if line in (b'\r\n',b'\n'): break
  k,v=line.decode().split(':',1);headers[k.lower()]=v.strip()
 request=json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
 with open(wire,'a') as f: f.write(json.dumps(request)+'\n')
 method=request.get('method')
 if method=='exit': break
 if 'id' in request:
  result={'capabilities':{}} if method=='initialize' else ({'contents':'owned hover'} if method=='textDocument/hover' else None)
  payload=json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}).encode()
  sys.stdout.buffer.write(('Content-Length: '+str(len(payload))+'\r\n\r\n').encode()+payload);sys.stdout.buffer.flush()
"#;
    for mode in ["inside-link", "protected-race", "unsafe-control"] {
        let scratch = Scratch::new();
        let workspace = scratch.0.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let source = workspace.join("source.ts");
        let file = workspace.join("actual.ts");
        std::fs::write(&file, b"OWNED").unwrap();
        if mode == "inside-link" {
            symlink(&file, &source).unwrap();
        } else {
            std::fs::write(&source, b"OWNED").unwrap();
        }
        let canary = scratch.0.join("outside-canary.ts");
        std::fs::write(&canary, b"FAKE_OUTSIDE_CANARY").unwrap();
        let script = workspace.join("server.py");
        let wire = workspace.join("wire.jsonl");
        std::fs::write(&script, SERVER).unwrap();
        let native =
            Arc::new(NativeLspExecutionPort::new(Default::default(), workspace.clone()).unwrap());
        let hook = if mode == "inside-link" {
            None
        } else {
            let file = source.clone();
            Some(Box::new(move || {
                std::fs::remove_file(&file).unwrap();
                symlink(&canary, &file).unwrap();
            }) as Box<dyn FnOnce() + Send>)
        };
        let io = Arc::new(RacePort {
            native,
            hook: std::sync::Mutex::new(hook),
            unsafe_control: mode == "unsafe-control",
        });
        let mut config = StdioLspConfig::new(
            "secure-fixture",
            "python3",
            BTreeMap::from([(".ts".into(), "typescript".into())]),
        );
        config.args = vec![
            "-u".into(),
            script.to_string_lossy().into_owned(),
            wire.to_string_lossy().into_owned(),
        ];
        config.max_document_bytes = 100;
        config.request_timeout = std::time::Duration::from_secs(2);
        config.shutdown_timeout = std::time::Duration::from_millis(100);
        let provider = StdioLspProvider::new(config).unwrap();
        let bound = provider
            .bind_session(LspSessionExecution {
                workspace_root: workspace.clone(),
                io,
            })
            .unwrap();
        let result = bound
            .query(
                LspProviderQuery {
                    request: LspQueryRequest {
                        operation: LspOperation::Hover,
                        file_path: "source.ts".into(),
                        position: LspPosition {
                            line: 0.0,
                            character: 0.0,
                        },
                        workspace_root: workspace,
                    },
                    language_id: "typescript".into(),
                },
                Cancellation::new(),
            )
            .await;
        bound.dispose().await.unwrap();
        provider.dispose().await.unwrap();
        let rows = std::fs::read_to_string(&wire)
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str::<serde_json::Value>(s).unwrap())
            .collect::<Vec<_>>();
        let opened = rows
            .iter()
            .filter(|r| r["method"] == "textDocument/didOpen")
            .map(|r| {
                r["params"]["textDocument"]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        match mode {
            "inside-link" => {
                result.unwrap();
                assert_eq!(opened, vec!["OWNED"]);
            }
            "protected-race" => {
                assert!(result.is_err());
                assert!(opened.is_empty());
            }
            _ => {
                result.unwrap();
                assert_eq!(
                    opened,
                    vec!["FAKE_OUTSIDE_CANARY"],
                    "actual wrong-read control must be rejected by no-canary guard"
                );
                assert!(
                    !opened
                        .iter()
                        .all(|text| !text.contains("FAKE_OUTSIDE_CANARY"))
                );
            }
        }
        for pid in rows.iter().filter_map(|r| r["pid"].as_i64()) {
            let pid = rustix::process::Pid::from_raw(
                i32::try_from(pid).expect("actual LSP pid fits i32"),
            )
            .expect("actual LSP pid is positive");
            assert_eq!(
                rustix::process::test_kill_process(pid),
                Err(rustix::io::Errno::SRCH)
            );
        }
    }
}

#[tokio::test]
#[cfg(unix)]
async fn workspace_realpath_is_the_original_pinned_root_after_logical_alias_replacement() {
    use std::os::unix::fs::symlink;
    let s = Scratch::new();
    let owned = s.0.join("owned");
    let foreign = s.0.join("foreign");
    let alias = s.0.join("alias");
    std::fs::create_dir(&owned).unwrap();
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(owned.join("source.ts"), b"OWNED").unwrap();
    std::fs::write(foreign.join("source.ts"), b"FAKE_OUTSIDE_CANARY").unwrap();
    symlink(&owned, &alias).unwrap();
    let port = NativeLspExecutionPort::new(Default::default(), alias.clone()).unwrap();
    std::fs::remove_file(&alias).unwrap();
    symlink(&foreign, &alias).unwrap();
    assert_eq!(
        port.realpath(alias.clone(), Cancellation::new())
            .await
            .unwrap(),
        owned
    );
    assert!(
        port.read_file_bounded(alias.join("source.ts"), 100, Cancellation::new())
            .await
            .is_err()
    );
    let relative = NativeLspExecutionPort::new(Default::default(), PathBuf::from(".")).unwrap();
    assert_eq!(
        relative
            .realpath(std::env::current_dir().unwrap(), Cancellation::new())
            .await
            .unwrap(),
        std::env::current_dir().unwrap().canonicalize().unwrap()
    );
}

#[tokio::test]
#[cfg(unix)]
async fn ordinary_source_stat_and_realpath_preserve_observed_node_enoent() {
    let s = Scratch::new();
    let port = NativeLspExecutionPort::new(Default::default(), s.0.clone()).unwrap();
    let missing = s.0.join("missing.ts");
    let stat = port
        .stat(missing.clone(), Cancellation::new())
        .await
        .unwrap_err();
    assert_eq!(stat.code, "ENOENT");
    assert_eq!(
        stat.message,
        format!(
            "ENOENT: no such file or directory, stat '{}'",
            missing.display()
        )
    );
    let realpath = port
        .realpath(missing.clone(), Cancellation::new())
        .await
        .unwrap_err();
    assert_eq!(realpath.code, "ENOENT");
    assert_eq!(
        realpath.message,
        format!(
            "ENOENT: no such file or directory, realpath '{}'",
            missing.display()
        )
    );
}
