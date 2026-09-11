//! Edit tool usage telemetry (phase 1).
//!
//! Emits always-on structured logs for edit-surface tool calls so operators can
//! measure how often the canonical edit tools (`apply_text_edits`, `apply_patch`,
//! `apply_unified_diff`) are used relative to the intentional whole-file
//! rewrite path (`write_project_file`).
//!
//! Design constraints:
//! - No new database tables, Action Audit columns, session ledger fields, or
//!   OpenAPI/MCP/schema changes.
//! - Never log arguments, file paths, file contents, patches, secrets, or tokens.
//! - Reuses existing `tracing` infrastructure (same family as `tool_request_trace`).
//! - Does not change tool execution semantics, permissions, or session behavior.

use super::{ToolCall, ToolResult};
use std::time::Instant;

/// High-level tool family for this telemetry stream.
pub(crate) const TELEMETRY_CATEGORY_EDIT: &str = "edit";

/// Event name written to structured logs / metrics pipelines.
pub(crate) const EDIT_TOOL_USAGE_EVENT: &str = "edit_tool_usage";

/// How a specific edit tool sits on the preferred-vs-legacy surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditToolSurface {
    /// Preferred precise/local or unified-diff multi-file paths.
    Canonical,
    /// Valid but non-preferred specialized path (intentional whole-file write).
    Advanced,
}

impl EditToolSurface {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::Advanced => "advanced",
        }
    }
}

/// Classify an edit tool, or `None` when the tool is outside the edit surface
/// tracked by this phase-1 telemetry.
pub(crate) fn edit_tool_surface(tool_name: &str) -> Option<EditToolSurface> {
    match tool_name {
        "apply_text_edits" | "apply_patch" | "apply_unified_diff" => {
            Some(EditToolSurface::Canonical)
        }
        "write_project_file" => Some(EditToolSurface::Advanced),
        _ => None,
    }
}

/// Safe, argument-free usage record for one edit tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditToolUsageRecord {
    pub(crate) tool_name: &'static str,
    pub(crate) category: &'static str,
    pub(crate) edit_surface: EditToolSurface,
    pub(crate) success: bool,
    pub(crate) duration_ms: u64,
    /// Optional coarse error classification (never free-form user content).
    pub(crate) error_kind: Option<&'static str>,
    pub(crate) requested_matching_mode: Option<&'static str>,
    pub(crate) selected_match_mode: Option<&'static str>,
    pub(crate) candidate_count_bucket: Option<&'static str>,
    pub(crate) classification: Option<&'static str>,
    pub(crate) execution_state: Option<&'static str>,
    pub(crate) recovery_action: Option<&'static str>,
}

/// Start a usage timer when `tool_name` is an edit-surface tool.
pub(crate) fn start_edit_tool_usage(tool_name: &'static str) -> Option<EditToolUsageGuard> {
    let edit_surface = edit_tool_surface(tool_name)?;
    Some(EditToolUsageGuard {
        tool_name,
        edit_surface,
        started: Instant::now(),
        finished: false,
        requested_matching_mode: None,
    })
}

pub(crate) fn start_edit_tool_usage_for_call(call: &ToolCall) -> Option<EditToolUsageGuard> {
    let mut guard = start_edit_tool_usage(call.tool_name())?;
    if let ToolCall::ApplyPatch { matching_mode, .. } = call {
        guard.requested_matching_mode = Some(matching_mode.unwrap_or_default().as_str());
    }
    Some(guard)
}

/// RAII guard: records one structured log line when finished (or on drop if
/// the dispatch path aborts without an explicit finish).
pub(crate) struct EditToolUsageGuard {
    tool_name: &'static str,
    edit_surface: EditToolSurface,
    started: Instant,
    finished: bool,
    requested_matching_mode: Option<&'static str>,
}

