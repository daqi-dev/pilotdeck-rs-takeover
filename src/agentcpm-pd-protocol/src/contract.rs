//! Public contract data types aligned with `state/cli-v1-public-contract.json`
//! and `state/cli-v1-operations.json`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::errors::ContractError;

pub const OPERATION_VERSION: &str = "agentcpm-pd.operation/1";
pub const EVENT_VERSION: &str = "agentcpm-pd.event/1";

/// Target resource identity attached to an operation envelope (`$defs.identity`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

/// Canonical operation request envelope (`$defs.request`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationRequest {
    pub version: String,
    pub request_id: String,
    pub method_id: String,
    pub target: Target,
    pub params: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
}

/// Persistent run states matching `$defs.run.properties.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    Waiting,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    ResultUnknown,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Failed
                | Self::Cancelled
                | Self::Interrupted
                | Self::ResultUnknown
        )
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Running | Self::Waiting | Self::Cancelling
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Cancelling => "cancelling",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::ResultUnknown => "result_unknown",
        }
    }
}

/// Durable submission receipt (`$defs.receipt`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Receipt {
    #[serde(rename_all = "camelCase")]
    Run {
        run_id: String,
        durable: bool,
        accepted_revision: String,
        deduplicated: bool,
        state: RunState,
    },
    #[serde(rename_all = "camelCase")]
    Operation {
        operation_id: String,
        durable: bool,
        accepted_revision: String,
        deduplicated: bool,
        state: RunState,
    },
}

/// Envelope outcome discriminant (`$defs.response`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseOutcome {
    Succeeded,
    Accepted,
    Failed,
}

/// Canonical operation response envelope (`$defs.response`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationResponse {
    pub version: String,
    pub request_id: String,
    pub outcome: ResponseOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ContractError>,
}

/// Public run summary aligned with `$defs.run`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub instance_id: String,
    pub project_id: String,
    pub session_id: String,
    pub run_id: String,
    pub revision: String,
    pub state: RunState,
    pub terminal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_paused: Option<bool>,
}

/// Interaction item kind matching `$defs.interaction.properties.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    Approval,
    Question,
}

/// Interaction lifecycle state matching `$defs.interaction.properties.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionState {
    Pending,
    Answered,
    Expired,
    Cancelled,
}

/// Persistent interaction item aligned with `$defs.interaction`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionSummary {
    pub instance_id: String,
    pub project_id: String,
    pub session_id: String,
    pub run_id: String,
    pub interaction_id: String,
    pub revision: String,
    pub kind: InteractionKind,
    pub state: InteractionState,
    pub choices: Vec<String>,
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_owner: Option<String>,
}

/// Replayable event object matching `$defs.event`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub version: String,
    pub instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<String>,
    pub cursor: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: Map<String, Value>,
}

/// Cursor gap recovery policy (`$defs.gap.properties.recovery`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapRecoveryPolicy {
    FetchSnapshotThenResume,
    RestartQuery,
}

/// Stream/replay gap descriptor matching `$defs.gap`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GapFrame {
    pub kind: String,
    pub requested_cursor: Option<String>,
    pub earliest_cursor: Option<String>,
    pub snapshot_revision: String,
    pub recovery: GapRecoveryPolicy,
}

/// Snapshot section item returned inline in S2 (`data`) with optional S3 `artifactId` extension.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSection {
    pub name: String,
    pub schema_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Per-method runtime availability reported by `system.capabilities`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityStatus {
    pub method_id: String,
    pub schema_version: String,
    pub supported: bool,
    pub configured: Option<bool>,
    pub authorized: Option<bool>,
    pub available: bool,
    pub reason: Option<String>,
    pub required_permission: Vec<String>,
    pub limits: Vec<String>,
}

/// Server/instance identity returned by `system.capabilities` / `instance.initialize`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerIdentity {
    pub instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub owner_id: String,
    pub product: String,
    pub protocol_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_and_interaction_roundtrip() {
        let receipt = Receipt::Run {
            run_id: "local-01933600-0000-7000-8000-000000000001".into(),
            durable: true,
            accepted_revision: "rev-1".into(),
            deduplicated: false,
            state: RunState::Queued,
        };
        let v = serde_json::to_value(&receipt).unwrap();
        assert_eq!(v["kind"], "run");
        assert_eq!(v["durable"], true);
        let parsed: Receipt = serde_json::from_value(v).unwrap();
        assert_eq!(parsed, receipt);

        let ia = InteractionSummary {
            instance_id: "inst-1".into(),
            project_id: "proj-1".into(),
            session_id: "sess-1".into(),
            run_id: "run-1".into(),
            interaction_id: "ia-1".into(),
            revision: "rev-1".into(),
            kind: InteractionKind::Approval,
            state: InteractionState::Pending,
            choices: vec!["allow_once".into(), "deny".into()],
            expires_at: None,
            prompt: Some("Allow command?".into()),
            response_schema_id: None,
            lease_owner: None,
        };
        let s = serde_json::to_string(&ia).unwrap();
        let back: InteractionSummary = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ia);
    }
}
