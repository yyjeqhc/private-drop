//! Compatibility facade and root adapters for the Workflow Session domain crate.

#[allow(unused_imports)]
pub(crate) use webcodex_workflow_session::{
    aggregate_console_list, canonical_tool_call_finished_events, current_attempt_event_view,
    execution_output_summary_for_tool_result, exploration_tool_kind,
    is_tool_call_expectation_metadata_field, is_valid_session_id, normalize_observed_project_path,
    redact_and_bound_instruction, safe_model_facing_assertion_name,
    strip_tool_call_expectation_metadata, tool_failure_summary_from_events,
    tool_supports_model_facing_assertion_name, validate_model_facing_assertion_name,
    validate_model_facing_result_expectation, CodingSessionError, CodingSessionRequest,
    CompleteSessionMessageInput, ConsoleValidationHooks, ExplorationToolKind,
    ListSessionMessagesFilter, PostSessionMessageInput, RecordedModelFacingToolCall,
    ReplaceSessionMessageInput, SessionAckObservation, SessionCloseError,
    SessionContextRevisionAck, SessionCreateOptions, SessionDiscussionCounts,
    SessionDiscussionSummary, SessionEvent, SessionExecutionContext,
    SessionExecutionContextUpdateError, SessionGuardDenial, SessionGuards, SessionLifecycle,
    SessionLifecycleDenial, SessionMessage, SessionMessageError, SessionMessageKind,
    SessionMessageObservationError, SessionMessagePriority, SessionMessageStatus, SessionPathHint,
    SessionStore, SessionSummary, SessionToolContract, SessionTransport, ToolCallRecorderMetadata,
    ToolCallSessionMessageResolution, ToolCallStart, WorkflowSessionConsoleAggregate,
    WorkflowSessionConsoleAttentionOverview, WorkflowSessionConsoleDetail,
    WorkflowSessionConsoleList, WorkflowSessionConsoleListItem, DEFAULT_MAX_EVENTS_PER_SESSION,
    DEFAULT_MAX_SESSIONS, MAX_CODING_INSTRUCTION_CHARS, MAX_MESSAGE_COMPLETION_KEY_CHARS,
    MAX_MESSAGE_LIST_LIMIT, MAX_MESSAGE_RESOLUTION_CHARS,
    MAX_MODEL_VALIDATION_ASSERTION_NAME_CHARS, MAX_TOOL_CALL_ACK_MESSAGE_IDS,
    SESSION_INBOX_HIGH_GUIDANCE_ATTENTION_INSTRUCTION,
    SESSION_INBOX_HIGH_GUIDANCE_ATTENTION_REASON, TOOL_ACCEPTED_EXIT_CODES_FIELD,
    TOOL_ASSERTION_NAME_FIELD, TOOL_CALL_ACK_SESSION_CONTEXT_REVISION_FIELD,
    TOOL_CALL_ACK_SESSION_MESSAGE_IDS_FIELD, TOOL_CALL_RECORDING_SESSION_ID_FIELD,
    TOOL_CALL_SESSION_MESSAGE_RESOLUTION_FIELD, TOOL_EXPECTATION_RESULT_UNEXPECTED_FAILURE,
    TOOL_RESULT_EXPECTATION_FIELD,
};

use super::metadata::{ToolPathHint, ToolRisk};
use super::tool_definition::{
    runtime_tool_accepts_context_ack, runtime_tool_advances_context_checkpoint,
    runtime_tool_is_change_summary_like, runtime_tool_is_git_like, runtime_tool_is_read_like,
    runtime_tool_is_shell_like, runtime_tool_is_write_like, runtime_tool_metadata,
    runtime_tool_session_risk_class,
};

/// Project the canonical root tool declaration into the protocol-neutral facts
/// consumed by the Workflow Session ledger. No tool catalog is owned here.
pub(crate) fn session_tool_contract(tool_name: &str) -> SessionToolContract {
    let metadata = runtime_tool_metadata(tool_name);
    SessionToolContract {
        risk_class: runtime_tool_session_risk_class(tool_name),
        read_like: runtime_tool_is_read_like(tool_name),
        write_like: runtime_tool_is_write_like(tool_name),
        shell_like: runtime_tool_is_shell_like(tool_name),
        git_like: runtime_tool_is_git_like(tool_name),
        change_summary_like: runtime_tool_is_change_summary_like(tool_name),
        project_write: metadata.risk == ToolRisk::ProjectWrite,
        path_hint: match metadata.path_hint {
            ToolPathHint::None => SessionPathHint::None,
            ToolPathHint::SinglePath => SessionPathHint::SinglePath,
            ToolPathHint::PathList => SessionPathHint::PathList,
            ToolPathHint::Patch => SessionPathHint::Patch,
            ToolPathHint::Artifact => SessionPathHint::Artifact,
        },
        accepts_context_ack: runtime_tool_accepts_context_ack(tool_name),
        advances_context_checkpoint: runtime_tool_advances_context_checkpoint(tool_name),
    }
}

pub(crate) fn console_validation_hooks() -> ConsoleValidationHooks {
    ConsoleValidationHooks {
        event_observes_validation_activity:
            super::validation_events::event_observes_validation_activity,
        validation_summary_from_events: super::validation_events::validation_summary_from_events,
    }
}

#[cfg(test)]
pub(crate) const TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT: &str =
    webcodex_workflow_session::TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT;

#[cfg(test)]
pub(crate) use webcodex_workflow_session::root_test_support::{
    session_input_summary_for_tool, TOOL_CALL_EXPECTATION_METADATA_FIELDS,
};

#[cfg(test)]
pub(crate) mod events {
    use serde_json::Value;
    pub(crate) use webcodex_workflow_session::normalize_observed_project_path;
    pub(crate) use webcodex_workflow_session::root_test_support::{
        observed_paths_for_successful_result, session_input_summary_for_tool,
    };

    #[derive(Debug, Clone, Copy)]
    pub(crate) struct SessionToolClassification {
        pub(crate) risk_class: &'static str,
    }

    impl SessionToolClassification {
        pub(crate) fn for_tool(tool_name: &str) -> Self {
            Self {
                risk_class: super::session_tool_contract(tool_name).risk_class,
            }
        }
    }

    pub(crate) fn changed_paths_for_tool(tool_name: &str, arguments: &Value) -> Vec<String> {
        webcodex_workflow_session::root_test_support::changed_paths_for_tool(
            super::session_tool_contract(tool_name),
            arguments,
        )
    }
}

#[cfg(test)]
pub(crate) mod model {
    pub(crate) use webcodex_workflow_session::root_test_support::{
        PersistedSessionLedger, MAX_OBSERVED_PATHS_PER_EVENT, SESSION_LEDGER_VERSION,
    };
}

#[cfg(test)]
pub(crate) mod persistence {
    pub(crate) use webcodex_workflow_session::root_test_support::write_ledger_atomic;
}

#[cfg(test)]
mod tests;
