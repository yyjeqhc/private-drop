use super::*;

pub(crate) const MAX_WRITE_CONTENT_BYTES: usize = 256 * 1024; // 256 KiB

/// Maximum serialized batch payload sent to the owning Runner. Host-only (the
/// agent enforces a per-file cap instead), so it stays local.
pub(crate) const MAX_APPLY_FILE_CHANGES_BYTES: usize = 1024 * 1024;

fn recoverable_write_rejection(reason: impl AsRef<str>) -> String {
    format!(
        "Rejected before write: {}.\nNo files were modified.\nRetry guidance: read the file again to refresh line numbers/context, then retry with updated guards.",
        reason.as_ref()
    )
}

fn structured_edit_not_started_result(tool_name: &str, reason: impl AsRef<str>) -> ToolResult {
    ToolResult::err_with_output(
        format!(
            "{tool_name} was not dispatched: {}. No files were modified by this request. Restore Runner availability, then retry with the same guards.",
            reason.as_ref()
        ),
        json!({
            "execution_state": "not_started",
            "state_changed": false,
            "error_kind": "not_started",
            "failure_kind": "not_started",
            "tool_failure": true,
            "recovery_action": "retry_same_after_runner_recovery",
        }),
    )
    .with_recovery(crate::tool_runtime::RecoveryKind::RetrySame, None)
}

fn structured_edit_outcome_unknown_result(
    tool_name: &str,
    reason: impl AsRef<str>,
    mut output: Value,
) -> ToolResult {
    if !output.is_object() {
        output = json!({});
    }
    let fields = output
        .as_object_mut()
        .expect("structured edit uncertainty output normalized to object");
    for key in [
        "conflict_recovery",
        "retry_guidance",
        "state_changed",
        "execution_state",
        "error_kind",
        "failure_kind",
        "tool_failure",
        "recovery_action",
        "recovery_kind",
        "recovery_tool",
        "error",
    ] {
        fields.remove(key);
    }
    fields.insert("execution_state".to_string(), json!("outcome_unknown"));
    fields.insert("state_changed".to_string(), Value::Null);
    fields.insert("error_kind".to_string(), json!("outcome_unknown"));
    fields.insert("failure_kind".to_string(), json!("outcome_unknown"));
    fields.insert("tool_failure".to_string(), json!(true));
    fields.insert(
        "recovery_action".to_string(),
        json!("inspect_workspace_before_retry"),
    );
    ToolResult::err_with_output(
        format!(
            "{tool_name} outcome is unknown: {}. The Runner may already have changed files. Inspect current workspace state before issuing another write.",
            reason.as_ref()
        ),
        output,
    )
    .with_recovery(crate::tool_runtime::RecoveryKind::Reobserve, None)
}

fn structured_edit_delivery_failure(
    tool_name: &str,
    reason: impl AsRef<str>,
    state: ShellCommandExecutionState,
) -> ToolResult {
    if state == ShellCommandExecutionState::NotStarted {
        structured_edit_not_started_result(tool_name, reason)
    } else {
        structured_edit_outcome_unknown_result(tool_name, reason, json!({}))
    }
}

async fn await_structured_edit_response(
    runtime: &ToolRuntime,
    request_id: &str,
    rx: tokio::sync::oneshot::Receiver<ShellRunResponse>,
    wait_timeout_secs: u64,
    tool_name: &str,
) -> Result<ShellRunResponse, ToolResult> {
    match tokio::time::timeout(Duration::from_secs(wait_timeout_secs + 4), rx).await {
        Ok(Ok(response)) => {
            if response.request_dispatched == Some(false) {
                return Err(structured_edit_not_started_result(
                    tool_name,
                    "the returned lifecycle evidence proves the request never left the queue",
                ));
            }
            if response.error.is_some() {
                return Err(structured_edit_outcome_unknown_result(
                    tool_name,
                    "the Runner returned a transport or execution error after dispatch",
                    json!({}),
                ));
            }
            if response.exit_code != Some(0) {
                return Err(structured_edit_outcome_unknown_result(
                    tool_name,
                    "the Runner returned a non-zero terminal result after dispatch",
                    json!({}),
                ));
            }
            Ok(response)
        }
        Ok(Err(_)) => {
            let state = dispatch_uncertainty_lifecycle(
                runtime
                    .runner_registry
                    .cancel_request_dispatch_state(request_id)
                    .await,
            );
            Err(structured_edit_delivery_failure(
                tool_name,
                "the Runner response channel closed before a trustworthy result was received",
                state,
            ))
        }
        Err(_) => {
            let state = dispatch_uncertainty_lifecycle(
                runtime
                    .runner_registry
                    .cancel_request_dispatch_state(request_id)
                    .await,
            );
            Err(structured_edit_delivery_failure(
                tool_name,
                format!("no trustworthy result arrived within {wait_timeout_secs} seconds"),
                state,
            ))
        }
    }
}

fn write_project_file_preflight_rejection(
    detail: impl Into<String>,
    error_kind: &'static str,
    retry_guidance: &'static str,
) -> ToolResult {
    let detail = detail.into();
    ToolResult::err_with_output(
        format!(
            "Rejected before write: {detail}.\nNo files were modified.\nRetry guidance: {retry_guidance}."
        ),
        json!({
            "changed": false,
            "state_changed": false,
            "execution_state": "not_started",
            "error_kind": error_kind,
            "retry_guidance": retry_guidance,
        }),
    )
    .with_recovery(crate::tool_runtime::RecoveryKind::FixInput, None)
}

fn apply_text_edit_occurrence_capability_rejection(reason: impl AsRef<str>) -> ToolResult {
    let reason = reason.as_ref();
    ToolResult::err_with_output(
        format!(
            "Rejected before write: {reason}.\nNo files were modified.\nRetry guidance: the accepted Runner violated the generation-2 apply_text_edit_occurrence baseline; reconnect it or refine the edit to a unique exact match without occurrence."
        ),
        json!({
            "state_changed": false,
            "error_kind": "agent_capability_unavailable",
            "failure_kind": "capability_unavailable",
            "capability": crate::runner_protocol::RUNNER_CAPABILITY_APPLY_TEXT_EDIT_OCCURRENCE,
            "retry_guidance": "reconnect the Runner or refine the edit to a unique exact match without occurrence"
        }),
    )
}

fn apply_text_edit_line_scope_capability_rejection(reason: impl AsRef<str>) -> ToolResult {
    let reason = reason.as_ref();
    ToolResult::err_with_output(
        format!(
            "Rejected before write: {reason}.\nNo files were modified.\nRetry guidance: reconnect a Runner that explicitly supports apply_text_edit_line_scope, or remove line_scope only if an unscoped exact edit is safe."
        ),
        json!({
            "state_changed": false,
            "error_kind": "agent_capability_unavailable",
            "failure_kind": "capability_unavailable",
            "capability": crate::runner_protocol::RUNNER_CAPABILITY_APPLY_TEXT_EDIT_LINE_SCOPE,
            "retry_guidance": "reconnect a Runner with apply_text_edit_line_scope support; never silently downgrade a scoped edit to an unscoped edit"
        }),
    )
}

fn apply_patch_capability_rejection(
    reason: impl AsRef<str>,
    capability: &'static str,
) -> ToolResult {
    let reason = reason.as_ref();
    ToolResult::err_with_output(
        format!(
            "Rejected before write: {reason}.\nNo files were modified.\nRetry guidance: reconnect a current Runner that explicitly supports {capability}."
        ),
        json!({
            "changed": false,
            "state_changed": false,
            "execution_state": "not_started",
            "error_kind": "agent_capability_unavailable",
            "failure_kind": "capability_unavailable",
            "capability": capability,
            "recovery_action": "upgrade_or_reconnect_runner",
            "retry_guidance": format!("reconnect or upgrade the Runner so it explicitly advertises {capability}")
        }),
    )
    .with_recovery(crate::tool_runtime::RecoveryKind::RetrySame, None)
}

