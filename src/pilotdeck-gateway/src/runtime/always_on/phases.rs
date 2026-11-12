//! The workspace / execution / report phases of TS `DiscoveryFire`
//! (DiscoveryFire.ts:332-533 `rerunPlan`, :669-941 the tail of `run`,
//! :950-1061 `runWorkspacePhase` / `ensureActiveWorkCycle` /
//! `assertWorkspaceCwdSafe`, :1099-1123 `writeFallbackReport`). Both `run`
//! and `rerunPlan` share this tail once a plan exists.
use std::{io, sync::Arc};

use pilotdeck_protocol::{JsObject, JsValue};

use crate::{
    config::session_value::SessionValue,
    runtime::cron::iso_from_epoch_ms,
};

use super::{
    AlwaysOnError, DiscoveryPlanRecord, WorkCycleRecord,
    cycles::{NewCycleWorkspace, PlanUpdate},
    fire::{
        DiscoveryFire, derive_execution_session_key, derive_report_session_key,
        derive_workspace_session_key, error_value, pick_first_error, unattended_override,
    },
    manager::{AlwaysOnRunContext, ControlError},
    prompts::{build_execution_prompt, build_report_prompt, build_workspace_prompt},
    report::{ReportMetadata, build_fallback_report, parse_report_markdown},
    tools::{PhaseContext, PhaseKind, write_report},
    workspace::WorkspaceHandle,
};

const WORKSPACE_CHANNEL: &str = "always-on/workspace";
const EXECUTION_CHANNEL: &str = "always-on/execute";
const REPORT_CHANNEL: &str = "always-on/report";

/// `ALWAYS_ON_EXECUTION_DENY_RULES` (DiscoveryFire.ts:89-94).
const EXECUTION_DENY_PATTERNS: [&str; 4] =
    ["git push*", "git remote*", "*git push*", "*git remote*"];

/// What the shared tail needs about the plan.
pub(crate) struct TailPlan {
    pub record: DiscoveryPlanRecord,
    pub markdown: String,
}

/// The `DiscoveryRunHistoryEvent` base (`baseHistory`): key order differs
/// between `run` (planId appended later) and `rerunPlan` (planId second).
pub(crate) struct HistoryBase {
    pub run_id: String,
    pub started_at: String,
    pub plan_first: bool,
}

/// The tail's outcome as the callers return it.
pub(crate) struct TailResult {
    pub outcome: &'static str,
    pub error: Option<ControlError>,
}

fn history_head(base: &HistoryBase, plan_id: &str) -> JsObject {
    let mut out = JsObject::new();
    out.insert("schemaVersion".into(), JsValue::Number(1.0));
    out.insert("runId".into(), JsValue::from(base.run_id.as_str()));
    if base.plan_first {
        out.insert("planId".into(), JsValue::from(plan_id));
    }
    out.insert("startedAt".into(), JsValue::from(base.started_at.as_str()));
    out.insert("outcome".into(), JsValue::from("no_plan"));
    if !base.plan_first {
        out.insert("planId".into(), JsValue::from(plan_id));
    }
    out
}

fn workspace_ref(workspace: &WorkspaceHandle) -> JsValue {
    let mut out = JsObject::new();
    out.insert("strategy".into(), JsValue::from(workspace.strategy.as_str()));
    out.insert("handle".into(), JsValue::from(workspace.cwd.as_str()));
    JsValue::Object(out)
}

fn report_strategy(strategy: &str) -> &'static str {
    if strategy == "git-worktree" {
        "git-worktree"
    } else {
        "snapshot-copy"
    }
}

/// `extractAssistantText` (DiscoveryFire.ts:1174-1182).
fn assistant_text(events: &[JsValue]) -> String {
    let mut text = String::new();
    for event in events {
        if event
            .get("type")
            .and_then(JsValue::as_string)
            .is_some_and(|kind| kind.equals_str("assistant_text_delta"))
        {
            text.push_str(&super::fire::js_display(
                event.get("text").unwrap_or(&JsValue::Undefined),
            ));
        }
    }
    text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
        .to_string()
}

fn io_error(error: io::Error) -> AlwaysOnError {
    AlwaysOnError::new("internal", error.to_string())
}

impl DiscoveryFire {
    fn plan_title(record: &DiscoveryPlanRecord) -> JsValue {
        record
            .raw
            .get("title")
            .cloned()
            .unwrap_or(JsValue::Undefined)
    }

