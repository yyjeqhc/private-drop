//! Outcome construction, error envelopes, and read-side projections for the
//! connector runtime.
//!
//! These are transport/`&self`-free helpers shared by the `impl ConnectorRuntime`
//! capability-dispatch and lifecycle methods: building `ConnectorCallOutcome`
//! success/error envelopes, parsing and validating capability arguments,
//! hashing operation/transaction identifiers, paginating search output, and
//! rendering approval/result/review/validation projections. They live here so
//! the runtime module reads as orchestration rather than a wall of formatting.

use super::wire_models::{FilesSearchInput, SearchResultMode};
use super::{execution, ConnectorCallOutcome};
use crate::{ConnectorPermission, ConnectorWindowId};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use webcodex_core::apply_edits_shared::ApplyFileChangeInput;
use webcodex_store::{
    ConnectorApproval, ConnectorApprovalGate, ConnectorTaskResult, ConnectorTaskSnapshot,
    ConnectorTaskState, ConnectorTaskStoreError, ConnectorWindowBinding,
};
use webcodex_validation::RecipeError;
use webcodex_workspace::project_context::{ContextRefreshSummary, ProjectContextFingerprint};

pub(crate) const CONNECTOR_SEARCH_WINDOW: usize = 200;

// NOTE: The subject is intentionally passed explicitly rather than stored as
// current connector state. Two devices for one user share a subject; two users
// do not share task ids even when requests interleave on the same connector.

#[derive(Debug)]
pub(super) enum KernelFailure {
    Scope {
        required_permission: Option<ConnectorPermission>,
        message: String,
    },
    Adapter(String),
    Tool {
        error: Option<String>,
        output: Value,
    },
}

impl ConnectorCallOutcome {
    pub(super) fn success(task: &ConnectorTaskSnapshot, data: Value) -> Self {
        Self::success_at(task, task.event_cursor, data)
    }

    /// A successful project-scoped response with no task binding — the shape
    /// task_list needs, because a fresh session has no task yet.
    pub(super) fn success_project(data: Value) -> Self {
        Self {
            ok: true,
            body: json!({
                "ok": true,
                "task_id": null,
                "run_id": null,
                "event_cursor": null,
                "data": data,
                "warnings": [],
                "blocking": false
            }),
            http_status: 200,
            required_permission: None,
            protocol_error: false,
        }
    }

    pub(super) fn success_at(task: &ConnectorTaskSnapshot, cursor: i64, data: Value) -> Self {
        Self::success_blocking_at(task, cursor, data, false)
    }

