//! Stable protocol-neutral contracts shared with the Workflow Session domain.

use serde::{Deserialize, Serialize};

pub const SESSION_ID_PREFIX: &str = "wc_sess_";

pub const MAX_MODEL_VALIDATION_ASSERTION_NAME_CHARS: usize =
    crate::runner_protocol::VALIDATION_ASSERTION_NAME_MAX_CHARS;
pub const TOOL_CALL_RECORDING_SESSION_ID_FIELD: &str = "recording_session_id";
pub const TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD: &str = "ack_session_message_ids";
pub const TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD: &str = "session_message_resolution";
pub const TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD: &str = "ack_session_context_revision";
pub const MAX_TOOL_CALL_ACK_MESSAGE_IDS: usize = 8;
pub const TOOL_EXPECTED_FAILURE_FIELD: &str = "expected_failure";
pub const TOOL_EXPECTED_FAILURE_KIND_FIELD: &str = "expected_failure_kind";
pub const TOOL_RESULT_EXPECTATION_FIELD: &str = "result_expectation";
pub const TOOL_ACCEPTED_EXIT_CODES_FIELD: &str = "accepted_exit_codes";
pub const TOOL_ASSERTION_NAME_FIELD: &str = "assertion_name";
pub const TOOL_CALL_EXPECTATION_METADATA_FIELDS: &[&str] = &[
    TOOL_EXPECTED_FAILURE_FIELD,
    TOOL_EXPECTED_FAILURE_KIND_FIELD,
    TOOL_RESULT_EXPECTATION_FIELD,
    TOOL_ACCEPTED_EXIT_CODES_FIELD,
    TOOL_ASSERTION_NAME_FIELD,
];

pub fn is_tool_call_expectation_metadata_field(field: &str) -> bool {
    TOOL_CALL_EXPECTATION_METADATA_FIELDS.contains(&field)
}

pub fn is_valid_session_id(session_id: &str) -> bool {
    session_id.starts_with(SESSION_ID_PREFIX)
        && session_id.len() > SESSION_ID_PREFIX.len()
        && session_id
            .as_bytes()
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

pub const EXPLORATION_TOOL_NAMES: &[&str] = &[
    "read_file",
    "read_files",
    "search_project_text",
    "search_project_texts",
    "document_symbols",
    "document_diagnostics",
    "hover",
    "workspace_symbols",
    "goto_definition",
    "find_references",
    "call_hierarchy",
];

pub const SESSION_INBOX_HIGH_GUIDANCE_ATTENTION_REASON: &str =
    "high_priority_guidance_requires_ack";
pub const SESSION_INBOX_HIGH_GUIDANCE_ATTENTION_INSTRUCTION: &str =
    "High-priority Session guidance is pending. Read session_discussion_summary before continuing.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionShell {
    Sh,
    Bash,
}

impl ExecutionShell {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sh => "sh",
            Self::Bash => "bash",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    #[default]
    Normal,
    ReadOnly,
}

impl SessionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::ReadOnly => "read_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOutcome {
    AutoApproved,
    Approved,
    Denied,
    Pending,
    HardDenied,
}

impl PermissionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AutoApproved => "auto_approved",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Pending => "requested",
            Self::HardDenied => "hard_denied",
        }
    }

    pub fn allows_execution(self) -> bool {
        matches!(self, Self::AutoApproved | Self::Approved)
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "auto_approved" => Some(Self::AutoApproved),
            "approved" => Some(Self::Approved),
            "denied" | "expired" => Some(Self::Denied),
            "requested" | "pending" => Some(Self::Pending),
            "hard_denied" => Some(Self::HardDenied),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionDecision {
    pub required: bool,
    pub policy: String,
    pub request_id: String,
    pub status: String,
    pub reason: String,
    pub risk: String,
    pub tool_name: String,
    pub project: Option<String>,
}

impl PermissionDecision {
    pub fn outcome(&self) -> Option<PermissionOutcome> {
        PermissionOutcome::parse(&self.status)
    }

    pub fn allows_execution(&self) -> bool {
        self.outcome()
            .map(PermissionOutcome::allows_execution)
            .unwrap_or(false)
    }
}

pub fn is_safe_job_id(job_id: &str) -> bool {
    if job_id.is_empty() || job_id.len() > 80 || job_id.contains("..") {
        return false;
    }
    job_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}
