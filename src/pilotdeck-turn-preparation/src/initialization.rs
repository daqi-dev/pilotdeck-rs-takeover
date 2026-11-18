//! Original factory initialization order shared with the Native composition.
use crate::execute::FactoryPayload;
use crate::plan_addendum;
use pilotdeck_agent::ports::*;
use pilotdeck_agent::request::{RequestPreparationState, RouteTokenCaps};
use pilotdeck_agent::session_state::{
    AgentFileState, AgentModelSessionState, parse_file_state, parse_model_state,
};
use pilotdeck_agent::turn::{TurnInput, TurnRuntimeConfig};
use pilotdeck_protocol::JsObject;
use pilotdeck_protocol::{JsValue, model::BoundarySnapshot};

pub struct InitialPreparationState {
    pub state: RequestPreparationState,
    pub file_state: AgentFileState,
    pub model_state: AgentModelSessionState,
}

/// Preserve PlanTodo read → metadata read → file/model projection order. Every
/// call remains behind the caller's bound port and original cancellation token.
pub fn initialize(
    decoded: &FactoryPayload,
    host: &mut dyn HostPort,
    control: &CallControl,
) -> PortResult<InitialPreparationState> {
    let mut plan_todo_addendum = None;
    if host.advertised("capability", "plan_todo") {
        let reply = host.call(
            control,
            HostCall::PlanTodo(PlanTodoRequest {
                session_id: decoded.input.session_id.clone(),
                turn_id: decoded.input.turn_id.clone(),
                mutation: PlanTodoMutation::Read,
            }),
        )?;
        plan_todo_addendum = match reply {
            HostReply::PlanTodo(snapshot) => plan_addendum(&snapshot.view),
            HostReply::PlanTodoBoundaryOnly(_) => {
                return Err(gap("open PlanTodo prompt/blocking projection"));
            }
            _ => return Err(gap("PlanTodo read operation reply")),
        };
    }
    let mut metadata = Vec::new();
    if host.advertised("model", "get_metadata") {
        let route = decoded
            .input
            .model_override
            .as_ref()
            .map_or_else(|| decoded.config.route.clone(), |m| m.route.clone());
        let HostReply::ModelMetadata(value) =
            host.call(control, HostCall::ModelMetadata { route })?
        else {
            return Err(gap("metadata operation reply"));
        };
        metadata.push(value);
    }
    let file_state = parse_file_state(
        decoded
            .seed_state
            .as_ref()
            .map_or(&JsValue::Undefined, BoundarySnapshot::value),
    )?
    .unwrap_or_default();
    let model_state = parse_model_state(
        decoded
            .model_state
            .as_ref()
            .map_or(&JsValue::Undefined, BoundarySnapshot::value),
    )?
    .unwrap_or_default();
    // Host-owned sidecar tools do not receive native file maps. The volatile
    // projection is retained for terminal checkpoint round trips.
    let state = RequestPreparationState {
        tools: decoded.tools.clone(),
        metadata,
        plan_todo_addendum: plan_todo_addendum.take(),
        elicitation_available: decoded.elicitation_available,
        token_caps: model_state
            .token_caps
            .iter()
            .map(|cap| RouteTokenCaps {
                route: cap.route.clone(),
                max_context_tokens: cap.max_context_tokens,
                attempt_max_output_tokens: None,
                requested_max_output_tokens: None,
                session_max_output_tokens: cap.session_max_output_tokens,
                hard_max_output_tokens: cap.hard_max_output_tokens,
            })
            .collect(),
    };
    Ok(InitialPreparationState {
        state,
        file_state,
        model_state,
    })
}
fn gap(profile: &str) -> PortError {
    PortError::InvalidResponse {
        message: format!("Rust factory candidate has not implemented {profile}.").into(),
    }
}

pub fn model_hints(input: &TurnInput, execution: ExecutionHints) -> ModelHints {
    ModelHints {
        session_id: input.session_id.clone(),
        turn_id: input.turn_id.clone(),
        execution,
        workspace_id: None,
        storage_config_version: None,
        metadata: None,
        model_override: input
            .model_override
            .as_ref()
            .map(|value| value.route.clone()),
    }
}

pub fn tool_hints(
    config: &TurnRuntimeConfig,
    input: &TurnInput,
    execution: ExecutionHints,
) -> ToolHints {
    let mut extensions = JsObject::new();
    extensions.insert(
        "permissionContext".into(),
        crate::permission_context(config),
    );
    ToolHints {
        session_id: input.session_id.clone(),
        turn_id: input.turn_id.clone(),
        cwd: config.cwd.clone(),
        permission_mode: config.permission.mode,
        run_mode: Some(crate::run_mode(config.run_mode).into()),
        can_prompt: config.permission.can_prompt,
        can_elicit: Some(input.can_elicit),
        additional_working_directories: config.permission.additional_working_directories.clone(),
        context_extensions: extensions,
        execution,
    }
}
