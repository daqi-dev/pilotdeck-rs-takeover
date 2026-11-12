//! SOP's pinned node runner reuses the parent's native lifecycle. It changes
//! durable message ownership, not the external Hook session or its admission.
use super::*;

pub(in crate::native_process) struct SopNodeLifecycle {
    parent: Arc<ToolHooks>,
    session: JsString,
    cwd: JsString,
}
impl SopNodeLifecycle {
    pub(in crate::native_process) fn new(
        parent: Arc<ToolHooks>,
        session: JsString,
        cwd: JsString,
    ) -> Self {
        Self {
            parent,
            session,
            cwd,
        }
    }
}
impl LifecyclePort for SopNodeLifecycle {
    fn dispatch(&self, mut input: LifecycleDispatchInput) -> HostFuture<LifecycleDispatchResult> {
        if ["SessionStart", "Setup", "UserPromptSubmit", "SessionEnd"]
            .iter()
            .any(|event| input.event.equals_str(event))
        {
            return Box::pin(async { Ok(LifecycleDispatchResult::default()) });
        }
        input
            .base_input
            .insert("sessionId".into(), JsValue::String(self.session.clone()));
        input
            .base_input
            .insert("cwd".into(), JsValue::String(self.cwd.clone()));
        input.cwd = self.cwd.clone();
        if let Some(env) = &mut input.env {
            env.insert(
                "PILOTDECK_SESSION_ID".into(),
                JsValue::String(self.session.clone()),
            );
        }
        let port = if input.event.equals_str("PreModelRequest")
            || input.event.equals_str("InstructionsLoaded")
        {
            self.parent.session_port()
        } else {
            self.parent
                .port
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        match port {
            Some(port) => port.dispatch(input),
            None => Box::pin(async {
                Err(crate::host::ModuleError::new("SOP parent Hook binding is unavailable.").into())
            }),
        }
    }
    // The node owns neither the parent turn binding nor its advisory runtime.
    // Parent prepared resources release these only after every node drains.
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Capture(Arc<Mutex<Vec<LifecycleDispatchInput>>>);
    impl LifecyclePort for Capture {
        fn dispatch(&self, input: LifecycleDispatchInput) -> HostFuture<LifecycleDispatchResult> {
            self.0.lock().unwrap().push(input);
            Box::pin(async { Ok(LifecycleDispatchResult::default()) })
        }
    }
    #[tokio::test]
    async fn node_hooks_keep_parent_identity_and_skip_duplicate_session_admission() {
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let parent = Arc::new(ToolHooks::default());
        parent.bind(Some(Arc::new(Capture(inputs.clone()))));
        let delegate = SopNodeLifecycle::new(parent.clone(), "parent".into(), "/project".into());
        let signal = crate::runtime::hooks::Cancellation::new();
        for event in [
            "SessionStart",
            "Setup",
            "UserPromptSubmit",
            "SessionEnd",
            "PostToolUse",
        ] {
            let mut base_input = JsObject::new();
            base_input.insert("sessionId".into(), "child".into());
            base_input.insert("transcriptPath".into(), "".into());
            let mut env = JsObject::new();
            env.insert("PILOTDECK_SESSION_ID".into(), "child".into());
            delegate
                .dispatch(LifecycleDispatchInput {
                    event: event.into(),
                    base_input,
                    payload: None,
                    match_query: Some(event.into()),
                    cwd: "/wrong".into(),
                    env: Some(env),
                    signal: Some(signal.clone()),
                })
                .await
                .unwrap();
        }
        delegate.dispose().await.unwrap();
        assert!(parent.port.lock().unwrap().is_some());
        let inputs = inputs.lock().unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(
            inputs[0].base_input.get("sessionId"),
            Some(&JsValue::String("parent".into()))
        );
        assert_eq!(
            inputs[0].base_input.get("transcriptPath"),
            Some(&JsValue::String("".into()))
        );
        assert_eq!(
            inputs[0].env.as_ref().unwrap().get("PILOTDECK_SESSION_ID"),
            Some(&JsValue::String("parent".into()))
        );
        assert_eq!(inputs[0].cwd, JsString::from("/project"));
        assert!(inputs[0].signal.as_ref().unwrap().same_channel(&signal));
    }
}
