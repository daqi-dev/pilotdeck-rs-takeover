//! Session lifecycle authority is distinct from the sidecar turn authority.
//! User abort is an executor signal, not revocation: SessionEnd still runs.
use super::*;

pub struct SessionHooks(TurnHooks);
impl InteractionHooks {
    pub fn bind_session(
        &self,
        binding: lifecycle::Binding,
        run_id: JsString,
        settings: HooksSettings,
    ) -> Result<SessionHooks, HostError> {
        self.bind_session_observed(binding, run_id, settings, Default::default())
    }
    /// [`Self::bind_session`] with a host observer on the execution events.
    pub fn bind_session_observed(
        &self,
        mut binding: lifecycle::Binding,
        run_id: JsString,
        settings: HooksSettings,
        observers: crate::runtime::hooks::runtime::HookObservers,
    ) -> Result<SessionHooks, HostError> {
        let signal = binding.signal.take();
        let scope = Self {
            state: self.sessions.clone(),
            sessions: Arc::default(),
            permission: self.permission.clone(),
        };
        let hooks = scope.bind_turn_observed(
            binding,
            run_id,
            settings,
            None,
            &mut ModuleRouter::default(),
            observers,
        )?;
        // Retain borrowed user authority for the barrier and executor abort.
        // Scope revocation never aborts this source: dispatch derives an owned
        // executor signal and keeps its real completion in the scope's jobs.
        let state = lock(&hooks.state);
        hooks.check(&state)?;
        *lock(&hooks.run.session_signal) = signal;
        drop(state);
        Ok(SessionHooks(hooks))
    }
}
impl lifecycle::LifecyclePort for SessionHooks {
    fn dispatch(&self, input: LifecycleDispatchInput) -> HostFuture<LifecycleDispatchResult> {
        self.0.dispatch_owned(input, true)
    }
    fn dispose(&self) -> HostFuture<()> {
        lifecycle::LifecyclePort::dispose(&self.0)
    }
}
impl Drop for SessionHooks {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::modules::lifecycle::LifecyclePort;
    use tokio::{
        sync::Semaphore,
        time::{Duration, timeout},
    };
    fn binding(signal: Cancellation) -> lifecycle::Binding {
        lifecycle::Binding {
            session_id: "session-743".into(),
            turn_id: "turn-1".into(),
            cwd: "/tmp".into(),
            permission_mode: None,
            base_env: None,
            signal: Some(signal),
        }
    }
    fn input() -> LifecycleDispatchInput {
        LifecycleDispatchInput {
            event: "SessionEnd".into(),
            base_input: JsObject::new(),
            payload: Some(JsObject::new()),
            match_query: Some("SessionEnd".into()),
            cwd: "/tmp".into(),
            env: None,
            signal: None,
        }
    }
    fn settings() -> HooksSettings {
        vec![(
            "SessionEnd".into(),
            vec![HookMatcher {
                hooks: vec![HookCommand::Callback {
                    name: "held".into(),
                    condition: None,
                    status_message: None,
                }],
                ..Default::default()
            }],
        )]
    }
    struct Held {
        entered: Arc<Semaphore>,
        release: Arc<Semaphore>,
    }
    impl CallbackHookHandler for Held {
        fn call(&self, input: CallbackInput) -> CallbackFuture {
            let entered = self.entered.clone();
            let release = self.release.clone();
            Box::pin(async move {
                assert!(input.signal.is_some(), "#743 session scope signal supplied");
                entered.add_permits(1);
                release
                    .acquire()
                    .await
                    .expect("#743 session scope release")
                    .forget();
                Ok(CallbackResult::Void)
            })
        }
    }
    async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
        timeout(Duration::from_secs(5), future)
            .await
            .expect("#743 session scope bounded")
    }
    #[tokio::test]
    async fn session_abort_does_not_revoke_admitted_end() {
        let resource = InteractionHooks::new(InteractionPermission::default());
        let signal = Cancellation::new();
        let session = resource
            .bind_session(binding(signal.clone()), "run".into(), settings())
            .expect("#743 session scope bind");
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let _registration = session
            .0
            .run
            .runtime
            .hooks()
            .callback_executor()
            .register(
                &"held".into(),
                Arc::new(Held {
                    entered: entered.clone(),
                    release: release.clone(),
                }),
            )
            .expect("#743 session scope registration");
        let mut dispatch = session.dispatch(input());
        tokio::select! {_=bounded(entered.acquire())=>{},result=&mut dispatch=>panic!("#743 session scope entered before finish {result:?}")}
        signal.abort();
        tokio::task::yield_now().await;
        crate::negative_assert!(
            !session.0.run.closed.load(Ordering::SeqCst),
            "#743 session abort retains End authority"
        );
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(dispatch.as_mut().poll(cx)))
                .await
                .is_pending(),
            "#743 session scope holds ignored abort"
        );
        release.add_permits(1);
        crate::test_negative_receipt::expect_ok(
            bounded(dispatch).await,
            "#743 session abort returns actual hook result",
        );
        bounded(session.dispose())
            .await
            .expect("#743 session scope dispose");
    }
    #[tokio::test]
    async fn aborted_replacement_does_not_wait_for_unfinished_predecessor() {
        let resource = InteractionHooks::new(InteractionPermission::default());
        let old = resource
            .bind_session(binding(Cancellation::new()), "old".into(), settings())
            .expect("#743 session barrier old bind");
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let _registration = old
            .0
            .run
            .runtime
            .hooks()
            .callback_executor()
            .register(
                &"held".into(),
                Arc::new(Held {
                    entered: entered.clone(),
                    release: release.clone(),
                }),
            )
            .expect("#743 session barrier callback");
        let mut dispatch = old.dispatch(input());
        tokio::select! {_=bounded(entered.acquire())=>{},result=&mut dispatch=>panic!("#743 session barrier held predecessor {result:?}")}
        drop(dispatch); // Cancels receive, leaves admitted callback owned by cleanup.
        let signal = Cancellation::new();
        let next = resource
            .bind_session(binding(signal.clone()), "next".into(), vec![])
            .expect("#743 session barrier next bind");
        let mut pending = next.dispatch(input());
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx)))
                .await
                .is_pending(),
            "#743 session barrier predecessor blocks"
        );
        signal.abort();
        let result = timeout(Duration::from_secs(2), pending).await;
        release.add_permits(1); // Always release the predecessor before assertions.
        crate::negative_assert!(
            matches!(result, Ok(Err(_))),
            "#743 session barrier cancellation terminates wait"
        );
        bounded(next.dispose())
            .await
            .expect("#743 session barrier next close");
        bounded(resource.dispose())
            .await
            .expect("#743 session barrier generation close");
    }
    #[tokio::test]
    async fn preaborted_session_end_command_still_executes() {
        let root = std::env::temp_dir().join(format!("pdrs-session-743-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("#743 preabort command directory");
        let config=pilotdeck_protocol::parse_json(r#"{"SessionEnd":[{"hooks":[{"type":"command","command":"printf completed > completed"}]}]}"#).expect("#743 preabort command config");
        let resource = InteractionHooks::new(InteractionPermission::default());
        let signal = Cancellation::new();
        signal.abort();
        let session = resource
            .bind_session(
                binding(signal),
                "run".into(),
                crate::hooks::config::parse_hooks_config(&config).settings,
            )
            .expect("#743 preabort command bind");
        let mut request = input();
        request.cwd = root.to_string_lossy().as_ref().into();
        let result = bounded(session.dispatch(request)).await;
        let result =
            crate::test_negative_receipt::expect_ok(result, "#743 preabort End dispatch admitted");
        crate::negative_assert!(
            result
                .non_blocking_errors
                .iter()
                .any(|error| error.code == "hook_cancelled"),
            "#743 preabort End retains signal"
        );
        crate::negative_assert_eq!(
            std::fs::read_to_string(root.join("completed"))
                .ok()
                .as_deref(),
            Some("completed"),
            "#743 preabort End command completed"
        );
        bounded(session.dispose())
            .await
            .expect("#743 preabort command dispose");
        std::fs::remove_dir_all(root).expect("#743 preabort command cleanup");
    }
    struct CancelThenDrain {
        entered: Arc<Semaphore>,
        cancelled: Arc<Semaphore>,
        release: Arc<Semaphore>,
        source: Cancellation,
    }
    impl CallbackHookHandler for CancelThenDrain {
        fn call(&self, input: CallbackInput) -> CallbackFuture {
            let signal = input.signal.expect("owned session executor signal");
            assert!(
                !signal.same_channel(&self.source),
                "executor must not borrow cancel authority"
            );
            let entered = self.entered.clone();
            let cancelled = self.cancelled.clone();
            let release = self.release.clone();
            Box::pin(async move {
                entered.add_permits(1);
                signal.cancelled().await;
                cancelled.add_permits(1);
                release
                    .acquire()
                    .await
                    .expect("owned executor release")
                    .forget();
                Ok(CallbackResult::Void)
            })
        }
    }
    enum Revocation {
        Dispose,
        DropWaiter,
        Replace,
        UserAbort,
    }
    async fn assert_owned_session_cancellation(action: Revocation) {
        let resource = InteractionHooks::new(InteractionPermission::default());
        let source = Cancellation::new();
        let session = resource
            .bind_session(binding(source.clone()), "owned-run".into(), settings())
            .expect("session bind");
        let entered = Arc::new(Semaphore::new(0));
        let cancelled = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let _registration = session
            .0
            .run
            .runtime
            .hooks()
            .callback_executor()
            .register(
                &"held".into(),
                Arc::new(CancelThenDrain {
                    entered: entered.clone(),
                    cancelled: cancelled.clone(),
                    release: release.clone(),
                    source: source.clone(),
                }),
            )
            .expect("controlled callback");
        let mut dispatch = session.dispatch(input());
        tokio::select! { _ = bounded(entered.acquire()) => {}, result = &mut dispatch => panic!("executor ended before entry {result:?}") }
        let mut replacement = None;
        let mut dispatch = Some(dispatch);
        match action {
            Revocation::Dispose => session.0.close(),
            Revocation::DropWaiter => {
                drop(dispatch.take());
            }
            Revocation::Replace => {
                replacement = Some(
                    resource
                        .bind_session(binding(source.clone()), "replacement".into(), vec![])
                        .expect("replacement bind"),
                );
            }
            Revocation::UserAbort => source.abort(),
        }
        bounded(cancelled.acquire())
            .await
            .expect("executor observes cancellation")
            .forget();
        if matches!(action, Revocation::UserAbort) {
            assert!(
                !session.0.run.closed.load(Ordering::SeqCst),
                "user abort retains session tail authority"
            );
        } else {
            assert!(
                !source.is_aborted(),
                "scope revocation must not abort borrowed source"
            );
            let mut closing = session.0.wait_closed();
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(closing.as_mut().poll(cx)))
                    .await
                    .is_pending(),
                "close receipt must await actual executor drain"
            );
        }
        release.add_permits(1);
        if let Some(dispatch) = dispatch {
            let result = bounded(dispatch).await;
            if matches!(action, Revocation::UserAbort) {
                assert!(
                    result.is_ok(),
                    "user-aborted executor result remains dispatchable"
                );
            }
        }
        bounded(session.dispose())
            .await
            .expect("owned executor drained");
        if let Some(replacement) = replacement {
            bounded(replacement.dispose())
                .await
                .expect("replacement cleanup");
        }
        bounded(resource.dispose())
            .await
            .expect("generation cleanup");
        if !matches!(action, Revocation::UserAbort) {
            assert!(!source.is_aborted(), "cleanup preserves borrowed source");
        }
    }
    #[tokio::test]
    async fn ordinary_session_dispose_preserves_borrowed_user_signal() {
        let resource = InteractionHooks::new(InteractionPermission::default());
        let source = Cancellation::new();
        let session = resource
            .bind_session(binding(source.clone()), "ordinary".into(), vec![])
            .expect("ordinary session");
        bounded(session.dispose()).await.expect("ordinary disposal");
        assert!(
            !source.is_aborted(),
            "normal lifecycle completion cannot abort session advisory work"
        );
        bounded(resource.dispose())
            .await
            .expect("ordinary generation disposal");
        assert!(!source.is_aborted());
    }
    #[tokio::test]
    async fn session_revocation_drains_owned_executor_without_aborting_source() {
        assert_owned_session_cancellation(Revocation::Dispose).await;
    }
    #[tokio::test]
    async fn dropped_session_waiter_keeps_owned_executor_until_real_drain() {
        assert_owned_session_cancellation(Revocation::DropWaiter).await;
    }
    #[tokio::test]
    async fn replaced_session_scope_cancels_only_its_owned_executor() {
        assert_owned_session_cancellation(Revocation::Replace).await;
    }
    #[tokio::test]
    async fn user_abort_reaches_owned_executor_without_revoking_session() {
        assert_owned_session_cancellation(Revocation::UserAbort).await;
    }
}