    fn register(&self, session_key: &str, run_id: &str, phase: PhaseContext) -> Arc<PhaseContext> {
        let phase = Arc::new(phase);
        let _ = self.deps.run_contexts.register(AlwaysOnRunContext {
            session_key: session_key.into(),
            run_id: run_id.into(),
            project_key: self.deps.project_key.clone(),
            phase: Some(phase.clone()),
        });
        phase
    }

    async fn release_phase(&self, session_key: &str) {
        self.deps.run_contexts.unregister(session_key);
        self.release_session(session_key).await;
    }

    pub(crate) fn append_history(&self, entry: JsObject) {
        let _ = super::fire::append_json_line(
            &self.deps.paths.run_history_file,
            &JsValue::Object(entry),
        );
    }

    /// `writeFallbackReport` (DiscoveryFire.ts:1099-1123).
    fn write_fallback_report(
        &self,
        run_id: &str,
        plan: &DiscoveryPlanRecord,
        started_at: &str,
        finished_at: &str,
        reason: &str,
        workspace: &WorkspaceHandle,
    ) -> io::Result<String> {
        let markdown = build_fallback_report(
            &ReportMetadata {
                run_id,
                plan_id: &plan.id,
                started_at,
                finished_at,
                outcome: "failed",
                workspace_strategy: report_strategy(&workspace.strategy),
                workspace_handle: &workspace.cwd,
            },
            &super::fire::js_display(&Self::plan_title(plan)),
            reason,
        );
        write_report(&self.deps.paths, run_id, &markdown)
    }

    /// `assertWorkspaceCwdSafe` (DiscoveryFire.ts:1046-1061).
    fn assert_workspace_cwd_safe(&self, workspace: &WorkspaceHandle) -> Result<(), AlwaysOnError> {
        if workspace.cwd == self.deps.project_key {
            return Err(AlwaysOnError::new(
                "workspace_unavailable",
                "workspace cwd must not equal projectRoot — refusing to run Always-On turns in the project root.",
            ));
        }
        let worktrees = self.deps.paths.worktrees_dir.to_string_lossy().into_owned();
        let snapshots = self.deps.paths.snapshots_dir.to_string_lossy().into_owned();
        if !workspace.cwd.starts_with(&worktrees) && !workspace.cwd.starts_with(&snapshots) {
            return Err(AlwaysOnError::new(
                "workspace_unavailable",
                format!(
                    "workspace cwd {} is outside the configured Always-On workspace bases.",
                    workspace.cwd
                ),
            ));
        }
        Ok(())
    }

    fn handle_from_cycle(&self, cycle: &WorkCycleRecord) -> Option<WorkspaceHandle> {
        let workspace = cycle.workspace.as_ref()?;
        Some(WorkspaceHandle {
            run_id: cycle.created_by_run_id.clone(),
            project_key: self.deps.project_key.clone(),
            strategy: workspace.strategy.clone(),
            cwd: workspace.cwd.clone(),
            metadata: workspace
                .metadata
                .entries()
                .iter()
                .filter_map(|(key, value)| {
                    value
                        .as_string()
                        .map(|text| (String::from_utf16_lossy(key.units()), String::from_utf16_lossy(text.units())))
                })
                .collect(),
        })
    }

    /// The active cycle of `state` when it is `active` and its workspace is
    /// still on disk.
    pub(crate) fn live_active_cycle(
        &self,
        state: &JsObject,
    ) -> Result<Option<(WorkspaceHandle, WorkCycleRecord)>, AlwaysOnError> {
        let Some(JsValue::String(cycle_id)) = state.get("activeWorkCycleId") else {
            return Ok(None);
        };
        let Some(cycle) = self
            .deps
            .cycle_store
            .get_record(&String::from_utf16_lossy(cycle_id.units()))
            .map_err(io_error)?
        else {
            return Ok(None);
        };
        if cycle.status != "active" {
            return Ok(None);
        }
        let Some(handle) = self.handle_from_cycle(&cycle) else {
            return Ok(None);
        };
        if !std::path::Path::new(&handle.cwd).exists() {
            return Ok(None);
        }
        Ok(Some((handle, cycle)))
    }

    fn create_cycle(
        &self,
        handle: &WorkspaceHandle,
        run_id: &str,
        cycle_id: &str,
    ) -> Result<WorkCycleRecord, AlwaysOnError> {
        let metadata = handle.metadata_object();
        let cycle = self
            .deps
            .cycle_store
            .create(
                &NewCycleWorkspace {
                    project_key: &handle.project_key,
                    strategy: &handle.strategy,
                    cwd: &handle.cwd,
                    metadata: &metadata,
                },
                run_id,
                cycle_id,
                &iso_from_epoch_ms((self.deps.now)()),
            )
            .map_err(io_error)?;
        self.deps
            .state_store
            .set_active_work_cycle_id(&cycle.id, (self.deps.now)())
            .map_err(io_error)?;
        Ok(cycle)
    }