/// Maximum decoded size for whole-payload/model-facing artifact operations.
/// These paths aggregate content or return it as base64/JSON, so they remain at
/// 10 MiB even though data-plane upload/export paths admit larger files.
/// Validate a project-relative file path for the structured edit tools
/// (`write_project_file`, `apply_text_edits`). Unlike the patch preflight
/// path validator, this HARD-rejects sensitive path components (the task spec
/// for these tools says "拒绝敏感路径", not "warn"). Absolute paths, `..`
/// traversal, empty paths, NUL bytes, and sensitive components are all rejected
/// so the helper never touches secrets, version control, or build output.
pub(crate) fn validate_edit_file_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if path.contains('\0') {
        return Err("path cannot contain NUL bytes".to_string());
    }
    let p = Path::new(path);
    if p.has_root()
        || p.components()
            .any(|component| matches!(component, std::path::Component::Prefix(_)))
    {
        return Err("path must be project-relative".to_string());
    }
    if p.components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("path cannot contain parent traversal".to_string());
    }
    if is_sensitive_edit_path(path) {
        return Err(format!(
            "refusing sensitive path '{}': touches runner.toml, legacy agent.toml, webcodex.env, \
             .env, project-registry, projects.d, .git, target, or node_modules",
            path
        ));
    }
    Ok(())
}
fn validate_apply_text_edit(
    change_index: usize,
    edit_index: usize,
    edit: &ApplyTextEditInput,
) -> Result<(), String> {
    let validate_field = |label: &str, value: &Option<String>| -> Result<(), String> {
        if let Some(value) = value {
            if value.contains('\0') {
                return Err(format!(
                    "change {change_index} edit {edit_index} ({}): {label} cannot contain NUL bytes",
                    edit.kind.as_str()
                ));
            }
            if value.len() > MAX_APPLY_TEXT_EDIT_FIELD_BYTES {
                return Err(format!(
                    "change {change_index} edit {edit_index} ({}): {label} exceeds {} bytes",
                    edit.kind.as_str(),
                    MAX_APPLY_TEXT_EDIT_FIELD_BYTES
                ));
            }
        }
        Ok(())
    };
    validate_field("old_text", &edit.old_text)?;
    validate_field("new_text", &edit.new_text)?;
    validate_field("anchor_text", &edit.anchor_text)?;
    if edit.occurrence == Some(0) {
        return Err(format!(
            "change {change_index} edit {edit_index} ({}): occurrence must be at least 1",
            edit.kind.as_str()
        ));
    }
    if let Some(line_scope) = edit.line_scope {
        if let Err(reason) = line_scope.validate() {
            return Err(format!(
                "change {change_index} edit {edit_index} ({}): {reason}",
                edit.kind.as_str()
            ));
        }
    }
    match edit.kind {
        ApplyTextEditKind::ReplaceExact => {
            if edit
                .old_text
                .as_deref()
                .filter(|value| !value.is_empty())
                .is_none()
            {
                return Err(format!(
                    "change {change_index} edit {edit_index} (replace_exact): old_text must be non-empty"
                ));
            }
            if edit.anchor_text.is_some() {
                return Err(format!(
                    "change {change_index} edit {edit_index} (replace_exact): anchor_text is not allowed"
                ));
            }
        }
        ApplyTextEditKind::DeleteExact => {
            if edit
                .old_text
                .as_deref()
                .filter(|value| !value.is_empty())
                .is_none()
            {
                return Err(format!(
                    "change {change_index} edit {edit_index} (delete_exact): old_text must be non-empty"
                ));
            }
            if edit.new_text.is_some() || edit.anchor_text.is_some() {
                return Err(format!(
                    "change {change_index} edit {edit_index} (delete_exact): new_text and anchor_text are not allowed"
                ));
            }
        }
        ApplyTextEditKind::InsertBefore | ApplyTextEditKind::InsertAfter => {
            if edit
                .anchor_text
                .as_deref()
                .filter(|value| !value.is_empty())
                .is_none()
            {
                return Err(format!(
                    "change {change_index} edit {edit_index} ({}): anchor_text must be non-empty",
                    edit.kind.as_str()
                ));
            }
            if edit.new_text.is_none() {
                return Err(format!(
                    "change {change_index} edit {edit_index} ({}): new_text is required",
                    edit.kind.as_str()
                ));
            }
            if edit.old_text.is_some() {
                return Err(format!(
                    "change {change_index} edit {edit_index} ({}): old_text is not allowed",
                    edit.kind.as_str()
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct ApplyTextEditsPreflightValidationError {
    message: String,
    edit_index: Option<usize>,
}

impl From<String> for ApplyTextEditsPreflightValidationError {
    fn from(message: String) -> Self {
        Self {
            message,
            edit_index: None,
        }
    }
}

fn validate_apply_file_change(
    index: usize,
    change: &ApplyFileChangeInput,
) -> Result<(), ApplyTextEditsPreflightValidationError> {
    let expected_hash = || -> Result<(), ApplyTextEditsPreflightValidationError> {
        match change.expected_sha256.as_deref() {
            Some(hash) if is_hex_sha256(hash) => Ok(()),
            _ => Err(format!(
                "change {index} ({}): expected_sha256 is required and must be 64 lowercase hexadecimal characters",
                change.kind.as_str()
            )
            .into()),
        }
    };
    match change.kind {
        ApplyFileChangeKind::Edit => {
            expected_hash()?;
            if change.to_path.is_some() || change.content.is_some() {
                return Err(
                    format!("change {index} (edit): to_path and content are not allowed").into(),
                );
            }
            if change.edits.is_empty() || change.edits.len() > MAX_APPLY_TEXT_EDITS {
                return Err(format!(
                    "change {index} (edit): edits must contain 1..={MAX_APPLY_TEXT_EDITS} entries"
                )
                .into());
            }
            for (edit_index, edit) in change.edits.iter().enumerate() {
                if let Err(message) = validate_apply_text_edit(index, edit_index, edit) {
                    return Err(ApplyTextEditsPreflightValidationError {
                        message,
                        edit_index: Some(edit_index),
                    });
                }
            }
        }
        ApplyFileChangeKind::Create => {
            if change.to_path.is_some()
                || change.expected_sha256.is_some()
                || !change.edits.is_empty()
            {
                return Err(format!(
                    "change {index} (create): to_path, expected_sha256, and edits are not allowed"
                )
                .into());
            }
            let content = change
                .content
                .as_deref()
                .ok_or_else(|| format!("change {index} (create): content is required"))?;
            if content.contains('\0') {
                return Err(
                    format!("change {index} (create): content cannot contain NUL bytes").into(),
                );
            }
        }
        ApplyFileChangeKind::Delete => {
            expected_hash()?;
            if change.to_path.is_some() || change.content.is_some() || !change.edits.is_empty() {
                return Err(format!(
                    "change {index} (delete): to_path, content, and edits are not allowed"
                )
                .into());
            }
        }
        ApplyFileChangeKind::Rename => {
            expected_hash()?;
            let to_path = change
                .to_path
                .as_deref()
                .ok_or_else(|| format!("change {index} (rename): to_path is required"))?;
            if to_path == change.path {
                return Err(
                    format!("change {index} (rename): path and to_path must differ").into(),
                );
            }
            if change.content.is_some() || !change.edits.is_empty() {
                return Err(
                    format!("change {index} (rename): content and edits are not allowed").into(),
                );
            }
        }
    }
    Ok(())
}

fn transactional_edit_agent_stdout_result(
    tool_name: &str,
    stdout: &str,
    expected_change_count: usize,
    expected_dry_run: bool,
) -> ToolResult {
    let stdout = stdout.trim();
    let mut obj: Value = match serde_json::from_str::<Value>(stdout) {
        Ok(value) if value.is_object() => value,
        _ => {
            return structured_edit_outcome_unknown_result(
                tool_name,
                "the Runner returned malformed or non-object JSON after dispatch",
                json!({}),
            )
        }
    };
    if let Some(error) = obj.get("error").and_then(Value::as_str).map(str::to_string) {
        let rollback_complete = obj.get("rollback_complete").and_then(Value::as_bool);
        let changed = obj.get("changed").and_then(Value::as_bool);
        let state_changed = obj.get("state_changed").and_then(Value::as_bool);
        let uncertain = rollback_complete == Some(false)
            || changed == Some(true)
            || state_changed == Some(true);
        let no_effect_proven = !uncertain
            && (rollback_complete == Some(true)
                || changed == Some(false)
                || state_changed == Some(false));
        if !no_effect_proven {
            return structured_edit_outcome_unknown_result(tool_name, error, obj);
        }
        obj["changed"] = json!(false);
        obj["state_changed"] = json!(false);
        obj["execution_state"] = if rollback_complete == Some(true) {
            json!("completed")
        } else {
            json!("not_started")
        };
        let message = if obj.get("conflict_recovery").is_some_and(Value::is_object)
            || error.contains("No files were modified")
            || error.contains("was rolled back")
        {
            error
        } else {
            recoverable_write_rejection(&error)
        };
        return ToolResult::err_with_output(message, obj);
    }

    let dry_run = obj.get("dry_run").and_then(Value::as_bool);
    let applied_count = obj.get("applied_count").and_then(Value::as_u64);
    let changed = obj.get("changed").and_then(Value::as_bool);
    let would_change = obj.get("would_change").and_then(Value::as_bool);
    let valid = dry_run == Some(expected_dry_run)
        && applied_count == Some(expected_change_count as u64)
        && changed.is_some()
        && would_change.is_some()
        && !(expected_dry_run && changed == Some(true))
        && (expected_dry_run || changed == would_change);
    if !valid {
        return structured_edit_outcome_unknown_result(
            tool_name,
            "the Runner success payload omitted or contradicted authoritative edit-effect fields",
            obj,
        );
    }
    obj["state_changed"] = json!(changed.expect("validated changed field"));
    obj["execution_state"] = json!("completed");
    ToolResult::ok(obj)
}

fn apply_patch_sha256(value: &Value) -> bool {
    value.as_str().is_some_and(|value| {
        value.len() == 64
            && value
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    })
}

fn apply_patch_nullable_sha256(value: Option<&Value>, required: bool) -> bool {
    match (value, required) {
        (Some(value), true) => apply_patch_sha256(value),
        (Some(Value::Null), false) => true,
        _ => false,
    }
}

const APPLY_PATCH_SUCCESS_TOP_LEVEL_FIELDS: [&str; 9] = [
    "dry_run",
    "applied_count",
    "changed",
    "state_changed",
    "execution_state",
    "would_change",
    "files",
    "changed_paths",
    "requested_matching_mode",
];

const APPLY_PATCH_SUCCESS_FILE_FIELDS: [&str; 9] = [
    "index",
    "kind",
    "path",
    "to_path",
    "old_sha256",
    "new_sha256",
    "changed",
    "would_change",
    "edits",
];

const APPLY_PATCH_SUCCESS_EDIT_FIELDS: [&str; 11] = [
    "chunk_index",
    "change_context_present",
    "old_line_count",
    "new_line_count",
    "end_of_file",
    "match_mode",
    "match_source",
    "matched_start_line",
    "candidate_count",
    "unique_match",
    "strict_match",
];

const APPLY_PATCH_FAILURE_TOP_LEVEL_FIELDS: [&str; 19] = [
    "changed",
    "state_changed",
    "execution_state",
    "error_kind",
    "failure_kind",
    "tool_failure",
    "recovery_action",
    "recovery_kind",
    "recovery_tool",
    "rollback_complete",
    "change_index",
    "kind",
    "path",
    "patch_line",
    "expected_format",
    "retry_guidance",
    "error",
    "match_diagnostic",
    "capability",
];

const APPLY_PATCH_FAILURE_MATCH_DIAGNOSTIC_FIELDS: [&str; 10] = [
    "chunk_index",
    "match_source",
    "search_start_line",
    "expected_line_count",
    "available_line_count",
    "closest_start_line",
    "closest_exact_line_matches",
    "closest_trim_end_line_matches",
    "closest_trim_line_matches",
    "first_exact_mismatch_offset",
];

const APPLY_PATCH_RECOVERY_MARGIN_BEFORE: usize = 8;
const APPLY_PATCH_RECOVERY_MARGIN_AFTER: usize = 8;

#[derive(Debug)]
struct ValidatedApplyPatchMatchRejection {
    change_index: usize,
    chunk_index: usize,
    path: String,
    requested_matching_mode: crate::apply_patch_shared::ApplyPatchMatchingMode,
    match_mode: &'static str,
    match_source: &'static str,
    matched_start_line: Option<usize>,
    candidate_count: usize,
    candidate_start_lines: Vec<usize>,
    candidate_positions_truncated: bool,
    expected_line_count: usize,
    source_line_count: usize,
    classification: &'static str,
}

fn expected_apply_patch_failure_pattern_len(
    hunk: &crate::apply_patch_shared::CodexPatchHunk,
    chunk_index: usize,
    match_source: &str,
) -> Option<usize> {
    let crate::apply_patch_shared::CodexPatchHunk::UpdateFile { chunks, .. } = hunk else {
        return None;
    };
    let chunk = chunks.get(chunk_index)?;
    match match_source {
        "change_context" => chunk.change_context.as_ref().map(|_| 1),
        "old_lines" if !chunk.old_lines.is_empty() => {
            let count = chunk.old_lines.len()
                - usize::from(chunk.old_lines.last().is_some_and(String::is_empty));
            (count > 0).then_some(count)
        }
        _ => None,
    }
}

fn validated_apply_patch_match_rejection(
    patch: &crate::apply_patch_shared::CodexPatch,
    failure_output: &Value,
    expected_matching_mode: crate::apply_patch_shared::ApplyPatchMatchingMode,
) -> Option<ValidatedApplyPatchMatchRejection> {
    if expected_matching_mode == crate::apply_patch_shared::ApplyPatchMatchingMode::FirstMatch
        || failure_output.get("changed").and_then(Value::as_bool) != Some(false)
        || failure_output.get("state_changed").and_then(Value::as_bool) != Some(false)
        || failure_output
            .get("execution_state")
            .and_then(Value::as_str)
            != Some("not_started")
        || failure_output.get("error_kind").and_then(Value::as_str)
            != Some("matching_mode_rejected")
        || failure_output
            .get("requested_matching_mode")
            .and_then(Value::as_str)
            != Some(expected_matching_mode.as_str())
        || failure_output
            .get("matching_mode_satisfied")
            .and_then(Value::as_bool)
            != Some(false)
    {
        return None;
    }

    let change_index = failure_output
        .get("change_index")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let hunk = patch.hunks.get(change_index)?;
    if failure_output.get("path").and_then(Value::as_str) != Some(hunk.path()) {
        return None;
    }
    let crate::apply_patch_shared::CodexPatchHunk::UpdateFile { chunks, .. } = hunk else {
        return None;
    };
    let chunk_index = failure_output
        .get("chunk_index")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let chunk = chunks.get(chunk_index)?;
    let match_source = match failure_output.get("match_source").and_then(Value::as_str)? {
        "old_lines" if !chunk.old_lines.is_empty() => "old_lines",
        "change_context"
            if chunk.change_context.is_some()
                && (expected_matching_mode
                    == crate::apply_patch_shared::ApplyPatchMatchingMode::ExactUnique
                    || chunk.old_lines.is_empty()) =>
        {
            "change_context"
        }
        _ => {
            // Unanchored append performs no text matching and is strict-safe;
            // other sources contradict the parsed chunk shape or the selected
            // matching mode's positioning semantics.
            return None;
        }
    };
    let expected_line_count =
        expected_apply_patch_failure_pattern_len(hunk, chunk_index, match_source)?;
    let match_mode = match failure_output.get("match_mode").and_then(Value::as_str)? {
        "exact" => "exact",
        "trim_end" => "trim_end",
        "trim" => "trim",
        "normalized" => "normalized",
        _ => return None,
    };
    let search_start_line = failure_output
        .get("search_start_line")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let source_line_count = failure_output
        .get("source_line_count")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    if search_start_line == 0 || source_line_count < expected_line_count {
        return None;
    }
    let last_start_line = source_line_count
        .checked_sub(expected_line_count)?
        .checked_add(1)?;
    if search_start_line > last_start_line {
        return None;
    }
    if expected_matching_mode == crate::apply_patch_shared::ApplyPatchMatchingMode::Unique
        && match_source == "old_lines"
        && chunk.is_end_of_file
        && search_start_line != last_start_line
    {
        // Under Unique, *** End of File is a structural eligibility fence for
        // old_lines. A Runner cannot expand that eligible range back toward the
        // beginning of the file by forging search_start_line metadata.
        return None;
    }
    let max_candidate_count = last_start_line
        .checked_sub(search_start_line)?
        .checked_add(1)?;
    let candidate_count = failure_output
        .get("candidate_count")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    if candidate_count == 0 || candidate_count > max_candidate_count {
        return None;
    }

    let candidate_positions_truncated = failure_output
        .get("candidate_positions_truncated")?
        .as_bool()?;
    let candidate_start_lines = failure_output
        .get("candidate_start_lines")?
        .as_array()?
        .iter()
        .map(|value| value.as_u64().and_then(|value| usize::try_from(value).ok()))
        .collect::<Option<Vec<_>>>()?;
    if candidate_start_lines.is_empty()
        || candidate_start_lines.len()
            > crate::apply_patch_shared::MAX_CODEX_PATCH_CANDIDATE_POSITIONS
        || candidate_start_lines
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        || candidate_start_lines
            .iter()
            .any(|line| *line < search_start_line || *line > last_start_line)
    {
        return None;
    }
    let position_cap = crate::apply_patch_shared::MAX_CODEX_PATCH_CANDIDATE_POSITIONS;
    if candidate_count <= position_cap {
        if candidate_positions_truncated || candidate_start_lines.len() != candidate_count {
            return None;
        }
    } else if !candidate_positions_truncated || candidate_start_lines.len() != position_cap {
        return None;
    }

    let classification = if candidate_count == 1 {
        if expected_matching_mode != crate::apply_patch_shared::ApplyPatchMatchingMode::ExactUnique
            || match_mode == "exact"
        {
            return None;
        }
        "unique_fuzzy_candidate"
    } else {
        "ambiguous_candidate"
    };
    let matched_start_line = match classification {
        "unique_fuzzy_candidate" => {
            let line = failure_output
                .get("matched_start_line")?
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())?;
            if candidate_start_lines.as_slice() != [line] {
                return None;
            }
            Some(line)
        }
        "ambiguous_candidate" => {
            if failure_output.get("matched_start_line") != Some(&Value::Null) {
                return None;
            }
            None
        }
        _ => return None,
    };
    Some(ValidatedApplyPatchMatchRejection {
        change_index,
        chunk_index,
        path: hunk.path().to_string(),
        requested_matching_mode: expected_matching_mode,
        match_mode,
        match_source,
        matched_start_line,
        candidate_count,
        candidate_start_lines,
        candidate_positions_truncated,
        expected_line_count,
        classification,
        source_line_count,
    })
}

fn apply_patch_match_rejection_recovery(
    rejection: &ValidatedApplyPatchMatchRejection,
) -> Option<Value> {
    let requested_limit = rejection
        .expected_line_count
        .saturating_add(APPLY_PATCH_RECOVERY_MARGIN_BEFORE)
        .saturating_add(APPLY_PATCH_RECOVERY_MARGIN_AFTER)
        .min(crate::apply_patch_shared::MAX_CODEX_PATCH_RECOVERY_READ_LINES)
        .max(1);
    let mut items = Vec::with_capacity(rejection.candidate_start_lines.len());
    for candidate_start_line in &rejection.candidate_start_lines {
        let start_line = candidate_start_line
            .saturating_sub(APPLY_PATCH_RECOVERY_MARGIN_BEFORE)
            .max(1);
        let available_from_start = rejection
            .source_line_count
            .checked_sub(start_line)?
            .checked_add(1)?;
        let limit = requested_limit.min(available_from_start);
        if limit == 0 {
            return None;
        }
        items.push(json!({
            "path": rejection.path.as_str(),
            "start_line": start_line,
            "limit": limit,
        }));
    }
    if items.is_empty() {
        return None;
    }
    Some(json!({
        "action": "read_files",
        "reason": if rejection.classification == "ambiguous_candidate" {
            "matching_mode_rejected_ambiguous"
        } else {
            "matching_mode_rejected_unique_fuzzy"
        },
        "items": items,
        "change_index": rejection.change_index,
        "chunk_index": rejection.chunk_index,
    }))
}

fn apply_patch_match_rejection_diagnostic(rejection: &ValidatedApplyPatchMatchRejection) -> Value {
    json!({
        "classification": rejection.classification,
        "requested_matching_mode": rejection.requested_matching_mode.as_str(),
        "chunk_index": rejection.chunk_index,
        "match_mode": rejection.match_mode,
        "match_source": rejection.match_source,
        "matched_start_line": rejection.matched_start_line,
        "candidate_count": rejection.candidate_count,
        "candidate_start_lines": rejection.candidate_start_lines,
        "candidate_positions_truncated": rejection.candidate_positions_truncated,
        "expected_line_count": rejection.expected_line_count,
        "matching_mode_satisfied": false,
    })
}

fn valid_apply_patch_failure_match_diagnostic(
    value: &Value,
    patch: &crate::apply_patch_shared::CodexPatch,
    failure_output: &Value,
) -> bool {
    let Some(diagnostic) = value.as_object() else {
        return false;
    };
    if diagnostic.len() != APPLY_PATCH_FAILURE_MATCH_DIAGNOSTIC_FIELDS.len()
        || !diagnostic
            .keys()
            .all(|key| APPLY_PATCH_FAILURE_MATCH_DIAGNOSTIC_FIELDS.contains(&key.as_str()))
        || failure_output.get("error_kind").and_then(Value::as_str) != Some("context_mismatch")
    {
        return false;
    }

    let Some(change_index) = failure_output
        .get("change_index")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    let Some(hunk) = patch.hunks.get(change_index) else {
        return false;
    };
    if failure_output.get("path").and_then(Value::as_str) != Some(hunk.path()) {
        return false;
    }
    let Some(chunk_index) = diagnostic
        .get("chunk_index")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    let Some(match_source) = diagnostic.get("match_source").and_then(Value::as_str) else {
        return false;
    };
    let Some(expected_pattern_len) =
        expected_apply_patch_failure_pattern_len(hunk, chunk_index, match_source)
    else {
        return false;
    };
    let Some(expected_line_count) = diagnostic
        .get("expected_line_count")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|count| *count == expected_pattern_len)
    else {
        return false;
    };
    let Some(search_start_line) = diagnostic
        .get("search_start_line")
        .and_then(Value::as_u64)
        .filter(|line| *line >= 1)
    else {
        return false;
    };
    let Some(available_line_count) = diagnostic
        .get("available_line_count")
        .and_then(Value::as_u64)
    else {
        return false;
    };
    let Some(exact) = diagnostic
        .get("closest_exact_line_matches")
        .and_then(Value::as_u64)
    else {
        return false;
    };
    let Some(trim_end) = diagnostic
        .get("closest_trim_end_line_matches")
        .and_then(Value::as_u64)
    else {
        return false;
    };
    let Some(trim) = diagnostic
        .get("closest_trim_line_matches")
        .and_then(Value::as_u64)
    else {
        return false;
    };
    let expected_line_count = expected_line_count as u64;
    if !(exact <= trim_end && trim_end <= trim && trim < expected_line_count) {
        return false;
    }

    let closest_start_valid = match diagnostic.get("closest_start_line") {
        Some(Value::Null) => available_line_count == 0 && exact == 0 && trim_end == 0 && trim == 0,
        Some(value) => value.as_u64().is_some_and(|line| {
            available_line_count > 0
                && line >= search_start_line
                && line < search_start_line.saturating_add(available_line_count)
        }),
        None => false,
    };
    let mismatch_valid = diagnostic
        .get("first_exact_mismatch_offset")
        .and_then(Value::as_u64)
        .is_some_and(|offset| (1..=expected_line_count).contains(&offset));
    closest_start_valid && mismatch_valid
}

fn apply_patch_context_mismatch_recovery(
    patch: &crate::apply_patch_shared::CodexPatch,
    failure_output: &Value,
) -> Option<Value> {
    if failure_output.get("changed").and_then(Value::as_bool) != Some(false)
        || failure_output.get("state_changed").and_then(Value::as_bool) != Some(false)
        || failure_output
            .get("execution_state")
            .and_then(Value::as_str)
            != Some("not_started")
        || failure_output.get("error_kind").and_then(Value::as_str) != Some("context_mismatch")
    {
        return None;
    }

    let diagnostic = failure_output.get("match_diagnostic")?;
    if !valid_apply_patch_failure_match_diagnostic(diagnostic, patch, failure_output) {
        return None;
    }
    let change_index = failure_output
        .get("change_index")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let hunk = patch.hunks.get(change_index)?;
    let path = hunk.path();
    let diagnostic = diagnostic.as_object()?;
    let chunk_index = diagnostic
        .get("chunk_index")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let search_start_line = diagnostic
        .get("search_start_line")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let expected_line_count = diagnostic
        .get("expected_line_count")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let available_line_count = diagnostic
        .get("available_line_count")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let closest_start_line = diagnostic
        .get("closest_start_line")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    let first_exact_mismatch_offset = diagnostic
        .get("first_exact_mismatch_offset")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;

    let total_line_count = search_start_line
        .checked_sub(1)?
        .checked_add(available_line_count)?;
    if total_line_count == 0 || closest_start_line > total_line_count {
        return None;
    }

    let mismatch_line = closest_start_line
        .checked_add(first_exact_mismatch_offset.checked_sub(1)?)?
        .min(total_line_count);
    let initial_start_line = closest_start_line
        .saturating_sub(APPLY_PATCH_RECOVERY_MARGIN_BEFORE)
        .max(1);
    let requested_limit = expected_line_count
        .saturating_add(APPLY_PATCH_RECOVERY_MARGIN_BEFORE)
        .saturating_add(APPLY_PATCH_RECOVERY_MARGIN_AFTER)
        .min(crate::apply_patch_shared::MAX_CODEX_PATCH_RECOVERY_READ_LINES);
    // Short hunks retain context around the candidate start. Once the bounded
    // read cap applies, shift only as far as needed to include the first known
    // mismatch plus useful trailing context; otherwise a large stale hunk can
    // return a window containing only lines that still match.
    let desired_end_line = mismatch_line
        .saturating_add(APPLY_PATCH_RECOVERY_MARGIN_AFTER)
        .min(total_line_count);
    let mismatch_centered_start = desired_end_line
        .saturating_sub(requested_limit.saturating_sub(1))
        .max(1);
    let start_line = initial_start_line.max(mismatch_centered_start);
    let available_from_start = total_line_count.checked_sub(start_line)?.checked_add(1)?;
    let limit = requested_limit.min(available_from_start);
    if limit == 0 {
        return None;
    }

    Some(json!({
        "action": "read_files",
        "reason": "context_mismatch",
        "items": [{
            "path": path,
            "start_line": start_line,
            "limit": limit,
        }],
        "change_index": change_index,
        "chunk_index": chunk_index,
    }))
}

fn sanitize_apply_patch_match_rejection(
    result: &mut ToolResult,
    patch: &crate::apply_patch_shared::CodexPatch,
    expected_matching_mode: crate::apply_patch_shared::ApplyPatchMatchingMode,
) -> bool {
    if result.output.get("error_kind").and_then(Value::as_str) != Some("matching_mode_rejected") {
        return false;
    }
    let validated =
        validated_apply_patch_match_rejection(patch, &result.output, expected_matching_mode);
    let (message, recovery_action, retry_guidance) = match validated.as_ref() {
        Some(rejection)
            if rejection.requested_matching_mode
                == crate::apply_patch_shared::ApplyPatchMatchingMode::Unique =>
        {
            (
                "Rejected Codex patch before write: Server-validated positioning is ambiguous under matching_mode=unique. No files were modified.".to_string(),
                "read_equal_candidates_and_refine_context",
                "read every recovery.items window as an equal candidate, then add a stable parent/function/test/module anchor or small surrounding context and retry with matching_mode=unique; do not choose a candidate from its order",
            )
        }
        Some(rejection) if rejection.classification == "unique_fuzzy_candidate" => (
            "Rejected Codex patch before write: matching_mode=exact_unique found one non-exact candidate. No files were modified.".to_string(),
            "reread_and_regenerate_exact_unique_patch",
            "read recovery.items, regenerate this chunk from exact current source, and retry with matching_mode=exact_unique; do not downgrade the requested fence",
        ),
        Some(_) => (
            "Rejected Codex patch before write: Server-validated positioning is ambiguous under matching_mode=exact_unique. No files were modified.".to_string(),
            "read_equal_candidates_and_add_exact_context",
            "read every recovery.items window as an equal candidate, expand exact context until the target is unique, and retry with matching_mode=exact_unique; do not choose a candidate from its order",
        ),
        None => (
            "Rejected Codex patch before write: Runner matching metadata was invalid or contradictory and was suppressed. No files were modified.".to_string(),
            "reread_and_regenerate_patch",
            "do not trust the rejected target metadata; reread current source through normal read tooling, regenerate context, and retry with the same matching_mode",
        ),
    };

    if let Some(fields) = result.output.as_object_mut() {
        fields.retain(|key, _| APPLY_PATCH_FAILURE_TOP_LEVEL_FIELDS.contains(&key.as_str()));
        for key in [
            "change_index",
            "kind",
            "path",
            "patch_line",
            "expected_format",
            "match_diagnostic",
            "capability",
            "recovery_action",
            "recovery_kind",
            "recovery_tool",
            "retry_guidance",
            "error",
        ] {
            fields.remove(key);
        }
        fields.insert(
            "requested_matching_mode".to_string(),
            json!(expected_matching_mode.as_str()),
        );
        fields.insert("recovery_action".to_string(), json!(recovery_action));
        fields.insert("retry_guidance".to_string(), json!(retry_guidance));
        fields.insert("error".to_string(), json!(message.as_str()));
        if let Some(rejection) = validated.as_ref() {
            fields.insert("change_index".to_string(), json!(rejection.change_index));
            fields.insert("path".to_string(), json!(rejection.path.as_str()));
            fields.insert(
                "match_rejection_diagnostic".to_string(),
                apply_patch_match_rejection_diagnostic(rejection),
            );
            if let Some(recovery) = apply_patch_match_rejection_recovery(rejection) {
                fields.insert("recovery".to_string(), recovery);
            }
        }
    }
    result.error = Some(message);
    true
}

fn sanitize_apply_patch_failure_metadata(
    result: &mut ToolResult,
    patch: &crate::apply_patch_shared::CodexPatch,
    expected_matching_mode: crate::apply_patch_shared::ApplyPatchMatchingMode,
) {
    if result.output.get("error_kind").and_then(Value::as_str) == Some("matching_mode_rejected") {
        let _ = sanitize_apply_patch_match_rejection(result, patch, expected_matching_mode);
        return;
    }
    if result.output.get("error_kind").and_then(Value::as_str) == Some("strict_match_rejected") {
        let message = "Rejected apply_patch result: a current matching_mode request received legacy strict-match metadata; target metadata was suppressed.".to_string();
        if let Some(fields) = result.output.as_object_mut() {
            fields.retain(|key, _| {
                matches!(
                    key.as_str(),
                    "changed" | "state_changed" | "execution_state" | "error_kind" | "tool_failure"
                )
            });
            fields.insert(
                "requested_matching_mode".to_string(),
                json!(expected_matching_mode.as_str()),
            );
            fields.insert(
                "recovery_action".to_string(),
                json!("reread_and_regenerate_patch"),
            );
            fields.insert(
                "retry_guidance".to_string(),
                json!("do not trust legacy target metadata; reread current source and retry with the same matching_mode"),
            );
            fields.insert("error".to_string(), json!(message.as_str()));
        }
        result.error = Some(message);
        return;
    }

    let output = &mut result.output;
    let recovery = apply_patch_context_mismatch_recovery(patch, output);
    let diagnostic_valid = output
        .get("match_diagnostic")
        .is_none_or(|value| valid_apply_patch_failure_match_diagnostic(value, patch, output));
    let Some(fields) = output.as_object_mut() else {
        return;
    };
    fields.retain(|key, _| APPLY_PATCH_FAILURE_TOP_LEVEL_FIELDS.contains(&key.as_str()));
    fields.insert(
        "requested_matching_mode".to_string(),
        json!(expected_matching_mode.as_str()),
    );
    if !diagnostic_valid {
        fields.remove("match_diagnostic");
    }
    if let Some(recovery) = recovery {
        fields.insert("recovery".to_string(), recovery);
    }
}

fn sanitize_apply_patch_success_metadata(output: &mut Value) {
    let Some(top_level) = output.as_object_mut() else {
        return;
    };
    top_level.retain(|key, _| APPLY_PATCH_SUCCESS_TOP_LEVEL_FIELDS.contains(&key.as_str()));
    let Some(files) = top_level.get_mut("files").and_then(Value::as_array_mut) else {
        return;
    };
    for file in files {
        let Some(file) = file.as_object_mut() else {
            continue;
        };
        file.retain(|key, _| APPLY_PATCH_SUCCESS_FILE_FIELDS.contains(&key.as_str()));
        let Some(edits) = file.get_mut("edits").and_then(Value::as_array_mut) else {
            continue;
        };
        for edit in edits {
            if let Some(edit) = edit.as_object_mut() {
                edit.retain(|key, _| APPLY_PATCH_SUCCESS_EDIT_FIELDS.contains(&key.as_str()));
            }
        }
    }
}

fn validate_apply_patch_edit_summary(
    value: &Value,
    chunk_index: usize,
    chunk: &crate::apply_patch_shared::CodexPatchChunk,
    matching_mode: crate::apply_patch_shared::ApplyPatchMatchingMode,
) -> bool {
    let Some(edit) = value.as_object() else {
        return false;
    };
    if edit.get("chunk_index").and_then(Value::as_u64) != Some(chunk_index as u64)
        || edit.get("change_context_present").and_then(Value::as_bool)
            != Some(chunk.change_context.is_some())
        || edit.get("old_line_count").and_then(Value::as_u64) != Some(chunk.old_lines.len() as u64)
        || edit.get("new_line_count").and_then(Value::as_u64) != Some(chunk.new_lines.len() as u64)
        || edit.get("end_of_file").and_then(Value::as_bool) != Some(chunk.is_end_of_file)
        || !edit
            .get("matched_start_line")
            .and_then(Value::as_u64)
            .is_some_and(|line| line >= 1)
    {
        return false;
    }

    let expected_source = if !chunk.old_lines.is_empty() {
        "old_lines"
    } else if chunk.change_context.is_some() {
        "change_context"
    } else {
        "append"
    };
    if edit.get("match_source").and_then(Value::as_str) != Some(expected_source) {
        return false;
    }

    let match_mode = edit.get("match_mode");
    let candidate_count = edit.get("candidate_count");
    let positioning_shape_valid = if expected_source == "append" {
        match_mode == Some(&Value::Null) && candidate_count == Some(&Value::Null)
    } else {
        match_mode
            .and_then(Value::as_str)
            .is_some_and(|mode| matches!(mode, "exact" | "trim_end" | "trim" | "normalized"))
            && candidate_count
                .and_then(Value::as_u64)
                .is_some_and(|count| count >= 1)
    };
    if !positioning_shape_valid {
        return false;
    }

    let Some(unique_match) = edit.get("unique_match").and_then(Value::as_bool) else {
        return false;
    };
    let Some(strict_match) = edit.get("strict_match").and_then(Value::as_bool) else {
        return false;
    };
    if expected_source == "append" {
        return unique_match && strict_match;
    }
    let candidate_is_unique = candidate_count.and_then(Value::as_u64) == Some(1);
    if unique_match && !candidate_is_unique {
        return false;
    }
    if unique_match != candidate_is_unique {
        return false;
    }
    if strict_match
        && (match_mode.and_then(Value::as_str) != Some("exact")
            || !candidate_is_unique
            || !unique_match)
    {
        return false;
    }
    match matching_mode {
        crate::apply_patch_shared::ApplyPatchMatchingMode::FirstMatch => true,
        crate::apply_patch_shared::ApplyPatchMatchingMode::Unique => unique_match,
        crate::apply_patch_shared::ApplyPatchMatchingMode::ExactUnique => strict_match,
    }
}

fn validate_apply_patch_success_metadata(
    output: &Value,
    patch: &crate::apply_patch_shared::CodexPatch,
    expected_dry_run: bool,
    expected_matching_mode: crate::apply_patch_shared::ApplyPatchMatchingMode,
) -> bool {
    if !output.is_object()
        || output
            .get("requested_matching_mode")
            .and_then(Value::as_str)
            != Some(expected_matching_mode.as_str())
    {
        return false;
    }
    let Some(files) = output.get("files").and_then(Value::as_array) else {
        return false;
    };
    if files.len() != patch.hunks.len() {
        return false;
    }

    let mut expected_changed_paths = Vec::new();
    let mut any_would_change = false;
    for (index, (file, hunk)) in files.iter().zip(&patch.hunks).enumerate() {
        let Some(file) = file.as_object() else {
            return false;
        };
        let (expected_kind, expected_path, expected_to_path, expected_chunks, old_sha, new_sha) =
            match hunk {
                crate::apply_patch_shared::CodexPatchHunk::AddFile { path, .. } => {
                    ("create", path.as_str(), None, None, false, true)
                }
                crate::apply_patch_shared::CodexPatchHunk::DeleteFile { path } => {
                    ("delete", path.as_str(), None, None, true, false)
                }
                crate::apply_patch_shared::CodexPatchHunk::UpdateFile {
                    path,
                    move_path,
                    chunks,
                } => {
                    let destination = move_path
                        .as_deref()
                        .filter(|destination| *destination != path.as_str());
                    (
                        if destination.is_some() {
                            "rename"
                        } else {
                            "edit"
                        },
                        path.as_str(),
                        destination,
                        Some(chunks.as_slice()),
                        true,
                        true,
                    )
                }
            };
        if file.get("index").and_then(Value::as_u64) != Some(index as u64)
            || file.get("kind").and_then(Value::as_str) != Some(expected_kind)
            || file.get("path").and_then(Value::as_str) != Some(expected_path)
            || match expected_to_path {
                Some(path) => file.get("to_path").and_then(Value::as_str) != Some(path),
                None => file.get("to_path") != Some(&Value::Null),
            }
            || !apply_patch_nullable_sha256(file.get("old_sha256"), old_sha)
            || !apply_patch_nullable_sha256(file.get("new_sha256"), new_sha)
        {
            return false;
        }

        let Some(would_change) = file.get("would_change").and_then(Value::as_bool) else {
            return false;
        };
        if expected_kind != "edit" && !would_change {
            return false;
        }
        if file.get("changed").and_then(Value::as_bool) != Some(!expected_dry_run && would_change) {
            return false;
        }
        any_would_change |= would_change;
        if would_change {
            expected_changed_paths.push(expected_path.to_string());
            if let Some(destination) = expected_to_path {
                expected_changed_paths.push(destination.to_string());
            }
        }

        let Some(edits) = file.get("edits").and_then(Value::as_array) else {
            return false;
        };
        match expected_chunks {
            None if !edits.is_empty() => return false,
            Some(chunks) => {
                if edits.len() != chunks.len()
                    || !edits
                        .iter()
                        .zip(chunks)
                        .enumerate()
                        .all(|(chunk_index, (edit, chunk))| {
                            validate_apply_patch_edit_summary(
                                edit,
                                chunk_index,
                                chunk,
                                expected_matching_mode,
                            )
                        })
                {
                    return false;
                }
            }
            None => {}
        }
    }

    if output.get("would_change").and_then(Value::as_bool) != Some(any_would_change) {
        return false;
    }
    let Some(changed_paths) = output.get("changed_paths").and_then(Value::as_array) else {
        return false;
    };
    changed_paths.len() == expected_changed_paths.len()
        && changed_paths
            .iter()
            .zip(expected_changed_paths)
            .all(|(actual, expected)| actual.as_str() == Some(expected.as_str()))
}

fn apply_patch_agent_stdout_result(
    stdout: &str,
    patch: &crate::apply_patch_shared::CodexPatch,
    expected_dry_run: bool,
    expected_matching_mode: crate::apply_patch_shared::ApplyPatchMatchingMode,
) -> ToolResult {
    let mut result = transactional_edit_agent_stdout_result(
        "apply_patch",
        stdout,
        patch.hunks.len(),
        expected_dry_run,
    );
    if !result.success {
        sanitize_apply_patch_failure_metadata(&mut result, patch, expected_matching_mode);
        return result;
    }
    sanitize_apply_patch_success_metadata(&mut result.output);
    if validate_apply_patch_success_metadata(
        &result.output,
        patch,
        expected_dry_run,
        expected_matching_mode,
    ) {
        return result;
    }
    structured_edit_outcome_unknown_result(
        "apply_patch",
        "the Runner success payload contained invalid or contradictory patch-plan metadata",
        json!({}),
    )
}

fn apply_text_edits_agent_stdout_result(
    stdout: &str,
    expected_change_count: usize,
    expected_dry_run: bool,
) -> ToolResult {
    transactional_edit_agent_stdout_result(
        "apply_text_edits",
        stdout,
        expected_change_count,
        expected_dry_run,
    )
}

fn write_project_file_agent_stdout_result(stdout: &str) -> ToolResult {
    let stdout = stdout.trim();
    let mut obj: Value = match serde_json::from_str::<Value>(stdout) {
        Ok(value) if value.is_object() => value,
        _ => {
            return structured_edit_outcome_unknown_result(
                "write_project_file",
                "the Runner returned malformed or non-object JSON after dispatch",
                json!({}),
            )
        }
    };
    if let Some(error) = obj.get("error").and_then(Value::as_str).map(str::to_string) {
        let changed = obj.get("changed").and_then(Value::as_bool);
        let state_changed = obj.get("state_changed").and_then(Value::as_bool);
        let execution_state = obj.get("execution_state").and_then(Value::as_str);
        if changed != Some(false) || state_changed != Some(false) {
            return structured_edit_outcome_unknown_result("write_project_file", error, obj);
        }
        match execution_state {
            Some("not_started") => {
                obj["changed"] = json!(false);
                obj["state_changed"] = json!(false);
                obj["execution_state"] = json!("not_started");
                return ToolResult::err_with_output(error, obj);
            }
            Some("completed") => {
                obj["changed"] = json!(false);
                obj["state_changed"] = json!(false);
                obj["execution_state"] = json!("completed");
                return ToolResult::err_with_output(error, obj);
            }
            _ => return structured_edit_outcome_unknown_result("write_project_file", error, obj),
        }
    }
    let Some(changed) = obj.get("changed").and_then(Value::as_bool) else {
        return structured_edit_outcome_unknown_result(
            "write_project_file",
            "the Runner success payload omitted the authoritative changed field",
            obj,
        );
    };
    let state_changed = obj.get("state_changed").and_then(Value::as_bool);
    let execution_state = obj.get("execution_state").and_then(Value::as_str);
    if state_changed != Some(changed) || execution_state != Some("completed") {
        return structured_edit_outcome_unknown_result(
            "write_project_file",
            "the Runner success payload omitted or contradicted authoritative write-effect fields",
            obj,
        );
    }
    ToolResult::ok(obj)
}

fn apply_text_edits_preflight_rejection(
    message: impl Into<String>,
    error_kind: &'static str,
    change_index: Option<usize>,
    edit_index: Option<usize>,
    kind: Option<&str>,
    path: Option<&str>,
    retry_guidance: &'static str,
) -> ToolResult {
    let detail = message.into();
    let mut output = json!({
        "state_changed": false,
        "error_kind": error_kind,
        "retry_guidance": retry_guidance,
    });
    if let Some(change_index) = change_index {
        output["change_index"] = json!(change_index);
    }
    if let Some(edit_index) = edit_index {
        output["edit_index"] = json!(edit_index);
    }
    if let Some(kind) = kind {
        output["kind"] = json!(kind);
    }
    if let Some(path) = path {
        output["path"] = json!(path);
    }
    ToolResult::err_with_output(
        format!(
            "Rejected before write: {detail}.\nNo files were modified.\nRetry guidance: {retry_guidance}."
        ),
        output,
    )
}

fn apply_text_edits_path_policy_rejection(
    change_index: usize,
    kind: &str,
    path: &str,
    message: String,
) -> ToolResult {
    let mut result = super::permissions::edit_path_policy_rejected_result(path, message);
    if let Some(output) = result.output.as_object_mut() {
        // The rejected path may itself be absolute or sensitive. Keep exact
        // change provenance without copying that untrusted path into recovery
        // metadata.
        output.remove("path");
        output.remove("error");
    }
    result.output["change_index"] = json!(change_index);
    result.output["kind"] = json!(kind);
    result.output["retry_guidance"] =
        json!("correct the rejected project-relative path and retry the whole batch");
    result
}

/// Pure, allocation-only computation of an `apply_text_edits` plan against
/// `original` UTF-8 content. Performs every semantic validation (unique
/// match, no overlap, whole-file sha guard) and returns the new content plus
/// a structured summary. Never touches the filesystem — the runtime/agent
/// layer decides whether to write. Used directly by unit tests; the agent
/// handler mirrors these exact semantics for the production write path.
#[cfg(test)]
pub(crate) fn apply_text_edits_to_string(
    original: &str,
    path: &str,
    edits: &[ApplyTextEditInput],
    expected_file_sha256: Option<&str>,
    dry_run: bool,
) -> Result<(String, Value), String> {
    if edits.is_empty() {
        return Err("edits must contain at least one edit".to_string());
    }
    if edits.len() > MAX_APPLY_TEXT_EDITS {
        return Err(format!(
            "too many edits; maximum is {}",
            MAX_APPLY_TEXT_EDITS
        ));
    }
    let old_sha256 = sha256_hex_bytes(original.as_bytes());
    if let Some(expected) = expected_file_sha256 {
        if old_sha256 != expected {
            return Err(recoverable_write_rejection("expected_file_sha256 mismatch"));
        }
    }

    let raw_original = original;
    let line_ending =
        detect_apply_text_line_ending(original).map_err(recoverable_write_rejection)?;
    let canonical_original = canonicalize_apply_text_line_endings(original, line_ending)
        .map_err(recoverable_write_rejection)?;
    let original = canonical_original.as_ref();

    // Resolve each edit to a (start, end, replacement, index) op against the
    // original content. start/end are byte offsets; inserts are zero-width.
    let mut ops: Vec<(usize, usize, String, usize)> = Vec::with_capacity(edits.len());
    let ignored_noop_count = edits
        .iter()
        .filter(|edit| {
            matches!(
                edit.kind,
                ApplyTextEditKind::InsertBefore | ApplyTextEditKind::InsertAfter
            ) && edit.new_text.as_deref() == Some("")
        })
        .count();
    for (index, edit) in edits.iter().enumerate() {
        let kind = edit.kind;
        if edit.occurrence == Some(0) {
            return Err(edit_field_error(
                index,
                kind,
                "occurrence must be at least 1",
            ));
        }
        if let Some(line_scope) = edit.line_scope {
            line_scope
                .validate()
                .map_err(|reason| edit_field_error(index, kind, reason))?;
        }
        let (needle, replacement): (&str, String) = match kind {
            ApplyTextEditKind::ReplaceExact => {
                let old = edit
                    .old_text
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| edit_field_error(index, kind, "old_text must be non-empty"))?;
                let new = edit.new_text.clone().unwrap_or_default();
                (old, new)
            }
            ApplyTextEditKind::DeleteExact => {
                let old = edit
                    .old_text
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| edit_field_error(index, kind, "old_text must be non-empty"))?;
                (old, String::new())
            }
            ApplyTextEditKind::InsertBefore | ApplyTextEditKind::InsertAfter => {
                let anchor = edit
                    .anchor_text
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        edit_field_error(index, kind, "anchor_text must be non-empty")
                    })?;
                let new = edit
                    .new_text
                    .as_deref()
                    .ok_or_else(|| edit_field_error(index, kind, "new_text is required"))?;
                if new.is_empty() {
                    continue;
                }
                (anchor, new.to_string())
            }
        };
        if needle.contains('\0') {
            return Err(edit_field_error(
                index,
                kind,
                "match text cannot contain NUL bytes",
            ));
        }
        if replacement.contains('\0') {
            return Err(edit_field_error(
                index,
                kind,
                "replacement text cannot contain NUL bytes",
            ));
        }
        let needle = canonicalize_apply_text_line_endings(needle, line_ending)
            .map_err(|error| edit_match_error(index, kind, error))?;
        let replacement = canonicalize_apply_text_line_endings(&replacement, line_ending)
            .map_err(|error| edit_field_error(index, kind, error))?
            .into_owned();
        let needle = needle.as_ref();
        let (start, end) = crate::apply_edits_shared::resolve_apply_text_match(
            original,
            needle,
            edit.occurrence,
            edit.line_scope.as_ref(),
        )
        .map_err(|conflict| {
            use crate::apply_edits_shared::ApplyTextMatchConflictKind;
            let message = match conflict.kind {
                ApplyTextMatchConflictKind::MatchNotFound if conflict.line_scope.is_some() => {
                    "match text was not found within line_scope".to_string()
                }
                ApplyTextMatchConflictKind::MatchNotFound => "match text was not found".to_string(),
                ApplyTextMatchConflictKind::MultipleMatches => format!(
                    "match text matched {} times{}; refusing ambiguous edit",
                    conflict
                        .line_scope_match_count
                        .unwrap_or(conflict.match_count),
                    if conflict.line_scope.is_some() {
                        " within line_scope"
                    } else {
                        ""
                    }
                ),
                ApplyTextMatchConflictKind::OccurrenceOutOfRange => format!(
                    "requested occurrence {} is out of range for {} exact matches",
                    conflict.requested_occurrence.unwrap_or(0),
                    conflict.match_count
                ),
                ApplyTextMatchConflictKind::OccurrenceOutsideLineScope => format!(
                    "requested occurrence {} is outside line_scope",
                    conflict.requested_occurrence.unwrap_or(0)
                ),
            };
            edit_match_error(index, kind, &message)
        })?;
        let (range_start, range_end) = match kind {
            ApplyTextEditKind::InsertBefore => (start, start),
            ApplyTextEditKind::InsertAfter => (end, end),
            _ => (start, end),
        };
        ops.push((range_start, range_end, replacement, index));
    }

    // Stable sort by (start, end, original index) so the slice build is
    // deterministic and ties (e.g. multiple inserts at one point) keep caller
    // order.
    ops.sort_by_key(|&(s, e, _, i)| (s, e, i));

    // Reject overlapping edits: a later op must not start before an earlier
    // op ends. Zero-width ops (inserts) never trigger this because their
    // start == end.
    for w in ops.windows(2) {
        let (_, e1, _, _) = w[0];
        let (s2, _, _, _) = w[1];
        if s2 < e1 {
            return Err(recoverable_write_rejection(
                "edits overlap; refusing ambiguous atomic edit batch",
            ));
        }
    }

    // Build the new content by slicing the original at op boundaries.
    let mut new_content = String::with_capacity(original.len() + 64);
    let mut cursor = 0usize;
    let mut edit_summaries: Vec<Value> = Vec::with_capacity(ops.len());
    for &(start, end, ref replacement, index) in &ops {
        new_content.push_str(&original[cursor..start]);
        new_content.push_str(replacement);
        cursor = end;
        let edit = &edits[index];
        let old_start_line = 1 + original[..start].matches('\n').count();
        let mut old_end_line = 1 + original[..end].matches('\n').count();
        if end > start && end <= original.len() && original.as_bytes()[end - 1] == b'\n' {
            old_end_line = old_end_line.saturating_sub(1).max(old_start_line);
        }
        if end == start {
            old_end_line = old_start_line;
        }
        let new_line_count = if replacement.is_empty() {
            0
        } else {
            replacement.lines().count()
        };
        edit_summaries.push(json!({
            "index": index,
            "kind": edit.kind.as_str(),
            "old_start_line": old_start_line,
            "old_end_line": old_end_line,
            "new_line_count": new_line_count,
        }));
    }
    new_content.push_str(&original[cursor..]);
    let new_content = restore_apply_text_line_endings(new_content, line_ending);

    let new_sha256 = sha256_hex_bytes(new_content.as_bytes());
    let changed = new_content != raw_original;
    let output = json!({
        "path": path,
        "dry_run": dry_run,
        "applied_count": edits.len(),
        "ignored_noop_count": ignored_noop_count,
        "old_sha256": old_sha256,
        "new_sha256": new_sha256,
        "changed": changed,
        "would_change": changed,
        "edits": edit_summaries,
        "changed_paths": [path],
    });
    Ok((new_content, output))
}

