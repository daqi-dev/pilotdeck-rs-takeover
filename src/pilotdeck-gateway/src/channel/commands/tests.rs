use super::*;
use std::sync::Mutex;

#[derive(Default)]
struct Fixture {
    replies: Mutex<Vec<JsString>>,
    calls: Mutex<Vec<String>>,
    project: Mutex<Option<JsString>>,
}
impl CommandContext for Fixture {
    fn reply<'a>(&'a self, text: &'a JsString) -> CommandFuture<'a, ()> {
        Box::pin(async move {
            self.replies.lock().unwrap().push(text.clone());
            Ok(())
        })
    }
    fn current_project(&self) -> Option<JsString> {
        self.project.lock().unwrap().clone()
    }
    fn bind_project(&self, project: &JsString) -> Result<(), CommandError> {
        self.calls.lock().unwrap().push("bind".into());
        *self.project.lock().unwrap() = Some(project.clone());
        Ok(())
    }
    fn reset_session(&self) -> Result<(), CommandError> {
        self.calls.lock().unwrap().push("reset".into());
        Ok(())
    }
}
impl CommandHost for Fixture {
    fn list_projects(&self) -> CommandFuture<'_, Vec<Project>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push("projects".into());
            Ok(vec![
                Project {
                    name: "Alpha".into(),
                    project_key: "one".into(),
                    full_path: "/one".into(),
                },
                Project {
                    name: "alpha".into(),
                    project_key: "two".into(),
                    full_path: "/two".into(),
                },
            ])
        })
    }
    fn run_update(&self, check: bool) -> CommandFuture<'_, UpdateResult> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("update:{check}"));
            Ok(UpdateResult {
                code: Some(0),
                output: "build\r\nfinished\r\n\r\n".into(),
            })
        })
    }
    fn status(&self) -> CommandFuture<'_, Status> {
        Box::pin(async {
            Ok(Status {
                branch: " main\n".into(),
                commit: " abc\n".into(),
                uptime_seconds: 3660,
                node: "v22".into(),
                platform: "linux".into(),
            })
        })
    }
    fn search<'a>(
        &'a self,
        raw: &'a JsString,
        project: Option<&'a JsString>,
    ) -> CommandFuture<'a, Option<JsString>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(format!("search:{raw:?}:{project:?}"));
            Ok(Some("搜索结果".into()))
        })
    }
}
#[tokio::test]
async fn resolve_forward_and_exact_space_rules() {
    let f = Fixture::default();
    for input in ["hello", "/unknown", "/new", "/plan test", "/projects\tfoo"] {
        assert!(!execute(&input.into(), &f, &f).await.unwrap(), "{input}");
    }
    assert!(
        execute(&"  /项目列表 unused  ".into(), &f, &f)
            .await
            .unwrap()
    );
    assert_eq!(
        f.replies.lock().unwrap().as_slice(),
        &[JsString::from(
            "📂 项目列表：\n\n• Alpha\n• alpha\n\n发送 /switch-project <项目名> 切换 WorkSpace"
        )]
    );
}
#[tokio::test]
async fn switch_exact_before_case_fold_and_bind_before_reset() {
    let f = Fixture::default();
    assert!(execute(&"/切换项目 alpha".into(), &f, &f).await.unwrap());
    assert_eq!(*f.project.lock().unwrap(), Some("two".into()));
    assert_eq!(*f.calls.lock().unwrap(), ["projects", "bind", "reset"]);
    assert_eq!(
        f.replies.lock().unwrap()[0],
        JsString::from("已切换到项目：alpha\n路径：/two\n（已自动创建新会话）")
    );
}
#[tokio::test]
async fn update_status_and_search_use_host_and_preserve_reply_order() {
    let f = Fixture::default();
    *f.project.lock().unwrap() = Some("one".into());
    for input in ["/update check", "/status", "/search docker --all"] {
        assert!(execute(&input.into(), &f, &f).await.unwrap());
    }
    let r = f.replies.lock().unwrap();
    assert_eq!(r[0], JsString::from("⏳ 正在检查 Release 更新..."));
    assert_eq!(r[1], JsString::from("✅ finished"));
    assert_eq!(
        r[2],
        JsString::from(
            "📊 PilotDeck Status\n\n分支: main\n提交: abc\n运行时间: 1h 1m\nNode: v22\n平台: linux"
        )
    );
    assert_eq!(r[3], JsString::from("搜索结果"));
    assert!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|x| x.starts_with("search:"))
    );
}
#[tokio::test]
async fn help_and_unknown_project_are_complete() {
    let f = Fixture::default();
    execute(&"/help".into(), &f, &f).await.unwrap();
    execute(&"/switch-project missing".into(), &f, &f)
        .await
        .unwrap();
    let r = f.replies.lock().unwrap();
    assert_eq!(r[0], JsString::from(HELP));
    assert_eq!(
        r[1],
        JsString::from("未找到匹配「missing」的项目。\n\n发送 /projects 查看可用项目。")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn native_status_and_optional_updater_use_real_owned_processes() {
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, process::CommandExt},
        path::PathBuf,
        process::Command,
        time::{Duration, Instant},
    };
    const CHILD: &str = "PDRS_H1_COMMAND_HOST_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let host = host::NativeCommandsHost::new(
            root.join("pilot"),
            root.clone(),
            Some(root.join("bin/pilotdeck")),
            Some(Instant::now() - Duration::from_secs(3660)),
        );
        let status = host.status().await.unwrap();
        assert_eq!(status.branch, JsString::from("mock-branch\n"));
        assert_eq!(status.commit, JsString::from("abc123 mock commit\n"));
        assert!(status.uptime_seconds >= 3660 && status.uptime_seconds < 3670);
        assert_eq!(status.node, JsString::from("not used (native Rust)"));
        assert_eq!(
            status.platform,
            JsString::from(if cfg!(target_os = "macos") {
                "darwin"
            } else {
                std::env::consts::OS
            })
        );
        assert!(
            !root.join("node-called").exists(),
            "status must not execute Node"
        );
        let git_calls = fs::read_to_string(root.join("git-calls")).unwrap();
        assert_eq!(
            git_calls,
            format!(
                "{}|branch --show-current\n{}|log --oneline -1 HEAD\n",
                root.join("bin").display(),
                root.join("bin").display()
            )
        );
        let check = host.run_update(true).await.unwrap();
        assert_eq!(check.code, Some(2));
        assert_eq!(check.output, JsString::from("already latest"));
        let update = host.run_update(false).await.unwrap();
        assert_eq!(update.code, Some(0));
        assert_eq!(update.output, JsString::from("finished"));
        assert!(root.join("node-called").exists());
        let node_calls = fs::read_to_string(root.join("node-calls")).unwrap();
        assert_eq!(
            node_calls,
            format!(
                "{}|{} --check\n{}|{} --restart\n",
                root.display(),
                root.join("scripts/update-web.mjs").display(),
                root.display(),
                root.join("scripts/update-web.mjs").display()
            )
        );
        fs::write(root.join("fail-update"), "").unwrap();
        let failed = host.run_update(false).await.unwrap();
        assert_eq!(failed.code, Some(1));
        assert_eq!(failed.output, JsString::from("failed"));
        let missing = host::NativeCommandsHost::new(
            root.join("pilot"),
            root.clone(),
            Some(root.with_extension("missing").join("pilotdeck")),
            Some(Instant::now()),
        );
        assert_eq!(
            missing.run_update(false).await.unwrap_err().0,
            JsString::from(
                "This installation does not include the Git Web updater. Use application settings or update manually."
            )
        );
        // Keep a bystander in a distinct group and observe the updater's
        // leader + child before dropping its receipt. Only owned PIDs may die.
        struct Bystander(std::process::Child);
        impl Drop for Bystander {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut other = Command::new("/bin/sleep");
        other.arg("300").process_group(0);
        let mut other = Bystander(other.spawn().unwrap());
        fs::write(root.join("hold-update"), "").unwrap();
        let update_host = host::NativeCommandsHost::new(
            root.join("pilot"),
            root.clone(),
            Some(root.join("bin/pilotdeck")),
            Some(Instant::now()),
        );
        let mut operation = update_host.update_operation(false).unwrap().unwrap();
        let group = |pid: &str| {
            let out = Command::new("/bin/ps")
                .args(["-o", "pgid=", "-p", pid.trim()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let birth = |pid: &str| {
            let out = Command::new("/bin/ps")
                .args(["-o", "lstart=", "-p", pid.trim()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let (leader, child, _leader_birth, _child_birth) =
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let (Ok(leader), Ok(child)) = (
                        fs::read_to_string(root.join("updater-leader")),
                        fs::read_to_string(root.join("updater-child")),
                    ) && leader.trim().parse::<u32>().is_ok_and(|pid| pid > 0)
                        && child.trim().parse::<u32>().is_ok_and(|pid| pid > 0)
                        && group(&leader) == leader.trim()
                        && group(&child) == leader.trim()
                    {
                        let leader_birth = birth(&leader);
                        let child_birth = birth(&child);
                        if !leader_birth.is_empty() && !child_birth.is_empty() {
                            break (leader, child, leader_birth, child_birth);
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("updater descendants start with established identity");
        assert_ne!(group(&other.0.id().to_string()), leader.trim());
        let other_birth = birth(&other.0.id().to_string());
        assert!(!other_birth.is_empty());
        // Dropping a pending waiter must not discard the repeatable receipt.
        let mut first_wait = Box::pin(operation.cancel_and_drain());
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let _ = std::future::Future::poll(first_wait.as_mut(), &mut context);
        drop(first_wait);
        operation.cancel_and_drain().await.unwrap();
        operation.cancel_and_drain().await.unwrap();
        assert!(group(&leader).is_empty(), "receipt must follow leader reap");
        let child_state = Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", child.trim()])
            .output()
            .unwrap();
        let child_state = String::from_utf8_lossy(&child_state.stdout);
        assert!(
            child_state.trim().is_empty() || child_state.trim().starts_with('Z'),
            "receipt must follow owned child termination"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = |pid: &str| {
                    let out = Command::new("/bin/ps")
                        .args(["-o", "stat=", "-p", pid.trim()])
                        .output()
                        .unwrap();
                    String::from_utf8_lossy(&out.stdout).trim().to_owned()
                };
                let leader_state = state(&leader);
                let child_state = state(&child);
                // The leader is our direct child and must be reaped. An
                // orphaned grandchild may transiently remain an init-owned
                // zombie, but it must no longer be running.
                if leader_state.is_empty()
                    && (child_state.is_empty() || child_state.starts_with('Z'))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled updater group stops");
        assert!(
            other.0.try_wait().unwrap().is_none(),
            "unrelated process group survives"
        );
        assert_eq!(
            birth(&other.0.id().to_string()),
            other_birth,
            "bystander is the same process"
        );
        drop(other);
        fs::write(root.join("hang-git"), "").unwrap();
        let pending = tokio::spawn(async move { host.status().await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !root.join("git-pid").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("git starts");
        let pid = fs::read_to_string(root.join("git-pid")).unwrap();
        pending.abort();
        let _ = pending.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let alive = Command::new("/bin/kill")
                    .args(["-0", pid.trim()])
                    .status()
                    .unwrap()
                    .success();
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropped status kills and reaps owned git");
        return;
    }
    let root = std::env::temp_dir().join(format!(
        "h1-commands-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    for dir in ["bin", "scripts", "home", "pilot"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let root = fs::canonicalize(root).unwrap();
    fs::write(
        root.join("scripts/update-web.mjs"),
        "// not executed by fake node\n",
    )
    .unwrap();
    let git = root.join("bin/git");
    fs::write(&git,"#!/bin/sh\nprintf '%s|%s\\n' \"$PWD\" \"$*\" >> \"$PDRS_H1_COMMAND_HOST_CHILD/git-calls\"\nif [ -f \"$PDRS_H1_COMMAND_HOST_CHILD/hang-git\" ]; then printf '%s' \"$$\" > \"$PDRS_H1_COMMAND_HOST_CHILD/git-pid\"; exec /bin/sleep 30; fi\ncase \"$1 $2\" in\n 'branch --show-current') printf 'mock-branch\\n';;\n 'log --oneline') printf 'abc123 mock commit\\n';;\n *) exit 3;;\nesac\n").unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
    let node = root.join("bin/node");
    fs::write(&node,"#!/bin/sh\nprintf '%s|%s\\n' \"$PWD\" \"$*\" >> \"$PDRS_H1_COMMAND_HOST_CHILD/node-calls\"\nprintf called > \"$PDRS_H1_COMMAND_HOST_CHILD/node-called\"\nif [ -f \"$PDRS_H1_COMMAND_HOST_CHILD/hold-update\" ]; then printf '%s' \"$$\" > \"$PDRS_H1_COMMAND_HOST_CHILD/updater-leader\"; /bin/sleep 300 & printf '%s' \"$!\" > \"$PDRS_H1_COMMAND_HOST_CHILD/updater-child\"; wait; exit 0; fi\nif [ -f \"$PDRS_H1_COMMAND_HOST_CHILD/fail-update\" ]; then printf 'failed\\n' >&2; exit 1; fi\ncase \"$2\" in --check) printf 'already latest\\n'; exit 2;; --restart) printf 'finished\\n'; exit 0;; *) exit 3;; esac\n").unwrap();
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
    let exact = format!(
        "{}::native_status_and_optional_updater_use_real_owned_processes",
        module_path!().split_once("::").unwrap().1
    );
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture"])
        .env(CHILD, &root)
        .env("HOME", root.join("home"))
        .env("PILOT_HOME", root.join("pilot"))
        .env("PATH", root.join("bin"))
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&root);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn native_search_reads_real_history_and_honours_scope_and_role() {
    use std::{fs, time::Instant};
    let root = std::env::temp_dir().join(format!(
        "h1-search-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let chats = root.join("pilot/projects/example/chats");
    fs::create_dir_all(&chats).unwrap();
    fs::write(chats.join("s.jsonl"),"{\"type\":\"accepted_input\",\"sessionId\":\"s\",\"messages\":[{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"needle hello\"}]}]}\n").unwrap();
    let host =
        host::NativeCommandsHost::new(root.join("pilot"), root.clone(), None, Some(Instant::now()));
    let result = host
        .search(&"needle --all --role user".into(), None)
        .await
        .unwrap()
        .unwrap();
    let text = result.to_utf8().unwrap();
    assert!(text.contains("找到 1 条匹配「needle」"), "{text}");
    assert!(text.contains("needle hello"), "{text}");
    assert!(text.contains(" · example"), "{text}");
    // JS strings and whitespace are UTF-16, including an unpaired surrogate
    // and BOM; neither can be normalized through Rust's lossy UTF-8 path.
    fs::write(chats.join("surrogate.jsonl"), "{\"type\":\"accepted_input\",\"sessionId\":\"u\",\"createdAt\":\"\\ud800\",\"messages\":[{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"\\ud800\"}]}]}\n").unwrap();
    let mut raw = vec![0xd800, 0xfeff];
    raw.extend("--all --limit 0x1".encode_utf16());
    let lone = host
        .search(&JsString::from_utf16(raw), None)
        .await
        .unwrap()
        .unwrap();
    assert!(
        lone.units().iter().filter(|unit| **unit == 0xd800).count() >= 3,
        "query, snippet and invalid createdAt retain the unit"
    );
    assert!(
        lone.units().contains(&0xd800),
        "search result must preserve the original unit"
    );
    let mut path_units = root
        .join("project-")
        .to_string_lossy()
        .encode_utf16()
        .collect::<Vec<_>>();
    path_units.push(0xd800);
    let bad_project = JsString::from_utf16(path_units);
    let replaced = String::from_utf16_lossy(bad_project.units());
    let scoped = crate::session_chat_dir(&root.join("pilot"), &root, &replaced);
    fs::create_dir_all(&scoped).unwrap();
    fs::copy(chats.join("s.jsonl"), scoped.join("scoped.jsonl")).unwrap();
    // TS compares the original UTF-16 identity to marker text before the OS
    // converts a lone surrogate to U+FFFD. A marker for the replacement path
    // must not capture the distinct, unrepresentable JS identity.
    fs::create_dir_all(&replaced).unwrap();
    let marker = root.join("pilot/projects/collision");
    fs::create_dir_all(marker.join("chats")).unwrap();
    fs::write(marker.join(".cwd"), &replaced).unwrap();
    fs::write(marker.join("chats/wrong.jsonl"),"{\"type\":\"accepted_input\",\"sessionId\":\"wrong\",\"messages\":[{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"needle marker-only\"}]}]}\n").unwrap();
    let scoped_result = host
        .search(&"needle --role user".into(), Some(&bad_project))
        .await
        .unwrap()
        .unwrap();
    assert!(
        scoped_result.to_utf8().unwrap().contains("needle hello"),
        "Node path encoding replacement must still find scoped history"
    );
    assert!(
        !scoped_result.to_utf8().unwrap().contains("marker-only"),
        "replacement marker belongs to another JS identity"
    );
    // Resolve JS path segments before deciding whether a marker identity can
    // be represented as UTF-8: the surrogate component is cancelled by `..`.
    let normalized_dir = root.join("normalized-project");
    fs::create_dir_all(&normalized_dir).unwrap();
    let normalized_marker = root.join("pilot/projects/normalized-marker");
    fs::create_dir_all(normalized_marker.join("chats")).unwrap();
    fs::write(
        normalized_marker.join(".cwd"),
        normalized_dir.to_str().unwrap(),
    )
    .unwrap();
    fs::write(normalized_marker.join("chats/fold.jsonl"), "{\"type\":\"accepted_input\",\"sessionId\":\"fold\",\"messages\":[{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"fold-needle\"}]}]}\n").unwrap();
    let mut folded_units = root
        .join("discard-")
        .to_string_lossy()
        .encode_utf16()
        .collect::<Vec<_>>();
    folded_units.push(0xd800);
    folded_units.extend("/../normalized-project".encode_utf16());
    let folded_project = JsString::from_utf16(folded_units);
    let folded_result = host
        .search(&"fold-needle --role user".into(), Some(&folded_project))
        .await
        .unwrap()
        .unwrap();
    assert!(
        folded_result.to_utf8().unwrap().contains("fold-needle"),
        "path normalization must precede marker identity comparison"
    );
    let relative = JsString::from("relative-project");
    let rel_dir = crate::session_chat_dir(&root.join("pilot"), &root, "relative-project");
    fs::create_dir_all(&rel_dir).unwrap();
    fs::copy(chats.join("s.jsonl"), rel_dir.join("rel.jsonl")).unwrap();
    let raw = search::search(search::SearchInput {
        home: &root.join("pilot"),
        cwd: &root,
        project: Some(&relative),
        session: None,
        query: &"needle".into(),
        limit: 20.0,
        regex: false,
        sensitive: false,
        role: "all",
    })
    .unwrap();
    assert_eq!(
        raw.matches[0].project_key, "relative-project",
        "projectKey keeps caller spelling"
    );
    let no_match = host
        .search(&"needle --all --role assistant".into(), None)
        .await
        .unwrap()
        .unwrap();
    assert!(no_match.to_utf8().unwrap().contains("未找到包含「needle」"));
    assert!(host.search(&"[ --all --regex".into(), None).await.is_err());
    let mut invalid = vec![b'[' as u16, 0xd800];
    invalid.extend(" --all --regex".encode_utf16());
    let error = host
        .search(&JsString::from_utf16(invalid), None)
        .await
        .unwrap_err();
    let mut expected = "Invalid regular expression: /["
        .encode_utf16()
        .collect::<Vec<_>>();
    expected.push(0xd800);
    expected.extend("/i: Unterminated character class".encode_utf16());
    assert_eq!(error.0.units(), expected.as_slice());
    fs::remove_dir_all(root).unwrap();
}
