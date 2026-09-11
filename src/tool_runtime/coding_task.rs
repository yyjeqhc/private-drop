//! Deterministic coding-task workflow aggregates.
//!
//! These tools reduce repetitive startup/finish calls for model-facing coding
//! loops. They only aggregate existing runtime state and never call an LLM,
//! generate prose summaries, parse validation output, or hide underlying tool
//! payloads.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::continuation_feedback::{
    continuation_feedback_value, continuation_projection_hooks, continuation_validation_snapshot,
    not_applicable_continuation_feedback_value, ContinuationFeedbackInput,
    ContinuationToolFailureSnapshot,
};
use super::handoff::{
    actionable_unexpected_failure_count, apply_compact_workflow_outcomes, closeout_work_projection,
    compact_jobs, compact_review_evidence, compact_tool_failures, compact_validation,
    reconcile_closeout_evidence, review_evidence_summary_for_session,
    validation_has_cargo_test_zero_tests,
};
use super::handoff_brief::{build_handoff_brief, HandoffBriefInput};
use super::permissions::{
    authority_profile_payload, permission_summary_from_events, PermissionDecision,
};
use super::project_instructions::{ProjectInstructionFile, ProjectInstructionsSnapshot};
use super::project_resolution::ResolvedProject;
use super::runtime_info::compact_runtime_status;
use super::session_context::{
    session_project_mismatch_result, workflow_session_authority_fingerprint, SessionProjectMismatch,
};
use super::sessions::tool_failure_summary_from_events;
use super::sessions::{self, SessionTransport, TOOL_CALL_RECORDING_SESSION_ID_FIELD};
use super::startup_brief::{
    bounded_extension_description, build_startup_brief, builtin_coding_workflow_projection,
    startup_brief_from_output, StartupBriefInput, StartupExtensions, StartupPluginEntry,
    StartupPluginsCatalog, REPOSITORY_OVERVIEW_NOT_REQUESTED_REASON,
};
use super::tool_catalog::TOOL_RECOMMENDED_FLOWS;
use super::tool_inputs::{SessionMode, StartupDetail};
use super::tool_result::{RecoveryKind, ToolResult};
use super::unknown_session_result;
use super::validation_events::skipped_validation_summary;
use super::window_activity::{
    ToolCallCorrelation, WorkflowSessionCorrelation, WorkflowSessionCorrelationRelation,
};
use super::{ToolCall, ToolRuntime};
use crate::auth::AuthContext;
use crate::runner_protocol::{
    ShellFileOpRequest, RUNNER_CAPABILITY_FILE_READ, RUNNER_CAPABILITY_GIT, RUNNER_CAPABILITY_SHELL,
};
use std::collections::HashSet;
use std::time::Duration;

const RULES_MAX_HEADINGS: usize = 8;
const RULES_MAX_FIRST_LINES: usize = 5;
const RULES_MAX_LINE_CHARS: usize = 180;
const FINISH_SESSION_EVENT_LIMIT: usize = 200;
/// Short startup probe budget for the repository overview, much tighter than
/// the standalone `project_overview` tool's 30s wait. An optional overview
/// failure must not block the coding task, so it fails over quickly.
pub(crate) const DEFAULT_REPOSITORY_OVERVIEW_PROBE_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ProjectResolutionMetadata {
    pub(crate) source: String,
    pub(crate) outcome: String,
    pub(crate) resolved_project: String,
    pub(crate) registered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) worktree: Option<ManagedWorktreeProjection>,
    #[serde(skip)]
    pub(crate) permission: Option<PermissionDecision>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ManagedWorktreeProjection {
    pub(crate) managed: bool,
    pub(crate) base_ref: String,
    pub(crate) base_sha: String,
    pub(crate) source_dirty: bool,
}

#[derive(Debug, Clone)]
struct ManagedWorktreeRequest {
    base_ref: Option<String>,
    operation_id: String,
    resume_project_id: Option<String>,
}

enum CodingProjectSource {
    Existing { project: String },
    RunnerPath { client_id: String, path: String },
}

#[derive(Debug, Clone, Copy)]
struct CodingStartupOptions {
    tool_name: &'static str,
    detail: StartupDetail,
    include_repository_overview: bool,
    include_project_instructions: bool,
    include_extension_catalog: bool,
    include_reused_instruction_content: bool,
}

impl CodingStartupOptions {
    #[cfg(test)]
    fn diagnostic(detail: StartupDetail) -> Self {
        Self {
            detail,
            tool_name: "work_on_project",
            include_repository_overview: true,
            include_project_instructions: true,
            include_extension_catalog: false,
            include_reused_instruction_content: false,
        }
    }

    fn work_on_project(
        include_project_instructions: bool,
        include_extension_catalog: bool,
    ) -> Self {
        Self {
            detail: StartupDetail::Standard,
            tool_name: "work_on_project",
            include_repository_overview: false,
            include_project_instructions,
            include_extension_catalog,
            include_reused_instruction_content: include_project_instructions,
        }
    }
}

fn invalid_project_source(message: impl Into<String>, fields: Value) -> ToolResult {
    let mut output = json!({
        "error_kind": "invalid_arguments",
        "failure_kind": "invalid_arguments",
        "constraint": "exactly_one_project_source",
        "state_changed": false,
    });
    if let (Some(output), Some(fields)) = (output.as_object_mut(), fields.as_object()) {
        output.extend(fields.clone());
    }
    ToolResult::err_with_output(message, output).with_recovery(RecoveryKind::FixInput, None)
}

#[cfg(test)]
#[test]
fn invalid_project_source_exposes_fix_input_recovery() {
    let result = invalid_project_source("choose one project source", json!({"project": "demo"}));
    assert!(!result.success);
    assert_eq!(result.output["error_kind"], "invalid_arguments");
    assert_eq!(result.output["failure_kind"], "invalid_arguments");
    assert_eq!(result.output["state_changed"], false);
    assert_eq!(result.output["recovery_kind"], "fix_input");
    assert!(result.output.get("recovery_tool").is_none());
}

fn non_empty_optional_field(
    field: &'static str,
    value: Option<String>,
) -> Result<Option<String>, ToolResult> {
    match value {
        Some(value) => {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                Err(invalid_project_source(
                    format!("{field} must not be empty"),
                    json!({"field": field}),
                ))
            } else {
                Ok(Some(trimmed))
            }
        }
        None => Ok(None),
    }
}

fn resolve_project_source(
    project: String,
    client_id: Option<String>,
    path: Option<String>,
) -> Result<CodingProjectSource, ToolResult> {
    let project = project.trim().to_string();
    let client_id = non_empty_optional_field("client_id", client_id)?;
    let path = non_empty_optional_field("path", path)?;

    if !project.is_empty() {
        let mut conflicts = Vec::new();
        if client_id.is_some() {
            conflicts.push("client_id");
        }
        if path.is_some() {
            conflicts.push("path");
        }
        if !conflicts.is_empty() {
            let mut fields = vec!["project"];
            fields.extend(conflicts);
            return Err(invalid_project_source(
                "project cannot be combined with client_id or path",
                json!({"conflicting_fields": fields}),
            ));
        }
        return Ok(CodingProjectSource::Existing { project });
    }

    if let Some(path) = path {
        if let Err(error) = super::projects::validate_project_op_path(&path) {
            return Err(invalid_project_source(
                error,
                json!({"field": "path", "expected": "absolute_path"}),
            ));
        }
        let Some(client_id) = client_id else {
            return Err(invalid_project_source(
                "path requires client_id",
                json!({"field": "client_id", "required_with": "path"}),
            ));
        };
        return Ok(CodingProjectSource::RunnerPath { client_id, path });
    }

    Err(if client_id.is_some() {
        invalid_project_source(
            "client_id requires path",
            json!({"field": "path", "required_with": "client_id"}),
        )
    } else {
        invalid_project_source(
            "project or client_id + path is required",
            json!({"required_any_of": ["project", "client_id + path"]}),
        )
    })
}

fn registration_scope_denied(auth: Option<&AuthContext>, operation: &str) -> Option<ToolResult> {
    auth.is_some_and(|auth| !auth.has_scope(crate::auth::SCOPE_PROJECT_WRITE))
        .then(|| {
            ToolResult::err_with_output(
                format!("{operation} requires project:write"),
                json!({
                    "error_kind": "insufficient_scope",
                    "failure_kind": "insufficient_scope",
                    "required_scope": crate::auth::SCOPE_PROJECT_WRITE,
                    "state_changed": false,
                }),
            )
            .with_recovery(RecoveryKind::UserAction, None)
        })
}

fn attach_permission(
    mut result: ToolResult,
    permission: Option<&PermissionDecision>,
) -> ToolResult {
    if let Some(permission) = permission {
        super::permissions::add_permission_to_result(&mut result, permission);
    }
    result
}

fn attach_project_resolution(
    mut result: ToolResult,
    resolution: &ProjectResolutionMetadata,
) -> ToolResult {
    // Existing-project aliases are not authoritative until runtime resolution
    // succeeds. Path sources already carry a Runner-issued full id, so their
    // metadata remains useful on later Session failures.
    if !resolution.resolved_project.is_empty() {
        result.output["project_resolution"] =
            serde_json::to_value(resolution).unwrap_or_else(|_| json!({}));
    }
    if resolution.registered && !result.success {
        result.output["state_changed"] = json!(true);
    }
    attach_permission(result, resolution.permission.as_ref())
}

impl ToolRuntime {
    async fn require_runner_coding_capability(
        &self,
        client_id: &str,
        auth: Option<&AuthContext>,
    ) -> Result<(), ToolResult> {
        let access = crate::runner_http::runner_access_from_auth(auth);
        let supports_shell = self
            .runner_registry
            .runner_supports_for_auth(client_id, RUNNER_CAPABILITY_SHELL, access.as_ref())
            .await
            .map_err(ToolResult::err)?;
        let supports_git = if supports_shell {
            false
        } else {
            self.runner_registry
                .runner_supports_for_auth(client_id, RUNNER_CAPABILITY_GIT, access.as_ref())
                .await
                .map_err(ToolResult::err)?
        };
        if supports_shell || supports_git {
            Ok(())
        } else {
            Err(ToolResult::err(format!(
                "Runner {client_id} does not support shell or git"
            )))
        }
    }

    async fn explicit_coding_session_project(
        &self,
        session_id: &str,
        tool_name: &str,
        auth: Option<&AuthContext>,
    ) -> Result<Option<ResolvedProject>, ToolResult> {
        if let Some(resolved) = self
            .authorize_session_target(session_id, tool_name, auth)
            .await?
        {
            return Ok(Some(resolved));
        }
        let Some(project) = self
            .sessions
            .session_project(session_id)
            .expect("authorized Workflow Session must still exist")
        else {
            return Ok(None);
        };
        self.resolve_project_input_for_auth(&project, auth)
            .await
            .map(Some)
            .map_err(|error| error.into_tool_result())
    }

