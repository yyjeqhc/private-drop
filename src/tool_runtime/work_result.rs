use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::auth::AuthContext;

use super::handoff::review_evidence_summary_for_session;
use super::session_context::{
    session_project_mismatch_result, unknown_session_result, SessionProjectMismatch,
};
use super::validation_events::{
    current_validation_evidence_for_session, validation_summary_from_events,
};
use super::{ToolResult, ToolRuntime};

const WORK_RESULT_SESSION_EVENT_LIMIT: usize = 200;
const WORK_RESULT_VALIDATION_LIMIT: usize = 20;
pub(crate) const MAX_WORK_RESULT_FILES: usize = 8;
const MAX_WORK_RESULT_PATH_CHARS: usize = 512;
const MAX_WORK_RESULT_BRANCH_CHARS: usize = 160;
const MAX_WORK_RESULT_REVIEW_TOOLS: usize = 12;
const MAX_WORK_RESULT_TOOL_CHARS: usize = 64;

impl ToolRuntime {
    pub(crate) async fn present_work_result(
        &self,
        project: String,
        session_id: String,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.exact_work_result(project, session_id, "present_work_result", auth)
            .await
    }

    pub(crate) async fn work_result_state(
        &self,
        project: String,
        session_id: String,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.exact_work_result(project, session_id, "work_result_state", auth)
            .await
    }

    /// Read one exact project-scoped coding Session without creating work or
    /// writing presentation state. The only live Project observation is the
    /// bounded, no-diff show_changes producer used for current worktree facts.
    async fn exact_work_result(
        &self,
        project: String,
        session_id: String,
        tool_name: &'static str,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        // Session authority is checked first from the exact business identity;
        // possession of an iframe/project string never grants Session access.
        if let Err(result) = self
            .authorize_session_target(&session_id, tool_name, auth)
            .await
        {
            return result;
        }
        // Independently resolve and authorize the caller-supplied Project on
        // every read rather than trusting the Session's stored project string.
        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(error) => return error.into_tool_result(),
        };
        if project.trim() != resolved.resolved_id {
            return ToolResult::err_with_output(
                "Work Result presentation requires the exact complete runtime project id",
                json!({
                    "error_kind": "work_result_project_not_exact",
                    "failure_kind": "invalid_arguments",
                    "state_changed": false,
                }),
            );
        }
        let Some(summary) = self
            .sessions
            .summary(&session_id, Some(WORK_RESULT_SESSION_EVENT_LIMIT))
        else {
            return unknown_session_result(&session_id);
        };
        if summary.project.as_deref() != Some(resolved.resolved_id.as_str()) {
            let mismatch = SessionProjectMismatch {
                session_project: summary
                    .project
                    .clone()
                    .unwrap_or_else(|| "<unscoped>".to_string()),
                request_project: resolved.resolved_id.clone(),
            };
            return session_project_mismatch_result(&session_id, tool_name, &mismatch);
        }

        // Deliberately omit business session_id here. This is a live Project
        // read, not Session evidence, and an explicit App refresh must never
        // append to the target Session merely because the card requested it.
        let workspace_result = self
            .show_changes(
                resolved.resolved_id.clone(),
                None,
                Some(false),
                None,
                None,
                None,
            )
            .await;
        let validation =
            validation_summary_from_events(&summary.events, WORK_RESULT_VALIDATION_LIMIT);
        let current_validation =
            current_validation_evidence_for_session(&summary, WORK_RESULT_VALIDATION_LIMIT)
                .evidence;
        let review = review_evidence_summary_for_session(&summary);
        let history_partial = summary.events_truncated;
        let mut projection = build_work_result_projection(
            &resolved.resolved_id,
            &session_id,
            workspace_result.success,
            &workspace_result.output,
            &validation,
            &current_validation,
            &review,
            history_partial,
        );
        let state_bytes = serde_json::to_vec(&projection).unwrap_or_default();
        projection["state_version"] = json!(format!("wr1_{:x}", Sha256::digest(state_bytes)));
        ToolResult::ok(json!({"work_result": projection}))
    }
}

pub(crate) fn build_work_result_projection(
    project: &str,
    session_id: &str,
    workspace_call_succeeded: bool,
    workspace_source: &Value,
    validation_source: &Value,
    current_validation_source: &Value,
    review_source: &Value,
    history_partial: bool,
) -> Value {
    json!({
        "version": 1,
        "project": project,
        "session_id": session_id,
        "workspace": work_result_workspace(workspace_call_succeeded, workspace_source),
        "validation": work_result_validation(validation_source, current_validation_source, history_partial),
        "review": work_result_review(review_source, history_partial),
    })
}

