//! Root adapter for canonical deterministic Workflow Session continuation projections.

use super::tool_definition::{
    runtime_tool_captures_validation_output, runtime_tool_is_git_like, runtime_tool_is_shell_like,
    runtime_tool_is_write_like,
};
use super::validation_events::CurrentValidationEvidenceProjection;

pub(crate) use webcodex_workflow_session::{
    continuation_feedback_value, not_applicable_continuation_feedback_value,
    validation_delta_value, ContinuationFeedbackInput, ContinuationProjectionHooks,
    ContinuationToolFailureSnapshot, ContinuationValidationSnapshot, EXPLORATION_CONTINUITY_ACTION,
};

pub(crate) fn continuation_projection_hooks() -> ContinuationProjectionHooks {
    ContinuationProjectionHooks::new(root_tool_is_meaningful)
}

pub(crate) fn continuation_validation_snapshot(
    current: &CurrentValidationEvidenceProjection,
) -> ContinuationValidationSnapshot<'_> {
    ContinuationValidationSnapshot::new(&current.evidence, &current.current_validation)
}

pub(crate) fn root_tool_is_meaningful(tool_name: &str) -> bool {
    runtime_tool_is_write_like(tool_name)
        || runtime_tool_is_shell_like(tool_name)
        || runtime_tool_is_git_like(tool_name)
        || runtime_tool_captures_validation_output(tool_name)
}