impl EditToolUsageGuard {
    pub(crate) fn finish_with_result(&mut self, result: &ToolResult) {
        if self.finished {
            return;
        }
        self.finished = true;
        let record = EditToolUsageRecord {
            tool_name: self.tool_name,
            category: TELEMETRY_CATEGORY_EDIT,
            edit_surface: self.edit_surface,
            success: result.success,
            duration_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            error_kind: safe_error_kind(result),
            requested_matching_mode: self
                .requested_matching_mode
                .or_else(|| safe_requested_matching_mode(result)),
            selected_match_mode: safe_selected_match_mode(result),
            candidate_count_bucket: safe_candidate_count_bucket(result),
            classification: safe_classification(result),
            execution_state: safe_execution_state(result),
            recovery_action: safe_recovery_action(result),
        };
        emit_edit_tool_usage(&record);
    }

    fn finish_incomplete(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let record = EditToolUsageRecord {
            tool_name: self.tool_name,
            category: TELEMETRY_CATEGORY_EDIT,
            edit_surface: self.edit_surface,
            success: false,
            duration_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            error_kind: Some("incomplete"),
            requested_matching_mode: self.requested_matching_mode,
            selected_match_mode: None,
            candidate_count_bucket: None,
            classification: None,
            execution_state: None,
            recovery_action: None,
        };
        emit_edit_tool_usage(&record);
    }
}

impl Drop for EditToolUsageGuard {
    fn drop(&mut self) {
        self.finish_incomplete();
    }
}

/// Build a log-safe record and emit it. Public to unit tests as pure field set.
pub(crate) fn emit_edit_tool_usage(record: &EditToolUsageRecord) {
    #[cfg(test)]
    test_sink::push(record.clone());

    tracing::info!(
        event = EDIT_TOOL_USAGE_EVENT,
        tool_name = record.tool_name,
        category = record.category,
        edit_surface = record.edit_surface.as_str(),
        success = record.success,
        duration_ms = record.duration_ms,
        error_kind = record.error_kind.unwrap_or("-"),
        requested_matching_mode = record.requested_matching_mode.unwrap_or("-"),
        selected_match_mode = record.selected_match_mode.unwrap_or("-"),
        candidate_count_bucket = record.candidate_count_bucket.unwrap_or("-"),
        classification = record.classification.unwrap_or("-"),
        execution_state = record.execution_state.unwrap_or("-"),
        recovery_action = record.recovery_action.unwrap_or("-"),
        "{EDIT_TOOL_USAGE_EVENT}"
    );
}

fn safe_requested_matching_mode(result: &ToolResult) -> Option<&'static str> {
    safe_matching_mode(
        result
            .output
            .get("requested_matching_mode")
            .and_then(|value| value.as_str())?,
    )
}

fn safe_matching_mode(value: &str) -> Option<&'static str> {
    match value {
        "first_match" => Some("first_match"),
        "unique" => Some("unique"),
        "exact_unique" => Some("exact_unique"),
        _ => None,
    }
}

fn safe_match_mode(value: &str) -> Option<&'static str> {
    match value {
        "exact" => Some("exact"),
        "trim_end" => Some("trim_end"),
        "trim" => Some("trim"),
        "normalized" => Some("normalized"),
        _ => None,
    }
}

fn match_mode_rank(value: &str) -> u8 {
    match value {
        "exact" => 0,
        "trim_end" => 1,
        "trim" => 2,
        "normalized" => 3,
        _ => 0,
    }
}

fn safe_selected_match_mode(result: &ToolResult) -> Option<&'static str> {
    if let Some(mode) = result
        .output
        .get("match_rejection_diagnostic")
        .and_then(|value| value.get("match_mode"))
        .and_then(|value| value.as_str())
        .and_then(safe_match_mode)
    {
        return Some(mode);
    }
    result
        .output
        .get("files")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|file| file.get("edits").and_then(|value| value.as_array()))
        .flatten()
        .filter_map(|edit| edit.get("match_mode").and_then(|value| value.as_str()))
        .filter_map(safe_match_mode)
        .max_by_key(|mode| match_mode_rank(mode))
}