#[cfg(test)]
fn edit_field_error(index: usize, kind: ApplyTextEditKind, msg: &str) -> String {
    format!(
        "Rejected before write: edit {} ({}): {}.\nNo files were modified.\nRetry guidance: read the file again to refresh context, then retry with corrected edit fields.",
        index,
        kind.as_str(),
        msg
    )
}

#[cfg(test)]
fn edit_match_error(index: usize, kind: ApplyTextEditKind, msg: &str) -> String {
    format!(
        "Rejected before write: edit {} ({}): {}.\nNo files were modified.\nRetry guidance: read the file again to refresh context, then retry with a more exact match text.",
        index,
        kind.as_str(),
        msg
    )
}

impl ToolRuntime {
    pub(crate) async fn delete_project_files(
        &self,
        project: String,
        paths: Vec<String>,
    ) -> ToolResult {
        let paths = match validate_limited_cleanup_paths(&paths, true) {
            Ok(paths) => paths,
            Err(e) => return ToolResult::err(e),
        };
        let proj = match self.resolve_project(&project).await {
            Ok(project) => project,
            Err(error) => return ToolResult::err(error),
        };

        self.delete_project_files_structured_agent(&proj, proj.client_id.clone(), paths, 30)
            .await
    }

    /// Bounded structured-delete failure result. Projects the shared
    /// `ShellCommandExecutionState` to the two authoritative effect states for
    /// a mutation: `not_started` (registry evidence proves the request was
    /// never dispatched) and `outcome_unknown` (dispatched, or dispatch cannot
    /// be proven false — the Runner may already have deleted files). No
    /// shell-command facts (`command_started` / `command_completed` /
    /// `command_ok`) are emitted: a structured file operation has no command
    /// lifecycle, and emitting command fields merely for symmetry would lie.
    fn delete_project_files_lifecycle_failure(
        message: impl Into<String>,
        state: ShellCommandExecutionState,
    ) -> ToolResult {
        let (execution_state, failure_kind) = match state {
            ShellCommandExecutionState::NotStarted => ("not_started", "not_started"),
            _ => ("outcome_unknown", "outcome_unknown"),
        };
        ToolResult::err_with_output(
            message.into(),
            json!({
                "execution_state": execution_state,
                "failure_kind": failure_kind,
                "tool_failure": true,
            }),
        )
    }