    fn path_source_session_mismatch(
        session_id: &str,
        tool_name: &str,
        session_project: Option<&ResolvedProject>,
        client_id: &str,
        path: &str,
    ) -> Option<ToolResult> {
        let request_project = format!("path:{client_id}:{path}");
        let Some(session_project) = session_project else {
            return Some(session_project_mismatch_result(
                session_id,
                tool_name,
                &SessionProjectMismatch {
                    session_project: "<unscoped>".to_string(),
                    request_project,
                },
            ));
        };
        if session_project.config.client_id != client_id || session_project.config.path != path {
            return Some(session_project_mismatch_result(
                session_id,
                tool_name,
                &SessionProjectMismatch {
                    session_project: session_project.resolved_id.clone(),
                    request_project,
                },
            ));
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_coding_workflow(
        &self,
        project: String,
        client_id: Option<String>,
        path: Option<String>,
        mut managed_worktree: Option<ManagedWorktreeRequest>,
        title: Option<String>,
        mode: SessionMode,
        deny_write_tools: bool,
        deny_shell_tools: bool,
        startup: CodingStartupOptions,
        resume_session_id: Option<String>,
        execution_context: Option<sessions::SessionExecutionContext>,
        auth: Option<&AuthContext>,
        trusted_recording_session_id: Option<&str>,
        trusted_recording_session_project: Option<&str>,
        transport: SessionTransport,
    ) -> ToolResult {
        let detail = startup.detail;
        let project_source = match resolve_project_source(project, client_id, path) {
            Ok(source) => source,
            Err(result) => return result,
        };
        let resume_requested = resume_session_id.is_some();
        let execution_context = match execution_context
            .map(sessions::SessionExecutionContext::validated)
            .transpose()
        {
            Ok(context) => context,
            Err(error) => {
                return ToolResult::err_with_output(
                    error,
                    json!({
                        "error_kind": "invalid_execution_context",
                        "failure_kind": "invalid_arguments",
                        "field": "execution_context",
                        "state_changed": false,
                    }),
                );
            }
        };
        let resume_session_id = match resume_session_id {
            Some(session_id)
                if session_id != session_id.trim()
                    || !sessions::is_valid_session_id(&session_id) =>
            {
                return ToolResult::err_with_output(
                    "resume_session_id must be a valid wc_sess_* Workflow Session id",
                    json!({
                        "error_kind": "invalid_resume_session_id",
                        "failure_kind": "invalid_arguments",
                        "field": "resume_session_id",
                        "expected_format": "wc_sess_*",
                        "state_changed": false,
                    }),
                );
            }
            Some(session_id) => Some(session_id),
            None => None,
        };
        let resume_session_project = match resume_session_id.as_deref() {
            Some(session_id) => match self
                .explicit_coding_session_project(session_id, startup.tool_name, auth)
                .await
            {
                Ok(project) => project,
                Err(result) => return result,
            },
            None => None,
        };
        if let (Some(worktree), Some(session_project)) =
            (managed_worktree.as_mut(), resume_session_project.as_ref())
        {
            let Some(project_id) =
                super::lsp_tools::runner_local_project_id(&session_project.resolved_id)
            else {
                return session_project_mismatch_result(
                    resume_session_id
                        .as_deref()
                        .expect("resume project requires session id"),
                    startup.tool_name,
                    &SessionProjectMismatch {
                        session_project: session_project.resolved_id.clone(),
                        request_project: "managed_worktree".to_string(),
                    },
                );
            };
            worktree.resume_project_id = Some(project_id.to_string());
        }
        let trusted_recording_session_resolved_project = match (
            trusted_recording_session_id,
            trusted_recording_session_project,
        ) {
            (Some(_), Some(project)) => {
                match self.resolve_project_input_for_auth(project, auth).await {
                    Ok(resolved) => Some(resolved),
                    Err(error) => return error.into_tool_result(),
                }
            }
            _ => None,
        };
        let title = match title {
            Some(title) => {
                let title = title.trim().to_string();
                if title.is_empty()
                    || title.chars().count() > sessions::MAX_CODING_INSTRUCTION_CHARS
                {
                    return ToolResult::err_with_output(
                        format!(
                            "title must contain 1..={} characters",
                            sessions::MAX_CODING_INSTRUCTION_CHARS
                        ),
                        json!({
                            "error_kind": "invalid_coding_instruction",
                            "field": "title",
                            "max_chars": sessions::MAX_CODING_INSTRUCTION_CHARS,
                        }),
                    );
                }
                Some(title)
            }
            None => None,
        };
        let (project, mut project_resolution) = match project_source {
            CodingProjectSource::Existing { project } => {
                let resolution = ProjectResolutionMetadata {
                    source: "project".to_string(),
                    outcome: "resolved_existing_project".to_string(),
                    resolved_project: String::new(),
                    registered: false,
                    worktree: None,
                    permission: None,
                };
                (project, resolution)
            }
            CodingProjectSource::RunnerPath { client_id, path } => {
                if managed_worktree.is_none() {
                    if let Some(session_id) = resume_session_id.as_deref() {
                        if let Some(result) = Self::path_source_session_mismatch(
                            session_id,
                            startup.tool_name,
                            resume_session_project.as_ref(),
                            &client_id,
                            &path,
                        ) {
                            return result;
                        }
                    }
                    if let Some(recording_session_id) = trusted_recording_session_id {
                        if let Some(result) = Self::path_source_session_mismatch(
                            recording_session_id,
                            startup.tool_name,
                            trusted_recording_session_resolved_project.as_ref(),
                            &client_id,
                            &path,
                        ) {
                            return result;
                        }
                    }
                }
                if let Some(result) = registration_scope_denied(auth, "project path registration") {
                    return result;
                }
                let permission = super::permissions::evaluate_permission_for_tool(
                    &self.permission_evaluator,
                    "register_project",
                    None,
                );
                if let Some(decision) = permission.as_ref() {
                    if !decision.allows_execution() {
                        let mut result =
                            super::permissions::permission_execution_denied_result(decision);
                        super::permissions::add_permission_to_result(&mut result, decision);
                        return result;
                    }
                }
                if let Err(result) = self
                    .require_runner_coding_capability(&client_id, auth)
                    .await
                {
                    return attach_permission(result, permission.as_ref());
                }
                let managed_requested = managed_worktree.is_some();
                if let (Some(worktree), Some(session_project), Some(session_id)) = (
                    managed_worktree.as_ref(),
                    resume_session_project.as_ref(),
                    resume_session_id.as_deref(),
                ) {
                    if session_project.config.client_id != client_id {
                        return session_project_mismatch_result(
                            session_id,
                            startup.tool_name,
                            &SessionProjectMismatch {
                                session_project: session_project.resolved_id.clone(),
                                request_project: format!("managed_worktree:{client_id}"),
                            },
                        );
                    }
                    debug_assert!(worktree.resume_project_id.is_some());
                }
                let resolved = if let Some(worktree) = managed_worktree.as_ref() {
                    self.prepare_managed_worktree(
                        client_id,
                        path,
                        worktree.base_ref.clone(),
                        worktree.operation_id.clone(),
                        worktree.resume_project_id.clone(),
                        auth,
                    )
                    .await
                } else {
                    self.resolve_or_register_project(client_id, path, auth)
                        .await
                };
                if !resolved.success {
                    return attach_permission(resolved, permission.as_ref());
                }
                let Some(project) = resolved
                    .output
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_string)
                else {
                    return attach_permission(
                        ToolResult::err_with_output(
                            "Runner returned a path resolution without a runtime project id",
                            json!({
                                "error_kind": "operation_failed",
                                "failure_kind": "operation_failed",
                                "state_changed": resolved.output["registered"]
                                    .as_bool()
                                    .unwrap_or(false),
                            }),
                        ),
                        permission.as_ref(),
                    );
                };
                let outcome = resolved
                    .output
                    .get("outcome")
                    .and_then(Value::as_str)
                    .filter(|outcome| {
                        if managed_requested {
                            matches!(
                                *outcome,
                                "managed_worktree_created" | "managed_worktree_recovered"
                            )
                        } else {
                            matches!(*outcome, "reused_existing_registration" | "auto_registered")
                        }
                    })
                    .map(str::to_string);
                let registered = resolved.output.get("registered").and_then(Value::as_bool);
                let (Some(outcome), Some(registered)) = (outcome, registered) else {
                    return attach_permission(
                        ToolResult::err_with_output(
                            "Runner returned malformed path resolution metadata",
                            json!({
                                "error_kind": "operation_failed",
                                "failure_kind": "operation_failed",
                                "state_changed": resolved.output["registered"]
                                    .as_bool()
                                    .unwrap_or(false),
                            }),
                        ),
                        permission.as_ref(),
                    );
                };
                if !managed_requested && registered != (outcome == "auto_registered") {
                    return attach_permission(
                        ToolResult::err_with_output(
                            "Runner returned inconsistent path resolution metadata",
                            json!({
                                "error_kind": "operation_failed",
                                "failure_kind": "operation_failed",
                                "state_changed": registered,
                            }),
                        ),
                        permission.as_ref(),
                    );
                }
                let worktree = if managed_requested {
                    let base_ref = resolved
                        .output
                        .get("base_ref")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let base_sha = resolved
                        .output
                        .get("base_sha")
                        .and_then(Value::as_str)
                        .filter(|sha| {
                            matches!(sha.len(), 40 | 64)
                                && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
                        })
                        .map(str::to_string);
                    let source_dirty = resolved.output.get("source_dirty").and_then(Value::as_bool);
                    let managed = resolved.output.get("managed").and_then(Value::as_bool);
                    match (managed, base_ref, base_sha, source_dirty) {
                        (Some(true), Some(base_ref), Some(base_sha), Some(source_dirty)) => {
                            Some(ManagedWorktreeProjection {
                                managed: true,
                                base_ref,
                                base_sha,
                                source_dirty,
                            })
                        }
                        _ => {
                            return attach_permission(
                                ToolResult::err_with_output(
                                    "Runner returned malformed managed worktree metadata",
                                    json!({
                                        "error_kind": "operation_failed",
                                        "failure_kind": "operation_failed",
                                        "state_changed": true,
                                    }),
                                ),
                                permission.as_ref(),
                            )
                        }
                    }
                } else {
                    None
                };
                let resolution = ProjectResolutionMetadata {
                    source: if managed_requested {
                        "managed_worktree".to_string()
                    } else {
                        "path".to_string()
                    },
                    outcome,
                    resolved_project: project.clone(),
                    registered,
                    worktree,
                    permission,
                };
                (project, resolution)
            }
        };
        // `detail` is the single startup projection control: full keeps the
        // complete runtime status, recent commits, rules, and tool manifest;
        // standard/minimal use the compact projections.
        let compact_startup = detail != StartupDetail::Full;
        let include_recent_commits = detail == StartupDetail::Full;
        let include_tool_manifest = detail == StartupDetail::Full;
        let tool_manifest = if include_tool_manifest {
            match self.compact_tool_manifest_payload_bounded(None, None, None) {
                Ok(payload) => Some(payload),
                Err(result) => return attach_project_resolution(result, &project_resolution),
            }
        } else {
            None
        };

        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(err) => {
                return attach_project_resolution(err.into_tool_result(), &project_resolution)
            }
        };
        project_resolution.resolved_project = resolved.resolved_id.clone();
        if let Some(session_id) = resume_session_id.as_deref() {
            if resume_session_project
                .as_ref()
                .map(|project| project.resolved_id.as_str())
                != Some(resolved.resolved_id.as_str())
            {
                return attach_project_resolution(
                    session_project_mismatch_result(
                        session_id,
                        startup.tool_name,
                        &SessionProjectMismatch {
                            session_project: resume_session_project
                                .as_ref()
                                .map(|project| project.resolved_id.clone())
                                .unwrap_or_else(|| "<unscoped>".to_string()),
                            request_project: resolved.resolved_id.clone(),
                        },
                    ),
                    &project_resolution,
                );
            }
        }
        if let Some(recording_session_id) = trusted_recording_session_id {
            if trusted_recording_session_project != Some(resolved.resolved_id.as_str()) {
                return attach_project_resolution(
                    session_project_mismatch_result(
                        recording_session_id,
                        startup.tool_name,
                        &SessionProjectMismatch {
                            session_project: trusted_recording_session_project
                                .unwrap_or("<unscoped>")
                                .to_string(),
                            request_project: resolved.resolved_id.clone(),
                        },
                    ),
                    &project_resolution,
                );
            }
        }
        // Semantic-navigation and fixed project-instruction observation remain
        // mandatory startup probes. Extension discovery is an independent,
        // bounded observation that runs concurrently only when the caller keeps
        // include_extension_catalog enabled. Diagnostic projections can still
        // run the optional repository overview concurrently.
        let extension_discovery = async {
            if startup.include_extension_catalog {
                Some(self.extension_discovery_for_startup(&resolved, auth).await)
            } else {
                None
            }
        };
        let (semantic_navigation, project_instructions, repository_overview, extensions) =
            if startup.include_repository_overview {
                let (semantic_navigation, project_instructions, repository_overview, extensions) =
                    futures_util::future::join4(
                        self.probe_semantic_navigation_for_startup(&resolved),
                        self.load_coding_project_instructions(&resolved.config),
                        self.repository_overview_for_startup(&resolved, auth),
                        extension_discovery,
                    )
                    .await;
                (
                    semantic_navigation,
                    project_instructions,
                    repository_overview,
                    extensions,
                )
            } else {
                let (semantic_navigation, project_instructions, extensions) =
                    futures_util::future::join3(
                        self.probe_semantic_navigation_for_startup(&resolved),
                        self.load_coding_project_instructions(&resolved.config),
                        extension_discovery,
                    )
                    .await;
                (
                    semantic_navigation,
                    project_instructions,
                    repository_overview_not_requested(),
                    extensions,
                )
            };
        let semantic_navigation = serde_json::to_value(semantic_navigation).unwrap_or_else(|_| {
            json!({
                "supported": false,
                "available": false,
                "status": "probe_failed",
                "reason_code": "status_probe_failed",
            })
        });
        // Coding startup always observes every fixed repository-rule
        // candidate. The complete bounded body remains only in the in-memory
        // Workflow Session; the ledger persistence path omits it.
        let mut warnings = Vec::new();
        if repository_overview.get("status").and_then(Value::as_str) == Some("unavailable")
            && repository_overview
                .get("reason_code")
                .and_then(Value::as_str)
                != Some(REPOSITORY_OVERVIEW_NOT_REQUESTED_REASON)
        {
            warnings.push(json!({
                "kind": "repository_overview_unavailable",
                "message": "repository structure overview was unavailable during startup",
            }));
        }
        let mut runtime_status_call_failed = false;
        let (runtime_status, runtime_status_for_brief) = {
            let result = self.runtime_status(auth).await;
            if !result.success {
                runtime_status_call_failed = true;
                warnings.push(json!({
                    "kind": "runtime_status_unavailable",
                    "message": "runtime status was unavailable during startup",
                }));
            }
            let raw = result.output;
            let projected = if compact_startup {
                compact_runtime_status(&raw)
            } else {
                raw.clone()
            };
            (projected, raw)
        };
        let owning_runner_available = owning_runner_available(
            &resolved,
            &runtime_status_for_brief,
            runtime_status_call_failed,
        );
        let git = self
            .coding_startup_git_summary(
                &resolved.resolved_id,
                include_recent_commits,
                &mut warnings,
            )
            .await;
        // Surface dirty/conflict worktree state at top-level so compact Action
        // responses that omit full git payloads still keep the warning reason.
        if !git.is_null() {
            append_workspace_warnings(&workspace_payload_from_git_summary(&git), &mut warnings);
        }
        let write_scope_verified =
            auth.is_none_or(|auth| auth.has_scope(crate::auth::SCOPE_PROJECT_WRITE));
        let authority_fingerprint = match workflow_session_authority_fingerprint(auth) {
            Ok(fingerprint) => fingerprint,
            Err(_) => {
                return attach_project_resolution(
                    ToolResult::err_with_output(
                        "caller has no canonical Workflow Session authority identity",
                        json!({
                            "error_kind": "session_authority_identity_unavailable",
                            "failure_kind": "session_authority_denied",
                            "state_changed": false,
                        }),
                    ),
                    &project_resolution,
                );
            }
        };
        let session_outcome = match self.sessions.ensure_coding_session(
            sessions::CodingSessionRequest {
                project: resolved.resolved_id.clone(),
                authority_fingerprint,
                resume_session_id: resume_session_id.clone(),
                instruction: title.clone(),
                mode,
                guards: sessions::SessionGuards {
                    deny_write_tools,
                    deny_shell_tools,
                },
                execution_context,
                project_instructions: Some(project_instructions.clone()),
                transport,
                // Startup always re-reads bounded current Git state and the
                // fixed project-instruction candidates.
                context_refreshed: true,
                write_scope_verified,
            },
        ) {
            Ok(outcome) => outcome,
            Err(sessions::CodingSessionError::InvalidResumeSessionId) => {
                return attach_project_resolution(
                    ToolResult::err_with_output(
                        "resume_session_id must be a valid wc_sess_* Workflow Session id",
                        json!({
                            "error_kind": "invalid_resume_session_id",
                            "failure_kind": "invalid_arguments",
                            "field": "resume_session_id",
                            "expected_format": "wc_sess_*",
                            "state_changed": false,
                        }),
                    ),
                    &project_resolution,
                );
            }
            Err(sessions::CodingSessionError::UnknownResumeSession { session_id }) => {
                return attach_project_resolution(
                    unknown_session_result(&session_id),
                    &project_resolution,
                );
            }
            Err(sessions::CodingSessionError::ResumeSessionNotActive {
                session_id,
                lifecycle,
            }) => {
                let error_kind = match lifecycle {
                    sessions::SessionLifecycle::Closed => "session_closed",
                    sessions::SessionLifecycle::Active => "session_lifecycle_denied",
                };
                return attach_project_resolution(
                    ToolResult::err_with_output(
                        format!(
                            "{error_kind}: work_on_project cannot resume a {} session",
                            lifecycle.as_str()
                        ),
                        json!({
                            "error_kind": error_kind,
                            "failure_kind": error_kind,
                            "session_id": session_id,
                            "lifecycle": lifecycle,
                            "resume_requested": true,
                            "state_changed": false,
                        }),
                    ),
                    &project_resolution,
                );
            }
            Err(sessions::CodingSessionError::ResumeProjectMismatch {
                session_id,
                session_project,
                request_project,
            }) => {
                return attach_project_resolution(
                    ToolResult::err_with_output(
                        "session_project_mismatch: explicit Workflow Session resume requires an exact project match",
                        json!({
                            "error_kind": "session_project_mismatch",
                            "failure_kind": "session_project_mismatch",
                            "session_id": session_id,
                            "session_project": session_project,
                            "request_project": request_project,
                            "resume_requested": true,
                            "state_changed": false,
                        }),
                    ),
                    &project_resolution,
                );
            }
            Err(sessions::CodingSessionError::ResumeAuthorityMismatch { session_id }) => {
                return attach_project_resolution(
                    ToolResult::err_with_output(
                        "session_authority_denied",
                        json!({
                            "error_kind": "session_authority_denied",
                            "failure_kind": "session_authority_denied",
                            "session_id": session_id,
                            "resume_requested": true,
                            "state_changed": false,
                        }),
                    ),
                    &project_resolution,
                );
            }
            Err(sessions::CodingSessionError::WriteScopeRequired) => {
                return attach_project_resolution(
                    ToolResult::err_with_output(
                        "session capability upgrade requires project:write",
                        json!({
                            "error_kind": "session_capability_upgrade_denied",
                            "required_scope": crate::auth::SCOPE_PROJECT_WRITE,
                            "mode": mode.as_str(),
                            "state_changed": false,
                        }),
                    ),
                    &project_resolution,
                );
            }
            Err(sessions::CodingSessionError::InvalidExecutionContext(error)) => {
                return attach_project_resolution(
                    ToolResult::err_with_output(
                        error,
                        json!({
                            "error_kind": "invalid_execution_context",
                            "failure_kind": "invalid_arguments",
                            "field": "execution_context",
                            "state_changed": false,
                        }),
                    ),
                    &project_resolution,
                );
            }
            Err(sessions::CodingSessionError::CommitFailed) => {
                return attach_project_resolution(
                    ToolResult::err_with_output(
                        "coding continuity state could not be committed",
                        json!({
                            "error_kind": "coding_continuity_commit_failed",
                            "state_changed": false,
                        }),
                    ),
                    &project_resolution,
                );
            }
        };
        let session_summary = &session_outcome.summary;
        let mut connection_state = runtime_status
            .get("connection_layers")
            .cloned()
            .unwrap_or_else(|| {
                json!({
                    "runner_process": {"status": "not_observed"},
                    "server_transport": {"status": "not_observed"},
                    "server_registration": {"status": "not_observed"},
                    "project_registry": {"status": "resolved", "resolved_project": resolved.resolved_id},
                    "connector_endpoint": {"status": "not_observed"},
                    "last_successful_tool_call": {"status": "not_observed"},
                })
            });
        connection_state["project_registry"]["resolved_project"] = json!(resolved.resolved_id);
        let recommended_flow = match &tool_manifest {
            Some(manifest) => recommended_flow_payload_for_manifest_tools(manifest),
            None => recommended_flow_payload(),
        };
        // Continuation feedback for reused/resumed/restored sessions. Pure
        // read-only projection over existing session ledger, validation evidence,
        // bounded job metadata, and the message board. Never executes shell,
        // reads project files, enqueues Runner requests, mutates the ledger,
        // refreshes activity, or consumes guidance. `created` (fresh empty
        // session) surfaces a compact `not_applicable` verdict.
        let continuation_kind = if resume_requested {
            "resumed_explicitly"
        } else {
            "created"
        };
        // Read the lifecycle-aware, project-scoped summary once, after the
        // potentially slow startup probes, then share it across continuation,
        // the legacy full verdict, and the model-facing brief.
        let active_jobs = self
            .active_jobs_summary(Some(&resolved.resolved_id), auth, 10)
            .await;
        let continuation_feedback = self
            .startup_continuation_feedback(
                &session_outcome.summary,
                session_outcome.pre_instruction_summary.as_ref(),
                continuation_kind,
                &active_jobs,
                git.pointer("/counts/conflicted")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0,
            )
            .await;
        let mut output = json!({
            "detail": detail.as_str(),
            "project": project.clone(),
            "project_resolution": project_resolution.clone(),
            "resolved_project": resolved_project_payload(&resolved),
            "session": {
                "session_id": session_summary.session_id,
                "mode": session_summary.mode,
                "guards": session_summary.guards,
                "execution_context": session_summary.execution_context,
                "lifecycle": session_summary.lifecycle,
                "continuation": if resume_requested { "resumed_explicitly" } else { "created" },
                "reused": session_outcome.reused,
                "resume_requested": resume_requested,
                "instruction_appended": title.is_some(),
                "root_title": session_summary.title,
                "capability": {
                    "changed": session_outcome.capability_changed,
                    "previous_mode": session_outcome.previous_mode,
                    "previous_guards": session_outcome.previous_guards,
                    "requested_mode": mode,
                    "mode": session_summary.mode,
                    "guards": session_summary.guards,
                    "write_scope_verified": write_scope_verified,
                },
                "context": {
                    "refreshed": true,
                    "git_state_recaptured": true,
                    "rules_recaptured": true,
                    "execution_context_changed": session_outcome.execution_context_changed,
                },
                "explicit_resume_required_for_continuation": true,
                "explicit_session_id_fields": {
                    "tool_business_input": "session_id",
                    "generic_wrapper_recorder": TOOL_CALL_RECORDING_SESSION_ID_FIELD
                },
            },
            "runtime_status": runtime_status.clone(),
            "connection_state": connection_state,
            "authority": authority_profile_payload(),
            "rules": rules_summary(Some(&project_instructions)),
            "git": git.clone(),
            "semantic_navigation": semantic_navigation.clone(),
            "recommended_flow": recommended_flow,
            "continuation_feedback": continuation_feedback.clone(),
            "deterministic": true,
            "llm_summary": false,
            "warnings": warnings,
        });
        if let Some(tool_manifest) = tool_manifest {
            output["tool_manifest"] = tool_manifest;
        }
        output["startup_verdict"] = startup_verdict(
            &output,
            &active_jobs,
            owning_runner_available,
            runtime_status_call_failed,
            include_tool_manifest,
        );
        let previous_instructions = session_outcome
            .pre_instruction_summary
            .as_ref()
            .and_then(|summary| summary.project_instructions.as_ref());
        // Reload rule bodies only when there is no prior snapshot to compare
        // against (fresh session, or a session whose rules were never
        // persisted, e.g. restored after a restart). Otherwise the shared
        // brief compares fingerprints and reports reused/changed. Whether a
        // reused body is projected is intentionally separate: diagnostic test
        // projections stay incremental, while work_on_project follows
        // its caller-explicit include_project_instructions preference.
        let force_instruction_load = previous_instructions.is_none();
        let canonical_repository_root_matches = if resume_requested {
            None
        } else {
            // A fresh Session starts at the currently resolved canonical root.
            Some(true)
        };
        let project_resolution_value =
            serde_json::to_value(&project_resolution).unwrap_or_else(|_| json!({}));
        let startup_brief = build_startup_brief(StartupBriefInput {
            detail,
            requested_project: &project,
            project_resolution: &project_resolution_value,
            resolved: &resolved,
            session: session_summary,
            continuation_kind,
            reused: session_outcome.reused,
            resume_requested,
            instructions: &project_instructions,
            previous_instructions,
            force_instruction_load,
            include_project_instructions: startup.include_project_instructions,
            include_reused_instruction_content: startup.include_reused_instruction_content,
            extensions: extensions.as_ref(),
            git: &git,
            semantic_navigation: &semantic_navigation,
            repository: &repository_overview,
            continuation_feedback: &continuation_feedback,
            active_jobs: &active_jobs,
            owning_runner_available,
            canonical_repository_root_matches,
            runtime_status_call_failed,
        });
        let result = if detail == StartupDetail::Full {
            output["startup_brief"] = startup_brief;
            ToolResult::ok(output)
        } else {
            ToolResult::ok(startup_brief)
        };
        attach_permission(result, project_resolution.permission.as_ref())
    }