    pub(super) fn success_blocking_at(
        task: &ConnectorTaskSnapshot,
        cursor: i64,
        data: Value,
        blocking: bool,
    ) -> Self {
        Self {
            ok: true,
            body: json!({
                "ok": true,
                "task_id": task.task_id,
                "run_id": task.run_id,
                "event_cursor": cursor,
                "data": data,
                "warnings": [],
                "blocking": blocking
            }),
            http_status: 200,
            required_permission: None,
            protocol_error: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn error(
        http_status: u16,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        user_action_required: bool,
        suggested_action: Option<&str>,
        required_permission: Option<ConnectorPermission>,
        protocol_error: bool,
    ) -> Self {
        Self {
            ok: false,
            body: error_envelope(
                None,
                None,
                None,
                Value::Null,
                code,
                message,
                retryable,
                user_action_required,
                suggested_action,
            ),
            http_status,
            required_permission,
            protocol_error,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn error_with_data(
        http_status: u16,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        user_action_required: bool,
        suggested_action: Option<&str>,
        data: Value,
        required_permission: Option<ConnectorPermission>,
        protocol_error: bool,
    ) -> Self {
        Self {
            ok: false,
            body: error_envelope(
                None,
                None,
                None,
                data,
                code,
                message,
                retryable,
                user_action_required,
                suggested_action,
            ),
            http_status,
            required_permission,
            protocol_error,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn error_for_task(
        http_status: u16,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        user_action_required: bool,
        suggested_action: Option<&str>,
        task: &ConnectorTaskSnapshot,
        data: Value,
    ) -> Self {
        Self::error_for_task_at(
            http_status,
            code,
            message,
            retryable,
            user_action_required,
            suggested_action,
            task,
            task.event_cursor,
            data,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn error_for_task_at(
        http_status: u16,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        user_action_required: bool,
        suggested_action: Option<&str>,
        task: &ConnectorTaskSnapshot,
        cursor: i64,
        data: Value,
    ) -> Self {
        Self::error_for_task_at_with_scope(
            http_status,
            code,
            message,
            retryable,
            user_action_required,
            suggested_action,
            task,
            cursor,
            data,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn error_for_task_at_with_scope(
        http_status: u16,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        user_action_required: bool,
        suggested_action: Option<&str>,
        task: &ConnectorTaskSnapshot,
        cursor: i64,
        data: Value,
        required_permission: Option<ConnectorPermission>,
    ) -> Self {
        Self {
            ok: false,
            body: error_envelope(
                Some(&task.task_id),
                Some(&task.run_id),
                Some(cursor),
                data,
                code,
                message,
                retryable,
                user_action_required,
                suggested_action,
            ),
            http_status,
            required_permission,
            protocol_error: false,
        }
    }

    pub(super) fn scope_denied(permission: ConnectorPermission) -> Self {
        Self::error(
            403,
            "insufficient_scope",
            "missing required connector permission",
            false,
            true,
            Some("Grant the required scope to this connector credential."),
            Some(permission),
            false,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn error_envelope(
    task_id: Option<&str>,
    run_id: Option<&str>,
    event_cursor: Option<i64>,
    data: Value,
    code: impl Into<String>,
    message: impl Into<String>,
    retryable: bool,
    user_action_required: bool,
    suggested_action: Option<&str>,
) -> Value {
    json!({
        "ok": false,
        "task_id": task_id,
        "run_id": run_id,
        "event_cursor": event_cursor,
        "data": data,
        "warnings": [],
        "blocking": true,
        "error": {
            "code": code.into(),
            "message": message.into(),
            "retryable": retryable,
            "user_action_required": user_action_required,
            "suggested_action": suggested_action
        }
    })
}

pub(super) fn host_review_projection(
    envelope: &Value,
    guidance_read_state: Option<webcodex_store::GuidanceReadState>,
) -> Value {
    let mut review = envelope["data"].clone();
    review["task_id"] = envelope["task_id"].clone();
    review["run_id"] = envelope["run_id"].clone();
    review["event_cursor"] = envelope["event_cursor"].clone();
    let pending = review["result"]["decision_status"] == "pending";
    review["can_accept"] = json!(pending && review["status"] == "ready_for_review");
    review["can_reject"] = json!(pending || review["run_status"] == "interrupted");
    review["can_cancel"] = json!(review["status"] == "active");
    review["next_action"] = json!(if review["can_accept"] == true {
        "review_and_accept"
    } else if review["run_status"] == "interrupted" {
        "resume_or_reject"
    } else if review["can_cancel"] == true {
        "monitor_or_cancel"
    } else {
        "review"
    });
    // Guidance read-state for the console timeline: the watermark the model
    // has claimed (`guidance_seen_seq`) and the newest still-pending
    // `human_guidance` sequence, or null when none is pending. The console
    // renders unread guidance events (sequence > guidance_seen_seq) distinctly.
    let (seen_seq, last_pending) = match guidance_read_state {
        Some(state) => (json!(state.seen_seq), json!(state.last_pending_seq)),
        None => (Value::Null, Value::Null),
    };
    review["guidance_seen_seq"] = seen_seq;
    review["unread_guidance_seq"] = last_pending;
    review
}

pub(super) fn parse_input<T: DeserializeOwned>(
    capability: &str,
    arguments: Value,
) -> Result<T, ConnectorCallOutcome> {
    serde_json::from_value(arguments)
        .map_err(|error| invalid_input(capability, format!("invalid input: {error}")))
}

pub(super) fn invalid_input(capability: &str, message: impl Into<String>) -> ConnectorCallOutcome {
    ConnectorCallOutcome::error(
        400,
        "invalid_arguments",
        format!("{capability}: {}", message.into()),
        false,
        false,
        Some("Correct the capability arguments using its advertised schema."),
        None,
        true,
    )
}

pub fn store_error_outcome(
    error: ConnectorTaskStoreError,
    task: Option<&ConnectorTaskSnapshot>,
) -> ConnectorCallOutcome {
    match error {
        ConnectorTaskStoreError::NotFound => ConnectorCallOutcome::error(
            404,
            "task_not_found",
            "task was not found in this connector project and identity context",
            false,
            false,
            Some("Use the task_id returned by task_start for this connector."),
            None,
            false,
        ),
        ConnectorTaskStoreError::OperationIdConflict(operation_id) => match task {
            Some(task) => ConnectorCallOutcome::error_for_task(
                409,
                "operation_id_conflict",
                "operation_id was already used with a different execution request",
                false,
                false,
                Some("Reuse operation_id only for an exact retry; use a new value for an intentional rerun or different request."),
                task,
                json!({ "operation_id": operation_id }),
            ),
            None => ConnectorCallOutcome::error(
                409,
                "operation_id_conflict",
                "operation_id was already used with a different execution request",
                false,
                false,
                Some("Reuse operation_id only for an exact retry; use a new value for an intentional rerun or different request."),
                None,
                false,
            ),
        },
        ConnectorTaskStoreError::Decision(code, message) => ConnectorCallOutcome::error(
            409,
            code,
            message,
            false,
            true,
            Some("Refresh the task review and resolve the stated precondition."),
            None,
            false,
        ),
        ConnectorTaskStoreError::InvalidState(message) => match task {
            Some(task) => ConnectorCallOutcome::error_for_task(
                409,
                "task_not_active",
                message,
                false,
                true,
                Some("Start a new task for additional work."),
                task,
                Value::Null,
            ),
            None => ConnectorCallOutcome::error(
                409,
                "task_not_active",
                message,
                false,
                true,
                Some("Start a new task for additional work."),
                None,
                false,
            ),
        },
        ConnectorTaskStoreError::Storage(error) => {
            tracing::error!(error = %error, "connector task store operation failed");
            match task {
                Some(task) => ConnectorCallOutcome::error_for_task(
                    500,
                    "task_store_error",
                    "connector could not durably record task state",
                    false,
                    true,
                    Some("Inspect server logs and task_review before retrying any consequential call."),
                    task,
                    Value::Null,
                ),
                None => ConnectorCallOutcome::error(
                    500,
                    "task_store_error",
                    "connector could not durably record task state",
                    false,
                    true,
                    Some("Inspect server logs before retrying."),
                    None,
                    false,
                ),
            }
        }
    }
}

pub(super) fn approval_gate_outcome(
    gate: ConnectorApprovalGate,
    task: &ConnectorTaskSnapshot,
) -> ConnectorCallOutcome {
    let (approval, code, message, suggested_action) = match gate {
        ConnectorApprovalGate::Pending(approval) => (
            approval,
            "approval_required",
            "this raw command is waiting for one-time approval on the WebCodex host".to_string(),
            "Ask the user to approve this exact action locally, then retry commands_run unchanged.",
        ),
        ConnectorApprovalGate::Denied(approval) => {
            // The operator's stated reason is the course correction — put it
            // where the model cannot miss it.
            let message = match approval.decision_reason.as_deref() {
                Some(reason) => format!("the user denied this exact raw command: {reason}"),
                None => "the user denied this exact raw command".to_string(),
            };
            (
                approval,
                "approval_denied",
                message,
                "Choose a safer action or ask the user for revised instructions.",
            )
        }
        ConnectorApprovalGate::Expired(approval) => (
            approval,
            "approval_expired",
            "the one-time approval request expired".to_string(),
            "Retry commands_run unchanged to create a fresh local approval window.",
        ),
        ConnectorApprovalGate::Consumed(approval) => (
            approval,
            "approval_consumed",
            "the approval for this exact raw command was already consumed".to_string(),
            "Review the task state before proposing a different action; approvals cannot be replayed.",
        ),
        ConnectorApprovalGate::Authorized(_) => {
            unreachable!("authorized commands continue to executor dispatch")
        }
    };
    ConnectorCallOutcome::error_for_task_at(
        409,
        code,
        message,
        false,
        true,
        Some(suggested_action),
        task,
        task.event_cursor,
        json!({
            "approval": approval_projection(&approval),
            "local_command": format!(
                "webcodex task approve {} {}",
                task.task_id, approval.approval_id
            )
        }),
    )
}

pub fn approval_projection(approval: &ConnectorApproval) -> Value {
    json!({
        "approval_id": approval.approval_id,
        "action_kind": approval.action_kind,
        "action_hash": approval.action_hash,
        "action_summary": approval.action_summary,
        "state": approval.state,
        "requested_at": approval.requested_at,
        "expires_at": approval.expires_at,
        "decided_by": approval.decided_by,
        "decided_at": approval.decided_at,
        "decision_reason": approval.decision_reason
    })
}

pub(super) fn validation_recipe_error(
    task: &ConnectorTaskSnapshot,
    error: RecipeError,
) -> ConnectorCallOutcome {
    ConnectorCallOutcome::error_for_task(
        409,
        error.code,
        "validation recipe planning failed; inspect the stable code and safe details",
        false,
        true,
        Some("Resolve the reported recipe, manifest, cwd, or package-manager evidence and retry."),
        task,
        error.details.unwrap_or(Value::Null),
    )
}

pub(super) fn command_request_hash(
    task: &ConnectorTaskSnapshot,
    command: &str,
    cwd: Option<&str>,
    timeout_secs: u64,
) -> String {
    let mut hasher = Sha256::new();
    for field in [
        b"webcodex.commands_run.v2".as_slice(),
        task.task_id.as_bytes(),
        task.run_id.as_bytes(),
        command.as_bytes(),
        cwd.unwrap_or("").as_bytes(),
        &timeout_secs.to_be_bytes(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

pub(super) fn check_request_hash(
    task: &ConnectorTaskSnapshot,
    recipe_identity: &Value,
    cwd: Option<&str>,
    test_filter: Option<&str>,
    timeout_secs: u64,
) -> String {
    let recipe_identity = serde_json::to_vec(recipe_identity).unwrap_or_default();
    let mut hasher = Sha256::new();
    for field in [
        b"webcodex.checks_run.v3".as_slice(),
        task.task_id.as_bytes(),
        task.run_id.as_bytes(),
        recipe_identity.as_slice(),
        cwd.unwrap_or("").as_bytes(),
        test_filter.unwrap_or("").as_bytes(),
        &timeout_secs.to_be_bytes(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

pub(super) fn command_action_hash(request_sha256: &str, precondition: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"approval\0");
    hasher.update(request_sha256);
    hasher.update(b"\0");
    hasher.update(precondition);
    format!("{:x}", hasher.finalize())
}

pub(super) fn edit_operation_hash(
    task: &ConnectorTaskSnapshot,
    changes: &[ApplyFileChangeInput],
    dry_run: bool,
) -> String {
    let serialized = serde_json::to_vec(changes).unwrap_or_default();
    let mut hasher = Sha256::new();
    for field in [
        b"webcodex.edits_apply.v2".as_slice(),
        task.task_id.as_bytes(),
        task.run_id.as_bytes(),
        &[u8::from(dry_run)],
        serialized.as_slice(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

pub(super) fn search_cursor_signature(input: &FilesSearchInput, page_limit: usize) -> String {
    let canonical = json!({
        "version": 1,
        "task_id": input.task_id,
        "pattern": input.pattern,
        "path": input.path,
        "page_limit": page_limit,
        "context_before": input.context_before.unwrap_or(0),
        "context_after": input.context_after.unwrap_or(0),
        "include_globs": input.include_globs,
        "exclude_globs": input.exclude_globs,
        "result_mode": input.result_mode.unwrap_or(SearchResultMode::Matches),
    });
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn parse_search_cursor(cursor: &str, expected_signature: &str) -> Result<usize, ()> {
    let payload = cursor.strip_prefix("wc_search_").ok_or(())?;
    let (offset, signature) = payload.split_once('_').ok_or(())?;
    if signature != expected_signature {
        return Err(());
    }
    offset.parse::<usize>().map_err(|_| ())
}

pub(super) fn paginate_search_output(
    mut output: Value,
    result_mode: SearchResultMode,
    offset: usize,
    page_limit: usize,
    signature: &str,
) -> Value {
    let key = if result_mode == SearchResultMode::Matches {
        "matches"
    } else {
        "files"
    };
    let records = output[key].as_array().cloned().unwrap_or_default();
    let page = records
        .iter()
        .skip(offset)
        .take(page_limit)
        .cloned()
        .collect::<Vec<_>>();
    let executor_truncated = output["truncated"].as_bool().unwrap_or(false);
    let more_in_records = records.len() > offset.saturating_add(page.len());
    let has_more = !page.is_empty() && (more_in_records || executor_truncated);
    let next_offset = offset.saturating_add(page.len());
    let next_cursor = (has_more && next_offset < CONNECTOR_SEARCH_WINDOW)
        .then(|| format!("wc_search_{next_offset}_{signature}"));
    let window_exhausted = has_more && next_cursor.is_none();
    output[key] = json!(page);
    output["truncated"] = json!(has_more);
    output["truncation_reason"] = if window_exhausted {
        json!("window_limit")
    } else if has_more {
        json!("page")
    } else {
        Value::Null
    };
    let returned = output[key].as_array().map(Vec::len).unwrap_or(0);
    if result_mode == SearchResultMode::Matches {
        output["count"] = json!(returned);
    } else {
        output["returned_file_count"] = json!(returned);
    }
    if result_mode == SearchResultMode::Count {
        let returned_match_count = output[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["match_count"].as_u64())
            .sum::<u64>();
        let complete = offset == 0 && !has_more;
        output["returned_match_count"] = json!(returned_match_count);
        output["count_complete"] = json!(complete);
        output["total_matches"] = if complete {
            json!(returned_match_count)
        } else {
            Value::Null
        };
    }
    output["page"] = json!({
        "offset": offset,
        "limit": page_limit,
        "returned": returned,
        "next_cursor": next_cursor,
        "window_limit": CONNECTOR_SEARCH_WINDOW,
        "window_exhausted": window_exhausted,
        "view": "live_sorted"
    });
    output
}

/// Whether a kernel failure may have applied workspace changes.
pub(super) fn kernel_failure_may_have_applied(error: &KernelFailure) -> bool {
    let KernelFailure::Tool { error, output } = error else {
        return false;
    };
    if output.get("execution_state").and_then(Value::as_str) == Some("outcome_unknown")
        || output.get("failure_kind").and_then(Value::as_str) == Some("outcome_unknown")
        || output.get("error_kind").and_then(Value::as_str) == Some("outcome_unknown")
        || output.get("rollback_complete").and_then(Value::as_bool) == Some(false)
        || output.get("changed").and_then(Value::as_bool) == Some(true)
    {
        return true;
    }
    error.as_deref().is_some_and(|message| {
        let message = message.to_ascii_lowercase();
        [
            "timed out",
            "request was dropped",
            "waiter was dropped",
            "disconnect",
        ]
        .iter()
        .any(|needle| message.contains(needle))
    })
}

pub fn result_projection(result: &ConnectorTaskResult) -> Value {
    json!({
        "result_id": result.result_id,
        "summary": result.summary,
        "patch_sha256": result.patch_sha256,
        "patch_bytes": result.patch_bytes,
        "changed_paths": result.changed_paths,
        "validation": result.validation,
        "warnings": result.warnings,
        "decision_status": result.decision_status,
        "decided_at": result.decided_at,
        "cleanup_warning": result.cleanup_warning,
        "recovery": result.recovery
    })
}

pub fn durable_task_review_projection(
    task: &ConnectorTaskSnapshot,
    result: Option<&ConnectorTaskResult>,
) -> Value {
    json!({
        "goal": task.goal,
        "mode": task.mode,
        "status": task.task_status,
        "run_status": task.run_status,
        "created_at": task.created_at,
        "updated_at": task.updated_at,
        "result": result.map(result_projection)
    })
}

pub(super) fn project_brief(
    task: &ConnectorTaskSnapshot,
    overview: Option<&Value>,
    git_dirty: Option<bool>,
    git_conflict_count: Option<usize>,
) -> Value {
    let languages = overview
        .and_then(|value| value["project_types"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item["kind"].as_str())
        .take(8)
        .collect::<Vec<_>>();
    let manifests = overview
        .and_then(|value| value["manifests"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item["path"].as_str())
        .take(12)
        .collect::<Vec<_>>();
    let instructions = overview
        .and_then(|value| value["key_files"].as_array())
        .into_iter()
        .flatten()
        .filter(|item| item["kind"] == "agent_instructions")
        .filter_map(|item| item["path"].as_str())
        .take(5)
        .collect::<Vec<_>>();
    let mut recommended_checks = Vec::new();
    for language in &languages {
        let checks: &[&str] = match *language {
            "rust" => &[
                "cargo fmt --check",
                "cargo check --all-targets",
                "cargo test",
            ],
            "node" => &["npm test"],
            "python" => &["python -m pytest"],
            "go" => &["go test -json ./..."],
            "jvm" => &["project test task"],
            "dotnet" => &["dotnet test"],
            "ruby" => &["bundle exec rake test"],
            "php" => &["composer test"],
            "cpp" => &["project build and test"],
            _ => &[],
        };
        for check in checks {
            if !recommended_checks.contains(check) {
                recommended_checks.push(*check);
            }
        }
    }
    recommended_checks.truncate(5);
    let mut warnings = Vec::new();
    if overview.is_none() {
        warnings.push("project_overview_unavailable");
    }
    if git_dirty.is_none() {
        warnings.push("git_status_unavailable");
    }
    json!({
        "git": {
            "baseline_commit": task.baseline_commit.as_deref().map(short_oid),
            "baseline_tree": task.baseline_tree.as_deref().map(short_oid),
            "dirty": git_dirty,
            "conflict_count": git_conflict_count
        },
        "workspace": {
            "isolated": task.isolated,
            "strategy": if task.isolated { "reusable_slot" } else { "target_checkout" }
        },
        "languages": languages,
        "manifests": manifests,
        "instructions": instructions,
        "recommended_checks": recommended_checks,
        "warnings": warnings
    })
}

pub(super) fn project_brief_from_fingerprint(
    task: &ConnectorTaskSnapshot,
    fingerprint: &ProjectContextFingerprint,
) -> Value {
    let mut language_kinds = Vec::new();
    for manifest in &fingerprint.manifests {
        let kind = match manifest.path.rsplit('/').next().unwrap_or_default() {
            "Cargo.toml" => Some("rust"),
            "package.json" => Some("node"),
            "pyproject.toml" | "setup.py" | "setup.cfg" | "requirements.txt" | "Pipfile" => {
                Some("python")
            }
            "go.mod" => Some("go"),
            "pom.xml" | "build.gradle" | "build.gradle.kts" => Some("jvm"),
            "Gemfile" => Some("ruby"),
            "composer.json" => Some("php"),
            "CMakeLists.txt" | "meson.build" => Some("cpp"),
            name if name.ends_with(".sln") || name.ends_with(".csproj") => Some("dotnet"),
            _ => None,
        };
        if let Some(kind) = kind {
            if !language_kinds.contains(&kind) {
                language_kinds.push(kind);
            }
        }
    }
    let overview = json!({
        "project_types": language_kinds
            .into_iter()
            .map(|kind| json!({"kind": kind}))
            .collect::<Vec<_>>(),
        "manifests": fingerprint
            .manifests
            .iter()
            .map(|manifest| json!({"path": manifest.path}))
            .collect::<Vec<_>>(),
        "key_files": fingerprint
            .rules
            .iter()
            .map(|rule| json!({"path": rule.path, "kind": "agent_instructions"}))
            .collect::<Vec<_>>()
    });
    project_brief(task, Some(&overview), fingerprint.git.dirty, None)
}

pub(super) fn context_refresh_payload(refresh: &ContextRefreshSummary) -> Value {
    json!({
        "reused": refresh.reused,
        "refreshed": refresh.refreshed,
        "partial": refresh.partial,
        "unknown": refresh.unknown,
        "warnings": refresh.warnings,
        "rules": {
            "reused": refresh.rules.reused,
            "refreshed": refresh.rules.refreshed,
            "removed": refresh.rules.removed,
            "unknown": refresh.rules.unknown
        },
        "manifests": {
            "reused_count": refresh.manifests.reused.len(),
            "refreshed": refresh.manifests.refreshed,
            "removed": refresh.manifests.removed,
            "unknown": refresh.manifests.unknown
        }
    })
}

pub(super) fn connector_window_binding<'a>(
    window: &'a ConnectorWindowId,
    fingerprint: &'a ProjectContextFingerprint,
    now: i64,
) -> ConnectorWindowBinding<'a> {
    ConnectorWindowBinding {
        window_key: window.key(),
        window_source: window.source(),
        project_root_sha256: &fingerprint.project_root_sha256,
        target_path: &fingerprint.target_directory,
        fingerprint,
        now,
    }
}

pub(super) fn navigation_payload(
    activation: Option<&webcodex_store::WindowProjectActivation>,
    reused_context: bool,
) -> Value {
    let restored_previous_context = reused_context
        && match activation {
            Some(activation) => {
                activation.previous_project.as_deref() != Some(activation.current_project.as_str())
            }
            None => true,
        };
    json!({
        "switched": activation.is_some_and(|activation| activation.switched),
        "restored_previous_context": restored_previous_context
    })
}

pub(super) fn validation_projection(
    execution: Option<&webcodex_store::ConnectorExecution>,
) -> Value {
    let Some(execution) = execution else {
        return json!({ "status": "not_run", "execution_id": null, "checks": [] });
    };
    let projection =
        execution::execution_projection(execution, chrono::Utc::now().timestamp(), None);
    json!({
        "status": projection["assertion_status"],
        "execution_id": execution.execution_id,
        "checks": projection["checks"],
        "recipe": projection["recipe"],
        "assertion_evidence": projection["assertion_evidence"]
    })
}

pub(super) fn checks_stale_outcome(
    task: &ConnectorTaskSnapshot,
    execution: &webcodex_store::ConnectorExecution,
    message: &str,
) -> ConnectorCallOutcome {
    ConnectorCallOutcome::error_for_task(
        409,
        "checks_stale",
        message,
        false,
        true,
        Some(
            "Call checks_run with a new operation_id to validate the current workspace, then retry task_finish.",
        ),
        task,
        json!({ "execution_id": execution.execution_id }),
    )
}

pub(super) fn short_oid(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

pub(super) const DEFAULT_TASK_LIST_LIMIT: usize = 10;
pub(super) const MAX_TASK_LIST_LIMIT: usize = 20;
const TASK_LIST_GOAL_BYTES: usize = 200;

/// Bound a goal for the list projection without splitting a UTF-8 character.
pub(super) fn bounded_goal(goal: &str) -> String {
    if goal.len() <= TASK_LIST_GOAL_BYTES {
        return goal.to_string();
    }
    let mut end = TASK_LIST_GOAL_BYTES;
    while !goal.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &goal[..end])
}

/// The host queue speaks reviewer verbs; the model needs capability verbs.
pub(super) fn model_next_action(
    task_status: ConnectorTaskState,
    host_action: &str,
) -> &'static str {
    match host_action {
        "in_progress" => "task_resume",
        "review_and_accept" => "task_review_then_ask_the_owner_to_decide_locally",
        "resume_or_reject" => "ask_the_owner_to_resume_or_reject_on_the_host",
        _ if task_status == ConnectorTaskState::Rejected => "task_resume_for_the_rejection_reason",
        _ => "task_start_new_work",
    }
}

pub(super) fn validate_task_id(task_id: &str) -> Result<(), &'static str> {
    let suffix = task_id.strip_prefix("wc_task_").unwrap_or_default();
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("task_id must be the opaque wc_task_* id returned by task_start");
    }
    Ok(())
}

pub(super) fn validate_operation_id(operation_id: &str) -> Result<(), &'static str> {
    let mut bytes = operation_id.bytes();
    if operation_id.len() > 100
        || !bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err("operation_id must be 1..=100 ASCII letters, digits, '-', '_', '.', or ':'");
    }
    Ok(())
}

pub(super) fn validate_path(path: &str) -> Result<(), &'static str> {
    if path.trim().is_empty() || path.len() > 1024 {
        return Err("path must be 1..=1024 bytes");
    }
    if path.starts_with('/') || path.contains('\0') {
        return Err("path must be project-relative and contain no NUL byte");
    }
    if std::path::Path::new(path)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("path cannot contain parent traversal");
    }
    Ok(())
}

pub fn validate_opaque_id(value: &str, prefix: &str, label: &str) -> Result<(), String> {
    let suffix = value.strip_prefix(prefix).unwrap_or_default();
    if suffix.len() < 10
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(format!("{label} must use the {prefix}<lowercase-id> form"));
    }
    Ok(())
}