fn work_result_workspace(call_succeeded: bool, source: &Value) -> Value {
    let git_available = call_succeeded
        && source
            .get("git_available")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let clean = git_available
        .then(|| source.get("clean").and_then(Value::as_bool))
        .flatten();
    let source_files = source
        .get("files")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let files_total = source
        .get("files_total")
        .and_then(Value::as_u64)
        .unwrap_or(source_files.len() as u64);
    let source_files_truncated = source
        .get("files_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || files_total > source_files.len() as u64;

    let mut files = Vec::new();
    let mut unsafe_file_omitted = false;
    for source_file in source_files.iter().take(MAX_WORK_RESULT_FILES) {
        match work_result_file(source_file) {
            Some(file) => files.push(file),
            None => unsafe_file_omitted = true,
        }
    }
    let projection_truncated =
        source_files_truncated || source_files.len() > MAX_WORK_RESULT_FILES || unsafe_file_omitted;

    let mut additions = 0_u64;
    let mut deletions = 0_u64;
    let mut line_stats_seen = 0_usize;
    let mut line_stats_missing = false;
    for source_file in source_files {
        match (
            source_file.get("additions").and_then(Value::as_u64),
            source_file.get("deletions").and_then(Value::as_u64),
        ) {
            (Some(add), Some(delete)) => {
                additions = additions.saturating_add(add);
                deletions = deletions.saturating_add(delete);
                line_stats_seen += 1;
            }
            _ => line_stats_missing = true,
        }
    }

    let mut workspace = Map::new();
    workspace.insert("git_available".to_string(), json!(git_available));
    if let Some(clean) = clean {
        workspace.insert("clean".to_string(), json!(clean));
    }
    if !call_succeeded {
        workspace.insert("reason_code".to_string(), json!("workspace_unavailable"));
    } else if source
        .get("non_git_project")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        workspace.insert("reason_code".to_string(), json!("non_git_project"));
    } else if !git_available {
        workspace.insert("reason_code".to_string(), json!("git_unavailable"));
    }
    if let Some(branch) = source
        .get("branch")
        .and_then(Value::as_str)
        .and_then(|value| bounded_plain_text(value, MAX_WORK_RESULT_BRANCH_CHARS))
    {
        workspace.insert("branch".to_string(), json!(branch));
    }
    if let Some(head) = work_result_head(source.get("head")) {
        workspace.insert("head".to_string(), head);
    }
    workspace.insert(
        "counts".to_string(),
        work_result_counts(source.get("counts")),
    );
    workspace.insert("files_total".to_string(), json!(files_total));
    workspace.insert("files".to_string(), Value::Array(files));
    workspace.insert("truncated".to_string(), json!(projection_truncated));

    if clean == Some(true) {
        workspace.insert("additions".to_string(), json!(0));
        workspace.insert("deletions".to_string(), json!(0));
        workspace.insert("line_stats_partial".to_string(), json!(false));
    } else if line_stats_seen > 0 {
        workspace.insert("additions".to_string(), json!(additions));
        workspace.insert("deletions".to_string(), json!(deletions));
        workspace.insert(
            "line_stats_partial".to_string(),
            json!(line_stats_missing || source_files_truncated),
        );
    } else if clean == Some(false) {
        workspace.insert("line_stats_partial".to_string(), json!(true));
    }

    Value::Object(workspace)
}

fn work_result_file(source: &Value) -> Option<Value> {
    let path = source
        .get("path")
        .and_then(Value::as_str)
        .and_then(safe_relative_path)?;
    let mut file = Map::new();
    file.insert("path".to_string(), json!(path));
    for field in ["status", "kind"] {
        if let Some(value) = source
            .get(field)
            .and_then(Value::as_str)
            .and_then(|value| bounded_token(value, 32))
        {
            file.insert(field.to_string(), json!(value));
        }
    }
    if let Some(old_path) = source
        .get("old_path")
        .and_then(Value::as_str)
        .and_then(safe_relative_path)
    {
        file.insert("old_path".to_string(), json!(old_path));
    }
    for field in ["staged", "unstaged"] {
        if let Some(value) = source.get(field).and_then(Value::as_bool) {
            file.insert(field.to_string(), json!(value));
        }
    }
    if let (Some(additions), Some(deletions)) = (
        source.get("additions").and_then(Value::as_u64),
        source.get("deletions").and_then(Value::as_u64),
    ) {
        file.insert("additions".to_string(), json!(additions));
        file.insert("deletions".to_string(), json!(deletions));
    }
    Some(Value::Object(file))
}

fn work_result_head(source: Option<&Value>) -> Option<Value> {
    let source = source?.as_object()?;
    let commit = source.get("commit")?.as_str()?;
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut head = Map::new();
    head.insert("commit".to_string(), json!(commit));
    if let Some(short) = source
        .get("short")
        .and_then(Value::as_str)
        .filter(|value| value.len() <= 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        head.insert("short".to_string(), json!(short));
    }
    Some(Value::Object(head))
}