    /// Test-only diagnostic projection over the same canonical coding workflow
    /// engine used by `work_on_project`. This is deliberately not a ToolCall or
    /// a model/API identity.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_coding_workflow_for_test(
        &self,
        project: String,
        client_id: Option<String>,
        path: Option<String>,
        title: Option<String>,
        mode: SessionMode,
        deny_write_tools: bool,
        deny_shell_tools: bool,
        detail: StartupDetail,
        resume_session_id: Option<String>,
        execution_context: Option<sessions::SessionExecutionContext>,
        auth: Option<&AuthContext>,
        trusted_recording_session_id: Option<&str>,
        trusted_recording_session_project: Option<&str>,
        transport: SessionTransport,
    ) -> ToolResult {
        self.start_coding_workflow(
            project,
            client_id,
            path,
            None,
            title,
            mode,
            deny_write_tools,
            deny_shell_tools,
            CodingStartupOptions::diagnostic(detail),
            resume_session_id,
            execution_context,
            auth,
            trusted_recording_session_id,
            trusted_recording_session_project,
            transport,
        )
        .await
    }

    async fn extension_discovery_for_startup(
        &self,
        project: &ResolvedProject,
        auth: Option<&AuthContext>,
    ) -> StartupExtensions {
        let skills = self.startup_skills_catalog(project, auth);
        let plugins = async {
            if auth.is_some_and(|auth| !auth.has_scope(crate::auth::SCOPE_PLUGIN_INSPECT)) {
                return StartupPluginsCatalog::unavailable("plugin_inspect_scope_unavailable");
            }
            match self.project_plugin_catalog(project, auth).await {
                Ok(catalog) => {
                    let entries = catalog
                        .entries
                        .into_iter()
                        .map(|entry| StartupPluginEntry {
                            plugin: entry.plugin,
                            name: entry.name,
                            tool: entry.tool,
                            title: entry.title,
                            description: entry
                                .description
                                .as_deref()
                                .map(bounded_extension_description),
                            annotations: entry.annotations,
                        })
                        .collect();
                    StartupPluginsCatalog::available(
                        catalog.catalog_revision,
                        catalog.total_count,
                        entries,
                    )
                }
                Err(reason_code) => StartupPluginsCatalog::unavailable(reason_code),
            }
        };
        let (skills, plugins) = futures_util::future::join(skills, plugins).await;
        StartupExtensions { skills, plugins }
    }

    /// Canonical entry for the daily model coding loop.
    ///
    /// This validates the public inputs, maps them onto the shared coding
    /// workflow engine, and projects a compact startup result. With `session_id` present, it
    /// exactly resumes that one Workflow Session after project/lifecycle/access/
    /// capability checks; without it, it always creates a fresh Session.
    pub(crate) async fn work_on_project(
        &self,
        project: String,
        client_id: Option<String>,
        path: Option<String>,
        mode: Option<String>,
        base_ref: Option<String>,
        instruction: String,
        session_id: Option<String>,
        include_project_instructions: bool,
        include_workflow_guidance: bool,
        include_extension_catalog: bool,
        auth: Option<&AuthContext>,
        trusted_recording_session_id: Option<&str>,
        trusted_recording_session_project: Option<&str>,
        transport: SessionTransport,
        correlation: &mut ToolCallCorrelation,
    ) -> ToolResult {
        let project_source = match resolve_project_source(project, client_id, path) {
            Ok(source) => source,
            Err(result) => return result,
        };
        let mode = mode.as_deref().unwrap_or("checkout");
        if !matches!(mode, "checkout" | "worktree") {
            return invalid_project_source(
                "mode must be 'checkout' or 'worktree'",
                json!({"field": "mode", "allowed": ["checkout", "worktree"]}),
            );
        }
        if mode == "checkout" && base_ref.is_some() {
            return invalid_project_source(
                "base_ref is only valid with mode='worktree'",
                json!({"field": "base_ref", "requires": {"mode": "worktree"}}),
            );
        }
        if let Some(base_ref) = base_ref.as_deref() {
            if base_ref.is_empty() || base_ref.len() > 1024 || base_ref.contains('\0') {
                return invalid_project_source(
                    "base_ref must contain 1..=1024 non-NUL bytes",
                    json!({"field": "base_ref"}),
                );
            }
        }
        if mode == "worktree" && matches!(project_source, CodingProjectSource::Existing { .. }) {
            return invalid_project_source(
                "mode='worktree' requires client_id + path source checkout",
                json!({"field": "mode", "required_with": "client_id + path"}),
            );
        }
        let managed_worktree = (mode == "worktree").then(|| ManagedWorktreeRequest {
            base_ref,
            operation_id: uuid::Uuid::new_v4().to_string(),
            resume_project_id: None,
        });
        let (project, client_id, path) = match project_source {
            CodingProjectSource::Existing { project } => (project, None, None),
            CodingProjectSource::RunnerPath { client_id, path } => {
                (String::new(), Some(client_id), Some(path))
            }
        };
        let instruction = instruction.trim().to_string();
        if instruction.is_empty()
            || instruction.chars().count() > sessions::MAX_CODING_INSTRUCTION_CHARS
        {
            return ToolResult::err_with_output(
                format!(
                    "instruction must contain 1..={} characters",
                    sessions::MAX_CODING_INSTRUCTION_CHARS
                ),
                json!({
                    "error_kind": "invalid_coding_instruction",
                    "field": "instruction",
                    "max_chars": sessions::MAX_CODING_INSTRUCTION_CHARS,
                    "state_changed": false,
                }),
            );
        }
        let session_id = match session_id {
            Some(session_id)
                if session_id != session_id.trim()
                    || !sessions::is_valid_session_id(&session_id) =>
            {
                return ToolResult::err_with_output(
                    "session_id must be a valid wc_sess_* Workflow Session id",
                    json!({
                        "error_kind": "invalid_session_id",
                        "failure_kind": "invalid_arguments",
                        "field": "session_id",
                        "expected_format": "wc_sess_*",
                        "state_changed": false,
                    }),
                );
            }
            Some(session_id) => Some(session_id),
            None => None,
        };
        // Map onto the canonical coding workflow engine. The work-on-project
        // profile keeps the standard shared brief,
        // including rules, semantic navigation, workspace, and job metadata,
        // while deliberately skipping the optional repository overview. Without
        // an explicit session_id this always creates a fresh Workflow Session.
        let result = self
            .start_coding_workflow(
                project.clone(),
                client_id,
                path,
                managed_worktree,
                Some(instruction.clone()),
                SessionMode::Normal,
                false,
                false,
                CodingStartupOptions::work_on_project(
                    include_project_instructions,
                    include_extension_catalog,
                ),
                session_id.clone(),
                None,
                auth,
                trusted_recording_session_id,
                trusted_recording_session_project,
                transport,
            )
            .await;
        if !result.success {
            return result;
        }
        let projected_project = if project.is_empty() {
            startup_brief_from_output(&result.output)
                .and_then(|brief| {
                    brief
                        .pointer("/project_resolution/resolved_project")
                        .and_then(Value::as_str)
                })
                .unwrap_or_default()
                .to_string()
        } else {
            project
        };
        project_work_on_project_output_with_workflow_inner(
            projected_project,
            result.output,
            include_workflow_guidance,
            Some(correlation),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn finish_coding_task(
        &self,
        project: String,
        session_id: String,
        summary_only: bool,
        include_diff: Option<bool>,
        include_workspace: Option<bool>,
        include_hygiene: Option<bool>,
        include_handoff: Option<bool>,
        include_validation_summary: Option<bool>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let include_diff = include_diff.unwrap_or(true);
        let include_workspace = include_workspace.unwrap_or(true);
        let include_hygiene = include_hygiene.unwrap_or(true);
        let include_handoff = include_handoff.unwrap_or(true);
        let include_validation_summary = include_validation_summary.unwrap_or(true);

        if let Err(result) = self
            .authorize_session_target(&session_id, "finish_coding_task", auth)
            .await
        {
            return result;
        }
        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(err) => return err.into_tool_result(),
        };
        let session_summary = match self
            .sessions
            .summary(&session_id, Some(FINISH_SESSION_EVENT_LIMIT))
        {
            Some(summary) => summary,
            None => return unknown_session_result(&session_id),
        };
        let session_project = session_summary
            .project
            .clone()
            .unwrap_or_else(|| "<projectless>".to_string());
        if session_summary.project.as_deref() != Some(resolved.resolved_id.as_str()) {
            return session_project_mismatch_result(
                &session_id,
                "finish_coding_task",
                &SessionProjectMismatch {
                    session_project,
                    request_project: resolved.resolved_id.clone(),
                },
            );
        }
        let mut final_warnings = Vec::new();

        let show_changes_call = ToolCall::ShowChanges {
            project: resolved.resolved_id.clone(),
            session_id: Some(session_id.clone()),
            include_diff: Some(include_diff),
            max_hunks: None,
            max_hunk_lines: None,
            session_event_limit: Some(50),
        };
        let show_changes_start = self.sessions.record_tool_call_started_with_options(
            Some(&session_id),
            SessionTransport::Api,
            show_changes_call.tool_name(),
            &show_changes_call.session_log_arguments(),
            Some(resolved.resolved_id.clone()),
            super::sessions::session_tool_contract(show_changes_call.tool_name()),
        );
        let changes_result = self
            .show_changes(
                resolved.resolved_id.clone(),
                Some(session_id.clone()),
                Some(include_diff),
                None,
                None,
                Some(50),
            )
            .await;
        self.sessions.record_tool_call_finished(
            show_changes_start,
            changes_result.success,
            &changes_result.output,
            changes_result.error.as_deref(),
            None,
        );
        if !changes_result.success {
            final_warnings.push(json!({
                "kind": "show_changes_failed",
                "message": changes_result.error,
            }));
        }
        let workspace = workspace_payload_from_show_changes(&changes_result.output);
        append_workspace_warnings(&workspace, &mut final_warnings);

        let permissions = permission_summary_from_events(
            &session_summary.events,
            super::permissions::DEFAULT_PERMISSION_RECENT_LIMIT,
        );

        let hygiene = if include_hygiene {
            let hygiene_call = ToolCall::WorkspaceHygieneCheck {
                project: resolved.resolved_id.clone(),
                max_findings: None,
                include_tracked: None,
                session_id: Some(session_id.clone()),
            };
            let hygiene_start = self.sessions.record_tool_call_started_with_options(
                Some(&session_id),
                SessionTransport::Api,
                hygiene_call.tool_name(),
                &hygiene_call.session_log_arguments(),
                Some(resolved.resolved_id.clone()),
                super::sessions::session_tool_contract(hygiene_call.tool_name()),
            );
            let result = self
                .workspace_hygiene_check(
                    resolved.resolved_id.clone(),
                    None,
                    None,
                    Some(session_id.clone()),
                )
                .await;
            self.sessions.record_tool_call_finished(
                hygiene_start,
                result.success,
                &result.output,
                result.error.as_deref(),
                None,
            );
            if !result.success {
                final_warnings.push(json!({
                    "kind": "workspace_hygiene_failed",
                    "message": result.error,
                }));
            }
            result.output
        } else {
            Value::Null
        };
        append_hygiene_warnings(&hygiene, &mut final_warnings);

        let jobs = self
            .active_jobs_summary(Some(&resolved.resolved_id), auth, 10)
            .await;
        if let Some(warnings) = jobs.get("warnings").and_then(Value::as_array) {
            final_warnings.extend(warnings.iter().cloned());
        }

        let handoff = if include_handoff {
            let result = self
                .session_handoff_summary(
                    session_id.clone(),
                    Some(resolved.resolved_id.clone()),
                    Some(include_workspace),
                    Some(true),
                    Some(include_validation_summary),
                    summary_only,
                    Some(20),
                    auth,
                )
                .await;
            if !result.success {
                final_warnings.push(json!({
                    "kind": "session_handoff_failed",
                    "message": result.error,
                }));
            }
            result.output
        } else {
            Value::Null
        };
        // Reconcile from the freshest post-inspection ledger snapshot. Validation
        // materialization may itself append authoritative terminal evidence, so
        // refresh once more after deriving validation before classifying tool
        // failure actionability.
        let closeout_pre_validation_summary = self
            .sessions
            .summary(&session_id, Some(FINISH_SESSION_EVENT_LIMIT))
            .unwrap_or_else(|| session_summary.clone());
        let validation = if include_validation_summary {
            self.validation_summary_for_session_with_jobs(
                &closeout_pre_validation_summary,
                10,
                auth,
            )
            .await
        } else {
            skipped_validation_summary()
        };
        let closeout_session_summary = self
            .sessions
            .summary(&session_id, Some(FINISH_SESSION_EVENT_LIMIT))
            .unwrap_or(closeout_pre_validation_summary);
        let review_evidence = review_evidence_summary_for_session(&closeout_session_summary);
        let (work_performed, changed_paths) =
            closeout_work_projection(&closeout_session_summary.events);

        // Continuation feedback reuses the same attempt summary and validation
        // delta projections as start/handoff. It is a read-only projection over
        // the existing closeout summary, validation, and job metadata; it never
        // re-runs validation, mutates the ledger, or replaces the closeout
        // verdict.
        let (discussion, guidance_available) = self.discussion_snapshot(&session_id);
        let continuation_validation = if include_validation_summary {
            validation.clone()
        } else {
            json!({ "available": false, "not_requested": true })
        };
        let continuation_current_validation =
            super::validation_events::current_validation_evidence_for_session(
                &closeout_session_summary,
                20,
            );
        let raw_tool_failures =
            tool_failure_summary_from_events(&closeout_session_summary.events, 10);
        let reconciliation =
            reconcile_closeout_evidence(&raw_tool_failures, &closeout_session_summary, &validation);
        let continuation_feedback = if closeout_session_summary.events.is_empty() {
            not_applicable_continuation_feedback_value("empty_session")
        } else {
            continuation_feedback_value(ContinuationFeedbackInput {
                session_summary: &closeout_session_summary,
                validation: &continuation_validation,
                jobs: &jobs,
                discussion: &discussion,
                continuation: "continued",
                suggest_exploration_continuity: false,
                workspace_conflicts: workspace
                    .pointer("/counts/conflicted")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0,
                hooks: continuation_projection_hooks(),
                current_validation: continuation_validation_snapshot(
                    &continuation_current_validation,
                ),
                tool_failures: ContinuationToolFailureSnapshot::new(
                    &reconciliation.actionable_unexpected_event_ids,
                ),
            })
        };

        let mut output = json!({
            "project": project,
            "resolved_project": resolved_project_payload(&resolved),
            "session_id": session_id,
            "workspace": workspace,
            "changes": {
                "show_changes": changes_result.output,
                "hunks_truncated": changes_result.output
                    .get("hunks_truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            "validation": reconciliation.validation,
            "continuation_feedback": continuation_feedback,
            "permissions": permissions,
            "tool_failures": reconciliation.tool_failures,
            "review_evidence": review_evidence,
            "work_performed": work_performed,
            "changed_paths": changed_paths,
            "hygiene": hygiene,
            "handoff": handoff,
            "jobs": jobs,
            "deterministic": true,
            "llm_summary": false,
            "final_warnings": final_warnings,
        });
        output["suggested_next_actions"] = json!(finish_suggested_next_actions(&output));
        output["handoff_brief"] = build_handoff_brief(HandoffBriefInput {
            session_summary: &closeout_session_summary,
            continuation_feedback: output.get("continuation_feedback").unwrap_or(&Value::Null),
            workspace_requested: include_workspace,
            workspace: output.get("workspace"),
            validation_requested: include_validation_summary,
            validation: output.get("validation"),
            jobs: output.get("jobs"),
            guidance_available,
            existing_suggested_actions: output.get("suggested_next_actions"),
        });
        let decision = finish_decision_output(&output);
        if summary_only {
            return ToolResult::ok(compact_finish_output(&decision));
        }
        for field in [
            "facts",
            "hard_blockers",
            "advisories",
            "task_outcome",
            "evidence_history",
            "evidence_integrity",
            "informational_notes",
        ] {
            output[field] = decision.get(field).cloned().unwrap_or(Value::Null);
        }
        output["suggested_next_actions"] = decision["suggested_next_actions"].clone();
        ToolResult::ok(output)
    }

    /// Build the bounded continuation feedback projection for coding startup.
    ///
    /// Pure read-only: validation is derived from the session ledger only
    /// (`validation_summary_from_events`, no job-status enrichment), jobs come
    /// from the bounded `active_jobs_summary` metadata, and guidance is read
    /// from the message board without marking anything read or resolved. No
    /// shell, no file reads, no Runner requests, no ledger mutation.
    async fn startup_continuation_feedback(
        &self,
        summary: &sessions::SessionSummary,
        pre_instruction_summary: Option<&sessions::SessionSummary>,
        continuation_kind: &'static str,
        jobs: &Value,
        workspace_conflicts: bool,
    ) -> Value {
        // Fresh new session: nothing to continue from.
        if continuation_kind == "created" {
            return not_applicable_continuation_feedback_value("fresh_session");
        }
        // For a reused/resumed/restored session, project over the snapshot taken
        // *before* this new `task_instruction` was appended, so the feedback
        // describes the previous attempt's work rather than the empty new
        // attempt. The returned session itself still contains the new
        // instruction; only the projection uses the pre-instruction window.
        // Guidance and job state are read from the live session id at the same
        // instant; neither is mutated.
        let projection_summary = pre_instruction_summary.unwrap_or(summary);
        if projection_summary.events.is_empty() {
            return not_applicable_continuation_feedback_value("empty_session");
        }
        let validation = super::validation_events::validation_summary_from_events(
            &projection_summary.events,
            20,
        );
        let current_validation = super::validation_events::current_validation_evidence_for_session(
            projection_summary,
            20,
        );
        let raw_tool_failures = tool_failure_summary_from_events(&projection_summary.events, 10);
        let reconciliation =
            reconcile_closeout_evidence(&raw_tool_failures, projection_summary, &validation);
        let (discussion, _) = self.discussion_snapshot(&summary.session_id);
        continuation_feedback_value(ContinuationFeedbackInput {
            session_summary: projection_summary,
            validation: &validation,
            jobs,
            discussion: &discussion,
            continuation: continuation_kind,
            suggest_exploration_continuity: true,
            workspace_conflicts,
            hooks: continuation_projection_hooks(),
            current_validation: continuation_validation_snapshot(&current_validation),
            tool_failures: ContinuationToolFailureSnapshot::new(
                &reconciliation.actionable_unexpected_event_ids,
            ),
        })
    }

    fn discussion_snapshot(&self, session_id: &str) -> (sessions::SessionDiscussionSummary, bool) {
        match self.sessions.discussion_summary(session_id, Some(20)) {
            Ok(discussion) => (discussion, true),
            Err(_) => (
                sessions::SessionDiscussionSummary {
                    counts: sessions::SessionDiscussionCounts {
                        total: 0,
                        open: 0,
                        resolved: 0,
                        guidance: 0,
                        progress: 0,
                        risk: 0,
                        todo: 0,
                        question: 0,
                        answer: 0,
                        decision: 0,
                        open_guidance: 0,
                        open_questions: 0,
                        open_risks: 0,
                        open_todos: 0,
                    },
                    open_guidance: Vec::new(),
                    open_questions: Vec::new(),
                    open_risks: Vec::new(),
                    open_todos: Vec::new(),
                    high_priority_open_todos: Vec::new(),
                    recent_answers: Vec::new(),
                    recent_completions: Vec::new(),
                    recent_progress: Vec::new(),
                    recent_decisions: Vec::new(),
                },
                false,
            ),
        }
    }

    async fn coding_startup_git_summary(
        &self,
        project: &str,
        include_recent_commits: bool,
        warnings: &mut Vec<Value>,
    ) -> Value {
        let mut output = json!({
            "available": false,
            "branch": Value::Null,
            "head": Value::Null,
            "clean": Value::Null,
            "changed_files_count": 0,
            "counts": {},
            "recent_commits": [],
            "warnings": [],
        });

        {
            let result = self
                .show_changes(project.to_string(), None, Some(false), None, None, None)
                .await;
            if !result.success {
                warnings.push(json!({
                    "kind": "git_status_unavailable",
                    "message": result.error,
                }));
            }
            output["available"] = json!(result
                .output
                .get("git_available")
                .and_then(Value::as_bool)
                .unwrap_or(result.success));
            output["branch"] = result.output.get("branch").cloned().unwrap_or(Value::Null);
            output["head"] = result.output.get("head").cloned().unwrap_or(Value::Null);
            output["clean"] = result.output.get("clean").cloned().unwrap_or(Value::Null);
            output["counts"] = result
                .output
                .get("counts")
                .cloned()
                .unwrap_or_else(|| json!({}));
            output["changed_files_count"] =
                json!(changed_files_count_from_counts(&output["counts"]));
            output["warnings"] = result
                .output
                .get("warnings")
                .cloned()
                .unwrap_or_else(|| json!([]));
            output["show_changes"] = result.output;
        }

        if include_recent_commits {
            let result = self.git_log(project.to_string(), Some(5), None).await;
            if result.success {
                output["recent_commits"] = result
                    .output
                    .get("commits")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                output["recent_commits_truncated"] = result
                    .output
                    .get("truncated")
                    .cloned()
                    .unwrap_or(json!(false));
            } else {
                warnings.push(json!({
                    "kind": "recent_commits_unavailable",
                    "message": result.error,
                }));
                output["recent_commits"] = json!([]);
                output["recent_commits_truncated"] = json!(false);
            }
        } else if let Some(object) = output.as_object_mut() {
            object.remove("recent_commits");
        }

        output
    }

    /// Deterministic repository structure overview for the coding startup
    /// brief. Reuses the existing `project_overview` implementation and keeps
    /// every safety property: directory entries, file types, and the git
    /// tracked index only; no file bodies, no project code execution, no
    /// symlink following, no protected/sensitive/build/cache paths, and only
    /// project-relative paths are returned.
    ///
    /// The overview is routed to the owning Runner
    /// via the `file_project_overview` op with a short startup probe timeout.
    /// On timeout the request is cancelled. An optional overview failure never
    /// fails the already-legal coding task: it returns a deterministic
    /// unavailable marker and the caller surfaces a
    /// `repository_overview_unavailable` warning without leaking raw errors,
    /// absolute paths, or Runner output.
    async fn repository_overview_for_startup(
        &self,
        resolved: &ResolvedProject,
        auth: Option<&AuthContext>,
    ) -> Value {
        let client_id = resolved.config.client_id.as_str();
        let access = crate::runner_http::runner_access_from_auth(auth);
        // The owning runner must support the structured file capability.
        if !self
            .runner_registry
            .runner_supports_for_auth(client_id, RUNNER_CAPABILITY_FILE_READ, access.as_ref())
            .await
            .unwrap_or(false)
        {
            return repository_overview_unavailable();
        }
        // Short startup probe budget, much tighter than the standalone
        // `project_overview` tool's 30s wait.
        let probe_wait_timeout = self.repository_overview_probe_timeout.as_secs().max(1);
        let (request_id, receiver) = match self
            .runner_registry
            .enqueue_file_op(
                ShellFileOpRequest {
                    op: "project_overview".to_string(),
                    client_id: client_id.to_string(),
                    path: ".".to_string(),
                    cwd: Some(resolved.config.path.clone()),
                    content: Some(
                        json!({
                            "max_depth": STARTUP_OVERVIEW_REQUEST_MAX_DEPTH,
                            "limit": STARTUP_OVERVIEW_REQUEST_LIMIT,
                        })
                        .to_string(),
                    ),
                    max_bytes: None,
                    old_text: None,
                    pattern: None,
                    expected_sha256: None,
                    expected_prefix: None,
                    start_line: None,
                    end_line: None,
                    line: None,
                    create_dirs: false,
                    wait_timeout_secs: probe_wait_timeout,
                },
                "coding_startup".to_string(),
            )
            .await
        {
            Ok(enqueued) => enqueued,
            Err(_) => return repository_overview_unavailable(),
        };
        match tokio::time::timeout(
            Duration::from_secs(probe_wait_timeout.saturating_add(2)),
            receiver,
        )
        .await
        {
            Ok(Ok(response)) if response.exit_code == Some(0) && response.error.is_none() => {
                // The Runner response is untrusted: it must parse and pass the
                // shared project-overview contract validation against the fixed
                // request bounds (root / depth 2 / limit 120). A malformed,
                // boundary-mismatched, or schema-violating payload fails closed
                // to an unavailable marker without leaking raw stdout, errors,
                // or absolute paths. The normalized result keeps only the
                // formal contract fields.
                match serde_json::from_str::<Value>(response.stdout.as_deref().unwrap_or_default())
                {
                    Ok(parsed) => match validate_project_overview_for_startup(&parsed) {
                        Ok(mut overview) => {
                            if let Some(object) = overview.as_object_mut() {
                                object.insert("status".to_string(), json!("available"));
                                object.insert("reason_code".to_string(), Value::Null);
                                object.insert(
                                    "project".to_string(),
                                    json!(resolved.resolved_id.clone()),
                                );
                            }
                            overview
                        }
                        Err(_) => repository_overview_unavailable(),
                    },
                    Err(_) => repository_overview_unavailable(),
                }
            }
            Ok(Ok(_)) => repository_overview_unavailable(),
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&request_id).await;
                repository_overview_unavailable()
            }
            Err(_) => {
                self.runner_registry.cancel_request(&request_id).await;
                repository_overview_unavailable()
            }
        }
    }
}

