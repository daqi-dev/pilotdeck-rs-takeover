//! One native gateway's SDK task-budget owner. The existing TaskBudgetStore
//! owns all persistence and run-id idempotence. Cached admission snapshots are
//! ephemeral turn options, never a second accounting ledger.
use super::{Options, SessionOverrides};
use crate::{
    runtime::{
        sdk_session::SdkTaskBudget,
        session::TurnCompletionHandle,
        turn::{
            admission::SubmitTaskBudgetSnapshot,
            submit::{SubmitError, SubmitFuture},
        },
    },
    storage::{TaskBudgetError, TaskBudgetScope, TaskBudgetStore},
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;
#[cfg(test)]
thread_local! {
    // A per-thread one-shot witness after the early cancellation check and
    // before blocking on the accounting owner. Other tests cannot observe it.
    static CONFIGURE_BEFORE_LOCK: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

#[derive(Clone)]
struct Snapshot {
    project: JsString,
    scope: TaskBudgetScope,
    total: f64,
    spent: f64,
}
struct Captured {
    budget: Snapshot,
    completion: TurnCompletionHandle,
}
struct State {
    store: TaskBudgetStore,
    write_failure: Option<String>,
    runs: HashMap<(JsString, JsString), Captured>,
}
pub(super) struct NativeTaskBudget {
    options: Options,
    overrides: Arc<SessionOverrides>,
    now: Clock,
    state: Mutex<State>,
}
fn error(error: TaskBudgetError) -> SubmitError {
    SubmitError::new(
        error.code().unwrap_or("TASK_BUDGET_UNAVAILABLE"),
        error.to_string(),
    )
}
fn scope(value: &JsValue) -> TaskBudgetScope {
    if value
        .get("scope")
        .and_then(JsValue::as_string)
        .is_some_and(|v| v.equals_str("project"))
    {
        TaskBudgetScope::Project
    } else {
        TaskBudgetScope::Session
    }
}
fn snapshot_value(snapshot: Option<&Snapshot>) -> JsValue {
    let Some(snapshot) = snapshot else {
        return JsValue::Undefined;
    };
    let mut value = JsObject::new();
    value.insert("totalUsd".into(), JsValue::Number(snapshot.total));
    value.insert("spentUsd".into(), JsValue::Number(snapshot.spent));
    JsValue::Object(value)
}
impl NativeTaskBudget {
    pub(super) fn new(
        options: Options,
        overrides: Arc<SessionOverrides>,
        env: &crate::config::Env,
    ) -> Self {
        let compact = env
            .get("PILOTDECK_SDK_TASK_BUDGET_LEDGER_COMPACT_AFTER_RECORDS")
            .map(|v| crate::runtime::mcp::client::parse_int(v))
            .filter(|v| {
                v.is_finite() && *v > 0.0 && v.fract() == 0.0 && *v <= 9_007_199_254_740_991.0
            })
            .map(|v| v as usize);
        Self::with_clock(
            options,
            overrides,
            compact,
            Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64()
                    * 1000.0
            }),
        )
    }
    fn with_clock(
        options: Options,
        overrides: Arc<SessionOverrides>,
        compact: Option<usize>,
        now: Clock,
    ) -> Self {
        let store = TaskBudgetStore::new(&options.home, compact);
        Self {
            options,
            overrides,
            now,
            state: Mutex::new(State {
                store,
                write_failure: None,
                runs: HashMap::new(),
            }),
        }
    }
    fn root(&self, session: &JsString, project: Option<&JsString>) -> JsString {
        let path = if project.is_some() {
            self.options.project_root_for(project)
        } else {
            self.overrides
                .sdk_config_project(session)
                .unwrap_or_else(|| self.options.project.clone())
        };
        JsString::from(path.to_string_lossy().into_owned())
    }
    /// Called before publishing the resolved SDK config. A cancelled completion
    /// cannot configure a project ledger or make a new config visible.
    pub(super) fn configure_sdk(
        &self,
        session: &JsString,
        project: Option<&JsString>,
        config: &JsValue,
        completion: &TurnCompletionHandle,
    ) -> Result<bool, SubmitError> {
        if completion.is_cancelled() {
            return Ok(false);
        }
        let Some(budget) = config
            .get("taskBudget")
            .filter(|v| !matches!(v, JsValue::Null | JsValue::Undefined))
        else {
            return Ok(true);
        };
        if scope(budget) != TaskBudgetScope::Project {
            return Ok(true);
        }
        let root = JsString::from(
            self.options
                .project_root_for(project)
                .to_string_lossy()
                .into_owned(),
        );
        let total = budget
            .get("total")
            .and_then(JsValue::as_number)
            .ok_or_else(|| {
                SubmitError::new(
                    "INVALID_SDK_SESSION_CONFIG",
                    "taskBudget.total must be a positive finite USD amount.",
                )
            })?;
        let retention = budget
            .get("projectRetentionMs")
            .and_then(JsValue::as_number);
        #[cfg(test)]
        CONFIGURE_BEFORE_LOCK.with(|witness| {
            if let Some(entered) = witness.borrow_mut().take() {
                let _ = entered.send(());
            }
        });
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // Cancellation can happen while another settlement owns this mutex.
        // Like TS's throwIfAborted after ensureTaskBudgetLedgerLoaded, check
        // at the actual write admission boundary, not only before waiting.
        if completion.is_cancelled() {
            return Ok(false);
        }
        write(&mut state, |store| {
            store.configure_project(&root, session, total, retention, (self.now)())
        })
        .map_err(error)?;
        Ok(!completion.is_cancelled())
    }
    fn current(
        &self,
        state: &mut State,
        session: &JsString,
        project: Option<&JsString>,
    ) -> Result<Option<Snapshot>, SubmitError> {
        let Some(config) = self.overrides.sdk_config(session) else {
            return Ok(None);
        };
        let Some(budget) = config
            .get("taskBudget")
            .filter(|v| !matches!(v, JsValue::Null | JsValue::Undefined))
        else {
            return Ok(None);
        };
        if let Some(failure) = &state.write_failure {
            return Err(SubmitError::new("TASK_BUDGET_UNAVAILABLE", failure.clone()));
        }
        let scope = scope(budget);
        let root = self.root(session, project);
        let total = budget
            .get("total")
            .and_then(JsValue::as_number)
            .unwrap_or(f64::NAN);
        let ledger = state.store.get(&root, scope, session).map_err(error)?;
        let spent = ledger.map_or(0.0, |v| v.spent_usd);
        let total = if scope == TaskBudgetScope::Project {
            ledger
                .and_then(|v| v.total_usd.as_number())
                .unwrap_or(total)
        } else {
            total
        };
        Ok(Some(Snapshot {
            project: root,
            scope,
            total,
            spent,
        }))
    }
    pub(super) fn capture(
        &self,
        session: &JsString,
        project: Option<&JsString>,
        completion: &TurnCompletionHandle,
    ) -> Result<JsValue, SubmitError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.runs.retain(|_, run| !run.completion.is_complete());
        if state.write_failure.is_some() {
            let snapshot = self.current(&mut state, session, project)?;
            return Ok(snapshot_value(snapshot.as_ref()));
        }
        let key = (session.clone(), completion.run_id().clone());
        if let Some(run) = state.runs.get(&key) {
            return Ok(snapshot_value(Some(&run.budget)));
        }
        let snapshot = self.current(&mut state, session, project)?;
        let value = snapshot_value(snapshot.as_ref());
        if !completion.is_cancelled()
            && let Some(budget) = snapshot
        {
            state.runs.insert(
                key,
                Captured {
                    budget,
                    completion: completion.clone(),
                },
            );
        }
        Ok(value)
    }
    pub(super) fn options_for_run(&self, session: &JsString, run: &JsString) -> JsValue {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.runs.retain(|_, run| !run.completion.is_complete());
        let Some(captured) = state.runs.get(&(session.clone(), run.clone())) else {
            return JsValue::Undefined;
        };
        let mut options = JsObject::new();
        options.insert(
            "taskBudgetUsd".into(),
            JsValue::Number(captured.budget.total),
        );
        options.insert(
            "initialTaskBudgetSpentUsd".into(),
            JsValue::Number(captured.budget.spent),
        );
        JsValue::Object(options)
    }
    pub(super) fn release(&self, session: &JsString, run: &JsString) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .runs
            .remove(&(session.clone(), run.clone()));
    }
    pub(super) fn observe(&self, input: &JsValue, event: &JsValue) -> Result<(), SubmitError> {
        if !event
            .get("type")
            .and_then(JsValue::as_string)
            .is_some_and(|v| v.equals_str("turn_completed"))
        {
            return Ok(());
        }
        let Some(budget) = event.get("result").and_then(|v| v.get("budget")) else {
            return Ok(());
        };
        if budget
            .get("taskBudgetUsd")
            .is_none_or(JsValue::is_undefined)
        {
            return Ok(());
        }
        let Some(spend) = budget
            .get("turnSpentUsd")
            .and_then(JsValue::as_number)
            .filter(|v| v.is_finite() && *v >= 0.0)
        else {
            return Ok(());
        };
        let Some(session) = input.get("sessionKey").and_then(JsValue::as_string) else {
            return Ok(());
        };
        let Some(run) = event
            .get("turnId")
            .and_then(JsValue::as_string)
            .or_else(|| input.get("runId").and_then(JsValue::as_string))
        else {
            return Err(SubmitError::new(
                "TASK_BUDGET_UNAVAILABLE",
                "Task budget settlement requires the accepted turn runId.",
            ));
        };
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let snapshot = state
            .runs
            .get(&(session.clone(), run.clone()))
            .map(|v| v.budget.clone());
        let snapshot = match snapshot {
            Some(value) => Some(value),
            None => self.current(
                &mut state,
                session,
                input.get("projectKey").and_then(JsValue::as_string),
            )?,
        };
        if let Some(snapshot) = snapshot {
            write(&mut state, |store| {
                store.settle(
                    &snapshot.project,
                    snapshot.scope,
                    session,
                    run,
                    spend,
                    (self.now)(),
                )
            })
            .map_err(error)?;
        }

        Ok(())
    }
    pub(super) fn clear_sdk(
        &self,
        session: &JsString,
        project: Option<&JsString>,
    ) -> Result<(), SubmitError> {
        let root = self.root(session, project);
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        write(&mut state, |store| store.clear_session(&root, session)).map_err(error)?;
        state.runs.retain(|(key, _), _| key != session);
        Ok(())
    }
}
impl SubmitTaskBudgetSnapshot for NativeTaskBudget {
    fn snapshot<'a>(
        &'a self,
        session: &'a JsString,
        project: Option<&'a JsString>,
    ) -> SubmitFuture<'a, Result<JsValue, SubmitError>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let snapshot = self.current(&mut state, session, project)?;
            Ok(snapshot_value(snapshot.as_ref()))
        })
    }
    fn snapshot_for_run<'a>(
        &'a self,
        session: &'a JsString,
        project: Option<&'a JsString>,
        completion: &'a TurnCompletionHandle,
    ) -> SubmitFuture<'a, Result<JsValue, SubmitError>> {
        Box::pin(async move { self.capture(session, project, completion) })
    }
    fn turn_options(&self, session: &JsString, run: &JsString) -> JsValue {
        self.options_for_run(session, run)
    }
    fn observe_event<'a>(
        &'a self,
        input: &'a JsValue,
        event: &'a JsValue,
    ) -> SubmitFuture<'a, Result<(), SubmitError>> {
        Box::pin(async move { self.observe(input, event) })
    }
    fn release_run(&self, session: &JsString, run: &JsString) {
        self.release(session, run);
    }
}
impl SdkTaskBudget for NativeTaskBudget {
    fn configure_project(
        &self,
        project: &JsString,
        session: &JsString,
        total: f64,
        retention: Option<f64>,
        now: f64,
    ) -> Result<(), TaskBudgetError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        write(&mut state, |store| {
            store.configure_project(project, session, total, retention, now)
        })
    }
    fn clear_session(&self, project: &JsString, session: &JsString) -> Result<(), TaskBudgetError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        write(&mut state, |store| store.clear_session(project, session))
    }
}