fn safe_candidate_count_bucket(result: &ToolResult) -> Option<&'static str> {
    let diagnostic_count = result
        .output
        .get("match_rejection_diagnostic")
        .and_then(|value| value.get("candidate_count"))
        .and_then(|value| value.as_u64());
    let success_max = result
        .output
        .get("files")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|file| file.get("edits").and_then(|value| value.as_array()))
        .flatten()
        .filter_map(|edit| edit.get("candidate_count").and_then(|value| value.as_u64()))
        .max();
    let count = diagnostic_count.or(success_max)?;
    Some(match count {
        0 => "0",
        1 => "1",
        2 => "2",
        3..=4 => "3-4",
        _ => "5+",
    })
}

fn safe_classification(result: &ToolResult) -> Option<&'static str> {
    if let Some(classification) = result
        .output
        .get("match_rejection_diagnostic")
        .and_then(|value| value.get("classification"))
        .and_then(|value| value.as_str())
    {
        return match classification {
            "unique_fuzzy_candidate" => Some("unique_fuzzy_candidate"),
            "ambiguous_candidate" => Some("ambiguous_candidate"),
            _ => None,
        };
    }
    match result
        .output
        .get("error_kind")
        .and_then(|value| value.as_str())
    {
        Some("context_mismatch") => Some("context_mismatch"),
        Some("matching_mode_rejected") => Some("matching_metadata_suppressed"),
        _ => None,
    }
}

fn safe_execution_state(result: &ToolResult) -> Option<&'static str> {
    match result
        .output
        .get("execution_state")
        .and_then(|value| value.as_str())
    {
        Some("not_started") => Some("not_started"),
        Some("completed") => Some("completed"),
        Some("outcome_unknown") => Some("outcome_unknown"),
        _ => None,
    }
}

fn safe_recovery_action(result: &ToolResult) -> Option<&'static str> {
    let raw = result
        .output
        .get("recovery_action")
        .and_then(|value| value.as_str())
        .or_else(|| {
            result
                .output
                .get("recovery")
                .and_then(|value| value.get("action"))
                .and_then(|value| value.as_str())
        })?;
    match raw {
        "read_files" => Some("read_files"),
        "read_equal_candidates_and_refine_context" => {
            Some("read_equal_candidates_and_refine_context")
        }
        "reread_and_regenerate_exact_unique_patch" => {
            Some("reread_and_regenerate_exact_unique_patch")
        }
        "read_equal_candidates_and_add_exact_context" => {
            Some("read_equal_candidates_and_add_exact_context")
        }
        "reread_and_regenerate_patch" => Some("reread_and_regenerate_patch"),
        "inspect_workspace_before_retry" => Some("inspect_workspace_before_retry"),
        "upgrade_or_reconnect_runner" => Some("upgrade_or_reconnect_runner"),
        "retry_same_after_runner_recovery" => Some("retry_same_after_runner_recovery"),
        _ => None,
    }
}

/// Extract only coarse, allowlisted error kinds from tool results.
///
/// Never returns free-form error messages (which may include paths or snippets).
fn safe_error_kind(result: &ToolResult) -> Option<&'static str> {
    if result.success {
        return None;
    }
    let raw = result
        .output
        .get("error_kind")
        .and_then(|v| v.as_str())
        .or_else(|| result.output.get("failure_kind").and_then(|v| v.as_str()))
        .or_else(|| result.output.get("code").and_then(|v| v.as_str()));
    raw.and_then(sanitize_error_kind).or(Some("runtime_error"))
}

fn sanitize_error_kind(kind: &str) -> Option<&'static str> {
    // Allowlist only; anything else collapses to a coarse bucket so free-form
    // codes/messages cannot leak into telemetry fields.
    match kind {
        "invalid_arguments" => Some("invalid_arguments"),
        "insufficient_scope" => Some("insufficient_scope"),
        "session_guard_denied" => Some("session_guard_denied"),
        "session_closed" => Some("session_closed"),
        "session_project_mismatch" => Some("session_project_mismatch"),
        "tool_disabled" => Some("tool_disabled"),
        "permission_denied" | "permission_execution_denied" => Some("permission_denied"),
        "policy_rejected" => Some("policy_rejected"),
        "confirmation_required" => Some("confirmation_required"),
        "agent_offline" | "agent_unavailable" => Some("agent_unavailable"),
        "timeout" => Some("timeout"),
        "not_started" => Some("not_started"),
        "outcome_unknown" => Some("outcome_unknown"),
        "context_mismatch" => Some("context_mismatch"),
        "matching_mode_rejected" => Some("matching_mode_rejected"),
        "agent_capability_unavailable" => Some("agent_capability_unavailable"),
        "not_found" => Some("not_found"),
        "runtime_error" => Some("runtime_error"),
        "incomplete" => Some("incomplete"),
        _ => None,
    }
}