    /// Structured-delete effect-boundary failure message: the request may
    /// already have executed, so the model must inspect workspace state rather
    /// than blindly retrying.
    fn delete_project_files_outcome_unknown_message() -> String {
        "agent delete_project_files outcome is unknown; the request may already have deleted files. Inspect current workspace state before deciding whether to retry."
            .to_string()
    }

    /// Structured-delete not-started failure message: registry evidence proved
    /// the request was never handed to the Runner, so nothing was executed.
    fn delete_project_files_not_started_message() -> String {
        "agent delete_project_files was not dispatched; the structured delete did not start and the Runner executed no deletion from this request."
            .to_string()
    }

    pub(crate) async fn delete_project_files_structured_agent(
        &self,
        proj: &ProjectConfig,
        client_id: String,
        paths: Vec<String>,
        wait_timeout_secs: u64,
    ) -> ToolResult {
        let payload = match serde_json::to_string(&json!({"paths": paths})) {
            Ok(payload) => payload,
            Err(_) => return ToolResult::err("failed to encode delete_project_files request"),
        };
        let (request_id, rx) = match self
            .runner_registry
            .enqueue_structured_file_delete(
                ShellFileOpRequest {
                    op: "delete_project_files".to_string(),
                    client_id,
                    path: ".".to_string(),
                    cwd: Some(proj.path.clone()),
                    content: Some(payload),
                    max_bytes: None,
                    old_text: None,
                    pattern: None,
                    expected_sha256: None,
                    expected_prefix: None,
                    start_line: None,
                    end_line: None,
                    line: None,
                    create_dirs: false,
                    wait_timeout_secs: wait_timeout_secs,
                },
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(request) => request,
            Err(error) if error.starts_with("capability_unavailable:") => {
                return Self::delete_project_files_lifecycle_failure(
                    format!("agent delete_project_files generation-2 capability invariant failed: {error}"),
                    ShellCommandExecutionState::NotStarted,
                );
            }
            Err(_) => return ToolResult::err("agent delete_project_files is unavailable"),
        };
        let response = tokio::time::timeout(Duration::from_secs(wait_timeout_secs + 2), rx).await;
        let response = match response {
            Ok(Ok(response)) => {
                // Authoritative effect-boundary evidence: only a definite
                // terminal success enters the success path; every other
                // response classifies as not_started (registry-proven
                // undispatch, e.g. runner replacement before poll) or
                // outcome_unknown (dispatched, or dispatch cannot be proven
                // false — the Runner may already have deleted files).
                let state = runner_command_lifecycle(&response, wait_timeout_secs);
                match state {
                    ShellCommandExecutionState::Completed
                        if response.error.is_none() && response.exit_code == Some(0) =>
                    {
                        response
                    }
                    ShellCommandExecutionState::NotStarted => {
                        return Self::delete_project_files_lifecycle_failure(
                            Self::delete_project_files_not_started_message(),
                            state,
                        )
                    }
                    _ => {
                        return Self::delete_project_files_lifecycle_failure(
                            Self::delete_project_files_outcome_unknown_message(),
                            state,
                        )
                    }
                }
            }
            Ok(Err(_)) => {
                // Waiter channel closed. Atomically remove the pending request
                // and classify from recovered dispatch truth; a missing record
                // cannot prove undispatch, so only explicit `Some(false)` is
                // not_started.
                let dispatch = self
                    .runner_registry
                    .cancel_request_dispatch_state(&request_id)
                    .await;
                let state = dispatch_uncertainty_lifecycle(dispatch);
                if state == ShellCommandExecutionState::NotStarted {
                    return Self::delete_project_files_lifecycle_failure(
                        Self::delete_project_files_not_started_message(),
                        state,
                    );
                }
                return Self::delete_project_files_lifecycle_failure(
                    "agent delete_project_files waiter was dropped and dispatch cannot be proven false; the request may already have deleted files. Inspect current workspace state before deciding whether to retry."
                        .to_string(),
                    state,
                );
            }
            Err(_) => {
                // Wait timeout: cancel with dispatch truth instead of erasing
                // it. A timed-out mutation that may have dispatched must never
                // be presented as definitely not started.
                let dispatch = self
                    .runner_registry
                    .cancel_request_dispatch_state(&request_id)
                    .await;
                let state = dispatch_uncertainty_lifecycle(dispatch);
                if state == ShellCommandExecutionState::NotStarted {
                    return Self::delete_project_files_lifecycle_failure(
                        format!(
                            "timed out waiting {wait_timeout_secs} seconds for agent delete_project_files before dispatch; the structured delete did not start and the Runner executed no deletion from this request."
                        ),
                        state,
                    );
                }
                return Self::delete_project_files_lifecycle_failure(
                    format!(
                        "timed out waiting {wait_timeout_secs} seconds for agent delete_project_files; the request may already have deleted files. Inspect current workspace state before deciding whether to retry."
                    ),
                    state,
                );
            }
        };
        let output: Value =
            match serde_json::from_str(response.stdout.as_deref().unwrap_or_default()) {
                Ok(output) => output,
                Err(_) => {
                    return Self::delete_project_files_lifecycle_failure(
                        Self::delete_project_files_outcome_unknown_message(),
                        ShellCommandExecutionState::OutcomeUnknown,
                    )
                }
            };
        let expected = json!(paths);
        if output.get("deleted_paths") != Some(&expected) {
            return Self::delete_project_files_lifecycle_failure(
                Self::delete_project_files_outcome_unknown_message(),
                ShellCommandExecutionState::OutcomeUnknown,
            );
        }
        ToolResult::ok(json!({
            "ok": true,
            "state_changed": !paths.is_empty(),
            "deleted_paths": paths,
            "stdout_present": false,
            "stderr_present": false,
        }))
    }

