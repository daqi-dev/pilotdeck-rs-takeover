//! The gateway's SDK task-budget accounting (`taskBudgetSnapshotForSdk` /
//! `recordTaskBudgetSpendForSdk`, ProjectRuntimeRegistry.ts:1058-1072,
//! 1375-1406), over the process's single ledger store in
//! [`SessionOverrides`].
use super::session_overrides::SessionOverrides;
use crate::runtime::turn::{
    admission::SubmitTaskBudgetSnapshot,
    submit::{SubmitError, SubmitFuture},
};
use crate::storage::{TaskBudgetError, TaskBudgetScope};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::Arc;

pub(super) struct NativeTaskBudget {
    pub options: super::Options,
    pub overrides: Arc<SessionOverrides>,
}

pub(super) fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// A ledger failure as the SDK-config window reports it: the project-budget
/// conflicts carry their own codes; anything else is a plain error, which
/// that window labels `INVALID_SDK_SESSION_CONFIG` (InProcessGateway.ts:
/// 1034-1043).
pub(super) fn submit_error(error: TaskBudgetError) -> SubmitError {
    SubmitError::new(
        error.code().unwrap_or("INVALID_SDK_SESSION_CONFIG"),
        error.to_string(),
    )
}

impl NativeTaskBudget {
    /// The session's configured budget: `(total, scope)`.
    fn configured(&self, session: &JsString) -> Option<(f64, TaskBudgetScope)> {
        let config = self.overrides.sdk_config(session)?;
        let budget = config.get("taskBudget").filter(|b| !b.is_undefined())?;
        let scope = if budget
            .get("scope")
            .and_then(JsValue::as_string)
            .is_some_and(|scope| scope.equals_str("project"))
        {
            TaskBudgetScope::Project
        } else {
            TaskBudgetScope::Session
        };
        Some((
            budget
                .get("total")
                .and_then(JsValue::as_number)
                .unwrap_or(f64::NAN),
            scope,
        ))
    }

    /// `this.resolve(input.projectKey ?? this.sdkSessionProjects.get(sessionKey))
    /// .projectRoot`.
    fn project_root(&self, session: &JsString, project: Option<&JsString>) -> JsString {
        let root = match project {
            Some(project) => self.options.project_root_for(Some(project)),
            None => self
                .overrides
                .sdk_config_project(session)
                .unwrap_or_else(|| self.options.project_root_for(None)),
        };
        root.to_string_lossy().as_ref().into()
    }
}

impl SubmitTaskBudgetSnapshot for NativeTaskBudget {
    fn snapshot<'a>(
        &'a self,
        session: &'a JsString,
        project: Option<&'a JsString>,
    ) -> SubmitFuture<'a, Result<JsValue, SubmitError>> {
        Box::pin(async move {
            let Some((total, scope)) = self.configured(session) else {
                return Ok(JsValue::Undefined);
            };
            let root = self.project_root(session, project);
            let (ledger_total, spent) = self
                .overrides
                .with_task_budget(&self.options.home, |ledger| {
                    ledger.get(&root, scope, session).map(|state| {
                        state.map(|state| (state.total_usd.as_number(), state.spent_usd))
                    })
                })
                .map_err(|error| SubmitError::new("TASK_BUDGET_UNAVAILABLE", error.to_string()))?
                .unwrap_or((None, 0.0));
            let total = match scope {
                TaskBudgetScope::Project => ledger_total.unwrap_or(total),
                TaskBudgetScope::Session => total,
            };
            let mut snapshot = JsObject::new();
            snapshot.insert("totalUsd".into(), JsValue::Number(total));
            snapshot.insert("spentUsd".into(), JsValue::Number(spent));
            Ok(JsValue::Object(snapshot))
        })
    }

    fn record<'a>(
        &'a self,
        session: &'a JsString,
        project: Option<&'a JsString>,
        run: &'a JsString,
        turn_spent_usd: f64,
    ) -> SubmitFuture<'a, ()> {
        Box::pin(async move {
            if !turn_spent_usd.is_finite() || turn_spent_usd < 0.0 {
                return;
            }
            let Some((_, scope)) = self.configured(session) else {
                return;
            };
            let root = self.project_root(session, project);
            let _ = self
                .overrides
                .with_task_budget(&self.options.home, |ledger| {
                    ledger.settle(&root, scope, session, run, turn_spent_usd, now_ms())
                });
        })
    }
}
