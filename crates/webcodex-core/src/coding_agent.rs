//! Closed transport-neutral protocol for Runner-owned ACP coding-agent runs.
//!
//! Raw ACP JSON-RPC never crosses the Server↔Runner boundary. The Runner owns
//! ACP methods, request ids, private session ids, executable/argv/environment,
//! and protocol callbacks; the Server sees only the bounded typed structures in
//! this module.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CODING_AGENT_MAX_PROVIDERS: usize = 8;
pub const CODING_AGENT_MAX_PROVIDER_ID_BYTES: usize = 64;
pub const CODING_AGENT_MAX_PROVIDER_NAME_BYTES: usize = 128;
pub const CODING_AGENT_MAX_RUN_ID_BYTES: usize = 96;
pub const CODING_AGENT_MAX_INTENT_FINGERPRINT_BYTES: usize = 96;
pub const CODING_AGENT_MAX_PROJECT_ID_BYTES: usize = 512;
pub const CODING_AGENT_MAX_PROJECT_ROOT_BYTES: usize = 4096;
pub const CODING_AGENT_MAX_INSTRUCTION_BYTES: usize = 64 * 1024;
pub const CODING_AGENT_MAX_CONFIG_OPTIONS: usize = 32;
pub const CODING_AGENT_MAX_CONFIG_KEY_BYTES: usize = 128;
pub const CODING_AGENT_MAX_CONFIG_VALUE_BYTES: usize = 4096;
pub const CODING_AGENT_MAX_EVENT_TEXT_BYTES: usize = 16 * 1024;
pub const CODING_AGENT_MAX_ERROR_KIND_BYTES: usize = 64;
pub const CODING_AGENT_MAX_ERROR_MESSAGE_BYTES: usize = 16 * 1024;
pub const CODING_AGENT_MAX_EVENT_METADATA_BYTES: usize = 1024;
pub const CODING_AGENT_MAX_TERMINAL_METADATA_BYTES: usize = 1024;
pub const CODING_AGENT_MAX_EVENTS_PER_RESPONSE: usize = 64;
pub const CODING_AGENT_MAX_RETAINED_EVENTS: usize = 256;
pub const CODING_AGENT_MAX_INVENTORY_RUNS: usize = 128;
pub const CODING_AGENT_TIMEOUT_MIN_SECS: u64 = 1;
pub const CODING_AGENT_TIMEOUT_MAX_SECS: u64 = 3600;
pub const CODING_AGENT_OBSERVE_WAIT_MAX_SECS: u64 = 60;