    // -------------------------------------------------------------------------
    pub(crate) async fn write_project_file(
        &self,
        project: String,
        path: String,
        content: String,
        overwrite: Option<bool>,
        expected_sha256: Option<String>,
    ) -> ToolResult {
        // ---- Input validation (before project resolution) ----
        if let Err(e) = validate_edit_file_path(&path) {
            return super::permissions::edit_path_policy_rejected_result(&path, e);
        }
        if content.contains('\0') {
            return write_project_file_preflight_rejection(
                "content cannot contain NUL bytes",
                "invalid_content",
                "remove the NUL byte and retry",
            );
        }
        if content.len() > MAX_WRITE_CONTENT_BYTES {
            return write_project_file_preflight_rejection(
                format!("content exceeds {MAX_WRITE_CONTENT_BYTES} bytes"),
                "content_too_large",
                "use apply_text_edits for a smaller local change or reduce the full rewrite",
            );
        }
        let overwrite = overwrite.unwrap_or(false);
        if let Some(hash) = expected_sha256.as_deref() {
            if !is_hex_sha256(hash) {
                return write_project_file_preflight_rejection(
                    "expected_sha256 must be a lowercase 64-character hex digest",
                    "invalid_expected_sha256",
                    "reread the file and provide its exact current sha256",
                );
            }
        }
        match (overwrite, expected_sha256.is_some()) {
            (true, false) => {
                return write_project_file_preflight_rejection(
                    "overwrite=true requires expected_sha256",
                    "missing_expected_sha256",
                    "reread the existing file and retry with its exact current sha256",
                )
            }
            (false, true) => {
                return write_project_file_preflight_rejection(
                    "expected_sha256 is allowed only when overwrite=true",
                    "unexpected_expected_sha256",
                    "omit expected_sha256 for a new-file create, or set overwrite=true for an existing-file rewrite",
                )
            }
            _ => {}
        }

        // ---- Project resolution (Runner-registered only) ----
        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let client_id = proj.client_id.clone();

        let payload = json!({
            "path": path.clone(),
            "content": content,
            "overwrite": overwrite,
            "expected_sha256": expected_sha256,
        });
        let wait_timeout = 60_u64;
        let (request_id, rx) = match self
            .runner_registry
            .enqueue_file_op(
                ShellFileOpRequest {
                    op: "write_project_file".to_string(),
                    client_id,
                    path: path.clone(),
                    cwd: Some(proj.path.clone()),
                    content: Some(payload.to_string()),
                    max_bytes: None,
                    old_text: None,
                    pattern: None,
                    expected_sha256: None,
                    expected_prefix: None,
                    start_line: None,
                    end_line: None,
                    line: None,
                    create_dirs: false,
                    wait_timeout_secs: wait_timeout,
                },
                "tool_runtime".to_string(),
            )
            .await
        {
            Ok(request) => request,
            Err(_) => {
                return structured_edit_not_started_result(
                    "write_project_file",
                    "the Runner queue rejected the request before dispatch",
                )
            }
        };
        let response = match await_structured_edit_response(
            self,
            &request_id,
            rx,
            wait_timeout,
            "write_project_file",
        )
        .await
        {
            Ok(response) => response,
            Err(result) => return result,
        };
        write_project_file_agent_stdout_result(&response.stdout.unwrap_or_default())
    }

    pub(crate) async fn apply_patch(
        &self,
        project: String,
        patch: String,
        dry_run: Option<bool>,
        matching_mode: Option<crate::apply_patch_shared::ApplyPatchMatchingMode>,
    ) -> ToolResult {
        let parsed = match crate::apply_patch_shared::parse_codex_patch(&patch) {
            Ok(parsed) => parsed,
            Err(error) => {
                return write_project_file_preflight_rejection(
                    error.to_string(),
                    error.kind,
                    "regenerate a valid Codex *** Begin Patch payload and retry",
                )
            }
        };
        let mut touched_paths = HashSet::new();
        for (change_index, hunk) in parsed.hunks.iter().enumerate() {
            let kind = hunk.kind();
            let path = hunk.path();
            if let Err(error) = validate_edit_file_path(path) {
                return apply_text_edits_path_policy_rejection(change_index, kind, path, error);
            }
            if !touched_paths.insert(path) {
                return apply_text_edits_preflight_rejection(
                    format!("change {change_index} reuses path '{path}'; each source/destination path may appear only once"),
                    "path_overlap",
                    Some(change_index),
                    None,
                    Some(kind),
                    Some(path),
                    "correct the duplicate source/destination path and retry the whole patch",
                );
            }
            if let Some(to_path) = hunk.move_path().filter(|to_path| *to_path != path) {
                if let Err(error) = validate_edit_file_path(to_path) {
                    return apply_text_edits_path_policy_rejection(
                        change_index,
                        kind,
                        to_path,
                        error,
                    );
                }
                if !touched_paths.insert(to_path) {
                    return apply_text_edits_preflight_rejection(
                        format!("change {change_index} reuses destination path '{to_path}'; each source/destination path may appear only once"),
                        "path_overlap",
                        Some(change_index),
                        None,
                        Some(kind),
                        Some(to_path),
                        "correct the duplicate source/destination path and retry the whole patch",
                    );
                }
            }
        }

        let expected_dry_run = dry_run.unwrap_or(false);
        let expected_matching_mode = matching_mode.unwrap_or_default();
        let payload = json!({
            "patch": patch,
            "dry_run": expected_dry_run,
            "matching_mode": expected_matching_mode.as_str(),
        });
        let serialized = match serde_json::to_string(&payload) {
            Ok(serialized) if serialized.len() <= MAX_APPLY_FILE_CHANGES_BYTES => serialized,
            Ok(_) => {
                return apply_text_edits_preflight_rejection(
                    format!(
                        "serialized patch payload exceeds {MAX_APPLY_FILE_CHANGES_BYTES} bytes"
                    ),
                    "payload_too_large",
                    None,
                    None,
                    None,
                    None,
                    "reduce the patch payload size and retry",
                )
            }
            Err(error) => {
                return apply_text_edits_preflight_rejection(
                    format!("failed to serialize patch payload: {error}"),
                    "serialization_failed",
                    None,
                    None,
                    None,
                    None,
                    "regenerate the patch and retry",
                )
            }
        };

        let proj = match self.resolve_project(&project).await {
            Ok(project) => project,
            Err(error) => return ToolResult::err(error),
        };
        let client_id = proj.client_id.clone();
        let routing_path = parsed
            .hunks
            .first()
            .map(|hunk| hunk.path().to_string())
            .expect("non-empty Codex patch validated above");
        let wait_timeout = 60_u64;
        let request = ShellFileOpRequest {
            op: "apply_patch".to_string(),
            client_id,
            path: routing_path,
            cwd: Some(proj.path.clone()),
            content: Some(serialized),
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            wait_timeout_secs: wait_timeout,
        };
        let (request_id, rx) = match self
            .runner_registry
            .enqueue_apply_patch(request, "tool_runtime".to_string())
            .await
        {
            Ok(request) => request,
            Err(error)
                if error.starts_with("capability_unavailable:")
                    && error.contains(
                        crate::runner_protocol::RUNNER_CAPABILITY_APPLY_PATCH_MATCHING_MODE,
                    ) =>
            {
                return apply_patch_capability_rejection(
                    error,
                    crate::runner_protocol::RUNNER_CAPABILITY_APPLY_PATCH_MATCHING_MODE,
                )
            }
            Err(error)
                if error.starts_with("capability_unavailable:")
                    && error.contains(
                        crate::runner_protocol::RUNNER_CAPABILITY_APPLY_PATCH_MATCH_METADATA,
                    ) =>
            {
                return apply_patch_capability_rejection(
                    error,
                    crate::runner_protocol::RUNNER_CAPABILITY_APPLY_PATCH_MATCH_METADATA,
                )
            }
            Err(error)
                if error.starts_with("capability_unavailable:")
                    && error.contains(crate::runner_protocol::RUNNER_CAPABILITY_APPLY_PATCH) =>
            {
                return apply_patch_capability_rejection(
                    error,
                    crate::runner_protocol::RUNNER_CAPABILITY_APPLY_PATCH,
                )
            }
            Err(_) => {
                return structured_edit_not_started_result(
                    "apply_patch",
                    "the Runner queue rejected the request before dispatch",
                )
            }
        };
        let response = match await_structured_edit_response(
            self,
            &request_id,
            rx,
            wait_timeout,
            "apply_patch",
        )
        .await
        {
            Ok(response) => response,
            Err(result) => return result,
        };
        apply_patch_agent_stdout_result(
            &response.stdout.unwrap_or_default(),
            &parsed,
            expected_dry_run,
            expected_matching_mode,
        )
    }

