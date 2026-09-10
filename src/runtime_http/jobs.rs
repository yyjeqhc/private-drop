use super::{parse_json_body, render_result, require_runtime};
use crate::action_audit::ActionAudit;
use crate::tool_runtime::ToolCall;
use salvo::prelude::*;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct JobStatusRequest {
    pub job_id: String,
}

#[derive(Debug, Deserialize)]
struct JobLogRequest {
    pub job_id: String,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub tail_lines: Option<usize>,
    #[serde(default)]
    pub after_observation_token: Option<String>,
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct JobStopRequest {
    pub job_id: String,
}

#[derive(Debug, Deserialize)]
struct RunShellRequest {
    pub project: String,
    pub command: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// `POST /api/projects/run_job` - thin REST wrapper over
/// `ToolCall::RunJob`. Starts an async background shell job in an
/// Runner-registered project and returns a `job_id`. Execution with side
/// effects; requires Bearer auth and the Runner async shell job capability.
/// Dedicated GPT Action (`startProjectShellJob`); also reachable via
/// callRuntimeTool / MCP tools/call. Poll with `getRuntimeJobStatus` and read
/// output with `getRuntimeJobTail` / `getRuntimeJobLog`.
#[derive(Debug, Deserialize)]
struct StartProjectShellJobRequest {
    pub project: String,
    pub command: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<i64>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListJobsRequest {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JobTailRequest {
    pub job_id: String,
    #[serde(default)]
    pub tail_lines: Option<usize>,
    #[serde(default)]
    pub after_observation_token: Option<String>,
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

#[handler]
pub async fn job_status(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/jobs/status", "jobStatus");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<JobStatusRequest>(req, res).await else {
        return;
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::JobStatus {
                job_id: body.job_id,
                include_command_preview: false,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "job_status", None, result);
}

#[handler]
pub async fn job_log(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/jobs/log", "jobLog");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<JobLogRequest>(req, res).await else {
        return;
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::JobLog {
                job_id: body.job_id,
                offset: body.offset,
                tail_lines: body.tail_lines,
                after_observation_token: body.after_observation_token,
                wait_secs: body.wait_secs,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "job_log", None, result);
}

/// Stop a runtime Job through its owning Runner. This is a thin wrapper over
/// `ToolRuntime::stop_job`; it is
/// intentionally NOT exposed as a GPT Action (absent from openapi.json) so
/// remote ChatGPT callers cannot drive an explicit stop. Only Jobs the
/// runtime registry owns can be stopped.
#[handler]
pub async fn job_stop(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/jobs/stop", "jobStop");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<JobStopRequest>(req, res).await else {
        return;
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime.stop_job(body.job_id, auth.as_ref()).await;
    render_result(res, &audit, "job_stop", None, result);
}

/// `POST /api/projects/run_job` handler. Thin wrapper: parse request, auth,
/// audit, and dispatch to `ToolRuntime` via `ToolCall::RunJob`. All business
/// logic (capability checks, owner boundary, job creation) stays in
/// `ToolRuntime`.
#[handler]
pub async fn projects_run_job(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/projects/run_job", "startProjectShellJob");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<StartProjectShellJobRequest>(req, res).await else {
        return;
    };
    let project = Some(body.project.clone());
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project: body.project,
                command: body.command,
                session_id: body.session_id,
                timeout_secs: body.timeout_secs,
                cwd: body.cwd,
                purpose: None,
                shell: None,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "run_job", project, result);
}

/// `POST /api/projects/run_shell` - thin GPT Actions wrapper over
/// `ToolCall::RunShell`. Executable with side effects; requires the owning
/// agent's shell capability and Bearer auth.
#[handler]
pub async fn projects_run_shell(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(
        req,
        depot,
        "/api/projects/run_shell",
        "runProjectShellCommand",
    );
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<RunShellRequest>(req, res).await else {
        return;
    };
    let project = Some(body.project.clone());
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::RunShell {
                project: body.project,
                command: body.command,
                session_id: body.session_id,
                timeout_secs: body.timeout_secs,
                cwd: body.cwd,
                purpose: None,
                shell: None,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "run_shell", project, result);
}

/// `ToolCall::ListJobs`. Bounded job summaries (no stdout/stderr bodies).
#[handler]
pub async fn jobs_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/jobs/list", "listRuntimeJobs");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<ListJobsRequest>(req, res).await else {
        return;
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::ListJobs {
                limit: body.limit,
                status: body.status,
                project: body.project,
                session_id: body.session_id,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "list_jobs", None, result);
}

/// `ToolCall::JobTail`. Bounded stdout/stderr tails for a job.
#[handler]
pub async fn job_tail(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/jobs/tail", "getRuntimeJobTail");
    let Some(runtime) = require_runtime(depot, res) else {
        return;
    };
    let Some(body) = parse_json_body::<JobTailRequest>(req, res).await else {
        return;
    };
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let result = runtime
        .dispatch_with_auth(
            ToolCall::JobTail {
                job_id: body.job_id,
                tail_lines: body.tail_lines,
                after_observation_token: body.after_observation_token,
                wait_secs: body.wait_secs,
            },
            auth.as_ref(),
        )
        .await;
    render_result(res, &audit, "job_tail", None, result);
}