/// Deterministic unavailable marker for the startup repository overview. Never
/// carries raw errors, absolute paths, or Runner output.
fn repository_overview_unavailable() -> Value {
    json!({
        "status": "unavailable",
        "reason_code": "unsupported_or_unavailable",
    })
}

/// Compact marker for the ordinary work_on_project profile. This is an
/// intentional omission, not a failed repository probe, so it must not produce
/// an unavailable warning or lower readiness.
fn repository_overview_not_requested() -> Value {
    json!({
        "status": "unavailable",
        "reason_code": REPOSITORY_OVERVIEW_NOT_REQUESTED_REASON,
    })
}

/// Fixed startup overview request bounds. The overview is always scoped to the
/// project root with depth 2 and limit 120; a Runner response that reports a
/// different `path`, `max_depth`, or `limit` is malformed and fails closed.
const STARTUP_OVERVIEW_REQUEST_PATH: &str = "";
const STARTUP_OVERVIEW_REQUEST_MAX_DEPTH: usize = 2;
const STARTUP_OVERVIEW_REQUEST_LIMIT: usize = 120;

/// Validate a startup overview payload against the fixed request bounds using
/// the shared contract entry. Returns the normalized formal-contract payload
/// on success.
fn validate_project_overview_for_startup(payload: &Value) -> Result<Value, String> {
    crate::project_overview::validate_project_overview(
        payload,
        STARTUP_OVERVIEW_REQUEST_PATH,
        STARTUP_OVERVIEW_REQUEST_MAX_DEPTH,
        STARTUP_OVERVIEW_REQUEST_LIMIT,
    )
}

