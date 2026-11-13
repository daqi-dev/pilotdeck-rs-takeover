use super::*;
use crate::runtime::lsp::{LspExecutionPort, LspFileStat, LspQueryRequest};

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("pdrs-lsp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        std::fs::write(root.join("source.ts"), "const 😀answer = 42;\n").unwrap();
        std::fs::write(root.join("server.py"), SERVER).unwrap();
        Self { root }
    }
    fn config(&self, mode: &str) -> StdioLspConfig {
        let mut config = StdioLspConfig::new(
            "fixture",
            "python3",
            BTreeMap::from([(" .TS ".into(), "typescript".into())]),
        );
        config.args = vec![
            "-u".into(),
            self.root.join("server.py").to_string_lossy().into_owned(),
            mode.into(),
            self.root.join("wire.jsonl").to_string_lossy().into_owned(),
        ];
        config.request_timeout = Duration::from_secs(2);
        config.shutdown_timeout = Duration::from_millis(100);
        config.configuration = Some(json!({"typescript":{"strict":true}}));
        config
    }
    fn request(&self, operation: LspOperation) -> LspProviderQuery {
        LspProviderQuery {
            request: LspQueryRequest {
                operation,
                file_path: "source.ts".into(),
                position: LspPosition {
                    line: 0.0,
                    character: 8.0,
                },
                workspace_root: self.root.clone(),
            },
            language_id: "typescript".into(),
        }
    }
    fn wire(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.join("wire.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    async fn wait_method(&self, method: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self
                    .wire()
                    .iter()
                    .any(|value| value.get("method") == Some(&Value::String(method.into())))
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    #[cfg(unix)]
    fn assert_reaped(&self) {
        for pid in self
            .wire()
            .iter()
            .filter_map(|v| v.get("pid").and_then(Value::as_u64))
        {
            let pid = rustix::process::Pid::from_raw(pid as i32).unwrap();
            assert_eq!(
                rustix::process::test_kill_process(pid),
                Err(rustix::io::Errno::SRCH),
                "actual LSP child must be reaped"
            );
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
const SERVER: &str = r#"
import sys,json,os
mode,log=sys.argv[1:3]
def record(v):
 with open(log,'a') as f:f.write(json.dumps(v)+'\n')
record({'pid':os.getpid(),'cwd':os.getcwd(),'argv':sys.argv,'executable':sys.executable,'preparedEnv':os.environ.get('LSP_PREPARED_CANARY')})
def send(v):
 b=json.dumps(v).encode(); head=('Content-Length: %d\r\n\r\n'%len(b)).encode()
 # Exercise fragmented byte framing, including inside UTF8 bodies.
 for part in [head[:4],head[4:],b[:2],b[2:]]:
  sys.stdout.buffer.write(part);sys.stdout.buffer.flush()
pending=None
file=None
while True:
 length=None
 while True:
  line=sys.stdin.buffer.readline()
  if not line:sys.exit(0)
  if line==b'\r\n':break
  if line.lower().startswith(b'content-length:'):length=int(line.split(b':',1)[1])
 if length is None:sys.exit(2)
 msg=json.loads(sys.stdin.buffer.read(length));record(msg)
 method=msg.get('method')
 if method=='initialize':
  if mode=='stall-init':continue
  pending=msg['id']
  if mode=='workspace-name':
   send({'jsonrpc':'2.0','id':100,'method':'workspace/workspaceFolders','params':None});continue
  send({'jsonrpc':'2.0','id':99,'method':'workspace/configuration','params':{'items':[{'section':'typescript'}]}})
  continue
 if msg.get('id')==100:
  if msg.get('result')!=[{'uri':'file:///workspace','name':sys.argv[4]}]:sys.exit(5)
  send({'jsonrpc':'2.0','id':99,'method':'workspace/configuration','params':{'items':[{'section':'typescript'}]}});continue
 if msg.get('id')==99:
  if msg.get('result')!=[{'strict':True}]:sys.exit(4)
  send({'jsonrpc':'2.0','id':pending,'result':{'capabilities':{}}});continue
 if method=='textDocument/didOpen':file=msg['params']['textDocument']['uri']
 if method=='exit':sys.exit(0)
 if method in ['textDocument/definition','textDocument/references','textDocument/implementation','textDocument/hover']:
  if mode=='stall-query':continue
  if mode=='malformed':result={'unexpected':True}
  elif method=='textDocument/hover':result={'contents':[{'language':'typescript','value':'answer: number'},'π😀'],'range':{'start':{'line':0,'character':6},'end':{'line':0,'character':12}}}
  else:result=[{'targetUri':file,'targetSelectionRange':{'start':{'line':0,'character':6},'end':{'line':0,'character':12}}}]
  sys.stderr.write('diagnostic'*20000);sys.stderr.flush()
  send({'jsonrpc':'2.0','id':msg['id'],'result':result})
 elif method=='shutdown':send({'jsonrpc':'2.0','id':msg['id'],'result':None})
"#;

#[tokio::test]
async fn stdio_four_operations_use_actual_server_and_close_each_process() {
    for operation in [
        LspOperation::Definition,
        LspOperation::References,
        LspOperation::Implementation,
        LspOperation::Hover,
    ] {
        let fixture = Fixture::new();
        let provider = StdioLspProvider::new(fixture.config("normal")).unwrap();
        assert_eq!(
            provider.extension_to_language(),
            BTreeMap::from([(".ts".into(), "typescript".into())])
        );
        let result = provider
            .query(fixture.request(operation), Cancellation::new())
            .await
            .unwrap();
        match result {
            LspQueryResult::Locations {
                locations,
                resolved_workspace_uri,
            } => {
                assert_eq!(locations.len(), 1);
                assert_eq!(
                    locations[0].uri,
                    uri(&fixture.root.join("source.ts")).unwrap()
                );
                assert_eq!(
                    locations[0].range.start,
                    LspPosition {
                        line: 0.0,
                        character: 6.0
                    }
                );
                assert_eq!(resolved_workspace_uri, uri(&fixture.root).unwrap());
            }
            LspQueryResult::Hover { hover } => assert_eq!(
                hover.unwrap()["contents"],
                "```typescript\nanswer: number\n```\n\nπ😀"
            ),
        }
        let wire = fixture.wire();
        let methods = wire
            .iter()
            .filter_map(|v| v.get("method").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(methods.first(), Some(&"initialize"));
        assert_eq!(&methods[1..3], ["initialized", "textDocument/didOpen"]);
        assert_eq!(&methods[4..], ["textDocument/didClose", "shutdown", "exit"]);
        let init = wire
            .iter()
            .find(|v| v.get("method") == Some(&json!("initialize")))
            .unwrap();
        assert_eq!(init["params"]["processId"], Value::Null);
        assert_eq!(init["params"]["capabilities"], json!({}));
        assert_eq!(init["params"]["initializationOptions"], Value::Null);
        let request = wire
            .iter()
            .find(|v| {
                v.get("method").and_then(Value::as_str).is_some_and(|s| {
                    matches!(
                        s,
                        "textDocument/definition"
                            | "textDocument/references"
                            | "textDocument/implementation"
                            | "textDocument/hover"
                    )
                })
            })
            .unwrap();
        assert_eq!(
            request["params"]["position"],
            json!({"line":0,"character":8})
        );
        if operation == LspOperation::References {
            assert_eq!(
                request["params"]["context"],
                json!({"includeDeclaration":true})
            );
        }
        provider.dispose().await.unwrap();
        #[cfg(unix)]
        fixture.assert_reaped();
    }
}
#[tokio::test]
async fn stdio_timeout_sends_cancel_and_waits_for_actual_process_teardown() {
    let fixture = Fixture::new();
    let mut config = fixture.config("stall-init");
    config.request_timeout = Duration::from_millis(50);
    let provider = StdioLspProvider::new(config).unwrap();
    let error = provider
        .query(fixture.request(LspOperation::Hover), Cancellation::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "LSP_TIMEOUT");
    let wire = fixture.wire();
    let init = wire
        .iter()
        .find(|v| v.get("method") == Some(&json!("initialize")))
        .unwrap();
    let cancel = wire
        .iter()
        .find(|v| v.get("method") == Some(&json!("$/cancelRequest")))
        .unwrap();
    assert_eq!(cancel["params"]["id"], init["id"]);
    provider.dispose().await.unwrap();
    #[cfg(unix)]
    fixture.assert_reaped();
}
#[tokio::test]
async fn stdio_cancel_after_real_query_preserves_wire_order_and_reaps() {
    let fixture = Fixture::new();
    let provider = StdioLspProvider::new(fixture.config("stall-query")).unwrap();
    let cancel = Cancellation::new();
    let query = tokio::spawn(provider.query(fixture.request(LspOperation::Hover), cancel.clone()));
    fixture.wait_method("textDocument/hover").await;
    cancel.cancel();
    assert_eq!(query.await.unwrap().unwrap_err().code, "ABORT_ERR");
    let wire = fixture.wire();
    let methods = wire
        .iter()
        .filter_map(|v| v.get("method").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "textDocument/didOpen",
            "textDocument/hover",
            "$/cancelRequest",
            "textDocument/didClose",
            "shutdown",
            "exit"
        ]
    );
    provider.dispose().await.unwrap();
    #[cfg(unix)]
    fixture.assert_reaped();
}
#[tokio::test]
async fn stdio_dropped_waiter_dispose_joins_real_query_owner() {
    let fixture = Fixture::new();
    let provider = StdioLspProvider::new(fixture.config("stall-query")).unwrap();
    let query =
        tokio::spawn(provider.query(fixture.request(LspOperation::Hover), Cancellation::new()));
    fixture.wait_method("textDocument/hover").await;
    query.abort();
    assert!(query.await.unwrap_err().is_cancelled());
    provider.dispose().await.unwrap();
    #[cfg(unix)]
    fixture.assert_reaped();
    assert!(
        fixture
            .wire()
            .iter()
            .any(|v| v.get("method") == Some(&json!("exit")))
    );
    assert_eq!(
        provider
            .query(fixture.request(LspOperation::Hover), Cancellation::new())
            .await
            .unwrap_err()
            .code,
        "LSP_DISPOSED"
    );
}
#[tokio::test]
async fn stdio_rejects_malformed_result_after_real_close() {
    let fixture = Fixture::new();
    let provider = StdioLspProvider::new(fixture.config("malformed")).unwrap();
    assert_eq!(
        provider
            .query(fixture.request(LspOperation::Hover), Cancellation::new())
            .await
            .unwrap_err()
            .code,
        "LSP_MALFORMED_RESPONSE"
    );
    provider.dispose().await.unwrap();
    #[cfg(unix)]
    fixture.assert_reaped();
    assert!(
        fixture
            .wire()
            .iter()
            .any(|v| v.get("method") == Some(&json!("textDocument/didClose")))
    );
}
struct Execution {
    root: PathBuf,
}
impl LspExecutionPort for Execution {
    fn stat(&self, path: PathBuf, _: Cancellation) -> LspFuture<LspFileStat> {
        Box::pin(async move {
            let s = tokio::fs::metadata(&path)
                .await
                .map_err(|e| source_io_error("stat", &path, e))?;
            Ok(LspFileStat {
                is_file: s.is_file(),
                is_directory: s.is_dir(),
                size: s.len(),
            })
        })
    }
    fn realpath(&self, path: PathBuf, _: Cancellation) -> LspFuture<PathBuf> {
        Box::pin(async move {
            tokio::fs::canonicalize(&path)
                .await
                .map_err(|e| source_io_error("realpath", &path, e))
        })
    }
    fn read_file(&self, path: PathBuf, _: Cancellation) -> LspFuture<Vec<u8>> {
        Box::pin(async move {
            tokio::fs::read(path)
                .await
                .map_err(|e| malformed(e.to_string()))
        })
    }
    fn prepare_subprocess(
        &self,
        mut command: LspCommand,
        _: Cancellation,
    ) -> LspFuture<LspCommand> {
        let root = self.root.clone();
        Box::pin(async move {
            assert_eq!(command.cwd, root);
            command.workspace_uri = Some("file:///workspace".into());
            command.source_uri = Some("file:///workspace/source.ts".into());
            Ok(command)
        })
    }
}
#[tokio::test]
async fn bound_stdio_maps_guest_locations_and_rejects_foreign_workspace_and_symlink() {
    let fixture = Fixture::new();
    let provider = StdioLspProvider::new(fixture.config("normal")).unwrap();
    let bound = provider
        .bind_session(LspSessionExecution {
            workspace_root: fixture.root.clone(),
            io: Arc::new(Execution {
                root: fixture.root.clone(),
            }),
        })
        .unwrap();
    let result = bound
        .query(
            fixture.request(LspOperation::Definition),
            Cancellation::new(),
        )
        .await
        .unwrap();
    let LspQueryResult::Locations {
        locations,
        resolved_workspace_uri,
    } = result
    else {
        panic!("locations required")
    };
    assert_eq!(
        locations[0].uri,
        uri(&fixture.root.join("source.ts")).unwrap()
    );
    assert_eq!(resolved_workspace_uri, uri(&fixture.root).unwrap());
    let mut request = fixture.request(LspOperation::Hover);
    request.request.workspace_root = fixture.root.join("foreign");
    assert_eq!(
        bound
            .query(request, Cancellation::new())
            .await
            .unwrap_err()
            .code,
        "LSP_WORKSPACE_REQUIRED"
    );
    #[cfg(unix)]
    {
        let outside = Fixture::new();
        std::os::unix::fs::symlink(
            outside.root.join("source.ts"),
            fixture.root.join("outside.ts"),
        )
        .unwrap();
        let mut request = fixture.request(LspOperation::Hover);
        request.request.file_path = "outside.ts".into();
        assert!(
            bound
                .query(request, Cancellation::new())
                .await
                .unwrap_err()
                .message
                .contains("outside its session")
        );
    }
    bound.dispose().await.unwrap();
    provider.dispose().await.unwrap();
    #[cfg(unix)]
    fixture.assert_reaped();
}
#[test]
fn stdio_server_requests_and_normalizers_preserve_baseline_shapes() {
    assert_eq!(
        server_reply("workspace/applyEdit", None, "file:///w", "w", None),
        json!({"applied":false,"failureReason":"LSP provider does not apply workspace edits."})
    );
    assert_eq!(
        server_reply("workspace/workspaceFolders", None, "file:///w", "w", None),
        json!([{"uri":"file:///w","name":"w"}])
    );
    assert_eq!(
        server_reply("unknown", None, "file:///w", "w", None),
        Value::Null
    );
    assert!(normalize(LspOperation::Definition,json!({"uri":"file:///w/a","range":{"start":{"line":-1,"character":0},"end":{"line":0,"character":1}}}),"file:///w").is_err());
    assert!(
        remap_locations(
            LspQueryResult::Locations {
                locations: vec![LspLocation {
                    uri: "file:///other/a".into(),
                    range: LspRange {
                        start: LspPosition {
                            line: 0.0,
                            character: 0.0
                        },
                        end: LspPosition {
                            line: 0.0,
                            character: 1.0
                        }
                    }
                }],
                resolved_workspace_uri: "file:///workspace".into()
            },
            Path::new("/host"),
            "file:///workspace"
        )
        .is_err()
    );
}

#[tokio::test]
async fn bound_stdio_dispose_cannot_cancel_another_bound_session() {
    let fixture = Fixture::new();
    let provider = StdioLspProvider::new(fixture.config("stall-query")).unwrap();
    let execution = LspSessionExecution {
        workspace_root: fixture.root.clone(),
        io: Arc::new(Execution {
            root: fixture.root.clone(),
        }),
    };
    let first = provider.bind_session(execution.clone()).unwrap();
    let second = provider.bind_session(execution).unwrap();
    let one = tokio::spawn(first.query(fixture.request(LspOperation::Hover), Cancellation::new()));
    fixture.wait_method("textDocument/hover").await;
    let cancel = Cancellation::new();
    let two = tokio::spawn(second.query(fixture.request(LspOperation::Hover), cancel.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fixture
                .wire()
                .iter()
                .filter(|v| v.get("method") == Some(&json!("textDocument/hover")))
                .count()
                == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    first.dispose().await.unwrap();
    assert_eq!(one.await.unwrap().unwrap_err().code, "ABORT_ERR");
    assert!(
        !two.is_finished(),
        "first session disposal must not settle second session query"
    );
    cancel.cancel();
    assert_eq!(two.await.unwrap().unwrap_err().code, "ABORT_ERR");
    second.dispose().await.unwrap();
    provider.dispose().await.unwrap();
    #[cfg(unix)]
    fixture.assert_reaped();
}

#[tokio::test]
async fn unpolled_stdio_query_is_owned_and_pre_cancelled_without_spawning() {
    let fixture = Fixture::new();
    let provider = StdioLspProvider::new(fixture.config("normal")).unwrap();
    let query = provider.query(fixture.request(LspOperation::Hover), Cancellation::new());
    drop(query);
    provider.dispose().await.unwrap();
    assert!(
        fixture.wire().is_empty(),
        "unpolled canceled query cannot launch a server"
    );
}

/// Fixed TS actual wire keeps the source workspace name even when its
/// prepareSubprocess returns another cwd. TS ignores that cwd (recorded bug);
/// Rust executes the complete prepared command and preserves the source name.
#[tokio::test]
async fn bound_stdio_executes_prepared_command_but_keeps_source_workspace_name() {
    struct PreparedExecution {
        root: PathBuf,
        cwd: PathBuf,
        args: Vec<String>,
    }
    impl LspExecutionPort for PreparedExecution {
        fn stat(&self, path: PathBuf, cancellation: Cancellation) -> LspFuture<LspFileStat> {
            Execution {
                root: self.root.clone(),
            }
            .stat(path, cancellation)
        }
        fn realpath(&self, path: PathBuf, cancellation: Cancellation) -> LspFuture<PathBuf> {
            Execution {
                root: self.root.clone(),
            }
            .realpath(path, cancellation)
        }
        fn read_file(&self, path: PathBuf, cancellation: Cancellation) -> LspFuture<Vec<u8>> {
            Execution {
                root: self.root.clone(),
            }
            .read_file(path, cancellation)
        }
        fn prepare_subprocess(
            &self,
            mut command: LspCommand,
            _: Cancellation,
        ) -> LspFuture<LspCommand> {
            assert_eq!(command.command, "original-command-must-not-run");
            assert_eq!(command.args, ["original-args-must-not-run"]);
            assert_eq!(command.cwd, self.root);
            command.command = "python3".into();
            command.args = self.args.clone();
            command.cwd = self.cwd.clone();
            command
                .env
                .insert("LSP_PREPARED_CANARY".into(), "only-prepared-env".into());
            command.workspace_uri = Some("file:///workspace".into());
            command.source_uri = Some("file:///workspace/source.ts".into());
            Box::pin(async move { Ok(command) })
        }
    }
    let fixture = Fixture::new();
    let cwd = fixture.root.join("changed-process-cwd");
    std::fs::create_dir(&cwd).unwrap();
    let name = fixture
        .root
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut config = fixture.config("workspace-name");
    let mut prepared_args = config.args.clone();
    prepared_args.extend(["prepared-extra-argument".into(), name.clone()]);
    config.command = "original-command-must-not-run".into();
    config.args = vec!["original-args-must-not-run".into()];
    config.env.insert(
        "LSP_PREPARED_CANARY".into(),
        "original-env-must-not-win".into(),
    );
    let provider = StdioLspProvider::new(config).unwrap();
    let bound = provider
        .bind_session(LspSessionExecution {
            workspace_root: fixture.root.clone(),
            io: Arc::new(PreparedExecution {
                root: fixture.root.clone(),
                cwd: cwd.clone(),
                args: prepared_args.clone(),
            }),
        })
        .unwrap();
    let result = bound
        .query(
            fixture.request(LspOperation::Definition),
            Cancellation::new(),
        )
        .await
        .unwrap();
    let LspQueryResult::Locations {
        locations,
        resolved_workspace_uri,
    } = result
    else {
        panic!("locations required");
    };
    assert_eq!(resolved_workspace_uri, uri(&fixture.root).unwrap());
    assert_eq!(
        locations[0].uri,
        uri(&fixture.root.join("source.ts")).unwrap()
    );
    let wire = fixture.wire();
    let child = &wire[0];
    assert_eq!(child["cwd"], cwd.to_string_lossy().as_ref());
    assert_eq!(child["preparedEnv"], "only-prepared-env");
    assert!(!child["executable"].as_str().unwrap().is_empty());
    assert_eq!(child["argv"], json!(prepared_args[1..]));
    let initialize = wire
        .iter()
        .find(|row| row["method"] == "initialize")
        .unwrap();
    assert_eq!(initialize["params"]["rootUri"], "file:///workspace");
    assert_eq!(
        initialize["params"]["workspaceFolders"],
        json!([{"uri":"file:///workspace","name":name}])
    );
    let folders = wire
        .iter()
        .find(|row| row["id"] == 100 && row.get("result").is_some())
        .unwrap();
    assert_eq!(folders["result"], initialize["params"]["workspaceFolders"]);
    let did_open = wire
        .iter()
        .find(|row| row["method"] == "textDocument/didOpen")
        .unwrap();
    assert_eq!(
        did_open["params"]["textDocument"]["uri"],
        "file:///workspace/source.ts"
    );
    assert_eq!(
        did_open["params"]["textDocument"]["text"],
        "const 😀answer = 42;\n"
    );
    assert!(
        wire.iter()
            .any(|row| row["method"] == "textDocument/didClose")
    );
    assert!(wire.iter().any(|row| row["method"] == "shutdown"));
    assert!(wire.iter().any(|row| row["method"] == "exit"));
    bound.dispose().await.unwrap();
    provider.dispose().await.unwrap();
    #[cfg(unix)]
    fixture.assert_reaped();
}

/// Fixed TS actual bound provider maps workspaceRoot "." and its absolute
/// source to identical host results. Use a cwd-relative owned child here so
/// parallel tests never change the process-global current directory.
#[tokio::test]
async fn bound_stdio_relative_workspace_maps_like_absolute_source() {
    let cwd = std::env::current_dir().unwrap();
    let relative = PathBuf::from(format!(".pdrs-lsp-relative-{}", uuid::Uuid::new_v4()));
    let root = cwd.join(&relative);
    std::fs::create_dir(&root).unwrap();
    let fixture = Fixture {
        root: std::fs::canonicalize(root).unwrap(),
    };
    std::fs::write(fixture.root.join("source.ts"), "const 😀answer = 42;\n").unwrap();
    std::fs::write(fixture.root.join("server.py"), SERVER).unwrap();
    let provider = StdioLspProvider::new(fixture.config("normal")).unwrap();
    let bound = provider
        .bind_session(LspSessionExecution {
            workspace_root: fixture.root.clone(),
            io: Arc::new(Execution {
                root: fixture.root.clone(),
            }),
        })
        .unwrap();
    let absolute_result = bound
        .query(
            fixture.request(LspOperation::Definition),
            Cancellation::new(),
        )
        .await
        .unwrap();
    let mut relative_request = fixture.request(LspOperation::Definition);
    relative_request.request.workspace_root = relative;
    let relative_result = bound
        .query(relative_request, Cancellation::new())
        .await
        .unwrap();
    assert_eq!(relative_result, absolute_result);
    let LspQueryResult::Locations {
        locations,
        resolved_workspace_uri,
    } = relative_result
    else {
        panic!("locations required");
    };
    assert_eq!(resolved_workspace_uri, uri(&fixture.root).unwrap());
    assert_eq!(
        locations[0].uri,
        uri(&fixture.root.join("source.ts")).unwrap()
    );
    #[cfg(unix)]
    {
        let alias = fixture.root.join("owned-root-alias");
        std::os::unix::fs::symlink(&fixture.root, &alias).unwrap();
        let aliased = provider
            .bind_session(LspSessionExecution {
                workspace_root: alias.clone(),
                io: Arc::new(Execution {
                    root: alias.clone(),
                }),
            })
            .unwrap();
        let mut request = fixture.request(LspOperation::Definition);
        request.request.workspace_root = alias.clone();
        let result = aliased.query(request, Cancellation::new()).await.unwrap();
        let LspQueryResult::Locations {
            locations,
            resolved_workspace_uri,
        } = result
        else {
            panic!("alias locations required");
        };
        assert_eq!(resolved_workspace_uri, uri(&alias).unwrap());
        assert_eq!(locations[0].uri, uri(&alias.join("source.ts")).unwrap());
        aliased.dispose().await.unwrap();
    }
    let opened = fixture
        .wire()
        .iter()
        .filter(|row| row["method"] == "textDocument/didOpen")
        .count();
    #[cfg(unix)]
    assert_eq!(opened, 3);
    #[cfg(not(unix))]
    assert_eq!(opened, 2);
    bound.dispose().await.unwrap();
    provider.dispose().await.unwrap();
    #[cfg(unix)]
    fixture.assert_reaped();
}

/// Four actual fixed TS observations: missing workspace always fails stat
/// first; an existing workspace with a missing source fails stat unbound,
/// realpath bound. All errors occur before prepare or any server spawn.
#[tokio::test]
async fn stdio_missing_source_paths_preserve_actual_node_error_phase() {
    let fixture = Fixture::new();
    for bound in [false, true] {
        for missing_workspace in [false, true] {
            let workspace = if missing_workspace {
                fixture.root.join("missing-workspace")
            } else {
                fixture.root.clone()
            };
            let provider = StdioLspProvider::new(fixture.config("normal")).unwrap();
            let selected: Arc<dyn LspProvider> = if bound {
                provider
                    .bind_session(LspSessionExecution {
                        workspace_root: workspace.clone(),
                        io: Arc::new(Execution {
                            root: workspace.clone(),
                        }),
                    })
                    .unwrap()
            } else {
                provider.clone()
            };
            let mut request = fixture.request(LspOperation::Hover);
            request.request.workspace_root = workspace.clone();
            request.request.file_path = "missing-source.ts".into();
            let error = selected
                .query(request, Cancellation::new())
                .await
                .unwrap_err();
            let syscall = if bound && !missing_workspace {
                "realpath"
            } else {
                "stat"
            };
            let target = if missing_workspace {
                workspace
            } else {
                workspace.join("missing-source.ts")
            };
            assert_eq!(error.code, "ENOENT");
            assert_eq!(
                error.message,
                format!(
                    "ENOENT: no such file or directory, {syscall} '{}'",
                    target.display()
                )
            );
            assert!(
                fixture.wire().is_empty(),
                "source failure must not launch LSP"
            );
            selected.dispose().await.unwrap();
            provider.dispose().await.unwrap();
        }
    }
}

/// Exercise the exact waiter constructor used by query, without a server:
/// full-owner capture must survive construction, cancel on unpolled Drop,
/// disarm after a completed reply, and stay armed if the reply owner is lost.
#[tokio::test]
async fn stdio_query_waiter_moves_the_whole_drop_owner() {
    let dropped = Cancellation::new();
    let (reply, receive) = oneshot::channel();
    let waiter = CancelWaiter {
        stop: dropped.clone(),
        armed: true,
    }
    .wait(receive);
    assert!(
        !dropped.is_cancelled(),
        "constructing the future must retain its guard"
    );
    drop(waiter);
    assert!(
        dropped.is_cancelled(),
        "dropping an unpolled future cancels its query"
    );
    assert!(
        reply
            .send(Ok(LspQueryResult::Hover { hover: None }))
            .is_err()
    );

    let finished = Cancellation::new();
    let (reply, receive) = oneshot::channel();
    let waiter = CancelWaiter {
        stop: finished.clone(),
        armed: true,
    }
    .wait(receive);
    assert!(!finished.is_cancelled());
    reply
        .send(Ok(LspQueryResult::Hover { hover: None }))
        .unwrap();
    assert_eq!(waiter.await.unwrap(), LspQueryResult::Hover { hover: None });
    assert!(
        !finished.is_cancelled(),
        "normal reply disarms the whole guard before Drop"
    );

    let lost = Cancellation::new();
    let (reply, receive) = oneshot::channel();
    let waiter = CancelWaiter {
        stop: lost.clone(),
        armed: true,
    }
    .wait(receive);
    drop(reply);
    assert_eq!(waiter.await.unwrap_err().code, "LSP_DISPOSED");
    assert!(
        lost.is_cancelled(),
        "missing reply keeps cancellation armed"
    );
}