    /// `runWorkspacePhase` (DiscoveryFire.ts:950-1044) with
    /// `ensureActiveWorkCycle` (:129-179) as its fallback.
    async fn workspace_phase(
        &self,
        run_id: &str,
        state: &JsObject,
        plan_title: &str,
    ) -> Result<(WorkspaceHandle, WorkCycleRecord, bool), AlwaysOnError> {
        if let Some((handle, cycle)) = self.live_active_cycle(state)? {
            return Ok((handle, cycle, true));
        }
        let session_key = derive_workspace_session_key(&self.deps.project_key, run_id);
        let mut phase = PhaseContext::new(
            PhaseKind::Workspace,
            self.deps.paths.clone(),
            self.deps.plan_store.clone(),
        );
        phase.plan_title = plan_title.into();
        phase.workspace_registry = Some(self.deps.workspace_registry.clone());
        let phase = self.register(&session_key, run_id, phase);
        self.set_override(&session_key, &unattended_override(&self.deps.project_key));
        let drained = self
            .drain_turn(
                &session_key,
                WORKSPACE_CHANNEL,
                &format!("{run_id}.workspace"),
                build_workspace_prompt(&self.deps.project_key, plan_title, self.language())
                    .into(),
                false,
            )
            .await;
        self.release_phase(&session_key).await;
        drained.map_err(|error| AlwaysOnError::new("workspace_prepare_failed", error.to_string()))?;
        let cycle_id = (self.deps.uuid)();
        let prepared = phase.state().handle.clone();
        if let Some(handle) = prepared {
            let cycle = self.create_cycle(&handle, run_id, &cycle_id)?;
            return Ok((handle, cycle, false));
        }
        // ensureActiveWorkCycle.
        if let Some((handle, cycle)) = self.live_active_cycle(state)? {
            return Ok((handle, cycle, true));
        }
        if let Some(JsValue::Object(legacy)) = state.get("currentWorkspace") {
            let text = |key: &str| match legacy.get(key) {
                Some(JsValue::String(value)) => String::from_utf16_lossy(value.units()),
                _ => String::new(),
            };
            let cwd = text("cwd");
            if std::path::Path::new(&cwd).exists() {
                let legacy_run = text("runId");
                let metadata = match legacy.get("metadata") {
                    Some(JsValue::Object(meta)) => meta
                        .entries()
                        .iter()
                        .filter_map(|(key, value)| {
                            value.as_string().map(|text| {
                                (
                                    String::from_utf16_lossy(key.units()),
                                    String::from_utf16_lossy(text.units()),
                                )
                            })
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let handle = WorkspaceHandle {
                    run_id: legacy_run.clone(),
                    project_key: self.deps.project_key.clone(),
                    strategy: text("strategy"),
                    cwd,
                    metadata,
                };
                let cycle = self.create_cycle(&handle, &legacy_run, &cycle_id)?;
                return Ok((handle, cycle, true));
            }
        }
        let handle = self
            .deps
            .workspace_registry
            .prepare(&self.deps.project_key, run_id, plan_title)
            .await?;
        let cycle = self.create_cycle(&handle, run_id, &cycle_id)?;
        Ok((handle, cycle, false))
    }

    fn language(&self) -> Option<&str> {
        self.deps.language.as_deref()
    }

    /// The shared tail from `workspace_started` to the run's last record:
    /// DiscoveryFire.ts:375-532 (`rerunPlan`) and :669-940 (`run`).
    pub(crate) async fn plan_tail(
        &self,
        run_id: &str,
        started_at_ms: f64,
        state: &JsObject,
        plan: &TailPlan,
        base: &HistoryBase,
    ) -> io::Result<TailResult> {
        let plan_id = plan.record.id.as_str();
        let title = Self::plan_title(&plan.record);
        let title_text = super::fire::js_display(&title);
        let started_at = iso_from_epoch_ms(started_at_ms);
        let mark_completed = |outcome: &str, finished_ms: f64| {
            self.deps
                .state_store
                .mark_fire_completed(outcome, run_id, Some(plan_id), finished_ms)
        };

        // ── Phase 2: Workspace ──
        self.emit_event(run_id, "workspace_started", vec![("planId", JsValue::from(plan_id))]);
        let prepared = match self.workspace_phase(run_id, state, &title_text).await {
            Ok((handle, cycle, reused)) => {
                let safe = if reused {
                    Ok(())
                } else {
                    self.assert_workspace_cwd_safe(&handle)
                };
                safe.map(|()| (handle, cycle))
            }
            Err(error) => Err(error),
        };
        let (mut workspace, cycle) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let finished_ms = (self.deps.now)();
                let control = ControlError {
                    code: error.code().into(),
                    message: String::from_utf16_lossy(error.message().units()),
                };
                self.emit_event(
                    run_id,
                    "run_failed",
                    vec![
                        ("planId", JsValue::from(plan_id)),
                        ("error", error_value(&control)),
                        ("outcome", JsValue::from("failed")),
                    ],
                );
                mark_completed("failed", finished_ms)?;
                let mut entry = history_head(base, plan_id);
                entry.insert("outcome".into(), JsValue::from("failed"));
                entry.insert(
                    "finishedAt".into(),
                    JsValue::from(iso_from_epoch_ms(finished_ms).as_str()),
                );
                entry.insert("error".into(), error_value(&control));
                self.append_history(entry);
                return Ok(TailResult {
                    outcome: "failed",
                    error: Some(control),
                });
            }
        };
        workspace
            .metadata
            .retain(|(key, _)| key != "startedAt");
        workspace
            .metadata
            .push(("startedAt".into(), started_at.clone()));
        self.emit_event(run_id, "workspace_ready", vec![("planId", JsValue::from(plan_id))]);

        // ── Phase 3: Execution ──
        let execution_key = derive_execution_session_key(&self.deps.project_key, run_id);
        let execution_override = unattended_override(&workspace.cwd);
        let deny = SessionValue::object();
        deny.set(
            "deny",
            SessionValue::array(
                EXECUTION_DENY_PATTERNS
                    .iter()
                    .map(|pattern| {
                        let rule = SessionValue::object();
                        rule.set("source", JsValue::from("policy"));
                        rule.set("behavior", JsValue::from("deny"));
                        rule.set("toolName", JsValue::from("bash"));
                        rule.set("pattern", JsValue::from(*pattern));
                        rule
                    })
                    .collect(),
            ),
        );
        execution_override.set("permissionRules", deny);
        self.set_override(&execution_key, &execution_override);
        let mut execution = PhaseContext::new(
            PhaseKind::Execution,
            self.deps.paths.clone(),
            self.deps.plan_store.clone(),
        );
        execution.plan = Some(plan.record.clone());
        execution.workspace = Some(workspace.clone());
        self.register(&execution_key, run_id, execution);
        let io_to_plan = |result: io::Result<Option<DiscoveryPlanRecord>>| result.map(|_| ());
        io_to_plan(self.deps.plan_store.update(
            plan_id,
            &PlanUpdate {
                status: Some("executing"),
                work_cycle_id: Some(&cycle.id),
                ..PlanUpdate::default()
            },
        ))?;
        self.deps.cycle_store.add_plan(&cycle.id, plan_id)?;
        self.emit_event(
            run_id,
            "execution_started",
            vec![("planId", JsValue::from(plan_id)), ("title", title.clone())],
        );
        let drained = self
            .drain_turn(
                &execution_key,
                EXECUTION_CHANNEL,
                &format!("{run_id}.execute"),
                build_execution_prompt(&plan.markdown, &workspace.cwd, &workspace.strategy, self.language())
                    .into(),
                true,
            )
            .await;
        self.release_phase(&execution_key).await;
        let execution_error = pick_first_error(&drained?);
        if let Some(error) = execution_error {
            let control = super::fire::control_error(&error, "execution_failed");
            self.emit_event(
                run_id,
                "run_failed",
                vec![
                    ("planId", JsValue::from(plan_id)),
                    ("error", error_value(&control)),
                    ("outcome", JsValue::from("failed")),
                ],
            );
            let finished_ms = (self.deps.now)();
            let finished_at = iso_from_epoch_ms(finished_ms);
            let report = self.write_fallback_report(
                run_id,
                &plan.record,
                &started_at,
                &finished_at,
                &format!("execution_failed: {}", control.message),
                &workspace,
            )?;
            io_to_plan(self.deps.plan_store.update(
                plan_id,
                &PlanUpdate {
                    status: Some("failed"),
                    report_file_path: Some(&report),
                    work_cycle_id: Some(&cycle.id),
                },
            ))?;
            mark_completed("failed", finished_ms)?;
            let mut entry = history_head(base, plan_id);
            entry.insert("outcome".into(), JsValue::from("failed"));
            entry.insert("finishedAt".into(), JsValue::from(finished_at.as_str()));
            entry.insert("workCycleId".into(), JsValue::from(cycle.id.as_str()));
            entry.insert("workspace".into(), workspace_ref(&workspace));
            entry.insert("error".into(), error_value(&control));
            self.append_history(entry);
            return Ok(TailResult {
                outcome: "failed",
                error: Some(control),
            });
        }
        self.emit_event(
            run_id,
            "execution_completed",
            vec![("planId", JsValue::from(plan_id)), ("title", title.clone())],
        );

        // ── Phase 4: Report ──
        self.emit_event(
            run_id,
            "report_started",
            vec![("planId", JsValue::from(plan_id)), ("title", title.clone())],
        );
        let report_key = derive_report_session_key(&self.deps.project_key, run_id);
        self.set_override(&report_key, &unattended_override(&workspace.cwd));
        let mut report_phase = PhaseContext::new(
            PhaseKind::Report,
            self.deps.paths.clone(),
            self.deps.plan_store.clone(),
        );
        report_phase.plan = Some(plan.record.clone());
        report_phase.workspace = Some(workspace.clone());
        let report_phase = self.register(&report_key, run_id, report_phase);
        let drained = self
            .drain_turn(
                &report_key,
                REPORT_CHANNEL,
                &format!("{run_id}.report"),
                build_report_prompt(&plan.markdown, &workspace.cwd, &workspace.strategy, self.language())
                    .into(),
                true,
            )
            .await;
        self.release_phase(&report_key).await;
        let report_events = drained?;
        let report_error = pick_first_error(&report_events)
            .map(|error| super::fire::control_error(&error, "report_degraded"));
        let finished_ms = (self.deps.now)();
        let finished_at = iso_from_epoch_ms(finished_ms);
        let mut report = report_phase.state().report.clone();
        if report.is_none() {
            let text = assistant_text(&report_events);
            if !text.is_empty() {
                let parsed = parse_report_markdown(
                    &text,
                    &ReportMetadata {
                        run_id,
                        plan_id,
                        started_at: &started_at,
                        finished_at: &finished_at,
                        outcome: "executed",
                        workspace_strategy: report_strategy(&workspace.strategy),
                        workspace_handle: &workspace.cwd,
                    },
                );
                let path = write_report(&self.deps.paths, run_id, &parsed.raw_content)?;
                report = Some((parsed.raw_content, path));
            }
        }
        let degraded = report.is_none() || report_error.is_some();
        if !degraded {
            self.emit_event(
                run_id,
                "report_produced",
                vec![
                    ("planId", JsValue::from(plan_id)),
                    ("title", title.clone()),
                    ("outcome", JsValue::from("executed")),
                ],
            );
        }
        self.emit_event(
            run_id,
            "run_completed",
            vec![
                ("planId", JsValue::from(plan_id)),
                ("title", title.clone()),
                ("outcome", JsValue::from("executed")),
            ],
        );
        let report_path = match report {
            Some((_, path)) => path,
            None => {
                let reason = match &report_error {
                    Some(error) => format!("report_failed: {}", error.message),
                    None => "report_tool_not_invoked".into(),
                };
                self.write_fallback_report(
                    run_id,
                    &plan.record,
                    &started_at,
                    &finished_at,
                    &reason,
                    &workspace,
                )?
            }
        };
        io_to_plan(self.deps.plan_store.update(
            plan_id,
            &PlanUpdate {
                status: Some(if degraded { "completed_no_report" } else { "completed" }),
                report_file_path: Some(&report_path),
                work_cycle_id: Some(&cycle.id),
            },
        ))?;
        mark_completed("executed", finished_ms)?;
        let mut entry = history_head(base, plan_id);
        entry.insert("outcome".into(), JsValue::from("executed"));
        entry.insert("finishedAt".into(), JsValue::from(finished_at.as_str()));
        entry.insert("workCycleId".into(), JsValue::from(cycle.id.as_str()));
        entry.insert("workspace".into(), workspace_ref(&workspace));
        if let Some(error) = &report_error {
            entry.insert("error".into(), error_value(error));
        }
        self.append_history(entry);
        Ok(TailResult {
            outcome: "executed",
            error: report_error,
        })
    }
}

