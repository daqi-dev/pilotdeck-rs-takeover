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