fn work_result_counts(source: Option<&Value>) -> Value {
    let mut counts = Map::new();
    for field in [
        "modified",
        "added",
        "deleted",
        "renamed",
        "copied",
        "untracked",
        "conflicted",
        "staged",
        "unstaged",
    ] {
        counts.insert(
            field.to_string(),
            json!(source
                .and_then(|value| value.get(field))
                .and_then(Value::as_u64)
                .unwrap_or(0)),
        );
    }
    Value::Object(counts)
}

fn work_result_validation(historical: &Value, current: &Value, history_partial: bool) -> Value {
    let mut validation = Map::new();
    let historical_status = validation_status(historical.get("status").and_then(Value::as_str));
    let historical_latest =
        validation_latest_status(historical.get("latest_status").and_then(Value::as_str));
    validation.insert(
        "status".to_string(),
        json!(if history_partial && historical_status == "not_run" {
            "unknown"
        } else {
            historical_status
        }),
    );
    validation.insert(
        "latest_status".to_string(),
        json!(if history_partial && historical_latest == "not_run" {
            "unknown"
        } else {
            historical_latest
        }),
    );
    validation.insert("history_partial".to_string(), json!(history_partial));
    validation.insert(
        "current_status".to_string(),
        json!(current_validation_status(
            current.get("status").and_then(Value::as_str)
        )),
    );
    for field in ["successes", "failures"] {
        if let Some(value) = historical.get(field).and_then(Value::as_u64) {
            validation.insert(field.to_string(), json!(value));
        }
    }
    validation.insert(
        "unresolved_failures".to_string(),
        json!(current
            .get("unresolved_failure_count")
            .and_then(Value::as_u64)
            .unwrap_or(0)),
    );
    validation.insert(
        "evidence_gaps".to_string(),
        json!(current
            .get("evidence_gap_event_count")
            .and_then(Value::as_u64)
            .unwrap_or(0)),
    );
    if let Some(reason) = current
        .get("reason")
        .and_then(Value::as_str)
        .and_then(|value| bounded_token(value, 96))
    {
        validation.insert("reason".to_string(), json!(reason));
    }
    Value::Object(validation)
}

fn work_result_review(source: &Value, history_partial: bool) -> Value {
    let mut review = Map::new();
    review.insert("history_partial".to_string(), json!(history_partial));
    review.insert(
        "available".to_string(),
        json!(source
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false)),
    );
    for field in [
        "total",
        "read_only_inspection_count",
        "search_count",
        "diff_review_count",
        "workspace_review_count",
        "hygiene_review_count",
    ] {
        review.insert(
            field.to_string(),
            json!(source.get(field).and_then(Value::as_u64).unwrap_or(0)),
        );
    }
    let tools = source
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|value| bounded_token(value, MAX_WORK_RESULT_TOOL_CHARS))
                .take(MAX_WORK_RESULT_REVIEW_TOOLS)
                .map(Value::String)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    review.insert("tools".to_string(), Value::Array(tools));
    Value::Object(review)
}

fn validation_status(status: Option<&str>) -> &'static str {
    match status {
        Some("not_run") => "not_run",
        Some("passed") => "passed",
        Some("failed") => "failed",
        Some("mixed") => "mixed",
        Some("expected") => "expected",
        Some("inconclusive") => "inconclusive",
        _ => "unknown",
    }
}

fn validation_latest_status(status: Option<&str>) -> &'static str {
    match status {
        Some("not_run") => "not_run",
        Some("passed") => "passed",
        Some("failed") => "failed",
        Some("expected") => "expected",
        Some("inconclusive") => "inconclusive",
        _ => "unknown",
    }
}

fn current_validation_status(status: Option<&str>) -> &'static str {
    match status {
        Some("not_run") => "not_run",
        Some("passed") => "passed",
        Some("failed") => "failed",
        Some("expected") => "expected",
        Some("inconclusive") => "inconclusive",
        Some("stale") => "stale",
        _ => "unknown",
    }
}

fn safe_relative_path(value: &str) -> Option<String> {
    if value.is_empty()
        || value.chars().count() > MAX_WORK_RESULT_PATH_CHARS
        || value.chars().any(char::is_control)
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.as_bytes().get(1) == Some(&b':')
        || value.split(['/', '\\']).any(|part| part == "..")
    {
        return None;
    }
    Some(value.to_string())
}

fn bounded_plain_text(value: &str, max_chars: usize) -> Option<String> {
    if value.is_empty() || value.chars().count() > max_chars || value.chars().any(char::is_control)
    {
        return None;
    }
    Some(value.to_string())
}

fn bounded_token(value: &str, max_chars: usize) -> Option<String> {
    if value.is_empty()
        || value.chars().count() > max_chars
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':' | '/'))
    {
        return None;
    }
    Some(value.to_string())
}