fn write(
    state: &mut State,
    operation: impl FnOnce(&mut TaskBudgetStore) -> Result<(), TaskBudgetError>,
) -> Result<(), TaskBudgetError> {
    if let Some(failure) = &state.write_failure {
        return Err(TaskBudgetError::Io(std::io::Error::other(failure.clone())));
    }
    let result = operation(&mut state.store);
    if let Err(TaskBudgetError::Io(error)) = &result {
        state.write_failure = Some(error.to_string());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::session::TurnCompletionFence;
    use pilotdeck_protocol::parse_json;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("pdrs-native-task-budget-{}", uuid::Uuid::new_v4()));
            for dir in ["home", "project", "other"] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }
            for name in ["token", "sidecar"] {
                std::fs::write(root.join(name), "fixture").unwrap();
            }
            Self(root)
        }
        fn options(&self) -> Options {
            let args = vec![
                "--home".to_owned(),
                self.0.join("home").to_string_lossy().into_owned(),
                "--project".into(),
                self.0.join("project").to_string_lossy().into_owned(),
                "--listen".into(),
                "127.0.0.1:0".into(),
                "--token-file".into(),
                self.0.join("token").to_string_lossy().into_owned(),
                "--sidecar".into(),
                self.0.join("sidecar").to_string_lossy().into_owned(),
            ];
            Options::parse(args.into_iter().map(std::ffi::OsString::from)).unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn value(text: &str) -> JsValue {
        parse_json(text).unwrap()
    }
    fn owner(scratch: &Scratch, now: Arc<AtomicU64>) -> Arc<NativeTaskBudget> {
        Arc::new(NativeTaskBudget::with_clock(
            scratch.options(),
            Arc::default(),
            None,
            Arc::new(move || now.load(Ordering::SeqCst) as f64),
        ))
    }
    fn completion(session: &JsString, run: &str) -> TurnCompletionHandle {
        TurnCompletionFence::default().begin(session, &run.into())
    }
    fn publish(owner: &NativeTaskBudget, session: &str, project: Option<&JsString>, config: &str) {
        let session = JsString::from(session);
        let config = value(config);
        let handle = completion(&session, "configure");
        assert!(
            owner
                .configure_sdk(&session, project, &config, &handle)
                .unwrap()
        );
        owner.overrides.set_sdk_config(&session, config);
        owner
            .overrides
            .mark_sdk_config(&session, owner.options.project_root_for(project));
    }
    fn terminal(session: &str, run: &str, spend: f64) -> (JsValue, JsValue) {
        use crate::metadata::object;
        let input = object([
            ("sessionKey", JsValue::from(session)),
            ("runId", JsValue::from(run)),
        ]);
        let budget = object([
            ("taskBudgetUsd", JsValue::Number(100.0)),
            ("turnSpentUsd", JsValue::Number(spend)),
        ]);
        let result = object([("type", JsValue::from("success")), ("budget", budget)]);
        let event = object([
            ("type", JsValue::from("turn_completed")),
            ("turnId", JsValue::from(run)),
            ("result", result),
        ]);
        (input, event)
    }
    async fn spent(owner: &NativeTaskBudget, session: &str, project: Option<&JsString>) -> f64 {
        owner
            .snapshot(&session.into(), project)
            .await
            .unwrap()
            .get("spentUsd")
            .and_then(JsValue::as_number)
            .unwrap()
    }
    #[tokio::test]
    async fn scopes_and_frozen_admission_options_share_one_store() {
        let scratch = Scratch::new();
        let owner = owner(&scratch, Arc::new(AtomicU64::new(1000)));
        publish(
            &owner,
            "a",
            None,
            r#"{"taskBudget":{"total":10,"scope":"project"}}"#,
        );
        publish(
            &owner,
            "b",
            None,
            r#"{"taskBudget":{"total":10,"scope":"project"}}"#,
        );
        let handle = completion(&"a".into(), "held");
        owner.capture(&"a".into(), None, &handle).unwrap();
        let (input, event) = terminal("b", "other", 2.0);
        owner.observe(&input, &event).unwrap();
        assert_eq!(spent(&owner, "a", None).await, 2.0);
        assert_eq!(
            owner
                .options_for_run(&"a".into(), &"held".into())
                .get("initialTaskBudgetSpentUsd"),
            Some(&JsValue::Number(0.0))
        );
        publish(&owner, "session-a", None, r#"{"taskBudget":{"total":5}}"#);
        publish(&owner, "session-b", None, r#"{"taskBudget":{"total":5}}"#);
        let (input, event) = terminal("session-a", "same-id", 1.0);
        owner.observe(&input, &event).unwrap();
        assert_eq!(spent(&owner, "session-a", None).await, 1.0);
        assert_eq!(spent(&owner, "session-b", None).await, 0.0);
        owner.clear_sdk(&"session-a".into(), None).unwrap();
        assert_eq!(spent(&owner, "session-a", None).await, 0.0);
        owner.clear_sdk(&"a".into(), None).unwrap();
        assert_eq!(spent(&owner, "b", None).await, 2.0);
    }
    #[tokio::test]
    async fn concurrent_same_run_settles_once_and_separate_projects_do_not_share_spend() {
        let scratch = Scratch::new();
        let owner = owner(&scratch, Arc::new(AtomicU64::new(1000)));
        publish(
            &owner,
            "a",
            None,
            r#"{"taskBudget":{"total":10,"scope":"project"}}"#,
        );
        let mut threads = Vec::new();
        for _ in 0..8 {
            let owner = owner.clone();
            threads.push(std::thread::spawn(move || {
                let (input, event) = terminal("a", "same-run", 1.25);
                owner.observe(&input, &event).unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(spent(&owner, "a", None).await, 1.25);
        let other = JsString::from(scratch.0.join("other").to_string_lossy().into_owned());
        publish(
            &owner,
            "b",
            Some(&other),
            r#"{"taskBudget":{"total":10,"scope":"project"}}"#,
        );
        assert_eq!(spent(&owner, "b", Some(&other)).await, 0.0);
        let bytes = std::fs::read_to_string(owner.state.lock().unwrap().store.path()).unwrap();
        assert_eq!(
            bytes
                .lines()
                .filter(|line| line.contains("\"kind\":\"settled\""))
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn restart_replays_spend_and_project_retention_only_expires_on_configure() {
        let scratch = Scratch::new();
        let now = Arc::new(AtomicU64::new(1000));
        let first = owner(&scratch, now.clone());
        publish(
            &first,
            "a",
            None,
            r#"{"taskBudget":{"total":10,"scope":"project","projectRetentionMs":100}}"#,
        );
        let (input, event) = terminal("a", "r1", 1.0);
        first.observe(&input, &event).unwrap();
        drop(first);
        now.store(1050, Ordering::SeqCst);
        let second = owner(&scratch, now.clone());
        publish(
            &second,
            "b",
            None,
            r#"{"taskBudget":{"total":10,"scope":"project","projectRetentionMs":100}}"#,
        );
        assert_eq!(spent(&second, "b", None).await, 1.0);
        now.store(1200, Ordering::SeqCst);
        assert_eq!(
            spent(&second, "b", None).await,
            1.0,
            "snapshot itself does not expire the ledger"
        );
        publish(
            &second,
            "c",
            None,
            r#"{"taskBudget":{"total":20,"scope":"project","projectRetentionMs":100}}"#,
        );
        assert_eq!(spent(&second, "c", None).await, 0.0);
    }
    #[tokio::test]
    async fn cancellation_and_nonbudget_terminals_do_not_create_charges() {
        let scratch = Scratch::new();
        let owner = owner(&scratch, Arc::new(AtomicU64::new(1000)));
        let session = JsString::from("a");
        let fence = TurnCompletionFence::default();
        let handle = fence.begin(&session, &"configure".into());
        fence.cancel(&session, Some(&"cancelled".into()), Some(handle.run_id()));
        assert!(
            !owner
                .configure_sdk(
                    &session,
                    None,
                    &value(r#"{"taskBudget":{"total":10,"scope":"project"}}"#),
                    &handle
                )
                .unwrap()
        );
        assert!(!owner.state.lock().unwrap().store.path().exists());
        publish(&owner, "a", None, r#"{"taskBudget":{"total":10}}"#);
        owner
            .observe(
                &value(r#"{"sessionKey":"a","runId":"r"}"#),
                &value(r#"{"type":"session_aborted","reason":"cancelled"}"#),
            )
            .unwrap();
        assert_eq!(spent(&owner, "a", None).await, 0.0);
        let (input, _) = terminal("a", "partial", 0.75);
        let event = value(
            r#"{"type":"turn_completed","turnId":"partial","result":{"type":"error","error":{"code":"agent_aborted","message":"cancelled"},"budget":{"taskBudgetUsd":10,"turnSpentUsd":0.75}}}"#,
        );
        owner.observe(&input, &event).unwrap();
        assert_eq!(
            spent(&owner, "a", None).await,
            0.75,
            "a real terminal budget is charged even when the turn status later indicates failure/cancel"
        );
        let handle = completion(&session, "snapshot");
        owner.capture(&session, None, &handle).unwrap();
        owner.release(&session, handle.run_id());
        assert!(
            owner
                .options_for_run(&session, handle.run_id())
                .is_undefined()
        );
    }
    #[tokio::test]
    async fn failed_append_cannot_turn_duplicate_run_into_false_success() {
        let scratch = Scratch::new();
        let owner = owner(&scratch, Arc::new(AtomicU64::new(1000)));
        publish(
            &owner,
            "a",
            None,
            r#"{"taskBudget":{"total":10,"scope":"project"}}"#,
        );
        let path = owner.state.lock().unwrap().store.path().to_owned();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let (input, event) = terminal("a", "failed-write", 1.0);
        assert!(owner.observe(&input, &event).is_err());
        assert!(
            owner.observe(&input, &event).is_err(),
            "same-run in-memory dedup must not mask a failed append"
        );
        assert!(owner.snapshot(&"a".into(), None).await.is_err());
    }
    #[tokio::test]
    async fn failed_clear_preserves_sdk_and_captured_turn_cache() {
        let scratch = Scratch::new();
        let owner = owner(&scratch, Arc::new(AtomicU64::new(1000)));
        publish(
            &owner,
            "a",
            None,
            r#"{"taskBudget":{"total":10,"scope":"project"}}"#,
        );
        let handle = completion(&"a".into(), "captured");
        owner.capture(&"a".into(), None, &handle).unwrap();
        let path = owner.state.lock().unwrap().store.path().to_owned();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(owner.clear_sdk(&"a".into(), None).is_err());
        assert!(owner.overrides.sdk_config(&"a".into()).is_some());
        assert!(
            !owner
                .options_for_run(&"a".into(), handle.run_id())
                .is_undefined()
        );
        assert!(owner.snapshot(&"a".into(), None).await.is_err());
    }
    #[test]
    fn production_priced_usage_settles_and_restart_admission_reads_the_same_ledger() {
        use crate::native_process::tests::sdk_output_controls_tests::ControlProvider;
        use crate::native_process::{LocalGateway, LocalOptions};
        let scratch = Scratch::new();
        let provider = ControlProvider::start("structured_output");
        let home = scratch.0.join("home");
        let project = scratch.0.join("project");
        super::super::assembly::register_project(&scratch.options()).unwrap();
        let path = home.join("pilotdeck.yaml");
        std::fs::write(&path,format!("schemaVersion: 1\nagent: {{model: fixture/mock}}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://{}/v1\n      apiKey: fixture-local-only\n      models: {{mock: {{capabilities: {{supportsToolUse: true}}}}}}\nrouter:\n  stats:\n    modelPricing:\n      fixture/mock: {{input: 1000000, output: 1000000}}\ntelemetry: {{enabled: false}}\n",provider.address)).unwrap();
        let options = || LocalOptions {
            pilot_home: home.clone(),
            project: project.clone(),
            os_home: Some(home.clone()),
            sidecar: scratch.0.join("absent-sidecar"),
            env: [
                ("HOME", home.to_string_lossy().into_owned()),
                ("PILOT_HOME", home.to_string_lossy().into_owned()),
                ("PILOTDECK_CONFIG_PATH", path.to_string_lossy().into_owned()),
            ]
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
            builtin_skills_root: None,
            builtin_plugins_root: None,
            telemetry_context: None,
        };
        let submit = |gateway: &LocalGateway, run: &str| {
            let events = Arc::new(Mutex::new(Vec::new()));
            let capture = events.clone();
            let budget = crate::metadata::object([("total", JsValue::Number(0.5))]);
            let sdk = crate::metadata::object([("taskBudget", budget)]);
            let input = crate::metadata::object([
                ("sessionKey", JsValue::from("priced-budget")),
                ("runId", JsValue::from(run)),
                ("channelKey", JsValue::from("test")),
                ("message", JsValue::from("one priced model turn")),
                ("sdkSessionConfig", sdk),
            ]);
            gateway
                .submit_turn(
                    input,
                    Arc::new(move |event| capture.lock().unwrap().push(event)),
                )
                .unwrap();
            events.lock().unwrap().clone()
        };
        let gateway = LocalGateway::start(options()).unwrap();
        let first = submit(&gateway, "priced-first");
        assert!(
            first
                .iter()
                .any(|event| event.get("type") == Some(&JsValue::from("turn_completed"))),
            "actual gateway events: {first:?}; model requests: {:?}",
            provider.requests.lock().unwrap()
        );
        let ledger_path = home.join("gateway/sdk-task-budget-ledger.jsonl");
        let records: Vec<_> = std::fs::read_to_string(&ledger_path)
            .unwrap()
            .lines()
            .map(value)
            .collect();
        let settled: Vec<_> = records
            .iter()
            .filter(|row| row.get("kind") == Some(&JsValue::from("settled")))
            .collect();
        assert_eq!(settled.len(), 1);
        assert_eq!(
            settled[0].get("runId"),
            Some(&JsValue::from("priced-first"))
        );
        assert!(
            settled[0]
                .get("turnSpentUsd")
                .and_then(JsValue::as_number)
                .is_some_and(|spend| spend >= 0.5)
        );
        let count = || {
            provider
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| {
                    // A main call retains the actual user's turn and declares the tool;
                    // title/judge calls do not meet this contract, regardless of streaming.
                    request["messages"].as_array().is_some_and(|messages| {
                        messages.iter().any(|message| {
                            message["role"] == "user"
                                && message["content"]
                                    .to_string()
                                    .contains("one priced model turn")
                        })
                    }) && request["tools"].as_array().is_some_and(|tools| {
                        tools
                            .iter()
                            .any(|tool| tool["function"]["name"] == "structured_output")
                    })
                })
                .count()
        };
        let after_first = count();
        assert!(
            after_first >= 1,
            "actual main model request contract; requests: {:?}",
            provider.requests.lock().unwrap()
        );
        let blocked = submit(&gateway, "priced-blocked");
        assert!(blocked.iter().any(|event|event.get("code")==Some(&JsValue::from("agent_task_budget_reached"))));
        assert!(
            blocked
                .iter()
                .any(|event| event.get("finishReason") == Some(&JsValue::from("task_budget")))
        );
        assert_eq!(count(), after_first);
        gateway
            .shutdown(std::time::Duration::from_secs(10))
            .unwrap();
        drop(gateway);
        let restarted = LocalGateway::start(options()).unwrap();
        let blocked = submit(&restarted, "priced-restart");
        assert!(blocked.iter().any(|event|event.get("code")==Some(&JsValue::from("agent_task_budget_reached"))));
        assert_eq!(count(), after_first);
        restarted
            .shutdown(std::time::Duration::from_secs(10))
            .unwrap();
    }
    #[test]
    fn cancelled_while_waiting_for_budget_owner_does_not_write_configuration() {
        let scratch = Scratch::new();
        let owner = owner(&scratch, Arc::new(AtomicU64::new(1000)));
        let fence = TurnCompletionFence::default();
        let session = JsString::from("a");
        let handle = fence.begin(&session, &"configure-wait".into());
        let held = owner.state.lock().unwrap();
        let (entered, ready) = std::sync::mpsc::channel();
        let worker_owner = owner.clone();
        let worker_session = session.clone();
        let worker_handle = handle.clone();
        let worker = std::thread::spawn(move || {
            CONFIGURE_BEFORE_LOCK.with(|witness| *witness.borrow_mut() = Some(entered));
            worker_owner.configure_sdk(
                &worker_session,
                None,
                &value(r#"{"taskBudget":{"total":10,"scope":"project"}}"#),
                &worker_handle,
            )
        });
        let reached = ready.recv_timeout(std::time::Duration::from_secs(3));
        fence.cancel(
            &session,
            Some(&"cancelled while queued".into()),
            Some(handle.run_id()),
        );
        drop(held);
        let outcome = worker.join().expect("configuration waiter is reaped");
        assert!(
            reached.is_ok(),
            "waiter must pass the early check and reach the held owner before cancellation: {reached:?}"
        );
        assert!(!outcome.unwrap());
        assert!(
            !owner.state.lock().unwrap().store.path().exists(),
            "a cancelled lock waiter must not append a configured ledger record"
        );
        assert!(owner.overrides.sdk_config(&session).is_none());
    }
}