#[derive(Deserialize)]
struct WorkOnProjectBriefProjection {
    session: WorkOnProjectSessionProjection,
    project: WorkOnProjectProjectProjection,
    project_resolution: ProjectResolutionMetadata,
    workspace: WorkOnProjectWorkspaceProjection,
    workflow: Value,
    instructions: WorkOnProjectInstructionsProjection,
    semantic_navigation: WorkOnProjectSemanticNavigationProjection,
    #[serde(default)]
    extensions: Option<Value>,
    repository: Value,
    continuation: WorkOnProjectContinuationProjection,
    blockers: Vec<String>,
    warnings: Vec<String>,
    startup_verdict: WorkOnProjectStartupVerdictProjection,
}

#[derive(Deserialize)]
struct WorkOnProjectSessionProjection {
    session_id: String,
    continuation: String,
    execution_context: sessions::SessionExecutionContext,
}

#[derive(Deserialize)]
struct WorkOnProjectProjectProjection {
    resolved_id: String,
}

#[derive(Deserialize)]
struct WorkOnProjectSemanticNavigationProjection {
    #[serde(default)]
    supported: bool,
    available: WorkOnProjectRequiredNullable<bool>,
    status: String,
    capability: WorkOnProjectRequiredNullable<String>,
    reason_code: WorkOnProjectRequiredNullable<String>,
}

