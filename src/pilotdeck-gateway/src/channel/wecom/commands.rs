use super::*;
use crate::channel::commands::{
    CommandContext, CommandError, CommandFuture, CommandHost, Project, Status, UpdateResult,
};
pub(super) struct UnavailableHost;
impl CommandHost for UnavailableHost {
    fn list_projects(&self) -> CommandFuture<'_, Vec<Project>> {
        Box::pin(async {
            Err(CommandError(
                "Native channel command host is unavailable".into(),
            ))
        })
    }
    fn run_update(&self, _: bool) -> CommandFuture<'_, UpdateResult> {
        Box::pin(async {
            Err(CommandError(
                "Native channel update host is unavailable".into(),
            ))
        })
    }
    fn status(&self) -> CommandFuture<'_, Status> {
        Box::pin(async {
            Err(CommandError(
                "Native channel status host is unavailable".into(),
            ))
        })
    }
    fn search<'a>(
        &'a self,
        _: &'a JsString,
        _: Option<&'a JsString>,
    ) -> CommandFuture<'a, Option<JsString>> {
        Box::pin(async { Ok(None) })
    }
}
pub(super) struct Context<'a> {
    pub shared: &'a Shared,
    pub input: &'a Inbound,
}
impl Context<'_> {
    fn scope(&self) -> Scope<'_> {
        Scope {
            chat_id: &self.input.chat,
            user_id: Some(&self.input.user),
            group: self.input.group,
            group_sessions_per_user: self.shared.per_user,
        }
    }
}
impl CommandContext for Context<'_> {
    fn is_stopping(&self) -> bool {
        *self.shared.stopping.borrow()
    }
    fn wait_update<'a>(
        &'a self,
        mut operation: crate::channel::commands::update::UpdateOperation,
    ) -> CommandFuture<'a, crate::channel::commands::UpdateResult> {
        Box::pin(async move {
            let result = tokio::select! {biased;_=self.shared.stopped()=>None,result=operation.result()=>Some(result)};
            if result.is_none() || *self.shared.stopping.borrow() {
                if let Err(error) = operation.cancel_and_drain().await {
                    self.shared
                        .failed_updates
                        .lock()
                        .expect("wecom failed updates")
                        .push(operation);
                    return Err(error);
                }
                return Err(CommandError("updater cancelled".into()));
            }
            result.expect("completed updater receipt")
        })
    }
    fn log_error(&self, message: &JsString) {
        self.shared
            .log("error", &String::from_utf16_lossy(message.units()));
    }
    fn reply<'a>(&'a self, text: &'a JsString) -> CommandFuture<'a, ()> {
        Box::pin(async move {
            if self
                .shared
                .send_reply(
                    &self.input.chat,
                    text,
                    Some(&self.input.message_id),
                    self.input.group,
                )
                .await
            {
                Ok(())
            } else {
                Err(CommandError("WeCom command reply failed".into()))
            }
        })
    }
    fn current_project(&self) -> Option<JsString> {
        self.shared
            .state
            .lock()
            .unwrap()
            .mapper
            .get_project(&self.scope())
    }
    fn bind_project(&self, project: &JsString) -> Result<(), CommandError> {
        let snapshot = {
            let mut state = self.shared.state.lock().unwrap();
            state.mapper.bind_project(&self.scope(), project.clone());
            state.mapper.snapshot()
        };
        if let Some(change) = &self.shared.on_state_change {
            change(snapshot);
        }
        Ok(())
    }
    fn reset_session(&self) -> Result<(), CommandError> {
        let snapshot = {
            let mut state = self.shared.state.lock().unwrap();
            state.mapper.resolve(&self.scope(), &"/new".into());
            state.mapper.snapshot()
        };
        if let Some(change) = &self.shared.on_state_change {
            change(snapshot);
        }
        Ok(())
    }
}