    pub(crate) async fn apply_text_edits(
        &self,
        project: String,
        changes: Vec<ApplyFileChangeInput>,
        dry_run: Option<bool>,
    ) -> ToolResult {
        if changes.is_empty() {
            return apply_text_edits_preflight_rejection(
                "changes must contain at least one file change",
                "empty_batch",
                None,
                None,
                None,
                None,
                "add at least one valid file change and retry",
            );
        }
        if changes.len() > MAX_APPLY_FILE_CHANGES {
            return apply_text_edits_preflight_rejection(
                format!(
                    "too many file changes; maximum is {}",
                    MAX_APPLY_FILE_CHANGES
                ),
                "batch_too_large",
                None,
                None,
                None,
                None,
                "reduce the batch to the supported file-change limit and retry",
            );
        }
        let mut touched_paths = HashSet::new();
        for (change_index, change) in changes.iter().enumerate() {
            if let Err(error) = validate_edit_file_path(&change.path) {
                return apply_text_edits_path_policy_rejection(
                    change_index,
                    change.kind.as_str(),
                    &change.path,
                    error,
                );
            }
            if !touched_paths.insert(change.path.as_str()) {
                return apply_text_edits_preflight_rejection(
                    format!(
                        "change {change_index} reuses path '{}'; each source/destination path may appear only once",
                        change.path
                    ),
                    "path_overlap",
                    Some(change_index),
                    None,
                    Some(change.kind.as_str()),
                    Some(&change.path),
                    "correct the duplicate source/destination path and retry the whole batch",
                );
            }
            if let Some(to_path) = change.to_path.as_deref() {
                if let Err(error) = validate_edit_file_path(to_path) {
                    return apply_text_edits_path_policy_rejection(
                        change_index,
                        change.kind.as_str(),
                        to_path,
                        error,
                    );
                }
                if !touched_paths.insert(to_path) {
                    return apply_text_edits_preflight_rejection(
                        format!(
                            "change {change_index} reuses destination path '{to_path}'; each source/destination path may appear only once"
                        ),
                        "path_overlap",
                        Some(change_index),
                        None,
                        Some(change.kind.as_str()),
                        Some(to_path),
                        "correct the duplicate source/destination path and retry the whole batch",
                    );
                }
            }
            if let Err(validation_error) = validate_apply_file_change(change_index, change) {
                let failed_kind = validation_error
                    .edit_index
                    .and_then(|edit_index| change.edits.get(edit_index))
                    .map(|edit| edit.kind.as_str())
                    .unwrap_or_else(|| change.kind.as_str());
                return apply_text_edits_preflight_rejection(
                    validation_error.message,
                    if validation_error.edit_index.is_some() {
                        "invalid_edit"
                    } else {
                        "invalid_change"
                    },
                    Some(change_index),
                    validation_error.edit_index,
                    Some(failed_kind),
                    Some(&change.path),
                    "correct the rejected change or edit and retry the whole batch",
                );
            }
        }
        let requires_occurrence_capability = changes
            .iter()
            .flat_map(|change| change.edits.iter())
            .any(|edit| edit.occurrence.is_some());
        let requires_line_scope_capability = changes
            .iter()
            .flat_map(|change| change.edits.iter())
            .any(|edit| edit.line_scope.is_some());

        let expected_change_count = changes.len();
        let expected_dry_run = dry_run.unwrap_or(false);
        let payload = json!({
            "changes": changes,
            "dry_run": expected_dry_run,
            "recovery_metadata_version": 1,
        });
        let serialized = match serde_json::to_string(&payload) {
            Ok(serialized) if serialized.len() <= MAX_APPLY_FILE_CHANGES_BYTES => serialized,
            Ok(_) => {
                return apply_text_edits_preflight_rejection(
                    format!(
                        "serialized file changes exceed {} bytes",
                        MAX_APPLY_FILE_CHANGES_BYTES
                    ),
                    "payload_too_large",
                    None,
                    None,
                    None,
                    None,
                    "reduce the batch payload size and retry",
                )
            }
            Err(error) => {
                return apply_text_edits_preflight_rejection(
                    format!("failed to serialize file changes payload: {error}"),
                    "serialization_failed",
                    None,
                    None,
                    None,
                    None,
                    "correct the request payload and retry",
                )
            }
        };

        let proj = match self.resolve_project(&project).await {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let client_id = proj.client_id.clone();

        let wait_timeout = 60_u64;
        let routing_path = changes
            .first()
            .map(|change| change.path.clone())
            .expect("non-empty changes validated above");
        let request = ShellFileOpRequest {
            op: "apply_text_edits".to_string(),
            client_id,
            path: routing_path,
            cwd: Some(proj.path.clone()),
            content: Some(serialized),
            max_bytes: None,
            old_text: None,
            pattern: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            line: None,
            create_dirs: false,
            wait_timeout_secs: wait_timeout,
        };
        let enqueue_result = if requires_line_scope_capability {
            self.runner_registry
                .enqueue_apply_text_edits_with_line_scope(
                    request,
                    "tool_runtime".to_string(),
                    requires_occurrence_capability,
                )
                .await
        } else if requires_occurrence_capability {
            self.runner_registry
                .enqueue_apply_text_edits_with_occurrence(request, "tool_runtime".to_string())
                .await
        } else {
            self.runner_registry
                .enqueue_file_op(request, "tool_runtime".to_string())
                .await
        };
        let (request_id, rx) = match enqueue_result {
            Ok(r) => r,
            Err(e)
                if e.starts_with("capability_unavailable:")
                    && e.contains(
                        crate::runner_protocol::RUNNER_CAPABILITY_APPLY_TEXT_EDIT_LINE_SCOPE,
                    ) =>
            {
                return apply_text_edit_line_scope_capability_rejection(e)
            }
            Err(e)
                if e.starts_with("capability_unavailable:")
                    && e.contains(
                        crate::runner_protocol::RUNNER_CAPABILITY_APPLY_TEXT_EDIT_OCCURRENCE,
                    ) =>
            {
                return apply_text_edit_occurrence_capability_rejection(e)
            }
            Err(_) => {
                return structured_edit_not_started_result(
                    "apply_text_edits",
                    "the Runner queue rejected the request before dispatch",
                )
            }
        };
        let response = match await_structured_edit_response(
            self,
            &request_id,
            rx,
            wait_timeout,
            "apply_text_edits",
        )
        .await
        {
            Ok(response) => response,
            Err(result) => return result,
        };
        apply_text_edits_agent_stdout_result(
            &response.stdout.unwrap_or_default(),
            expected_change_count,
            expected_dry_run,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_patch_success_payload(
        matching_mode: crate::apply_patch_shared::ApplyPatchMatchingMode,
        match_mode: &str,
        candidate_count: u64,
        unique_match: bool,
        strict_match: bool,
    ) -> Value {
        json!({
            "dry_run": true,
            "requested_matching_mode": matching_mode.as_str(),
            "applied_count": 1,
            "changed": false,
            "state_changed": false,
            "execution_state": "completed",
            "would_change": true,
            "files": [{
                "index": 0,
                "kind": "edit",
                "path": "file.txt",
                "to_path": null,
                "old_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "new_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "changed": false,
                "would_change": true,
                "edits": [{
                    "chunk_index": 0,
                    "change_context_present": false,
                    "old_line_count": 1,
                    "new_line_count": 1,
                    "end_of_file": false,
                    "match_mode": match_mode,
                    "match_source": "old_lines",
                    "matched_start_line": 1,
                    "candidate_count": candidate_count,
                    "unique_match": unique_match,
                    "strict_match": strict_match,
                }]
            }],
            "changed_paths": ["file.txt"]
        })
    }

    fn one_update_patch() -> crate::apply_patch_shared::CodexPatch {
        crate::apply_patch_shared::parse_codex_patch(
            "*** Begin Patch\n*** Update File: file.txt\n-old\n+new\n*** End Patch",
        )
        .unwrap()
    }

    fn update_patch_with_old_line_count(count: usize) -> crate::apply_patch_shared::CodexPatch {
        assert!(count > 0);
        let mut patch = String::from("*** Begin Patch\n*** Update File: file.txt\n");
        for index in 0..count {
            patch.push_str(&format!("-old-{index}\n"));
        }
        patch.push_str("+new\n*** End Patch");
        crate::apply_patch_shared::parse_codex_patch(&patch).unwrap()
    }

    fn context_mismatch_payload(
        expected_line_count: usize,
        search_start_line: usize,
        available_line_count: usize,
        closest_start_line: Option<usize>,
    ) -> Value {
        json!({
            "changed": false,
            "state_changed": false,
            "execution_state": "not_started",
            "error_kind": "context_mismatch",
            "change_index": 0,
            "path": "file.txt",
            "error": "Rejected Codex patch before write: context mismatch. No files were modified.",
            "match_diagnostic": {
                "chunk_index": 0,
                "match_source": "old_lines",
                "search_start_line": search_start_line,
                "expected_line_count": expected_line_count,
                "available_line_count": available_line_count,
                "closest_start_line": closest_start_line,
                "closest_exact_line_matches": 0,
                "closest_trim_end_line_matches": 0,
                "closest_trim_line_matches": 0,
                "first_exact_mismatch_offset": 1
            }
        })
    }

    fn matching_rejection_payload(
        matching_mode: crate::apply_patch_shared::ApplyPatchMatchingMode,
        match_mode: &str,
        candidate_start_lines: &[usize],
        candidate_count: usize,
        source_line_count: usize,
    ) -> Value {
        let ambiguous = candidate_count > 1;
        json!({
            "changed": false,
            "state_changed": false,
            "execution_state": "not_started",
            "error_kind": "matching_mode_rejected",
            "change_index": 0,
            "path": "file.txt",
            "chunk_index": 0,
            "requested_matching_mode": matching_mode.as_str(),
            "match_mode": match_mode,
            "match_source": "old_lines",
            "matched_start_line": if ambiguous {
                Value::Null
            } else {
                json!(candidate_start_lines.first().copied())
            },
            "candidate_count": candidate_count,
            "candidate_start_lines": candidate_start_lines,
            "candidate_positions_truncated": candidate_count > crate::apply_patch_shared::MAX_CODEX_PATCH_CANDIDATE_POSITIONS,
            "matching_mode_satisfied": false,
            "search_start_line": 1,
            "source_line_count": source_line_count,
            "recovery_action": "RUNNER_MUST_NOT_CHOOSE_RECOVERY",
            "retry_guidance": "RUNNER_MUST_NOT_CHOOSE_GUIDANCE",
            "error": "RUNNER_MUST_NOT_CHOOSE_ERROR",
        })
    }

    #[test]
    fn apply_patch_matching_mode_capability_rejection_fails_closed() {
        let result = apply_patch_capability_rejection(
            "capability_unavailable: demo lacks apply_patch_matching_mode",
            crate::runner_protocol::RUNNER_CAPABILITY_APPLY_PATCH_MATCHING_MODE,
        );

        assert!(!result.success);
        assert_eq!(result.output["state_changed"], false);
        assert_eq!(result.output["execution_state"], "not_started");
        assert_eq!(result.output["error_kind"], "agent_capability_unavailable");
        assert_eq!(
            result.output["capability"],
            crate::runner_protocol::RUNNER_CAPABILITY_APPLY_PATCH_MATCHING_MODE
        );
        assert_eq!(result.output["recovery_kind"], "retry_same");
    }

    #[test]
    fn apply_patch_failure_match_diagnostic_is_validated_before_projection() {
        let patch = one_update_patch();
        let valid = json!({
            "changed": false,
            "state_changed": false,
            "execution_state": "not_started",
            "error_kind": "context_mismatch",
            "change_index": 0,
            "path": "file.txt",
            "error": "Rejected Codex patch before write: context mismatch. No files were modified.",
            "future_body_field": "NEVER_SURVIVE_PATCH_FAILURE",
            "match_diagnostic": {
                "chunk_index": 0,
                "match_source": "old_lines",
                "search_start_line": 3,
                "expected_line_count": 1,
                "available_line_count": 8,
                "closest_start_line": 5,
                "closest_exact_line_matches": 0,
                "closest_trim_end_line_matches": 0,
                "closest_trim_line_matches": 0,
                "first_exact_mismatch_offset": 1
            }
        });
        let result = apply_patch_agent_stdout_result(
            &valid.to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert!(!result.success);
        assert_eq!(result.output["match_diagnostic"]["closest_start_line"], 5);
        let recovery_action = result.output["recovery"]["action"].as_str().unwrap();
        assert_eq!(recovery_action, "read_files");
        assert!(
            crate::tool_runtime::tool_definition::is_adaptive_runtime_direct_tool(recovery_action)
        );
        assert_eq!(result.output["recovery"]["reason"], "context_mismatch");
        assert_eq!(result.output["recovery"]["items"][0]["path"], "file.txt");
        assert_eq!(result.output["recovery"]["items"][0]["start_line"], 1);
        assert_eq!(result.output["recovery"]["items"][0]["limit"], 10);
        assert_eq!(result.output["recovery"]["change_index"], 0);
        assert_eq!(result.output["recovery"]["chunk_index"], 0);
        assert!(result.output.get("future_body_field").is_none());
        assert!(!serde_json::to_string(&result.output)
            .unwrap()
            .contains("NEVER_SURVIVE_PATCH_FAILURE"));

        let mut cases = Vec::new();
        let mut unexpected_field = valid.clone();
        unexpected_field["match_diagnostic"]["unexpected_field"] = json!("must-not-survive");
        cases.push(unexpected_field);
        let mut wrong_change = valid.clone();
        wrong_change["change_index"] = json!(1);
        cases.push(wrong_change);
        let mut wrong_path = valid.clone();
        wrong_path["path"] = json!("other.txt");
        cases.push(wrong_path);
        let mut wrong_chunk = valid.clone();
        wrong_chunk["match_diagnostic"]["chunk_index"] = json!(1);
        cases.push(wrong_chunk);
        let mut wrong_count = valid.clone();
        wrong_count["match_diagnostic"]["expected_line_count"] = json!(2);
        cases.push(wrong_count);
        let mut impossible_order = valid;
        impossible_order["match_diagnostic"]["closest_exact_line_matches"] = json!(1);
        cases.push(impossible_order);
        let mut out_of_range_candidate = context_mismatch_payload(1, 3, 8, Some(11));
        out_of_range_candidate["recovery"] = json!({
            "action": "read_file",
            "path": "other.txt",
            "start_line": 999999,
            "limit": 999999
        });
        cases.push(out_of_range_candidate);

        for invalid in cases {
            let result = apply_patch_agent_stdout_result(
                &invalid.to_string(),
                &patch,
                false,
                crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            );
            assert!(!result.success);
            assert_eq!(result.output["execution_state"], "not_started");
            assert_eq!(result.output["state_changed"], false);
            assert!(result.output.get("match_diagnostic").is_none());
            assert!(result.output.get("recovery").is_none());
            assert!(!serde_json::to_string(&result.output)
                .unwrap()
                .contains("must-not-survive"));
        }
    }

    #[test]
    fn apply_patch_context_recovery_uses_deterministic_bounded_read_windows() {
        let patch = update_patch_with_old_line_count(5);
        let result = apply_patch_agent_stdout_result(
            &context_mismatch_payload(5, 120, 50, Some(130)).to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        let schema = crate::tool_runtime::registry::output_schema_for_tool("apply_patch");
        crate::tool_runtime::startup_brief::validate_schema_instance_for_test(
            &serde_json::to_value(&result).unwrap(),
            &schema,
        )
        .unwrap_or_else(|error| panic!("apply_patch recovery must match output schema: {error}"));
        assert_eq!(result.output["recovery"]["items"][0]["start_line"], 122);
        assert_eq!(result.output["recovery"]["items"][0]["limit"], 21);

        let near_start = apply_patch_agent_stdout_result(
            &context_mismatch_payload(5, 1, 20, Some(1)).to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert_eq!(near_start.output["recovery"]["items"][0]["start_line"], 1);
        assert_eq!(near_start.output["recovery"]["items"][0]["limit"], 20);

        let eof_partial = apply_patch_agent_stdout_result(
            &context_mismatch_payload(5, 11, 2, Some(12)).to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        let recovery = &eof_partial.output["recovery"]["items"][0];
        assert_eq!(recovery["start_line"], 4);
        assert_eq!(recovery["limit"], 9);
        assert_eq!(
            recovery["start_line"].as_u64().unwrap() + recovery["limit"].as_u64().unwrap() - 1,
            12
        );

        let large_patch = update_patch_with_old_line_count(100);
        let large = apply_patch_agent_stdout_result(
            &context_mismatch_payload(100, 1, 300, Some(100)).to_string(),
            &large_patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert_eq!(
            large.output["recovery"]["items"][0]["limit"],
            crate::apply_patch_shared::MAX_CODEX_PATCH_RECOVERY_READ_LINES
        );

        let mut distant_mismatch_payload = context_mismatch_payload(100, 1, 300, Some(100));
        distant_mismatch_payload["match_diagnostic"]["closest_exact_line_matches"] = json!(99);
        distant_mismatch_payload["match_diagnostic"]["closest_trim_end_line_matches"] = json!(99);
        distant_mismatch_payload["match_diagnostic"]["closest_trim_line_matches"] = json!(99);
        distant_mismatch_payload["match_diagnostic"]["first_exact_mismatch_offset"] = json!(90);
        let distant_mismatch = apply_patch_agent_stdout_result(
            &distant_mismatch_payload.to_string(),
            &large_patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        let recovery = &distant_mismatch.output["recovery"]["items"][0];
        assert_eq!(recovery["start_line"], 134);
        assert_eq!(
            recovery["limit"],
            crate::apply_patch_shared::MAX_CODEX_PATCH_RECOVERY_READ_LINES
        );
        let mismatch_line = 100 + 90 - 1;
        let recovery_start = recovery["start_line"].as_u64().unwrap() as usize;
        let recovery_end = recovery_start + recovery["limit"].as_u64().unwrap() as usize - 1;
        assert!((recovery_start..=recovery_end).contains(&mismatch_line));
    }

    #[test]
    fn apply_patch_context_recovery_does_not_invent_candidate_or_leak_bodies() {
        let patch = update_patch_with_old_line_count(3);
        let no_candidate = apply_patch_agent_stdout_result(
            &context_mismatch_payload(3, 5, 0, None).to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert!(no_candidate.output.get("match_diagnostic").is_some());
        assert!(no_candidate.output.get("recovery").is_none());

        let private_patch = crate::apply_patch_shared::parse_codex_patch(
            "*** Begin Patch\n*** Update File: file.txt\n-PATCH_PRIVATE_TOKEN\n+new\n*** End Patch",
        )
        .unwrap();
        let mut payload = context_mismatch_payload(1, 1, 3, Some(2));
        payload["future_body_field"] = json!("SOURCE_PRIVATE_TOKEN");
        payload["recovery"] = json!({
            "action": "read_file",
            "reason": "context_mismatch",
            "path": "SOURCE_PRIVATE_TOKEN",
            "start_line": 1,
            "limit": 999999,
            "change_index": 0,
            "chunk_index": 0
        });
        let result = apply_patch_agent_stdout_result(
            &payload.to_string(),
            &private_patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        let serialized = serde_json::to_string(&result.output).unwrap();
        assert!(!serialized.contains("SOURCE_PRIVATE_TOKEN"));
        assert!(!serialized.contains("PATCH_PRIVATE_TOKEN"));
        assert_eq!(result.output["recovery"]["items"][0]["path"], "file.txt");
        assert!(
            result.output["recovery"]["items"][0]["limit"]
                .as_u64()
                .unwrap()
                <= 64
        );
    }

    #[test]
    fn apply_patch_context_recovery_is_suppressed_for_outcome_unknown() {
        let patch = one_update_patch();
        let mut payload = context_mismatch_payload(1, 1, 3, Some(2));
        payload["changed"] = json!(true);
        payload["recovery"] = json!({"action": "read_file", "path": "other.txt"});

        let result = apply_patch_agent_stdout_result(
            &payload.to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert!(!result.success);
        assert_eq!(result.output["execution_state"], "outcome_unknown");
        assert_eq!(
            result.output["recovery_action"],
            "inspect_workspace_before_retry"
        );
        assert!(result.output.get("match_diagnostic").is_none());
        assert!(result.output.get("recovery").is_none());
        let schema = crate::tool_runtime::registry::output_schema_for_tool("apply_patch");
        crate::tool_runtime::startup_brief::validate_schema_instance_for_test(
            &serde_json::to_value(&result).unwrap(),
            &schema,
        )
        .unwrap_or_else(|error| {
            panic!("apply_patch outcome_unknown must match output schema: {error}")
        });
    }

    #[test]
    fn apply_patch_exact_unique_fuzzy_rejection_gets_validated_bounded_reread() {
        let patch = one_update_patch();
        let result = apply_patch_agent_stdout_result(
            &matching_rejection_payload(
                crate::apply_patch_shared::ApplyPatchMatchingMode::ExactUnique,
                "trim",
                &[20],
                1,
                100,
            )
            .to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::ExactUnique,
        );

        assert!(!result.success);
        assert_eq!(result.output["execution_state"], "not_started");
        assert_eq!(result.output["state_changed"], false);
        assert_eq!(
            result.output["match_rejection_diagnostic"]["classification"],
            "unique_fuzzy_candidate"
        );
        assert_eq!(
            result.output["match_rejection_diagnostic"]["match_mode"],
            "trim"
        );
        assert_eq!(
            result.output["match_rejection_diagnostic"]["match_source"],
            "old_lines"
        );
        assert_eq!(
            result.output["match_rejection_diagnostic"]["matched_start_line"],
            20
        );
        assert_eq!(
            result.output["match_rejection_diagnostic"]["candidate_count"],
            1
        );
        assert_eq!(
            result.output["match_rejection_diagnostic"]["candidate_start_lines"],
            json!([20])
        );
        assert_eq!(
            result.output["match_rejection_diagnostic"]["expected_line_count"],
            1
        );
        assert_eq!(
            result.output["match_rejection_diagnostic"]["matching_mode_satisfied"],
            false
        );
        assert_eq!(result.output["recovery"]["action"], "read_files");
        assert_eq!(
            result.output["recovery"]["reason"],
            "matching_mode_rejected_unique_fuzzy"
        );
        assert_eq!(result.output["recovery"]["items"][0]["path"], "file.txt");
        assert_eq!(result.output["recovery"]["items"][0]["start_line"], 12);
        assert_eq!(result.output["recovery"]["items"][0]["limit"], 17);
        assert_eq!(
            result.output["recovery_action"],
            "reread_and_regenerate_exact_unique_patch"
        );
        assert!(result.output["retry_guidance"]
            .as_str()
            .unwrap()
            .contains("matching_mode=exact_unique"));
        for raw_runner_field in [
            "chunk_index",
            "match_mode",
            "match_source",
            "matched_start_line",
            "candidate_count",
            "matching_mode_satisfied",
        ] {
            assert!(result.output.get(raw_runner_field).is_none());
        }

        let schema = crate::tool_runtime::registry::output_schema_for_tool("apply_patch");
        crate::tool_runtime::startup_brief::validate_schema_instance_for_test(
            &serde_json::to_value(&result).unwrap(),
            &schema,
        )
        .unwrap_or_else(|error| panic!("matching recovery must match output schema: {error}"));
    }

    #[test]
    fn apply_patch_unique_ambiguous_rejection_returns_equal_candidate_windows() {
        let patch = one_update_patch();
        let result = apply_patch_agent_stdout_result(
            &matching_rejection_payload(
                crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
                "exact",
                &[20, 60],
                2,
                100,
            )
            .to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );

        assert!(!result.success);
        assert_eq!(
            result.output["match_rejection_diagnostic"]["classification"],
            "ambiguous_candidate"
        );
        assert_eq!(
            result.output["match_rejection_diagnostic"]["candidate_count"],
            2
        );
        assert_eq!(
            result.output["match_rejection_diagnostic"]["candidate_start_lines"],
            json!([20, 60])
        );
        assert!(result.output["match_rejection_diagnostic"]["matched_start_line"].is_null());
        assert_eq!(
            result.output["recovery"]["items"].as_array().unwrap().len(),
            2
        );
        assert_eq!(result.output["recovery"]["items"][0]["path"], "file.txt");
        assert_eq!(result.output["recovery"]["items"][1]["path"], "file.txt");
        assert_eq!(
            result.output["recovery_action"],
            "read_equal_candidates_and_refine_context"
        );
        assert!(result.output["retry_guidance"]
            .as_str()
            .unwrap()
            .contains("equal candidate"));
        assert!(!result.output["retry_guidance"]
            .as_str()
            .unwrap()
            .contains("preferred"));
    }

    #[test]
    fn apply_patch_unique_large_ambiguity_returns_four_truncated_equal_windows() {
        let patch = one_update_patch();
        let result = apply_patch_agent_stdout_result(
            &matching_rejection_payload(
                crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
                "exact",
                &[10, 20, 30, 40],
                5,
                100,
            )
            .to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );

        assert!(!result.success);
        let diagnostic = &result.output["match_rejection_diagnostic"];
        assert_eq!(diagnostic["classification"], "ambiguous_candidate");
        assert_eq!(diagnostic["candidate_count"], 5);
        assert_eq!(diagnostic["candidate_start_lines"], json!([10, 20, 30, 40]));
        assert_eq!(diagnostic["candidate_positions_truncated"], true);
        assert_eq!(
            result.output["recovery"]["items"].as_array().unwrap().len(),
            4
        );
        assert!(diagnostic.get("winner").is_none());
        assert!(diagnostic.get("preferred").is_none());
        assert!(result.output["recovery"].get("winner").is_none());
        assert!(result.output["recovery"].get("preferred").is_none());
    }

    #[test]
    fn apply_patch_unique_eof_rejection_cannot_expand_effective_search_range() {
        let patch = crate::apply_patch_shared::parse_codex_patch(
            "*** Begin Patch\n*** Update File: file.txt\n-old\n+new\n*** End of File\n*** End Patch",
        )
        .unwrap();
        let mut payload = matching_rejection_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            &[1, 3],
            2,
            3,
        );
        payload["search_start_line"] = json!(1);

        let result = apply_patch_agent_stdout_result(
            &payload.to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert!(!result.success);
        assert!(result.output.get("match_rejection_diagnostic").is_none());
        assert!(result.output.get("recovery").is_none());
    }

    #[test]
    fn apply_patch_unique_ambiguous_context_fact_is_valid_for_pure_addition() {
        let patch = crate::apply_patch_shared::parse_codex_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@ ctx\n+new\n*** End Patch",
        )
        .unwrap();
        let mut payload = matching_rejection_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            &[1, 3],
            2,
            4,
        );
        payload["match_source"] = json!("change_context");

        let result = apply_patch_agent_stdout_result(
            &payload.to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert!(!result.success);
        assert_eq!(
            result.output["match_rejection_diagnostic"]["classification"],
            "ambiguous_candidate"
        );
        assert_eq!(
            result.output["match_rejection_diagnostic"]["match_source"],
            "change_context"
        );
        assert!(result.output["match_rejection_diagnostic"]["matched_start_line"].is_null());
        assert_eq!(
            result.output["recovery"]["items"].as_array().unwrap().len(),
            2
        );
    }

    #[test]
    fn apply_patch_unique_rejects_context_only_ambiguity_for_replacement_chunk() {
        let patch = crate::apply_patch_shared::parse_codex_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@ ctx\n-old\n+new\n*** End Patch",
        )
        .unwrap();
        let mut payload = matching_rejection_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            &[1, 3],
            2,
            4,
        );
        payload["match_source"] = json!("change_context");

        let result = apply_patch_agent_stdout_result(
            &payload.to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert!(!result.success);
        assert!(result.output.get("match_rejection_diagnostic").is_none());
        assert!(result.output.get("recovery").is_none());
        assert!(result.output.get("path").is_none());
        assert!(result.output.get("change_index").is_none());
    }

    #[test]
    fn apply_patch_matching_recovery_suppresses_spoofed_or_contradictory_metadata() {
        let patch = one_update_patch();
        let mut cases = Vec::new();

        let base = || {
            matching_rejection_payload(
                crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
                "exact",
                &[20, 60],
                2,
                100,
            )
        };
        let mut wrong_path = base();
        wrong_path["path"] = json!("other.txt");
        cases.push(wrong_path);
        let mut wrong_change = base();
        wrong_change["change_index"] = json!(1);
        cases.push(wrong_change);
        let mut wrong_chunk = base();
        wrong_chunk["chunk_index"] = json!(1);
        cases.push(wrong_chunk);
        let mut wrong_source = base();
        wrong_source["match_source"] = json!("change_context");
        cases.push(wrong_source);
        let mut equal_positions = base();
        equal_positions["candidate_start_lines"] = json!([20, 20]);
        cases.push(equal_positions);
        let mut out_of_range = base();
        out_of_range["candidate_start_lines"] = json!([20, 101]);
        cases.push(out_of_range);
        let mut bad_truncation = matching_rejection_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            &[10, 20, 30, 40],
            5,
            100,
        );
        bad_truncation["candidate_positions_truncated"] = json!(false);
        cases.push(bad_truncation);
        let mut wrong_mode = base();
        wrong_mode["requested_matching_mode"] = json!("exact_unique");
        cases.push(wrong_mode);

        for payload in cases {
            let result = apply_patch_agent_stdout_result(
                &payload.to_string(),
                &patch,
                false,
                crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            );
            assert!(!result.success);
            assert!(result.output.get("match_rejection_diagnostic").is_none());
            assert!(result.output.get("recovery").is_none());
            assert!(result.output.get("path").is_none());
            assert!(result.output.get("change_index").is_none());
        }
    }

    #[test]
    fn apply_patch_matching_recovery_is_suppressed_for_outcome_unknown() {
        let patch = one_update_patch();
        let mut payload = matching_rejection_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            &[20, 60],
            2,
            100,
        );
        payload["changed"] = json!(true);
        payload["state_changed"] = json!(true);

        let result = apply_patch_agent_stdout_result(
            &payload.to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert!(!result.success);
        assert_eq!(result.output["execution_state"], "outcome_unknown");
        assert_eq!(
            result.output["recovery_action"],
            "inspect_workspace_before_retry"
        );
        assert!(result.output.get("match_rejection_diagnostic").is_none());
        assert!(result.output.get("recovery").is_none());
    }

    #[test]
    fn apply_patch_matching_recovery_never_leaks_source_or_patch_bodies() {
        let patch = crate::apply_patch_shared::parse_codex_patch(
            "*** Begin Patch\n*** Update File: file.txt\n-PATCH_PRIVATE_TOKEN\n+new\n*** End Patch",
        )
        .unwrap();
        let mut payload = matching_rejection_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            &[4, 8],
            2,
            20,
        );
        payload["error"] = json!("SOURCE_PRIVATE_TOKEN");
        payload["future_body_field"] = json!("SOURCE_PRIVATE_TOKEN");
        payload["recovery"] = json!({
            "action": "read_files",
            "items": [{"path": "SOURCE_PRIVATE_TOKEN", "start_line": 1, "limit": 999999}]
        });
        payload["match_rejection_diagnostic"] = json!({"source": "SOURCE_PRIVATE_TOKEN"});
        payload["recovery_kind"] = json!("reobserve");
        payload["recovery_tool"] = json!("list_jobs");

        let result = apply_patch_agent_stdout_result(
            &payload.to_string(),
            &patch,
            false,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("SOURCE_PRIVATE_TOKEN"));
        assert!(!serialized.contains("PATCH_PRIVATE_TOKEN"));
        assert!(result.output.get("recovery_kind").is_none());
        assert!(result.output.get("recovery_tool").is_none());
        assert_eq!(result.output["recovery"]["items"][0]["path"], "file.txt");
    }

    #[test]
    fn apply_patch_success_metadata_is_bound_to_requested_matching_mode() {
        let patch = one_update_patch();
        for (matching_mode, mode, count, unique_match, strict_match) in [
            (
                crate::apply_patch_shared::ApplyPatchMatchingMode::ExactUnique,
                "exact",
                1,
                true,
                true,
            ),
            (
                crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
                "trim_end",
                1,
                true,
                false,
            ),
            (
                crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
                "trim",
                1,
                true,
                false,
            ),
            (
                crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
                "normalized",
                1,
                true,
                false,
            ),
            (
                crate::apply_patch_shared::ApplyPatchMatchingMode::FirstMatch,
                "exact",
                2,
                false,
                false,
            ),
        ] {
            let payload =
                apply_patch_success_payload(matching_mode, mode, count, unique_match, strict_match)
                    .to_string();
            let result = apply_patch_agent_stdout_result(&payload, &patch, true, matching_mode);
            assert!(result.success, "{:?}", result.error);
            assert_eq!(result.output["execution_state"], "completed");
            assert_eq!(result.output["files"][0]["edits"][0]["match_mode"], mode);
            assert_eq!(
                result.output["requested_matching_mode"],
                matching_mode.as_str()
            );
        }
    }

    #[test]
    fn apply_patch_success_metadata_rejects_untrusted_match_summaries_and_sanitizes_them() {
        let patch = one_update_patch();
        let mut cases = Vec::new();

        let mut unique_violation = apply_patch_success_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            2,
            false,
            false,
        );
        cases.push(unique_violation.take());

        let mut wrong_source = apply_patch_success_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            1,
            true,
            true,
        );
        wrong_source["files"][0]["edits"][0]["match_source"] = json!("append");
        cases.push(wrong_source.clone());

        let mut wrong_chunk = wrong_source;
        wrong_chunk["files"][0]["edits"][0]["match_source"] = json!("old_lines");
        wrong_chunk["files"][0]["edits"][0]["chunk_index"] = json!(1);
        cases.push(wrong_chunk);

        let mut wrong_path = apply_patch_success_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            1,
            true,
            true,
        );
        wrong_path["files"][0]["path"] = json!("other.txt");
        cases.push(wrong_path);

        let contradictory_strict = apply_patch_success_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "trim",
            1,
            true,
            true,
        );
        cases.push(contradictory_strict);

        let wrong_requested_mode = apply_patch_success_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::ExactUnique,
            "exact",
            1,
            true,
            true,
        );
        cases.push(wrong_requested_mode);

        for payload in cases {
            let result = apply_patch_agent_stdout_result(
                &payload.to_string(),
                &patch,
                true,
                crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            );
            assert!(!result.success);
            assert_eq!(result.output["execution_state"], "outcome_unknown");
            assert!(result.output["state_changed"].is_null());
            assert!(result.output.get("files").is_none());
            assert!(result.output.get("changed_paths").is_none());
            assert!(!serde_json::to_string(&result.output)
                .unwrap()
                .contains("NEVER_SURVIVE_PATCH_METADATA"));
            assert!(result
                .error
                .as_deref()
                .unwrap()
                .contains("invalid or contradictory patch-plan metadata"));
        }
    }

    #[test]
    fn apply_patch_success_metadata_strips_unknown_fields_without_losing_known_result() {
        let patch = one_update_patch();
        let mut payload = apply_patch_success_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            1,
            true,
            true,
        );
        payload["future_top_level"] = json!("NEVER_SURVIVE_PATCH_METADATA");
        payload["files"][0]["future_file_field"] = json!("NEVER_SURVIVE_PATCH_METADATA");
        payload["files"][0]["edits"][0]["future_edit_field"] =
            json!("NEVER_SURVIVE_PATCH_METADATA");

        let result = apply_patch_agent_stdout_result(
            &payload.to_string(),
            &patch,
            true,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["execution_state"], "completed");
        let serialized = serde_json::to_string(&result.output).unwrap();
        assert!(!serialized.contains("future_top_level"));
        assert!(!serialized.contains("future_file_field"));
        assert!(!serialized.contains("future_edit_field"));
        assert!(!serialized.contains("NEVER_SURVIVE_PATCH_METADATA"));
    }

    #[test]
    fn apply_patch_success_metadata_accepts_create_delete_rename_and_append_shapes() {
        let patch = crate::apply_patch_shared::parse_codex_patch(
            "*** Begin Patch\n*** Add File: new.txt\n+hello\n*** Delete File: old.txt\n*** Update File: move.txt\n*** Move to: moved.txt\n-old\n+new\n*** Update File: append.txt\n+tail\n*** End Patch",
        )
        .unwrap();
        let payload = json!({
            "dry_run": true,
            "requested_matching_mode": "unique",
            "applied_count": 4,
            "changed": false,
            "state_changed": false,
            "execution_state": "completed",
            "would_change": true,
            "files": [
                {
                    "index": 0,
                    "kind": "create",
                    "path": "new.txt",
                    "to_path": null,
                    "old_sha256": null,
                    "new_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "changed": false,
                    "would_change": true,
                    "edits": []
                },
                {
                    "index": 1,
                    "kind": "delete",
                    "path": "old.txt",
                    "to_path": null,
                    "old_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "new_sha256": null,
                    "changed": false,
                    "would_change": true,
                    "edits": []
                },
                {
                    "index": 2,
                    "kind": "rename",
                    "path": "move.txt",
                    "to_path": "moved.txt",
                    "old_sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                    "new_sha256": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                    "changed": false,
                    "would_change": true,
                    "edits": [{
                        "chunk_index": 0,
                        "change_context_present": false,
                        "old_line_count": 1,
                        "new_line_count": 1,
                        "end_of_file": false,
                        "match_mode": "exact",
                        "match_source": "old_lines",
                        "matched_start_line": 1,
                        "candidate_count": 1,
                        "unique_match": true,
                        "strict_match": true
                    }]
                },
                {
                    "index": 3,
                    "kind": "edit",
                    "path": "append.txt",
                    "to_path": null,
                    "old_sha256": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                    "new_sha256": "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                    "changed": false,
                    "would_change": true,
                    "edits": [{
                        "chunk_index": 0,
                        "change_context_present": false,
                        "old_line_count": 0,
                        "new_line_count": 1,
                        "end_of_file": false,
                        "match_mode": null,
                        "match_source": "append",
                        "matched_start_line": 2,
                        "candidate_count": null,
                        "unique_match": true,
                        "strict_match": true
                    }]
                }
            ],
            "changed_paths": ["new.txt", "old.txt", "move.txt", "moved.txt", "append.txt"]
        });

        let result = apply_patch_agent_stdout_result(
            &payload.to_string(),
            &patch,
            true,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["files"].as_array().unwrap().len(), 4);
        assert_eq!(result.output["changed_paths"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn apply_patch_missing_current_match_metadata_is_outcome_unknown() {
        let patch = one_update_patch();
        let mut payload = apply_patch_success_payload(
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
            "exact",
            1,
            true,
            true,
        );
        payload["files"][0]["edits"][0]
            .as_object_mut()
            .expect("current edit object")
            .remove("unique_match");

        let result = apply_patch_agent_stdout_result(
            &payload.to_string(),
            &patch,
            true,
            crate::apply_patch_shared::ApplyPatchMatchingMode::Unique,
        );
        assert!(!result.success);
        assert_eq!(result.output["execution_state"], "outcome_unknown");
        assert!(result.output.get("files").is_none());
    }

    #[test]
    fn apply_text_edits_path_policy_recovery_omits_untrusted_path_metadata() {
        let result = apply_text_edits_path_policy_rejection(
            2,
            "edit",
            "/private/secret.txt",
            "path must be project-relative".to_string(),
        );

        assert!(!result.success);
        assert_eq!(result.output["state_changed"], false);
        assert_eq!(result.output["error_kind"], "policy_rejected");
        assert_eq!(result.output["change_index"], 2);
        assert_eq!(result.output["kind"], "edit");
        assert!(result.output.get("path").is_none());
        assert!(result.output.get("error").is_none());
        assert!(!serde_json::to_string(&result.output)
            .unwrap()
            .contains("/private/secret.txt"));
    }

    #[test]
    fn incomplete_apply_text_edits_rollback_is_not_reported_as_no_write() {
        let result = apply_text_edits_agent_stdout_result(
            r#"{"changed":true,"state_changed":false,"rollback_complete":false,"retry_guidance":"retry directly","conflict_recovery":{"schema_version":1,"conflict_kind":"multiple_matches","occurrence_selector_supported":true,"direct_retry_safe":true,"reread_required":false,"recovery_action":"select_occurrence_or_refine_match"},"error":"rollback failed"}"#,
            1,
            false,
        );

        assert!(!result.success);
        assert_eq!(result.output["rollback_complete"], false);
        assert!(result.output.get("conflict_recovery").is_none());
        assert!(result.output.get("retry_guidance").is_none());
        assert!(result.output.get("error").is_none());
        assert!(result.output["state_changed"].is_null());
        assert_eq!(result.output["execution_state"], "outcome_unknown");
        let error = result.error.unwrap();
        assert!(error.contains("outcome is unknown"));
        assert!(!error.contains("No files were modified"));
    }

    #[test]
    fn write_project_file_effect_payload_requires_complete_consistent_state() {
        for payload in [
            r#"{"changed":false,"execution_state":"completed","error":"missing state_changed"}"#,
            r#"{"changed":false,"state_changed":true,"execution_state":"completed","error":"contradictory effect"}"#,
            r#"{"changed":false,"state_changed":false,"execution_state":"outcome_unknown","error":"uncertain"}"#,
            r#"{"changed":false,"execution_state":"completed"}"#,
        ] {
            let result = write_project_file_agent_stdout_result(payload);
            assert!(!result.success);
            assert_eq!(result.output["execution_state"], "outcome_unknown");
            assert!(result.output["state_changed"].is_null());
            assert!(result.output.get("error").is_none());
        }

        let rolled_back = write_project_file_agent_stdout_result(
            r#"{"changed":false,"state_changed":false,"execution_state":"completed","error":"write failed and parent creation was rolled back"}"#,
        );
        assert!(!rolled_back.success);
        assert_eq!(rolled_back.output["changed"], false);
        assert_eq!(rolled_back.output["state_changed"], false);
        assert_eq!(rolled_back.output["execution_state"], "completed");
    }
}