#[derive(Deserialize)]
struct WorkOnProjectJobsProjection {
    active_count: u64,
    blocking_active_count: u64,
    nonblocking_active_count: u64,
    recovering_count: u64,
    terminal_pending_count: u64,
    latest_status: String,
}

#[derive(Deserialize, Serialize)]
#[serde(transparent)]
struct WorkOnProjectRequiredNullable<T>(Option<T>);

#[derive(Deserialize)]
struct WorkOnProjectWorkspaceProjection {
    status: String,
    git_available: WorkOnProjectRequiredNullable<bool>,
    branch: WorkOnProjectRequiredNullable<String>,
    head: WorkOnProjectRequiredNullable<String>,
    clean: WorkOnProjectRequiredNullable<bool>,
    conflicts: u64,
}

#[derive(Deserialize)]
struct WorkOnProjectInstructionsProjection {
    status: String,
    sources: Vec<WorkOnProjectInstructionSourceProjection>,
    #[serde(default)]
    changed_sources: Option<Vec<String>>,
    #[serde(default)]
    content_included: bool,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    total_chars: u64,
}

#[derive(Deserialize, Serialize)]
struct WorkOnProjectInstructionSourceProjection {
    path: String,
    fingerprint: String,
    truncated: bool,
    headings: Vec<String>,
    content: WorkOnProjectRequiredNullable<String>,
    read_more: WorkOnProjectRequiredNullable<WorkOnProjectReadMoreProjection>,
}

#[derive(Deserialize, Serialize)]
struct WorkOnProjectReadMoreProjection {
    path: String,
    start_line: u64,
    limit: u64,
}

#[derive(Deserialize)]
struct WorkOnProjectContinuationProjection {
    suggested_next_actions: WorkOnProjectActionItemsProjection,
    jobs: WorkOnProjectJobsProjection,
}

#[derive(Deserialize)]
struct WorkOnProjectActionItemsProjection {
    items: Vec<String>,
}

#[derive(Deserialize)]
struct WorkOnProjectStartupVerdictProjection {
    status: String,
    blocking: bool,
    suggested_next_actions: Vec<String>,
}

fn sparse_work_on_project_instruction_source(
    source: WorkOnProjectInstructionSourceProjection,
) -> Value {
    let WorkOnProjectInstructionSourceProjection {
        path,
        fingerprint,
        truncated,
        headings,
        content,
        read_more,
    } = source;
    let mut projected = json!({
        "path": path,
        "fingerprint": fingerprint,
    });
    if truncated {
        projected["truncated"] = json!(true);
    }
    if let Some(content) = content.0 {
        if !headings.is_empty() {
            projected["headings"] = json!(headings);
        }
        projected["content"] = json!(content);
        if let Some(read_more) = read_more.0 {
            projected["read_more"] = json!(read_more);
        }
    }
    projected
}

fn sparse_work_on_project_workspace(workspace: WorkOnProjectWorkspaceProjection) -> Value {
    let WorkOnProjectWorkspaceProjection {
        status,
        git_available,
        branch,
        head,
        clean,
        conflicts,
    } = workspace;
    let status_unavailable = status == "unavailable";
    let mut projected = json!({"status": status});
    if git_available.0 == Some(false) {
        projected["git_available"] = json!(false);
    }
    if let Some(branch) = branch.0 {
        projected["branch"] = json!(branch);
    }
    if let Some(head) = head.0 {
        projected["head"] = json!(head);
    }
    if status_unavailable {
        if let Some(clean) = clean.0 {
            projected["clean"] = json!(clean);
        }
    }
    if conflicts > 0 {
        projected["conflicts"] = json!(conflicts);
    }
    projected
}

fn sparse_work_on_project_jobs(jobs: WorkOnProjectJobsProjection) -> Option<Value> {
    let mut projected = json!({});
    for (key, count) in [
        ("active_count", jobs.active_count),
        ("blocking_active_count", jobs.blocking_active_count),
        ("nonblocking_active_count", jobs.nonblocking_active_count),
        ("recovering_count", jobs.recovering_count),
        ("terminal_pending_count", jobs.terminal_pending_count),
    ] {
        if count > 0 {
            projected[key] = json!(count);
        }
    }
    if jobs.latest_status != "not_observed" {
        projected["latest_status"] = json!(jobs.latest_status);
    }
    projected.as_object().filter(|object| !object.is_empty())?;
    Some(projected)
}

fn is_default_work_on_project_resolution(
    resolution: &ProjectResolutionMetadata,
    resolved_project: &str,
) -> bool {
    resolution.source == "project"
        && resolution.outcome == "resolved_existing_project"
        && resolution.resolved_project == resolved_project
        && !resolution.registered
}

fn is_default_work_on_project_repository(repository: &Value) -> bool {
    repository.as_object().is_some_and(|object| {
        object.len() == 2
            && repository.get("status").and_then(Value::as_str) == Some("unavailable")
            && repository.get("reason_code").and_then(Value::as_str)
                == Some(REPOSITORY_OVERVIEW_NOT_REQUESTED_REASON)
    })
}

/// Convert a successful canonical coding-startup result into the compact
/// `work_on_project` contract. The delegated engine may already have changed
/// Session state, so protocol drift fails closed with `state_changed=true`.
#[cfg(test)]
pub(crate) fn project_work_on_project_output(project: String, output: Value) -> ToolResult {
    project_work_on_project_output_with_workflow(project, output, true)
}

#[cfg(test)]
pub(crate) fn project_work_on_project_output_with_workflow(
    project: String,
    output: Value,
    include_workflow_guidance: bool,
) -> ToolResult {
    project_work_on_project_output_with_workflow_inner(
        project,
        output,
        include_workflow_guidance,
        None,
    )
}

#[cfg(test)]
pub(crate) fn project_work_on_project_output_with_correlation_for_test(
    project: String,
    output: Value,
    correlation: &mut ToolCallCorrelation,
) -> ToolResult {
    project_work_on_project_output_with_workflow_inner(project, output, true, Some(correlation))
}

fn project_work_on_project_output_with_workflow_inner(
    project: String,
    output: Value,
    include_workflow_guidance: bool,
    correlation: Option<&mut ToolCallCorrelation>,
) -> ToolResult {
    let permission = output.get("permission").cloned();
    let Some(brief) = startup_brief_from_output(&output) else {
        return work_on_project_projection_failed(
            "output",
            "complete startup brief object",
            "non-object",
            None,
        );
    };
    let projection = match serde_json::from_value::<WorkOnProjectBriefProjection>(brief.clone()) {
        Ok(projection) => projection,
        Err(error) => {
            return work_on_project_projection_failed(
                "output",
                "complete typed startup brief",
                "missing or wrongly typed field",
                Some(error.to_string()),
            )
        }
    };
    if !sessions::is_valid_session_id(&projection.session.session_id) {
        return work_on_project_projection_failed(
            "session.session_id",
            "valid wc_sess_* string",
            "invalid string",
            None,
        );
    }
    if !matches!(
        projection.session.continuation.as_str(),
        "created" | "continued" | "resumed_explicitly"
    ) {
        return work_on_project_projection_failed(
            "session.continuation",
            "created, continued, or resumed_explicitly",
            "unsupported string",
            None,
        );
    }
    if !matches!(
        projection.workspace.status.as_str(),
        "clean" | "dirty" | "blocked" | "unavailable"
    ) {
        return work_on_project_projection_failed(
            "workspace.status",
            "clean, dirty, blocked, or unavailable",
            "unsupported string",
            None,
        );
    }
    if !matches!(
        projection.instructions.status.as_str(),
        "loaded" | "reused" | "changed" | "not_found" | "unavailable"
    ) {
        return work_on_project_projection_failed(
            "instructions.status",
            "loaded, reused, changed, not_found, or unavailable",
            "unsupported string",
            None,
        );
    }
    if projection.instructions.sources.len() > 5 {
        return work_on_project_projection_failed(
            "instructions.sources",
            "at most 5 source objects",
            "invalid array contents",
            None,
        );
    }
    if projection.workflow != builtin_coding_workflow_projection() {
        return work_on_project_projection_failed(
            "workflow",
            "canonical built-in coding workflow contract",
            "non-canonical workflow projection",
            None,
        );
    }

    if let Some(correlation) = correlation {
        correlation.resolved_project = Some(projection.project.resolved_id.clone());
        correlation.add_workflow_session(WorkflowSessionCorrelation {
            session_id: projection.session.session_id.clone(),
            project: Some(projection.project.resolved_id.clone()),
            relation: WorkflowSessionCorrelationRelation::WorkOnProject,
        });
    }

    let suggested_next_actions = if projection.startup_verdict.suggested_next_actions.is_empty() {
        projection.continuation.suggested_next_actions.items
    } else {
        projection.startup_verdict.suggested_next_actions
    };
    let generic_begin_action_only = suggested_next_actions.len() == 1
        && suggested_next_actions[0] == "begin the requested coding task";
    let project_resolution_is_default = is_default_work_on_project_resolution(
        &projection.project_resolution,
        &projection.project.resolved_id,
    );
    let worktree = projection.project_resolution.worktree.clone();
    let repository_is_default = is_default_work_on_project_repository(&projection.repository);
    let execution_context_is_empty = projection.session.execution_context.is_empty();
    let jobs = sparse_work_on_project_jobs(projection.continuation.jobs);
    let workspace = sparse_work_on_project_workspace(projection.workspace);

    let WorkOnProjectInstructionsProjection {
        status,
        sources,
        changed_sources,
        content_included,
        truncated,
        total_chars,
    } = projection.instructions;
    let mut instructions = json!({
        "status": status,
        "sources": sources
            .into_iter()
            .map(sparse_work_on_project_instruction_source)
            .collect::<Vec<_>>(),
    });
    if changed_sources
        .as_ref()
        .is_some_and(|sources| !sources.is_empty())
    {
        instructions["changed_sources"] = json!(changed_sources);
    }
    if content_included {
        instructions["content_included"] = json!(true);
    }
    if truncated {
        instructions["truncated"] = json!(true);
        if total_chars > 0 {
            instructions["total_chars"] = json!(total_chars);
        }
    }

    let semantic_navigation = json!({
        "supported": projection.semantic_navigation.supported,
        "available": projection.semantic_navigation.available,
        "status": projection.semantic_navigation.status,
        "capability": projection.semantic_navigation.capability,
        "reason_code": projection.semantic_navigation.reason_code,
    });
    let mut result = ToolResult::ok(json!({
        "session_id": projection.session.session_id,
        "project": project,
        "resolved_project": projection.project.resolved_id,
        "continuation": projection.session.continuation,
        "workspace": workspace,
        "instructions": instructions,
        "semantic_navigation": semantic_navigation,
    }));
    if let Some(extensions) = projection.extensions {
        result.output["extensions"] = extensions;
    }
    if include_workflow_guidance {
        result.output["workflow"] = projection.workflow;
    }
    if !project_resolution_is_default {
        let mut project_resolution = json!(projection.project_resolution);
        if let Some(project_resolution) = project_resolution.as_object_mut() {
            project_resolution.remove("worktree");
        }
        result.output["project_resolution"] = project_resolution;
    }
    if let Some(worktree) = worktree {
        result.output["worktree"] = json!(worktree);
    }
    if !execution_context_is_empty {
        result.output["execution_context"] = json!(projection.session.execution_context);
    }
    if projection.startup_verdict.status != "pass" || projection.startup_verdict.blocking {
        result.output["readiness"] = json!({
            "status": projection.startup_verdict.status,
            "blocking": projection.startup_verdict.blocking,
        });
    }
    if !repository_is_default {
        result.output["repository"] = projection.repository;
    }
    if let Some(jobs) = jobs {
        result.output["jobs"] = jobs;
    }
    if !projection.blockers.is_empty() {
        result.output["blockers"] = json!(projection.blockers);
    }
    if !projection.warnings.is_empty() {
        result.output["warnings"] = json!(projection.warnings);
    }
    if !suggested_next_actions.is_empty() && !generic_begin_action_only {
        result.output["suggested_next_actions"] = json!(suggested_next_actions);
    }
    if let Some(permission) = permission {
        result.output["permission"] = permission;
    }
    result
}

