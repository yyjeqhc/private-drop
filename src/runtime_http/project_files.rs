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