pub const CODING_AGENT_STOP_REASON_END_TURN: &str = "end_turn";
pub const CODING_AGENT_STOP_REASON_CANCELLED: &str = "cancelled";
pub const CODING_AGENT_STOP_REASON_MAX_TOKENS: &str = "max_tokens";
pub const CODING_AGENT_STOP_REASON_MAX_TURN_REQUESTS: &str = "max_turn_requests";
pub const CODING_AGENT_STOP_REASON_REFUSAL: &str = "refusal";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentProvider {
    pub provider_id: String,
    pub provider_instance_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentRunState {
    Starting,
    Running,
    WaitingPermission,
    Completed,
    Failed,
    Cancelled,
    Lost,
}

impl CodingAgentRunState {
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Lost
        )
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::WaitingPermission => "waiting_permission",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Lost => "lost",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "starting" => Self::Starting,
            "running" => Self::Running,
            "waiting_permission" => Self::WaitingPermission,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "lost" => Self::Lost,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentExecutionState {
    NotStarted,
    Started,
    OutcomeUnknown,
    Completed,
}

impl CodingAgentExecutionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Started => "started",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Completed => "completed",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "not_started" => Self::NotStarted,
            "started" => Self::Started,
            "outcome_unknown" => Self::OutcomeUnknown,
            "completed" => Self::Completed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentDispatchState {
    NotStarted,
    OutcomeUnknown,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodingAgentConfigValue {
    String(String),
    Bool(bool),
    Integer(i64),
}

impl CodingAgentConfigValue {
    pub fn serialized_len(&self) -> usize {
        serde_json::to_vec(self)
            .map(|value| value.len())
            .unwrap_or(usize::MAX)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentEventKind {
    AgentMessage,
    Reasoning,
    Plan,
    ToolActivity,
    FileChange,
    TerminalActivity,
    Usage,
    PermissionRequest,
    Terminal,
}

impl CodingAgentEventKind {
    /// Canonical model-facing and wire vocabulary. Keep this exhaustive match
    /// aligned with the serde snake_case representation instead of relying on
    /// Debug formatting, which does not preserve word boundaries.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::AgentMessage => "agent_message",
            Self::Reasoning => "reasoning",
            Self::Plan => "plan",
            Self::ToolActivity => "tool_activity",
            Self::FileChange => "file_change",
            Self::TerminalActivity => "terminal_activity",
            Self::Usage => "usage",
            Self::PermissionRequest => "permission_request",
            Self::Terminal => "terminal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentUsage {
    /// Stable ACP v1 `usage_update.used`: tokens currently in context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_tokens: Option<u64>,
    /// Stable ACP v1 `usage_update.size`: total context-window size in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u64>,
    /// Optional cumulative session cost rendered as bounded decimal text to
    /// keep this transport type Eq/JSON-stable without exposing raw metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_currency: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentEvent {
    pub sequence: u64,
    pub kind: CodingAgentEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<CodingAgentUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentTerminal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub completed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentRunSnapshot {
    pub run_id: String,
    pub intent_fingerprint: String,
    /// Domain-separated hash of the stable authenticated caller identity.
    pub authority_fingerprint: String,
    pub runtime_project_id: String,
    pub provider_id: String,
    pub provider_instance_id: String,
    pub state: CodingAgentRunState,
    pub execution_state: CodingAgentExecutionState,
    pub observation_revision: u64,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<CodingAgentTerminal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingAgentObservationMerge {
    Stale,
    ExactReplay,
    Advance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentRunInventory {
    #[serde(default)]
    pub runs: Vec<CodingAgentRunSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentStartRequest {
    pub run_id: String,
    pub intent_fingerprint: String,
    pub authority_fingerprint: String,
    pub runtime_project_id: String,
    pub project_root: String,
    pub provider_id: String,
    pub provider_instance_id: String,
    pub instruction: String,
    #[serde(default)]
    pub config: BTreeMap<String, CodingAgentConfigValue>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentObserveRequest {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sequence: Option<u64>,
    pub limit: usize,
    pub wait_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentCancelRequest {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum CodingAgentRequest {
    Start(CodingAgentStartRequest),
    Observe(CodingAgentObserveRequest),
    Cancel(CodingAgentCancelRequest),
}

impl CodingAgentRequest {
    pub fn run_id(&self) -> &str {
        match self {
            Self::Start(request) => &request.run_id,
            Self::Observe(request) => &request.run_id,
            Self::Cancel(request) => &request.run_id,
        }
    }

    pub fn provider_binding(&self) -> Option<(&str, &str)> {
        match self {
            Self::Start(request) => Some((&request.provider_id, &request.provider_instance_id)),
            Self::Observe(_) | Self::Cancel(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentObserveResult {
    pub run: CodingAgentRunSnapshot,
    pub events: Vec<CodingAgentEvent>,
    pub first_retained_sequence: u64,
    pub next_sequence: u64,
    pub has_more: bool,
    pub history_lost: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CodingAgentResponsePayload {
    Start {
        run: CodingAgentRunSnapshot,
    },
    Observe {
        observation: CodingAgentObserveResult,
    },
    Cancel {
        run: CodingAgentRunSnapshot,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentResponse {
    pub dispatch_state: CodingAgentDispatchState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<CodingAgentResponsePayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<CodingAgentError>,
}

impl CodingAgentResponse {
    pub fn success(payload: CodingAgentResponsePayload) -> Self {
        Self {
            dispatch_state: CodingAgentDispatchState::Completed,
            payload: Some(payload),
            error: None,
        }
    }

    pub fn error(
        dispatch_state: CodingAgentDispatchState,
        code: impl Into<String>,
        message: impl Into<String>,
        failure_kind: Option<&str>,
        recovery_kind: Option<&str>,
    ) -> Self {
        Self {
            dispatch_state,
            payload: None,
            error: Some(CodingAgentError {
                code: code.into(),
                message: message.into(),
                failure_kind: failure_kind.map(str::to_string),
                recovery_kind: recovery_kind.map(str::to_string),
            }),
        }
    }
}

pub fn validate_provider_id(value: &str) -> Result<(), String> {
    validate_identifier(
        value,
        "provider_id",
        CODING_AGENT_MAX_PROVIDER_ID_BYTES,
        true,
    )
}

pub fn validate_provider_instance_id(value: &str) -> Result<(), String> {
    validate_identifier(
        value,
        "provider_instance_id",
        CODING_AGENT_MAX_PROVIDER_ID_BYTES,
        false,
    )
}

pub fn validate_run_id(value: &str) -> Result<(), String> {
    if !value.starts_with("wc_agent_run_") {
        return Err("run_id must use the wc_agent_run_ namespace".to_string());
    }
    validate_identifier(value, "run_id", CODING_AGENT_MAX_RUN_ID_BYTES, false)
}

pub fn validate_intent_fingerprint(value: &str) -> Result<(), String> {
    validate_identifier(
        value,
        "intent_fingerprint",
        CODING_AGENT_MAX_INTENT_FINGERPRINT_BYTES,
        false,
    )
}

pub fn validate_authority_fingerprint(value: &str) -> Result<(), String> {
    if !value.starts_with("auth_") {
        return Err("authority_fingerprint must use the auth_ namespace".to_string());
    }
    validate_identifier(value, "authority_fingerprint", 80, false)
}

pub fn validate_request(request: &CodingAgentRequest) -> Result<(), String> {
    validate_run_id(request.run_id())?;
    match request {
        CodingAgentRequest::Start(request) => {
            validate_intent_fingerprint(&request.intent_fingerprint)?;
            validate_authority_fingerprint(&request.authority_fingerprint)?;
            if request.runtime_project_id.trim().is_empty()
                || request.runtime_project_id.len() > CODING_AGENT_MAX_PROJECT_ID_BYTES
            {
                return Err("runtime_project_id is invalid".to_string());
            }
            if request.project_root.is_empty()
                || request.project_root.len() > CODING_AGENT_MAX_PROJECT_ROOT_BYTES
                || request.project_root.contains('\0')
            {
                return Err("project_root is invalid".to_string());
            }
            validate_provider_id(&request.provider_id)?;
            validate_provider_instance_id(&request.provider_instance_id)?;
            if request.instruction.is_empty()
                || request.instruction.len() > CODING_AGENT_MAX_INSTRUCTION_BYTES
                || request.instruction.contains('\0')
            {
                return Err("instruction is invalid".to_string());
            }
            if request.config.len() > CODING_AGENT_MAX_CONFIG_OPTIONS {
                return Err("too many config overrides".to_string());
            }
            for (key, value) in &request.config {
                if key.is_empty()
                    || key.len() > CODING_AGENT_MAX_CONFIG_KEY_BYTES
                    || key.contains(['\0', '\r', '\n'])
                    || value.serialized_len() > CODING_AGENT_MAX_CONFIG_VALUE_BYTES
                {
                    return Err("config override is invalid".to_string());
                }
            }
            if !(CODING_AGENT_TIMEOUT_MIN_SECS..=CODING_AGENT_TIMEOUT_MAX_SECS)
                .contains(&request.timeout_secs)
            {
                return Err("timeout_secs is outside the supported range".to_string());
            }
        }
        CodingAgentRequest::Observe(request) => {
            if request.limit == 0 || request.limit > CODING_AGENT_MAX_EVENTS_PER_RESPONSE {
                return Err("observe limit is outside the supported range".to_string());
            }
            if request.wait_secs > CODING_AGENT_OBSERVE_WAIT_MAX_SECS {
                return Err("observe wait_secs is outside the supported range".to_string());
            }
        }
        CodingAgentRequest::Cancel(_) => {}
    }
    Ok(())
}

pub fn validate_response_for_request(
    request: &CodingAgentRequest,
    response: &CodingAgentResponse,
) -> Result<(), String> {
    if response.payload.is_some() == response.error.is_some() {
        return Err(
            "CodingAgentRun response must contain exactly one payload or error".to_string(),
        );
    }
    let Some(payload) = response.payload.as_ref() else {
        let error = response
            .error
            .as_ref()
            .expect("response payload/error exclusivity checked above");
        validate_coding_agent_error(error)?;
        return Ok(());
    };
    let run = match (request, payload) {
        (CodingAgentRequest::Start(_), CodingAgentResponsePayload::Start { run }) => run,
        (CodingAgentRequest::Observe(_), CodingAgentResponsePayload::Observe { observation }) => {
            if observation.events.len() > CODING_AGENT_MAX_EVENTS_PER_RESPONSE {
                return Err("CodingAgentRun response contains too many events".to_string());
            }
            let mut previous = None;
            for event in &observation.events {
                if event
                    .text
                    .as_ref()
                    .is_some_and(|text| text.len() > CODING_AGENT_MAX_EVENT_TEXT_BYTES)
                    || previous.is_some_and(|sequence| event.sequence <= sequence)
                {
                    return Err(
                        "CodingAgentRun response event bounds/order are invalid".to_string()
                    );
                }
                if event
                    .label
                    .as_ref()
                    .is_some_and(|value| value.len() > CODING_AGENT_MAX_EVENT_METADATA_BYTES)
                    || event
                        .status
                        .as_ref()
                        .is_some_and(|value| value.len() > CODING_AGENT_MAX_EVENT_METADATA_BYTES)
                    || event.usage.as_ref().is_some_and(|usage| {
                        usage.cost_amount.as_ref().is_some_and(|value| {
                            value.len() > CODING_AGENT_MAX_EVENT_METADATA_BYTES
                        }) || usage.cost_currency.as_ref().is_some_and(|value| {
                            value.len() > CODING_AGENT_MAX_EVENT_METADATA_BYTES
                        })
                    })
                {
                    return Err("CodingAgentRun response event metadata is too large".to_string());
                }
                previous = Some(event.sequence);
            }
            if observation.first_retained_sequence == 0
                || observation.next_sequence.saturating_add(1) < observation.first_retained_sequence
            {
                return Err("CodingAgentRun response retention metadata is invalid".to_string());
            }
            &observation.run
        }
        (CodingAgentRequest::Cancel(_), CodingAgentResponsePayload::Cancel { run }) => run,
        _ => {
            return Err("CodingAgentRun response kind does not match request operation".to_string())
        }
    };
    if run.run_id != request.run_id() {
        return Err("CodingAgentRun response run_id does not match request".to_string());
    }
    validate_coding_agent_run_snapshot(run)?;
    Ok(())
}

/// Validate the bounded identity and semantic state matrix of one Runner-owned
/// CodingAgentRun snapshot. This is the canonical Server/Runner reconciliation
/// contract: structurally decodable snapshots that contradict the closed ACP v1
/// terminal truth fail closed before they can become retry/recovery authority.
pub fn validate_coding_agent_run_snapshot(run: &CodingAgentRunSnapshot) -> Result<(), String> {
    validate_run_id(&run.run_id)?;
    validate_intent_fingerprint(&run.intent_fingerprint)?;
    validate_authority_fingerprint(&run.authority_fingerprint)?;
    validate_provider_id(&run.provider_id)?;
    validate_provider_instance_id(&run.provider_instance_id)?;
    if run.runtime_project_id.trim().is_empty()
        || run.runtime_project_id.len() > CODING_AGENT_MAX_PROJECT_ID_BYTES
    {
        return Err("CodingAgentRun snapshot project id is invalid".to_string());
    }

    if let Some(terminal) = run.terminal.as_ref() {
        if terminal
            .stop_reason
            .as_ref()
            .is_some_and(|value| value.len() > CODING_AGENT_MAX_TERMINAL_METADATA_BYTES)
            || terminal
                .error_code
                .as_ref()
                .is_some_and(|value| value.len() > CODING_AGENT_MAX_TERMINAL_METADATA_BYTES)
            || terminal
                .message
                .as_ref()
                .is_some_and(|value| value.len() > CODING_AGENT_MAX_ERROR_MESSAGE_BYTES)
        {
            return Err("CodingAgentRun snapshot terminal metadata is too large".to_string());
        }
        if let Some(error_code) = terminal.error_code.as_deref() {
            validate_low_cardinality_kind(
                error_code,
                "CodingAgentRun terminal error code",
                CODING_AGENT_MAX_TERMINAL_METADATA_BYTES,
            )?;
        }
    }

    match run.state {
        CodingAgentRunState::Starting => {
            if run.execution_state != CodingAgentExecutionState::NotStarted
                || run.terminal.is_some()
            {
                return Err(
                    "starting CodingAgentRun snapshot is semantically inconsistent".to_string(),
                );
            }
        }
        CodingAgentRunState::Running => {
            if !matches!(
                run.execution_state,
                CodingAgentExecutionState::OutcomeUnknown | CodingAgentExecutionState::Started
            ) || run.terminal.is_some()
            {
                return Err(
                    "running CodingAgentRun snapshot is semantically inconsistent".to_string(),
                );
            }
        }
        CodingAgentRunState::WaitingPermission => {
            if run.execution_state != CodingAgentExecutionState::Started || run.terminal.is_some() {
                return Err(
                    "waiting_permission CodingAgentRun snapshot is semantically inconsistent"
                        .to_string(),
                );
            }
        }
        CodingAgentRunState::Completed => {
            let terminal = terminal_for_state(run, CodingAgentExecutionState::Completed)?;
            if terminal.stop_reason.as_deref() != Some(CODING_AGENT_STOP_REASON_END_TURN)
                || terminal.error_code.is_some()
            {
                return Err("completed CodingAgentRun snapshot lacks end_turn truth".to_string());
            }
        }
        CodingAgentRunState::Cancelled => {
            let terminal = run.terminal.as_ref().ok_or_else(|| {
                "cancelled CodingAgentRun snapshot lacks terminal metadata".to_string()
            })?;
            match run.execution_state {
                CodingAgentExecutionState::NotStarted => {
                    if terminal.stop_reason.is_some()
                        || terminal.error_code.is_some()
                        || terminal.message.as_deref().is_none_or(str::is_empty)
                    {
                        return Err(
                            "pre-prompt cancelled CodingAgentRun snapshot claims ACP terminal truth"
                                .to_string(),
                        );
                    }
                }
                CodingAgentExecutionState::Completed => {
                    if terminal.stop_reason.as_deref() != Some(CODING_AGENT_STOP_REASON_CANCELLED)
                        || terminal.error_code.is_some()
                    {
                        return Err(
                            "post-prompt cancelled CodingAgentRun snapshot lacks cancelled truth"
                                .to_string(),
                        );
                    }
                }
                _ => {
                    return Err(
                        "cancelled CodingAgentRun snapshot has inconsistent execution state"
                            .to_string(),
                    )
                }
            }
        }
        CodingAgentRunState::Failed => {
            let terminal = run.terminal.as_ref().ok_or_else(|| {
                "failed CodingAgentRun snapshot lacks terminal metadata".to_string()
            })?;
            match terminal.stop_reason.as_deref() {
                Some(
                    reason @ (CODING_AGENT_STOP_REASON_MAX_TOKENS
                    | CODING_AGENT_STOP_REASON_MAX_TURN_REQUESTS
                    | CODING_AGENT_STOP_REASON_REFUSAL),
                ) => {
                    if run.execution_state != CodingAgentExecutionState::Completed
                        || terminal.error_code.as_deref() != Some(reason)
                    {
                        return Err(
                            "failed CodingAgentRun ACP terminal truth is inconsistent".to_string()
                        );
                    }
                }
                None => {
                    if !matches!(
                        run.execution_state,
                        CodingAgentExecutionState::NotStarted
                            | CodingAgentExecutionState::Completed
                    ) || terminal.error_code.is_none()
                    {
                        return Err(
                            "failed CodingAgentRun internal terminal truth is inconsistent"
                                .to_string(),
                        );
                    }
                }
                Some(_) => {
                    return Err(
                        "CodingAgentRun snapshot has unknown or contradictory stop_reason"
                            .to_string(),
                    )
                }
            }
        }
        CodingAgentRunState::Lost => {
            let terminal = terminal_for_state(run, CodingAgentExecutionState::OutcomeUnknown)?;
            if terminal.stop_reason.is_some() || terminal.error_code.is_none() {
                return Err(
                    "lost CodingAgentRun snapshot claims definite terminal truth".to_string(),
                );
            }
        }
    }
    Ok(())
}

/// Classify one newer observation against the canonical snapshot already retained
/// for the same CodingAgentRun. Revision ordering is authoritative: stale snapshots
/// are ignored, exact same-revision replays are idempotent, conflicting same-revision
/// truth fails closed, and only legal higher-revision state transitions may advance.
pub fn merge_coding_agent_run_snapshot(
    stored: &CodingAgentRunSnapshot,
    incoming: &CodingAgentRunSnapshot,
) -> Result<CodingAgentObservationMerge, String> {
    validate_coding_agent_run_snapshot(stored)?;
    validate_coding_agent_run_snapshot(incoming)?;
    validate_coding_agent_run_identity_transition(stored, incoming)?;

    match incoming
        .observation_revision
        .cmp(&stored.observation_revision)
    {
        std::cmp::Ordering::Less => Ok(CodingAgentObservationMerge::Stale),
        std::cmp::Ordering::Equal => {
            if incoming == stored {
                Ok(CodingAgentObservationMerge::ExactReplay)
            } else {
                Err(
                    "CodingAgentRun observation revision has conflicting authoritative snapshots"
                        .to_string(),
                )
            }
        }
        std::cmp::Ordering::Greater => {
            validate_coding_agent_run_transition(stored, incoming)?;
            Ok(CodingAgentObservationMerge::Advance)
        }
    }
}

fn validate_coding_agent_run_identity_transition(
    stored: &CodingAgentRunSnapshot,
    incoming: &CodingAgentRunSnapshot,
) -> Result<(), String> {
    if stored.run_id != incoming.run_id
        || stored.intent_fingerprint != incoming.intent_fingerprint
        || stored.authority_fingerprint != incoming.authority_fingerprint
        || stored.runtime_project_id != incoming.runtime_project_id
        || stored.provider_id != incoming.provider_id
        || stored.provider_instance_id != incoming.provider_instance_id
        || stored.created_at != incoming.created_at
    {
        return Err("CodingAgentRun observation identity changed across revisions".to_string());
    }
    Ok(())
}

/// Validate an accepted higher-revision transition using the existing ACP v1 Run
/// state machine. The validator intentionally permits skipped intermediate revisions:
/// an observer may see Starting and then a terminal snapshot without seeing Running.
pub fn validate_coding_agent_run_transition(
    stored: &CodingAgentRunSnapshot,
    incoming: &CodingAgentRunSnapshot,
) -> Result<(), String> {
    if incoming.observation_revision <= stored.observation_revision {
        return Err("CodingAgentRun transition does not advance observation revision".to_string());
    }
    if matches!(
        stored.state,
        CodingAgentRunState::Completed
            | CodingAgentRunState::Failed
            | CodingAgentRunState::Cancelled
    ) {
        return Err(
            "definitive terminal CodingAgentRun observation cannot advance to new truth"
                .to_string(),
        );
    }
    if stored.state == CodingAgentRunState::Lost
        && !matches!(
            incoming.state,
            CodingAgentRunState::Lost
                | CodingAgentRunState::Completed
                | CodingAgentRunState::Failed
                | CodingAgentRunState::Cancelled
        )
    {
        return Err(
            "lost CodingAgentRun may only reconcile to newer lost or definitive terminal truth"
                .to_string(),
        );
    }
    if stored.state != CodingAgentRunState::Starting
        && incoming.state == CodingAgentRunState::Starting
    {
        return Err("CodingAgentRun state cannot regress to starting".to_string());
    }

    let execution_transition_valid = match stored.execution_state {
        CodingAgentExecutionState::NotStarted => true,
        CodingAgentExecutionState::OutcomeUnknown => {
            matches!(
                incoming.execution_state,
                CodingAgentExecutionState::OutcomeUnknown
                    | CodingAgentExecutionState::Started
                    | CodingAgentExecutionState::Completed
            ) || (incoming.execution_state == CodingAgentExecutionState::NotStarted
                && matches!(
                    incoming.state,
                    CodingAgentRunState::Failed | CodingAgentRunState::Cancelled
                ))
        }
        CodingAgentExecutionState::Started => {
            matches!(
                incoming.execution_state,
                CodingAgentExecutionState::Started | CodingAgentExecutionState::Completed
            ) || (incoming.execution_state == CodingAgentExecutionState::OutcomeUnknown
                && incoming.state == CodingAgentRunState::Lost)
        }
        CodingAgentExecutionState::Completed => false,
    };
    if !execution_transition_valid {
        return Err("CodingAgentRun execution state regressed across observations".to_string());
    }
    Ok(())
}

fn terminal_for_state(
    run: &CodingAgentRunSnapshot,
    execution_state: CodingAgentExecutionState,
) -> Result<&CodingAgentTerminal, String> {
    if run.execution_state != execution_state {
        return Err("terminal CodingAgentRun execution_state is inconsistent".to_string());
    }
    run.terminal
        .as_ref()
        .ok_or_else(|| "terminal CodingAgentRun snapshot lacks terminal metadata".to_string())
}

fn validate_coding_agent_error(error: &CodingAgentError) -> Result<(), String> {
    validate_low_cardinality_kind(
        &error.code,
        "CodingAgentRun error code",
        CODING_AGENT_MAX_ERROR_KIND_BYTES,
    )?;
    if error.message.len() > CODING_AGENT_MAX_ERROR_MESSAGE_BYTES {
        return Err("CodingAgentRun error message is too large".to_string());
    }
    if let Some(failure_kind) = error.failure_kind.as_deref() {
        validate_low_cardinality_kind(
            failure_kind,
            "CodingAgentRun failure kind",
            CODING_AGENT_MAX_ERROR_KIND_BYTES,
        )?;
    }
    if let Some(recovery_kind) = error.recovery_kind.as_deref() {
        if !matches!(
            recovery_kind,
            "fix_input"
                | "retry_same"
                | "reobserve"
                | "reconcile"
                | "wait"
                | "user_action"
                | "none"
        ) {
            return Err("CodingAgentRun recovery kind is invalid".to_string());
        }
    }
    Ok(())
}

fn validate_low_cardinality_kind(value: &str, field: &str, max_bytes: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > max_bytes
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(format!("{field} is invalid"));
    }
    Ok(())
}

fn validate_identifier(
    value: &str,
    field: &str,
    max_bytes: usize,
    lowercase_only: bool,
) -> Result<(), String> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(format!("{field} must contain 1..={max_bytes} bytes"));
    }
    let valid = value.bytes().all(|byte| {
        (if lowercase_only {
            byte.is_ascii_lowercase()
        } else {
            byte.is_ascii_alphanumeric()
        }) || byte.is_ascii_digit()
            || matches!(byte, b'_' | b'-' | b'.')
    });
    if !valid {
        return Err(format!("{field} contains unsupported characters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_request() -> CodingAgentRequest {
        CodingAgentRequest::Start(CodingAgentStartRequest {
            run_id: "wc_agent_run_0123456789abcdef".to_string(),
            intent_fingerprint: "cafebabe".to_string(),
            authority_fingerprint: "auth_0123456789abcdef".to_string(),
            runtime_project_id: "agent:test:demo".to_string(),
            project_root: "/tmp/demo".to_string(),
            provider_id: "codex".to_string(),
            provider_instance_id: "provider_123".to_string(),
            instruction: "inspect the repository".to_string(),
            config: BTreeMap::new(),
            timeout_secs: 60,
        })
    }

    #[test]
    fn event_kind_model_vocabulary_is_exact_snake_case() {
        let cases = [
            (CodingAgentEventKind::AgentMessage, "agent_message"),
            (CodingAgentEventKind::Reasoning, "reasoning"),
            (CodingAgentEventKind::Plan, "plan"),
            (CodingAgentEventKind::ToolActivity, "tool_activity"),
            (CodingAgentEventKind::FileChange, "file_change"),
            (CodingAgentEventKind::TerminalActivity, "terminal_activity"),
            (CodingAgentEventKind::Usage, "usage"),
            (
                CodingAgentEventKind::PermissionRequest,
                "permission_request",
            ),
            (CodingAgentEventKind::Terminal, "terminal"),
        ];
        for (kind, expected) in cases {
            assert_eq!(kind.as_str(), expected);
            assert_eq!(serde_json::to_value(&kind).unwrap(), expected);
        }
    }

    #[test]
    fn run_and_execution_state_wire_vocabulary_is_exact_and_parseable() {
        for (state, expected) in [
            (CodingAgentRunState::Starting, "starting"),
            (CodingAgentRunState::Running, "running"),
            (CodingAgentRunState::WaitingPermission, "waiting_permission"),
            (CodingAgentRunState::Completed, "completed"),
            (CodingAgentRunState::Failed, "failed"),
            (CodingAgentRunState::Cancelled, "cancelled"),
            (CodingAgentRunState::Lost, "lost"),
        ] {
            assert_eq!(state.as_str(), expected);
            assert_eq!(CodingAgentRunState::from_str(expected), Some(state.clone()));
            assert_eq!(serde_json::to_value(&state).unwrap(), expected);
        }
        assert!(CodingAgentRunState::from_str("future_state").is_none());

        for (state, expected) in [
            (CodingAgentExecutionState::NotStarted, "not_started"),
            (CodingAgentExecutionState::Started, "started"),
            (CodingAgentExecutionState::OutcomeUnknown, "outcome_unknown"),
            (CodingAgentExecutionState::Completed, "completed"),
        ] {
            assert_eq!(state.as_str(), expected);
            assert_eq!(CodingAgentExecutionState::from_str(expected), Some(state));
            assert_eq!(serde_json::to_value(state).unwrap(), expected);
        }
        assert!(CodingAgentExecutionState::from_str("future_state").is_none());
    }

    #[test]
    fn response_validation_closes_error_and_model_facing_string_bounds() {
        let request = test_request();
        let valid_error = CodingAgentResponse::error(
            CodingAgentDispatchState::NotStarted,
            "coding_agent_unavailable",
            "provider unavailable",
            Some("unavailable"),
            Some("reobserve"),
        );
        validate_response_for_request(&request, &valid_error).unwrap();

        let mut invalid = valid_error.clone();
        invalid.error.as_mut().unwrap().code = "PRIVATE arbitrary / path".to_string();
        assert!(validate_response_for_request(&request, &invalid).is_err());

        let mut invalid = valid_error.clone();
        invalid.error.as_mut().unwrap().recovery_kind = Some("blind_retry".to_string());
        assert!(validate_response_for_request(&request, &invalid).is_err());

        let mut invalid = valid_error;
        invalid.error.as_mut().unwrap().message =
            "x".repeat(CODING_AGENT_MAX_ERROR_MESSAGE_BYTES + 1);
        assert!(validate_response_for_request(&request, &invalid).is_err());

        let run = CodingAgentRunSnapshot {
            run_id: request.run_id().to_string(),
            intent_fingerprint: "cafebabe".to_string(),
            authority_fingerprint: "auth_0123456789abcdef".to_string(),
            runtime_project_id: "agent:test:demo".to_string(),
            provider_id: "codex".to_string(),
            provider_instance_id: "provider_123".to_string(),
            state: CodingAgentRunState::Running,
            execution_state: CodingAgentExecutionState::Started,
            observation_revision: 1,
            created_at: 1,
            updated_at: 1,
            terminal: None,
        };
        let response = CodingAgentResponse::success(CodingAgentResponsePayload::Observe {
            observation: CodingAgentObserveResult {
                run,
                events: vec![CodingAgentEvent {
                    sequence: 1,
                    kind: CodingAgentEventKind::ToolActivity,
                    text: None,
                    label: None,
                    status: Some("x".repeat(CODING_AGENT_MAX_EVENT_METADATA_BYTES + 1)),
                    usage: None,
                }],
                first_retained_sequence: 1,
                next_sequence: 1,
                has_more: false,
                history_lost: false,
            },
        });
        let observe_request = CodingAgentRequest::Observe(CodingAgentObserveRequest {
            run_id: request.run_id().to_string(),
            after_sequence: None,
            limit: 8,
            wait_secs: 0,
        });
        assert!(validate_response_for_request(&observe_request, &response).is_err());
    }

    fn snapshot(
        state: CodingAgentRunState,
        execution_state: CodingAgentExecutionState,
        stop_reason: Option<&str>,
        error_code: Option<&str>,
    ) -> CodingAgentRunSnapshot {
        CodingAgentRunSnapshot {
            run_id: "wc_agent_run_semantic_matrix".to_string(),
            intent_fingerprint: "cafebabe".to_string(),
            authority_fingerprint: "auth_0123456789abcdef".to_string(),
            runtime_project_id: "agent:test:demo".to_string(),
            provider_id: "codex".to_string(),
            provider_instance_id: "provider_123".to_string(),
            state,
            execution_state,
            observation_revision: 1,
            created_at: 1,
            updated_at: 1,
            terminal: stop_reason.or(error_code).map(|_| CodingAgentTerminal {
                stop_reason: stop_reason.map(str::to_string),
                error_code: error_code.map(str::to_string),
                message: None,
                completed_at: 1,
            }),
        }
    }

    #[test]
    fn run_snapshot_semantic_matrix_is_closed_and_fail_closed() {
        for valid in [
            snapshot(
                CodingAgentRunState::Starting,
                CodingAgentExecutionState::NotStarted,
                None,
                None,
            ),
            snapshot(
                CodingAgentRunState::Running,
                CodingAgentExecutionState::OutcomeUnknown,
                None,
                None,
            ),
            snapshot(
                CodingAgentRunState::Running,
                CodingAgentExecutionState::Started,
                None,
                None,
            ),
            snapshot(
                CodingAgentRunState::WaitingPermission,
                CodingAgentExecutionState::Started,
                None,
                None,
            ),
            snapshot(
                CodingAgentRunState::Completed,
                CodingAgentExecutionState::Completed,
                Some(CODING_AGENT_STOP_REASON_END_TURN),
                None,
            ),
            snapshot(
                CodingAgentRunState::Cancelled,
                CodingAgentExecutionState::Completed,
                Some(CODING_AGENT_STOP_REASON_CANCELLED),
                None,
            ),
            {
                let mut run = snapshot(
                    CodingAgentRunState::Cancelled,
                    CodingAgentExecutionState::NotStarted,
                    None,
                    None,
                );
                run.terminal = Some(CodingAgentTerminal {
                    stop_reason: None,
                    error_code: None,
                    message: Some("ACP prompt was not dispatched".to_string()),
                    completed_at: 1,
                });
                run
            },
            snapshot(
                CodingAgentRunState::Failed,
                CodingAgentExecutionState::Completed,
                Some(CODING_AGENT_STOP_REASON_MAX_TOKENS),
                Some(CODING_AGENT_STOP_REASON_MAX_TOKENS),
            ),
            snapshot(
                CodingAgentRunState::Failed,
                CodingAgentExecutionState::Completed,
                Some(CODING_AGENT_STOP_REASON_MAX_TURN_REQUESTS),
                Some(CODING_AGENT_STOP_REASON_MAX_TURN_REQUESTS),
            ),
            snapshot(
                CodingAgentRunState::Failed,
                CodingAgentExecutionState::Completed,
                Some(CODING_AGENT_STOP_REASON_REFUSAL),
                Some(CODING_AGENT_STOP_REASON_REFUSAL),
            ),
            snapshot(
                CodingAgentRunState::Failed,
                CodingAgentExecutionState::NotStarted,
                None,
                Some("setup_failed"),
            ),
            snapshot(
                CodingAgentRunState::Failed,
                CodingAgentExecutionState::Completed,
                None,
                Some("prompt_error"),
            ),
            snapshot(
                CodingAgentRunState::Lost,
                CodingAgentExecutionState::OutcomeUnknown,
                None,
                Some("coding_agent_transport_lost"),
            ),
        ] {
            validate_coding_agent_run_snapshot(&valid).unwrap();
        }

        let invalid = [
            snapshot(
                CodingAgentRunState::Completed,
                CodingAgentExecutionState::Completed,
                Some(CODING_AGENT_STOP_REASON_REFUSAL),
                Some(CODING_AGENT_STOP_REASON_REFUSAL),
            ),
            snapshot(
                CodingAgentRunState::Failed,
                CodingAgentExecutionState::Completed,
                Some(CODING_AGENT_STOP_REASON_END_TURN),
                Some(CODING_AGENT_STOP_REASON_END_TURN),
            ),
            snapshot(
                CodingAgentRunState::Cancelled,
                CodingAgentExecutionState::Completed,
                Some(CODING_AGENT_STOP_REASON_MAX_TOKENS),
                Some(CODING_AGENT_STOP_REASON_MAX_TOKENS),
            ),
            snapshot(
                CodingAgentRunState::Cancelled,
                CodingAgentExecutionState::NotStarted,
                Some(CODING_AGENT_STOP_REASON_CANCELLED),
                None,
            ),
            snapshot(
                CodingAgentRunState::Cancelled,
                CodingAgentExecutionState::NotStarted,
                None,
                None,
            ),
            snapshot(
                CodingAgentRunState::Lost,
                CodingAgentExecutionState::OutcomeUnknown,
                Some(CODING_AGENT_STOP_REASON_END_TURN),
                None,
            ),
            snapshot(
                CodingAgentRunState::Running,
                CodingAgentExecutionState::Completed,
                None,
                None,
            ),
            snapshot(
                CodingAgentRunState::WaitingPermission,
                CodingAgentExecutionState::OutcomeUnknown,
                None,
                None,
            ),
            snapshot(
                CodingAgentRunState::Failed,
                CodingAgentExecutionState::Completed,
                Some("future_stop_reason"),
                Some("future_stop_reason"),
            ),
        ];
        for invalid in invalid {
            assert!(
                validate_coding_agent_run_snapshot(&invalid).is_err(),
                "{invalid:?}"
            );
        }

        let mut nonterminal_with_terminal = snapshot(
            CodingAgentRunState::Running,
            CodingAgentExecutionState::Started,
            None,
            None,
        );
        nonterminal_with_terminal.terminal = Some(CodingAgentTerminal {
            stop_reason: None,
            error_code: Some("impossible".to_string()),
            message: None,
            completed_at: 1,
        });
        assert!(validate_coding_agent_run_snapshot(&nonterminal_with_terminal).is_err());

        let mut terminal_without_metadata = snapshot(
            CodingAgentRunState::Completed,
            CodingAgentExecutionState::Completed,
            Some(CODING_AGENT_STOP_REASON_END_TURN),
            None,
        );
        terminal_without_metadata.terminal = None;
        assert!(validate_coding_agent_run_snapshot(&terminal_without_metadata).is_err());
    }

    #[test]
    fn observation_merge_is_revision_monotonic_and_terminal_absorbing() {
        let mut running = snapshot(
            CodingAgentRunState::Running,
            CodingAgentExecutionState::Started,
            None,
            None,
        );
        running.observation_revision = 1;

        let mut completed = snapshot(
            CodingAgentRunState::Completed,
            CodingAgentExecutionState::Completed,
            Some(CODING_AGENT_STOP_REASON_END_TURN),
            None,
        );
        completed.observation_revision = 2;
        completed.updated_at = 2;
        completed.terminal.as_mut().unwrap().completed_at = 2;

        assert_eq!(
            merge_coding_agent_run_snapshot(&running, &completed).unwrap(),
            CodingAgentObservationMerge::Advance
        );
        assert_eq!(
            merge_coding_agent_run_snapshot(&completed, &running).unwrap(),
            CodingAgentObservationMerge::Stale
        );
        assert_eq!(
            merge_coding_agent_run_snapshot(&completed, &completed).unwrap(),
            CodingAgentObservationMerge::ExactReplay
        );

        let mut same_revision_conflict = running.clone();
        same_revision_conflict.observation_revision = 2;
        same_revision_conflict.updated_at = 2;
        assert!(merge_coding_agent_run_snapshot(&completed, &same_revision_conflict).is_err());

        let mut terminal_regression = running.clone();
        terminal_regression.observation_revision = 3;
        terminal_regression.updated_at = 3;
        assert!(merge_coding_agent_run_snapshot(&completed, &terminal_regression).is_err());

        let mut lost = snapshot(
            CodingAgentRunState::Lost,
            CodingAgentExecutionState::OutcomeUnknown,
            None,
            Some("coding_agent_transport_lost"),
        );
        lost.observation_revision = 2;
        lost.updated_at = 2;
        lost.terminal.as_mut().unwrap().completed_at = 2;
        assert_eq!(
            merge_coding_agent_run_snapshot(&running, &lost).unwrap(),
            CodingAgentObservationMerge::Advance
        );

        let mut completed_after_lost = completed.clone();
        completed_after_lost.observation_revision = 4;
        completed_after_lost.updated_at = 4;
        completed_after_lost.terminal.as_mut().unwrap().completed_at = 4;
        assert_eq!(
            merge_coding_agent_run_snapshot(&lost, &completed_after_lost).unwrap(),
            CodingAgentObservationMerge::Advance,
            "Lost is outcome-unknown recovery state, not definitive effect truth"
        );

        let mut active_after_lost = running.clone();
        active_after_lost.observation_revision = 3;
        active_after_lost.updated_at = 3;
        assert!(merge_coding_agent_run_snapshot(&lost, &active_after_lost).is_err());

        let mut execution_regression = running.clone();
        execution_regression.observation_revision = 2;
        execution_regression.updated_at = 2;
        execution_regression.execution_state = CodingAgentExecutionState::OutcomeUnknown;
        assert!(merge_coding_agent_run_snapshot(&running, &execution_regression).is_err());

        let mut identity_change = completed.clone();
        identity_change.observation_revision = 3;
        identity_change.provider_instance_id = "provider_456".to_string();
        assert!(merge_coding_agent_run_snapshot(&completed, &identity_change).is_err());

        let mut uncertain = snapshot(
            CodingAgentRunState::Running,
            CodingAgentExecutionState::OutcomeUnknown,
            None,
            None,
        );
        uncertain.observation_revision = 1;
        let mut proved_not_started = snapshot(
            CodingAgentRunState::Cancelled,
            CodingAgentExecutionState::NotStarted,
            None,
            None,
        );
        proved_not_started.observation_revision = 2;
        proved_not_started.updated_at = 2;
        proved_not_started.terminal = Some(CodingAgentTerminal {
            stop_reason: None,
            error_code: None,
            message: Some("ACP prompt was not dispatched".to_string()),
            completed_at: 2,
        });
        assert_eq!(
            merge_coding_agent_run_snapshot(&uncertain, &proved_not_started).unwrap(),
            CodingAgentObservationMerge::Advance
        );
    }

    #[test]
    fn response_validation_reuses_snapshot_semantics() {
        let request = test_request();
        let mut contradictory = snapshot(
            CodingAgentRunState::Completed,
            CodingAgentExecutionState::Completed,
            Some(CODING_AGENT_STOP_REASON_REFUSAL),
            Some(CODING_AGENT_STOP_REASON_REFUSAL),
        );
        contradictory.run_id = request.run_id().to_string();
        let response =
            CodingAgentResponse::success(CodingAgentResponsePayload::Start { run: contradictory });
        assert!(validate_response_for_request(&request, &response).is_err());
    }

    #[test]
    fn request_round_trip_is_closed_and_bounded() {
        let request = CodingAgentRequest::Start(CodingAgentStartRequest {
            run_id: "wc_agent_run_0123456789abcdef".to_string(),
            intent_fingerprint: "cafebabe".to_string(),
            authority_fingerprint: "auth_0123456789abcdef".to_string(),
            runtime_project_id: "agent:test:demo".to_string(),
            project_root: "/tmp/demo".to_string(),
            provider_id: "codex".to_string(),
            provider_instance_id: "provider_123".to_string(),
            instruction: "inspect the repository".to_string(),
            config: BTreeMap::new(),
            timeout_secs: 60,
        });
        validate_request(&request).unwrap();
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("method"));
        assert!(!json.contains("argv"));
        assert_eq!(
            serde_json::from_str::<CodingAgentRequest>(&json).unwrap(),
            request
        );
    }
}