fn work_on_project_projection_failed(
    field: &str,
    expected: &str,
    actual: &str,
    detail: Option<String>,
) -> ToolResult {
    ToolResult::err_with_output(
        format!("work_on_project projection failed: {field} expected {expected}, got {actual}"),
        json!({
            "error_kind": "work_on_project_projection_failed",
            "failure_kind": "work_on_project_projection_failed",
            "underlying_tool": "work_on_project",
            "field": field,
            "expected": expected,
            "actual": actual,
            "detail": detail,
            "state_changed": true,
        }),
    )
}

fn resolved_project_payload(resolved: &ResolvedProject) -> Value {
    json!({
        "input": resolved.input.clone(),
        "id": resolved.resolved_id.clone(),
        "path": resolved.config.path.clone(),
        "client_id": resolved.config.client_id.clone(),
        "allow_patch": resolved.config.allow_patch,
    })
}

fn rules_summary(snapshot: Option<&ProjectInstructionsSnapshot>) -> Value {
    let Some(snapshot) = snapshot else {
        return Value::Null;
    };
    let sources: Vec<Value> = snapshot.files.iter().map(rule_source_summary).collect();
    json!({
        "present": snapshot.loaded,
        "loaded": snapshot.loaded,
        "sources": sources,
        "candidate_paths": snapshot.candidate_paths.clone(),
        "total_chars": snapshot.total_chars,
        "max_total_chars": snapshot.max_total_chars,
        "truncated": snapshot.truncated,
        "scan_complete": snapshot.scan_complete,
        "summary": if snapshot.loaded {
            "deterministic instruction source summary; read listed sources for full content"
        } else {
            "no project instruction source loaded from the fixed candidate list"
        },
        "note": snapshot.note.clone(),
    })
}

fn rule_source_summary(file: &ProjectInstructionFile) -> Value {
    json!({
        "path": file.path.clone(),
        "fingerprint": file.fingerprint.clone(),
        "chars": file.chars,
        "total_lines": file.total_lines,
        "start_line": file.start_line,
        "limit": file.limit,
        "truncated": file.truncated,
        "read_more": file.read_more.clone(),
        "headings": extract_headings(&file.content),
        "first_lines": extract_first_lines(&file.content),
    })
}

fn extract_headings(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('#'))
        .take(RULES_MAX_HEADINGS)
        .map(bound_line)
        .collect()
}

fn extract_first_lines(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(RULES_MAX_FIRST_LINES)
        .map(bound_line)
        .collect()
}

fn bound_line(line: &str) -> String {
    let mut out = String::new();
    for ch in line.chars().take(RULES_MAX_LINE_CHARS) {
        out.push(ch);
    }
    out
}

/// Full default startup recommended flow. Reuses the shared
/// `TOOL_RECOMMENDED_FLOWS` group definitions so top-level startup guidance
/// does not drift from `tool_manifest.recommended_flows`.
fn recommended_flow_payload() -> Value {
    recommended_flow_groups(None)
}

/// Project top-level `recommended_flow` onto tools present in the embedded
/// `tool_manifest`. Group keys stay fixed; empty groups are allowed.
fn recommended_flow_payload_for_manifest_tools(manifest: &Value) -> Value {
    let visible: HashSet<&str> = manifest
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    recommended_flow_groups(Some(&visible))
}