/// True when a telemetry field map (or serialized log payload) would be unsafe.
/// Used by tests to assert absence of sensitive keys/values.
#[cfg(test)]
pub(crate) fn record_contains_sensitive_keys(record: &EditToolUsageRecord) -> bool {
    // Structural: only the known safe fields exist on the record type.
    // Also reject if any string field accidentally embeds path/content markers
    // from test fixtures that should never appear in telemetry.
    let surface = record.edit_surface.as_str();
    let kind = record.error_kind.unwrap_or("");
    let haystacks = [
        record.tool_name,
        record.category,
        surface,
        kind,
        record.requested_matching_mode.unwrap_or(""),
        record.selected_match_mode.unwrap_or(""),
        record.candidate_count_bucket.unwrap_or(""),
        record.classification.unwrap_or(""),
        record.execution_state.unwrap_or(""),
        record.recovery_action.unwrap_or(""),
    ];
    for h in haystacks {
        if h.contains('/') || h.contains('\\') || h.contains('\n') {
            return true;
        }
        for banned in [
            "content",
            "patch",
            "old_text",
            "new_text",
            "arguments",
            "secret",
            "token",
            "password",
        ] {
            // tool_name may legitimately contain none of these; category/surface
            // are fixed. error_kind allowlist is fixed. This is a safety net.
            if h.contains(banned) && h != record.tool_name {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod test_sink {
    use super::EditToolUsageRecord;
    use std::cell::RefCell;

    thread_local! {
        static EVENTS: RefCell<Vec<EditToolUsageRecord>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn push(record: EditToolUsageRecord) {
        EVENTS.with(|events| events.borrow_mut().push(record));
    }

    pub(crate) fn take() -> Vec<EditToolUsageRecord> {
        EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
    }

    pub(crate) fn clear() {
        let _ = take();
    }
}

#[cfg(test)]
pub(crate) use test_sink::{
    clear as clear_test_edit_tool_usage, take as take_test_edit_tool_usage,
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_canonical_and_advanced_edit_tools() {
        assert_eq!(
            edit_tool_surface("apply_text_edits"),
            Some(EditToolSurface::Canonical)
        );
        assert_eq!(
            edit_tool_surface("apply_patch"),
            Some(EditToolSurface::Canonical)
        );
        assert_eq!(
            edit_tool_surface("apply_unified_diff"),
            Some(EditToolSurface::Canonical)
        );
        assert_eq!(
            edit_tool_surface("write_project_file"),
            Some(EditToolSurface::Advanced)
        );
        // Removed legacy compatibility tools are no longer classified.
        for name in [
            "replace_in_file",
            "replace_exact_block",
            "insert_before_pattern",
            "insert_after_pattern",
            "replace_line_range",
            "insert_at_line",
            "delete_line_range",
            "apply_patch_checked",
            "validate_patch",
        ] {
            assert_eq!(edit_tool_surface(name), None, "{name}");
        }
    }

    #[test]
    fn non_edit_tools_are_not_tracked() {
        for name in [
            "read_files",
            "run_shell",
            "list_tools",
            "save_project_artifact",
            "git_status",
            "cargo_check",
        ] {
            assert_eq!(edit_tool_surface(name), None, "{name}");
        }
    }

    #[test]
    fn telemetry_record_has_no_sensitive_fields() {
        let record = EditToolUsageRecord {
            tool_name: "write_project_file",
            category: TELEMETRY_CATEGORY_EDIT,
            edit_surface: EditToolSurface::Advanced,
            success: false,
            duration_ms: 12,
            error_kind: Some("runtime_error"),
            requested_matching_mode: None,
            selected_match_mode: None,
            candidate_count_bucket: None,
            classification: None,
            execution_state: None,
            recovery_action: None,
        };
        assert!(!record_contains_sensitive_keys(&record));
        assert_eq!(record.category, "edit");
        assert_eq!(record.edit_surface.as_str(), "advanced");
        assert_eq!(record.tool_name, "write_project_file");
    }

    #[test]
    fn safe_error_kind_never_returns_freeform_messages() {
        let result = ToolResult::err_with_output(
            "failed to write /secret/path/with secrets token=abc",
            json!({
                "path": "/secret/path",
                "content": "user source code here",
                "patch": "@@ -1 +1 @@\n-secret",
                "error_kind": "policy_rejected",
            }),
        );
        assert_eq!(safe_error_kind(&result), Some("policy_rejected"));

        let unknown = ToolResult::err_with_output(
            "boom /tmp/foo",
            json!({ "error_kind": "something_custom_with_/path" }),
        );
        // Unknown kinds collapse to runtime_error rather than echoing free-form.
        assert_eq!(safe_error_kind(&unknown), Some("runtime_error"));
    }

    #[test]
    fn guard_emits_one_event_with_correct_name_and_surface() {
        clear_test_edit_tool_usage();
        let mut guard = start_edit_tool_usage("apply_text_edits").expect("edit tool");
        guard.finish_with_result(&ToolResult::ok(json!({ "ok": true })));
        drop(guard);
        let events = take_test_edit_tool_usage();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tool_name, "apply_text_edits");
        assert_eq!(events[0].category, "edit");
        assert_eq!(events[0].edit_surface, EditToolSurface::Canonical);
        assert!(events[0].success);
        assert!(events[0].error_kind.is_none());
        assert!(!record_contains_sensitive_keys(&events[0]));
    }

    #[test]
    fn apply_patch_telemetry_records_default_unique_and_bounded_match_facts() {
        clear_test_edit_tool_usage();
        let call = ToolCall::ApplyPatch {
            project: "agent:test:project".to_string(),
            patch: "*** Begin Patch\n*** Update File: secret.rs\n-old\n+new\n*** End Patch"
                .to_string(),
            dry_run: None,
            matching_mode: None,
            session_id: None,
        };
        let mut guard = start_edit_tool_usage_for_call(&call).expect("apply_patch edit tool");
        guard.finish_with_result(&ToolResult::err_with_output(
            "ambiguous",
            json!({
                "requested_matching_mode": "unique",
                "execution_state": "not_started",
                "error_kind": "matching_mode_rejected",
                "recovery_action": "read_equal_candidates_and_refine_context",
                "match_rejection_diagnostic": {
                    "classification": "ambiguous_candidate",
                    "match_mode": "exact",
                    "candidate_count": 2
                },
                "path": "/must/not/appear",
                "patch": "must not appear"
            }),
        ));
        drop(guard);

        let events = take_test_edit_tool_usage();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.requested_matching_mode, Some("unique"));
        assert_eq!(event.selected_match_mode, Some("exact"));
        assert_eq!(event.candidate_count_bucket, Some("2"));
        assert_eq!(event.classification, Some("ambiguous_candidate"));
        assert_eq!(event.execution_state, Some("not_started"));
        assert_eq!(
            event.recovery_action,
            Some("read_equal_candidates_and_refine_context")
        );
        assert_eq!(event.error_kind, Some("matching_mode_rejected"));
        assert!(!record_contains_sensitive_keys(event));
    }

    #[test]
    fn start_returns_none_for_non_edit_tools() {
        assert!(start_edit_tool_usage("read_files").is_none());
    }

    #[test]
    fn incomplete_drop_emits_failure_once() {
        clear_test_edit_tool_usage();
        {
            let _guard = start_edit_tool_usage("write_project_file").expect("edit tool");
            // drop without finish
        }
        let events = take_test_edit_tool_usage();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tool_name, "write_project_file");
        assert_eq!(events[0].edit_surface, EditToolSurface::Advanced);
        assert!(!events[0].success);
        assert_eq!(events[0].error_kind, Some("incomplete"));
    }
}
