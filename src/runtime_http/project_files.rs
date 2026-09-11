use super::{parse_json_body, render_result, require_runtime};
use crate::action_audit::ActionAudit;
use crate::tool_runtime::ToolCall;
use salvo::prelude::*;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ProjectIdRequest {
    pub project: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadProjectFileRequest {
    pub project: String,
    pub path: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub with_line_numbers: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ProjectGitDiffRequest {
    pub project: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ApplyUnifiedDiffRequest {
    pub project: String,
    pub diff: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub deny_sensitive_paths: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GitRestorePathsRequest {
    pub project: String,
    pub paths: Vec<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscardUntrackedRequest {
    pub project: String,
    pub paths: Vec<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListProjectFilesRequest {
    pub project: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SearchProjectTextRequest {
    pub project: String,
    pub pattern: String,
    #[serde(default)]
    pub pattern_mode: Option<crate::tool_runtime::SearchPatternMode>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub context_before: Option<usize>,
    #[serde(default)]
    pub context_after: Option<usize>,
    #[serde(default)]
    pub include_globs: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_globs: Option<Vec<String>>,
    #[serde(default)]
    pub result_mode: Option<crate::tool_runtime::SearchResultMode>,
    #[serde(default)]
    pub timeout_secs: Option<i64>,
}

#[handler]
pub async fn projects_read_file(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/projects/read_file", "readProjectFile");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<ReadProjectFileRequest>(req, res).await else {
        return;
    };
    let project = Some(body.project.clone());
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::ReadFile {
                project: body.project,
                path: body.path,
                session_id: body.session_id,
                start_line: body.start_line,
                limit: body.limit,
                with_line_numbers: body.with_line_numbers,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "read_file", project, result);
}

#[handler]
pub async fn projects_git_status(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(
        req,
        depot,
        "/api/projects/git_status",
        "getProjectGitStatus",
    );
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<ProjectIdRequest>(req, res).await else {
        return;
    };
    let project = Some(body.project.clone());
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::GitStatus {
                project: body.project,
                session_id: body.session_id,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "git_status", project, result);
}

/// `POST /api/projects/git_diff` — thin GPT Actions wrapper over
/// `ToolCall::GitDiff`. Read-only inspection routed to the owning Runner.
#[handler]
pub async fn projects_git_diff(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/projects/git_diff", "getProjectGitDiff");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<ProjectGitDiffRequest>(req, res).await else {
        return;
    };
    let project = Some(body.project.clone());
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::GitDiff {
                project: body.project,
                session_id: body.session_id,
                args: body.args,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "git_diff", project, result);
}

/// `POST /api/projects/apply_unified_diff` — thin GPT Actions wrapper over the
/// external/raw unified-diff mutation. The runtime validates the bounded raw diff,
/// performs one `git apply --check -`, and dispatches `git apply -` only after
/// the preflight passes.
#[handler]
pub async fn projects_apply_unified_diff(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(
        req,
        depot,
        "/api/projects/apply_unified_diff",
        "applyUnifiedDiff",
    );
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<ApplyUnifiedDiffRequest>(req, res).await else {
        return;
    };
    let project = Some(body.project.clone());
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::ApplyUnifiedDiff {
                project: body.project,
                diff: body.diff,
                session_id: body.session_id,
                deny_sensitive_paths: body.deny_sensitive_paths,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "apply_unified_diff", project, result);
}

/// `POST /api/projects/git_restore_paths` — thin GPT Actions wrapper over
/// `ToolCall::GitRestorePaths`. Mutation with side effects: runs
/// `git restore -- <paths>` on selected tracked project-relative paths.
/// Requires Bearer auth and the agent `structured_process_argv` capability.
#[handler]
pub async fn projects_git_restore_paths(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(
        req,
        depot,
        "/api/projects/git_restore_paths",
        "gitRestorePaths",
    );
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<GitRestorePathsRequest>(req, res).await else {
        return;
    };
    let project = Some(body.project.clone());
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::GitRestorePaths {
                project: body.project,
                paths: body.paths,
                session_id: body.session_id,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "git_restore_paths", project, result);
}

/// `POST /api/projects/discard_untracked` — thin GPT Actions wrapper over
/// `ToolCall::DiscardUntracked`. Mutation with side effects: runs
/// `git clean -f -- <paths>` only for selected project-relative untracked
/// paths. Requires Bearer auth and the agent `structured_process_argv` capability.
#[handler]
pub async fn projects_discard_untracked(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(
        req,
        depot,
        "/api/projects/discard_untracked",
        "discardUntrackedFiles",
    );
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<DiscardUntrackedRequest>(req, res).await else {
        return;
    };
    let project = Some(body.project.clone());
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::DiscardUntracked {
                project: body.project,
                paths: body.paths,
                session_id: body.session_id,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "discard_untracked", project, result);
}

/// `ToolCall::ListProjectFiles`. Read-only, Runner-backed file listing.
#[handler]
pub async fn projects_list_files(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/projects/list_files", "listProjectFiles");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<ListProjectFilesRequest>(req, res).await else {
        return;
    };
    let project = body.project.clone();
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::ListProjectFiles {
                project: body.project,
                session_id: body.session_id,
                path: body.path,
                limit: body.limit,
                offset: body.offset,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "list_project_files", Some(project), result);
}

/// `ToolCall::SearchProjectText`. Read-only, Runner-backed bounded text search.
#[handler]
pub async fn projects_search_text(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/projects/search_text", "searchProjectText");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<SearchProjectTextRequest>(req, res).await else {
        return;
    };
    let project = body.project.clone();
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::SearchProjectText {
                project: body.project,
                pattern: body.pattern,
                pattern_mode: body.pattern_mode,
                session_id: body.session_id,
                path: body.path,
                limit: body.limit,
                context_before: body.context_before,
                context_after: body.context_after,
                include_globs: body.include_globs,
                exclude_globs: body.exclude_globs,
                result_mode: body.result_mode,
                timeout_secs: body.timeout_secs,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "search_project_text", Some(project), result);
}

/// `ToolCall::GitDiffSummary`. Read-only git inspection.
#[handler]
pub async fn projects_git_diff_summary(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(
        req,
        depot,
        "/api/projects/git_diff_summary",
        "getProjectGitDiffSummary",
    );
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<ProjectIdRequest>(req, res).await else {
        return;
    };
    let project = body.project.clone();
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::GitDiffSummary {
                project: body.project,
                session_id: body.session_id,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "git_diff_summary", Some(project), result);
}