fn recommended_flow_groups(visible: Option<&HashSet<&str>>) -> Value {
    const GROUPS: &[&str] = &["inspect", "edit", "validate", "review", "handoff"];
    let mut map = serde_json::Map::new();
    for group in GROUPS {
        let tools = TOOL_RECOMMENDED_FLOWS
            .iter()
            .find(|flow| flow.name == *group)
            .map(|flow| {
                let mut seen = HashSet::new();
                flow.tools
                    .iter()
                    .copied()
                    .filter(|tool| {
                        let allowed = match visible {
                            Some(set) => set.contains(*tool),
                            None => true,
                        };
                        allowed && seen.insert(*tool)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        map.insert((*group).to_string(), json!(tools));
    }
    Value::Object(map)
}

fn workspace_payload_from_show_changes(show_changes: &Value) -> Value {
    let counts = show_changes
        .get("counts")
        .cloned()
        .unwrap_or_else(|| json!({}));
    json!({
        "clean": show_changes
            .get("clean")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "git_available": show_changes
            .get("git_available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "non_git_project": show_changes
            .get("non_git_project")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "branch": show_changes.get("branch").cloned().unwrap_or(Value::Null),
        "head": show_changes.get("head").cloned().unwrap_or(Value::Null),
        "changed_files_count": changed_files_count_from_counts(&counts),
        "counts": counts,
        "warnings": show_changes
            .get("warnings")
            .cloned()
            .unwrap_or_else(|| json!([])),
    })
}

/// Map startup `git` summary fields into the workspace warning shape.
fn workspace_payload_from_git_summary(git: &Value) -> Value {
    let counts = git.get("counts").cloned().unwrap_or_else(|| json!({}));
    json!({
        "clean": git.get("clean").and_then(Value::as_bool).unwrap_or(false),
        "git_available": git
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "changed_files_count": git
            .get("changed_files_count")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| changed_files_count_from_counts(&counts)),
        "counts": counts,
    })
}

fn finish_decision_output(output: &Value) -> Value {
    let hygiene_checked = output
        .get("hygiene")
        .is_some_and(|hygiene| !hygiene.is_null());
    let workspace_clean = output
        .get("workspace")
        .and_then(|workspace| workspace.get("clean"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let workspace_conflicts = output
        .pointer("/workspace/counts/conflicted")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let hygiene_clean = output
        .get("hygiene")
        .and_then(|hygiene| hygiene.get("clean"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let hygiene_secret_like_paths = output
        .pointer("/hygiene/counts/secret_like_paths")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let hygiene_truncated = output
        .pointer("/hygiene/truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut decision = json!({
        "workspace_clean": workspace_clean,
        "workspace_conflicts": workspace_conflicts,
        "hygiene_clean": hygiene_clean,
        "hygiene_secret_like_paths": hygiene_secret_like_paths,
        "hygiene_truncated": hygiene_truncated,
        "jobs": compact_jobs(output.get("jobs").unwrap_or(&Value::Null)),
        "tool_failures": compact_tool_failures(output.get("tool_failures").unwrap_or(&Value::Null)),
        "validation": compact_validation(output.get("validation").unwrap_or(&Value::Null)),
        "review_evidence": compact_review_evidence(output.get("review_evidence").unwrap_or(&Value::Null)),
        "work_performed": output.get("work_performed").cloned().unwrap_or_else(|| json!([])),
        "changed_paths": output.get("changed_paths").cloned().unwrap_or_else(|| json!([])),
        "warnings": output.get("final_warnings").cloned().unwrap_or_else(|| json!([])),
        "suggested_next_actions": output.get("suggested_next_actions").cloned().unwrap_or_else(|| json!([])),
    });
    apply_compact_workflow_outcomes(&mut decision, true, Some(hygiene_checked));
    let verdict = decision
        .get("verdict")
        .cloned()
        .unwrap_or_else(|| json!({}));
    decision["suggested_next_actions"] = json!(merged_suggested_next_actions(&decision, &verdict));
    decision
        .as_object_mut()
        .expect("finish decision output is an object")
        .remove("verdict");
    decision
}

fn compact_finish_output(decision: &Value) -> Value {
    json!({
        "summary_only": true,
        "workspace_clean": decision.get("workspace_clean").cloned().unwrap_or(json!(false)),
        "workspace_conflicts": decision.get("workspace_conflicts").cloned().unwrap_or(json!(0)),
        "hygiene_clean": decision.get("hygiene_clean").cloned().unwrap_or(json!(true)),
        "hygiene_secret_like_paths": decision.get("hygiene_secret_like_paths").cloned().unwrap_or(json!(0)),
        "hygiene_truncated": decision.get("hygiene_truncated").cloned().unwrap_or(json!(false)),
        "jobs": decision.get("jobs").cloned().unwrap_or_else(|| compact_jobs(&Value::Null)),
        "validation": compact_finish_validation(decision.get("validation").unwrap_or(&Value::Null)),
        "tool_failures": decision.get("tool_failures").cloned().unwrap_or_else(|| compact_tool_failures(&Value::Null)),
        "task_outcome": decision.get("task_outcome").cloned().unwrap_or(Value::Null),
        "evidence_integrity": decision.get("evidence_integrity").cloned().unwrap_or(Value::Null),
        "warnings": decision.get("warnings").cloned().unwrap_or_else(|| json!([])),
        "suggested_next_actions": decision.get("suggested_next_actions").cloned().unwrap_or_else(|| json!([])),
    })
}

fn compact_finish_validation(validation: &Value) -> Value {
    let current = validation.get("current_evidence").unwrap_or(&Value::Null);
    json!({
        "status": validation.get("status").cloned().unwrap_or_else(|| json!("not_run")),
        "reason": validation.get("reason").cloned().unwrap_or(Value::Null),
        "latest_status": validation.get("latest_status").cloned().unwrap_or_else(|| json!("unknown")),
        "successes": validation.get("successes").and_then(Value::as_u64).unwrap_or(0),
        "failures": validation.get("failures").and_then(Value::as_u64).unwrap_or(0),
        "resolved_failure_count": validation.pointer("/resolved_failures/count").and_then(Value::as_u64).unwrap_or(0),
        "unresolved_failure_count": validation.pointer("/unresolved_failures/count").and_then(Value::as_u64).unwrap_or(0),
        "evidence_gap_count": validation.pointer("/evidence_gaps/count").and_then(Value::as_u64).unwrap_or(0),
        "current_status": current.get("status").cloned().unwrap_or_else(|| json!("unknown")),
        "current_reason": current.get("reason").cloned().unwrap_or(Value::Null),
        "current_validation_events": current.get("events_total").and_then(Value::as_u64).unwrap_or(0),
        "current_successes": current.get("successes").and_then(Value::as_u64).unwrap_or(0),
        "current_failures": current.get("failures").and_then(Value::as_u64).unwrap_or(0),
        "current_resolved_failure_count": current.get("resolved_failure_count").and_then(Value::as_u64).unwrap_or(0),
        "current_unresolved_failure_count": current.get("unresolved_failure_count").and_then(Value::as_u64).unwrap_or(0),
        "current_evidence_gap_event_count": current.get("evidence_gap_event_count").and_then(Value::as_u64).unwrap_or(0),
        "stale_failure_count": current.get("stale_failure_count").and_then(Value::as_u64).unwrap_or(0),
        "cargo_test_zero_tests_run": validation_has_cargo_test_zero_tests(validation),
    })
}

fn startup_verdict(
    output: &Value,
    active_jobs: &Value,
    owning_runner_available: Option<bool>,
    runtime_status_call_failed: bool,
    tool_manifest_requested: bool,
) -> Value {
    let mut checks = Vec::new();
    let mut actions: Vec<String> = Vec::new();

    push_startup_check(
        &mut checks,
        "runtime_status",
        runtime_status_check(output, runtime_status_call_failed),
    );
    push_startup_check(&mut checks, "workspace", workspace_check(output));
    push_startup_check(&mut checks, "jobs", startup_jobs_check(active_jobs));
    push_startup_check(
        &mut checks,
        "agent",
        startup_agent_check(output, owning_runner_available),
    );
    push_startup_check(
        &mut checks,
        "tool_manifest",
        startup_tool_manifest_check(output, tool_manifest_requested),
    );

    for check in &checks {
        match check.get("reason").and_then(Value::as_str) {
            Some("runtime_status_call_failed") => {
                push_unique_action(&mut actions, "inspect runtime_status directly")
            }
            Some("workspace_dirty") => push_unique_action(
                &mut actions,
                "inspect existing worktree changes with show_changes and preserve them while editing",
            ),
            Some("workspace_conflicts") => push_unique_action(
                &mut actions,
                "review merge/rebase conflicts carefully; do not reset or overwrite conflict markers unless resolving them",
            ),
            Some("active_jobs_present") | Some("blocking_active_jobs") => {
                push_unique_action(&mut actions, "inspect active jobs before proceeding")
            }
            Some("agent_offline") => {
                push_unique_action(&mut actions, "check Runner connectivity with list_runners")
            }
            Some("tool_manifest_not_requested") => push_unique_action(
                &mut actions,
                "request tool_manifest if workflow discovery is needed",
            ),
            Some("truncated_by_limit") => push_unique_action(
                &mut actions,
                "continue with the bounded tool_manifest or request a focused category",
            ),
            Some("tool_manifest_unavailable") => {
                push_unique_action(&mut actions, "inspect tool_manifest directly")
            }
            _ => {}
        }
    }

    if actions.is_empty() {
        actions.push("proceed with the coding task using the explicit session_id".to_string());
    }
    let status = aggregate_startup_status(&checks);
    json!({
        "status": status,
        "blocking": status == "fail",
        "checks": checks,
        "suggested_next_actions": actions,
    })
}

fn runtime_status_check(
    output: &Value,
    runtime_status_call_failed: bool,
) -> (&'static str, Option<&'static str>) {
    if runtime_status_call_failed {
        return ("fail", Some("runtime_status_call_failed"));
    }
    let runtime_status = output.get("runtime_status").unwrap_or(&Value::Null);
    if !runtime_status.is_object() {
        return ("fail", Some("runtime_status_unavailable"));
    }
    match runtime_status
        .pointer("/tools/count")
        .and_then(Value::as_u64)
    {
        Some(count) if count > 0 => ("pass", None),
        Some(_) => ("fail", Some("tool_count_zero")),
        None => ("warn", Some("tool_count_unknown")),
    }
}

fn workspace_check(output: &Value) -> (&'static str, Option<&'static str>) {
    let git = output.get("git").unwrap_or(&Value::Null);
    if git.get("available").and_then(Value::as_bool) == Some(false) {
        return ("warn", Some("git_unavailable"));
    }
    // Ordinary tracked/staged/untracked edits are expected development state.
    // An unresolved merge/rebase conflict is a deterministic blocker until it
    // is resolved; the session itself remains usable for inspection and repair.
    let conflicted = git
        .pointer("/counts/conflicted")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if conflicted > 0 {
        return ("fail", Some("workspace_conflicts"));
    }
    match git.get("clean").and_then(Value::as_bool) {
        Some(true) => ("pass", None),
        Some(false) => ("warn", Some("workspace_dirty")),
        None => ("warn", Some("workspace_unknown")),
    }
}

fn startup_jobs_check(jobs: &Value) -> (&'static str, Option<&'static str>) {
    if jobs
        .get("blocking_active_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        return ("fail", Some("blocking_active_jobs"));
    }
    match jobs.get("active_count").and_then(Value::as_u64) {
        Some(0) => ("pass", None),
        Some(_) => ("warn", Some("active_jobs_present")),
        None => ("warn", Some("jobs_unknown")),
    }
}

fn startup_agent_check(
    _output: &Value,
    owning_runner_available: Option<bool>,
) -> (&'static str, Option<&'static str>) {
    match owning_runner_available {
        Some(false) => ("fail", Some("agent_offline")),
        Some(true) => ("pass", None),
        None => ("warn", Some("agent_health_unknown")),
    }
}

fn owning_runner_available(
    resolved: &ResolvedProject,
    runtime_status: &Value,
    runtime_status_call_failed: bool,
) -> Option<bool> {
    if runtime_status_call_failed {
        return None;
    }
    Some(
        runtime_status
            .pointer("/agents/summary/clients")
            .and_then(Value::as_array)
            .and_then(|clients| {
                clients.iter().find(|client| {
                    client.get("client_id").and_then(Value::as_str)
                        == Some(resolved.config.client_id.as_str())
                })
            })
            .and_then(|client| client.get("status").and_then(Value::as_str))
            == Some("online"),
    )
}

fn startup_tool_manifest_check(
    output: &Value,
    tool_manifest_requested: bool,
) -> (&'static str, Option<&'static str>) {
    if !tool_manifest_requested {
        return ("warn", Some("tool_manifest_not_requested"));
    }
    let Some(manifest) = output.get("tool_manifest") else {
        return ("fail", Some("tool_manifest_unavailable"));
    };
    if !manifest.is_object() {
        return ("fail", Some("tool_manifest_unavailable"));
    }
    if manifest
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if manifest.get("truncation_reason").and_then(Value::as_str) == Some("limit") {
            return ("warn", Some("truncated_by_limit"));
        }
        return ("warn", Some("tool_manifest_truncated"));
    }
    ("pass", None)
}

fn push_startup_check(
    checks: &mut Vec<Value>,
    name: &'static str,
    (status, reason): (&'static str, Option<&'static str>),
) {
    let mut check = json!({
        "name": name,
        "status": status,
    });
    if let Some(reason) = reason {
        check["reason"] = json!(reason);
    }
    checks.push(check);
}

fn aggregate_startup_status(checks: &[Value]) -> &'static str {
    if checks
        .iter()
        .any(|check| check.get("status").and_then(Value::as_str) == Some("fail"))
    {
        "fail"
    } else if checks
        .iter()
        .any(|check| check.get("status").and_then(Value::as_str) == Some("warn"))
    {
        "warn"
    } else {
        "pass"
    }
}

fn push_unique_action(actions: &mut Vec<String>, action: &str) {
    if !actions.iter().any(|existing| existing == action) {
        actions.push(action.to_string());
    }
}

fn merged_suggested_next_actions(output: &Value, verdict: &Value) -> Vec<String> {
    let mut actions = string_array(output.get("suggested_next_actions"));
    for action in string_array(verdict.get("suggested_next_actions")) {
        push_unique_action(&mut actions, &action);
    }
    actions
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn finish_suggested_next_actions(output: &Value) -> Vec<String> {
    let mut actions = Vec::new();
    let push = |actions: &mut Vec<String>, action: &str| {
        if !actions.iter().any(|existing| existing == action) {
            actions.push(action.to_string());
        }
    };
    let tool_failures = output.get("tool_failures").unwrap_or(&Value::Null);
    let expectation_mismatch_count = tool_failures
        .get("expectation_mismatch_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let unexpected_success_count = tool_failures
        .get("unexpected_success_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    if actionable_unexpected_failure_count(tool_failures) > 0 {
        push(
            &mut actions,
            "review unexpected failed tool calls before proceeding",
        );
    }
    if expectation_mismatch_count > 0 {
        push(
            &mut actions,
            "review result expectation mismatches before proceeding",
        );
    }
    if unexpected_success_count > 0 {
        push(
            &mut actions,
            "review failure expectations that unexpectedly succeeded",
        );
    }
    if output
        .get("workspace")
        .and_then(|workspace| workspace.get("clean"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        if output
            .pointer("/changes/show_changes/diff_review_handoff/tool")
            .and_then(Value::as_str)
            == Some("git_diff_hunks")
        {
            push(&mut actions, "continue the diff review with git_diff_hunks");
        } else {
            push(&mut actions, "review workspace changes with show_changes");
        }
    }
    if output
        .get("jobs")
        .and_then(|jobs| jobs.get("blocking_active_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        push(&mut actions, "stop or await blocking active jobs");
    }
    if validation_has_cargo_test_zero_tests(output.get("validation").unwrap_or(&Value::Null)) {
        push(
            &mut actions,
            "cargo_test ran zero tests; verify the test filter or command",
        );
    }
    actions
}

fn changed_files_count_from_counts(counts: &Value) -> u64 {
    [
        "modified",
        "added",
        "deleted",
        "renamed",
        "copied",
        "untracked",
        "conflicted",
    ]
    .iter()
    .map(|key| counts.get(*key).and_then(Value::as_u64).unwrap_or(0))
    .sum()
}

fn append_workspace_warnings(workspace: &Value, warnings: &mut Vec<Value>) {
    if !workspace
        .get("clean")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let conflicted = workspace
            .pointer("/counts/conflicted")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let message = if conflicted > 0 {
            "workspace has merge/rebase conflicts; inspect and preserve existing worktree state"
        } else {
            "workspace has existing tracked or untracked changes; inspect and preserve them while editing"
        };
        warnings.push(json!({
            "kind": "dirty_worktree",
            "changed_files_count": workspace
                .get("changed_files_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "conflicted": conflicted,
            "message": message,
        }));
    }
    if !workspace
        .get("git_available")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        warnings.push(json!({
            "kind": "git_unavailable",
            "message": "git-backed workspace inspection unavailable",
        }));
    }
}

fn append_hygiene_warnings(hygiene: &Value, warnings: &mut Vec<Value>) {
    let finding_count = hygiene
        .get("counts")
        .and_then(|counts| counts.get("findings"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if finding_count > 0 {
        warnings.push(json!({
            "kind": "workspace_hygiene_findings",
            "findings": finding_count,
            "message": "workspace hygiene findings should be reviewed",
        }));
    }
}

#[cfg(test)]
mod startup_runner_tests {
    use super::*;
    use crate::projects::ProjectConfig;

    #[test]
    fn finish_actions_use_git_diff_hunks_when_nested_show_changes_hands_off() {
        let output = json!({
            "workspace": {"clean": false},
            "changes": {
                "show_changes": {
                    "diff_review_handoff": {"tool": "git_diff_hunks"}
                }
            },
            "jobs": {"blocking_active_count": 0},
            "validation": {},
            "tool_failures": {},
        });
        let actions = finish_suggested_next_actions(&output);
        assert!(actions
            .iter()
            .any(|action| action == "continue the diff review with git_diff_hunks"));
        assert!(
            crate::tool_runtime::tool_definition::is_adaptive_runtime_direct_tool(
                output["changes"]["show_changes"]["diff_review_handoff"]["tool"]
                    .as_str()
                    .unwrap()
            )
        );
        assert!(!actions
            .iter()
            .any(|action| action == "review workspace changes with show_changes"));
    }

    fn resolved_agent(client_id: &str) -> ResolvedProject {
        ResolvedProject {
            input: "demo".to_string(),
            resolved_id: format!("agent:{client_id}:demo"),
            config: ProjectConfig {
                path: "/tmp/demo".to_string(),
                client_id: client_id.to_string(),
                allow_patch: true,
            },
        }
    }

    #[test]
    fn missing_target_runner_is_unavailable_even_when_a_peer_is_online() {
        let runtime_status = json!({
            "agents": {
                "summary": {
                    "clients": [{"client_id": "peer", "status": "online"}]
                }
            }
        });
        assert_eq!(
            owning_runner_available(&resolved_agent("target"), &runtime_status, false),
            Some(false)
        );
    }

    #[test]
    fn target_runner_online_is_available_even_when_a_peer_is_stale() {
        let runtime_status = json!({
            "agents": {
                "summary": {
                    "clients": [
                        {"client_id": "peer", "status": "stale"},
                        {"client_id": "target", "status": "online"}
                    ]
                }
            }
        });
        assert_eq!(
            owning_runner_available(&resolved_agent("target"), &runtime_status, false),
            Some(true)
        );
    }
}
