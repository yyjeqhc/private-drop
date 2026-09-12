use crate::action_audit::{ActionAudit, ActionAuditRecord};
#[cfg(test)]
use crate::runner_protocol::{
    RunnerCapabilities, RunnerPollRequest, RunnerProjectSummary, RunnerRegisterRequest,
};
use crate::runner_protocol::{
    RunnerJobLogRequest, RunnerJobLogResponse, RunnerJobStatusRequest, RunnerJobStatusResponse,
    RunnerJobStopRequest, RunnerJobStopResponse, RunnerJobsListRequest, RunnerJobsListResponse,
    ShellFileOpRequest, ShellFileOpResponse, ShellJobInfo, ShellJobOpRequest, ShellJobOpResponse,
    ShellRunRequest, ShellRunResponse,
};
use salvo::prelude::*;
use serde_json::json;
use std::sync::Arc;
#[cfg(test)]
use tokio::sync::Notify;

mod auth;
mod handlers;
mod telemetry;

pub(crate) use auth::{
    detached_initiator_identity_from_auth, effective_register_owner, enforce_register_owner,
    enforce_runner_transport, requested_by_from_auth, require_runner_transport_scope,
    runner_access_from_auth,
};
pub use handlers::{
    runner_job_update, runner_offline, runner_persistent_shell_result, runner_poll,
    runner_register, runner_result,
};
pub(crate) use telemetry::tool_request_trace_telemetry;
pub(crate) use webcodex_runner_registry::{
    command_preview, process_preview, recovery_timeout_sweep, script_preview, EnqueueLspError,
    RunnerFeature, RunnerFeatureSet, RunnerRegistry, RunnerSemanticView, RunnerTransport,
    ShellJobStartMetadata, ShellJobVisibility, StructuredJobExecution, COMMAND_PREVIEW_MAX_CHARS,
    DETACHED_IDEMPOTENCY_CONFLICT, DETACHED_IDEMPOTENCY_RECOVERY_PREFIX,
    RECOVERY_SWEEP_INTERVAL_SECS, RUNNER_ONLINE_WINDOW_SECS,
};
#[cfg(test)]
pub(crate) use webcodex_runner_registry::{TRANSPORT_POLLING, TRANSPORT_WEBSOCKET};

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn get_registry(depot: &Depot) -> Option<Arc<RunnerRegistry>> {
    depot.obtain::<Arc<RunnerRegistry>>().ok().cloned()
}

async fn assert_registry_runner_owner(
    registry: &RunnerRegistry,
    auth: Option<&crate::auth::AuthContext>,
    client_id: &str,
) -> Result<(), (StatusCode, String)> {
    if registry.get_runner_view(client_id).await.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown shell client: {}", client_id),
        ));
    }
    let access = runner_access_from_auth(auth);
    registry
        .assert_runner_access(access.as_ref(), client_id)
        .await
        .map_err(|e| {
            let status = if e.contains("unknown shell client") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::FORBIDDEN
            };
            (status, e)
        })
}

fn record_shell_run_action(
    audit: &ActionAudit,
    response: &ShellRunResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new("run", response.success, http_status)
            .error(response.error.clone())
            .ids(json!({"request_id": response.request_id}))
            .summary(json!({
                "client_id": response.client_id,
                "cwd": response.cwd,
                "command_preview": response.command_preview,
                "exit_code": response.exit_code,
                "duration_ms": response.duration_ms,
            })),
    );
}

fn record_shell_file_action(
    audit: &ActionAudit,
    response: &ShellFileOpResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new(response.op.clone(), response.success, http_status)
            .error(response.error.clone())
            .ids(json!({"request_id": response.request_id}))
            .summary(json!({
                "client_id": response.client_id,
                "path": response.path,
                "cwd": response.cwd,
                "bytes": response.bytes,
                "sha256": response.sha256,
                "entries_count": response.entries.len(),
            })),
    );
}

fn record_shell_job_action(
    audit: &ActionAudit,
    response: &ShellJobOpResponse,
    http_status: StatusCode,
) {
    let job_id = response.job.as_ref().map(|job| job.job_id.clone());
    let job_ids = if response.jobs.is_empty() {
        Vec::<String>::new()
    } else {
        response.jobs.iter().map(|job| job.job_id.clone()).collect()
    };
    audit.record(
        ActionAuditRecord::new(response.op.clone(), response.success, http_status)
            .error(response.error.clone())
            .ids(json!({"job_id": job_id, "job_ids": job_ids}))
            .summary(json!({
                "job_status": response.job.as_ref().map(|job| job.status.clone()),
                "client_id": response.job.as_ref().map(|job| job.client_id.clone()),
                "jobs_count": response.jobs.len(),
                "stdout_included": response.stdout.is_some(),
                "stderr_included": response.stderr.is_some(),
            })),
    );
}

fn record_shell_job_status_action(
    audit: &ActionAudit,
    response: &RunnerJobStatusResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new("shell_job_status", response.success, http_status)
            .error(response.error.clone())
            .ids(json!({
                "job_id": response.job_id,
                "client_id": response.client_id,
            }))
            .summary(json!({
                "kind": response.kind,
                "status": response.status,
                "exit_code": response.exit_code,
                "elapsed_secs": response.elapsed_secs,
            })),
    );
}

fn record_shell_job_log_action(
    audit: &ActionAudit,
    response: &RunnerJobLogResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new("shell_job_log", response.success, http_status)
            .error(response.error.clone())
            .ids(json!({
                "job_id": response.job_id,
                "client_id": response.client_id,
            }))
            .summary(json!({
                "stdout_included": response.stdout_tail.is_some(),
                "stderr_included": response.stderr_tail.is_some(),
                "next_stdout_line": response.next_stdout_line,
                "next_stderr_line": response.next_stderr_line,
            })),
    );
}

fn record_shell_job_stop_action(
    audit: &ActionAudit,
    response: &RunnerJobStopResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new("shell_job_stop", response.success, http_status)
            .error(response.error.clone())
            .ids(json!({"job_id": response.job_id}))
            .summary(json!({"status": response.status})),
    );
}

fn record_shell_jobs_list_action(
    audit: &ActionAudit,
    response: &RunnerJobsListResponse,
    http_status: StatusCode,
) {
    audit.record(
        ActionAuditRecord::new("shell_job_list", response.success, http_status)
            .error(response.error.clone())
            .ids(json!({"client_id": response.client_id}))
            .summary(json!({"jobs_count": response.jobs.len()})),
    );
}

fn render_shell_run(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: ShellRunResponse,
) {
    res.status_code(status);
    record_shell_run_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_job_status(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: RunnerJobStatusResponse,
) {
    res.status_code(status);
    record_shell_job_status_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_job_log(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: RunnerJobLogResponse,
) {
    res.status_code(status);
    record_shell_job_log_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_job_stop_response(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: RunnerJobStopResponse,
) {
    res.status_code(status);
    record_shell_job_stop_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_jobs_list(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: RunnerJobsListResponse,
) {
    res.status_code(status);
    record_shell_jobs_list_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_file(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: ShellFileOpResponse,
) {
    res.status_code(status);
    record_shell_file_action(audit, &response, status);
    res.render(Json(response));
}

fn render_shell_job(
    res: &mut Response,
    audit: &ActionAudit,
    status: StatusCode,
    response: ShellJobOpResponse,
) {
    res.status_code(status);
    record_shell_job_action(audit, &response, status);
    res.render(Json(response));
}

#[handler]
pub async fn shell_run(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/run", "runShell");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_run(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            ShellRunResponse {
                success: false,
                request_id: String::new(),
                client_id: String::new(),
                cwd: None,
                command_preview: String::new(),
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: Some("Runner registry not configured".to_string()),
                request_dispatched: None,
                command_execution_state: None,
            },
        );
        return;
    };
    let body: ShellRunRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_run(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                ShellRunResponse {
                    success: false,
                    request_id: String::new(),
                    client_id: String::new(),
                    cwd: None,
                    command_preview: String::new(),
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: None,
                    error: Some(format!("Invalid JSON: {}", e)),
                    request_dispatched: None,
                    command_execution_state: None,
                },
            );
            return;
        }
    };
    let wait_timeout_secs = body.wait_timeout_secs;
    let client_id = body.client_id.clone();
    let cwd = body.cwd.clone();
    let preview = command_preview(&body.command);
    if let Err((status, e)) =
        assert_registry_runner_owner(&registry, auth.as_ref(), &client_id).await
    {
        render_shell_run(
            res,
            &audit,
            status,
            ShellRunResponse {
                success: false,
                request_id: String::new(),
                client_id,
                cwd,
                command_preview: preview,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: Some(e),
                request_dispatched: None,
                command_execution_state: None,
            },
        );
        return;
    }
    let requested_by = requested_by_from_auth(auth.as_ref());
    let (request_id, rx) = match registry.enqueue_run(body, requested_by).await {
        Ok(result) => result,
        Err(e) => {
            render_shell_run(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                ShellRunResponse {
                    success: false,
                    request_id: String::new(),
                    client_id,
                    cwd,
                    command_preview: preview,
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: None,
                    error: Some(e),
                    request_dispatched: None,
                    command_execution_state: None,
                },
            );
            return;
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(wait_timeout_secs), rx).await {
        Ok(Ok(response)) => render_shell_run(res, &audit, StatusCode::OK, response),
        Ok(Err(_closed)) => render_shell_run(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            ShellRunResponse {
                success: false,
                request_id,
                client_id,
                cwd,
                command_preview: preview,
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: Some("shell request waiter was dropped".to_string()),
                request_dispatched: None,
                command_execution_state: None,
            },
        ),
        Err(_elapsed) => {
            let request_dispatched = registry.cancel_request_dispatch_state(&request_id).await;
            render_shell_run(
                res,
                &audit,
                StatusCode::REQUEST_TIMEOUT,
                ShellRunResponse {
                    success: false,
                    request_id,
                    client_id,
                    cwd,
                    command_preview: preview,
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: None,
                    error: Some(format!(
                        "timed out waiting {} seconds for shell client result",
                        wait_timeout_secs
                    )),
                    request_dispatched,
                    command_execution_state: None,
                },
            );
        }
    }
}

fn shell_file_response_from_run(
    op: String,
    path: String,
    cwd: Option<String>,
    request_content: Option<String>,
    response: ShellRunResponse,
) -> ShellFileOpResponse {
    let success = response.error.is_none() && response.exit_code == Some(0);
    let stdout = response.stdout.unwrap_or_default();
    let entries = if op == "list" && success {
        stdout.lines().map(|line| line.to_string()).collect()
    } else {
        Vec::new()
    };
    let content = if op == "read" && success {
        Some(stdout.clone())
    } else {
        None
    };
    let bytes = match op.as_str() {
        "read" => content.as_ref().map(|s| s.len()),
        "write" if success => Some(stdout.trim().parse::<usize>().unwrap_or(0)),
        _ => None,
    };
    let sha256 = match op.as_str() {
        "read" if success => content.as_ref().map(|s| sha256_hex(s)),
        "write" if success => request_content.as_ref().map(|s| sha256_hex(s)),
        _ => None,
    };
    ShellFileOpResponse {
        success,
        op,
        request_id: response.request_id,
        client_id: response.client_id,
        path,
        cwd,
        content,
        entries,
        bytes,
        sha256,
        stderr: response.stderr,
        error: response.error,
    }
}

#[handler]
pub async fn shell_file_op(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/file", "shellFileOp");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        let response = shell_file_error_response(
            "unknown".to_string(),
            String::new(),
            String::new(),
            None,
            "Runner registry not configured".to_string(),
        );
        render_shell_file(res, &audit, StatusCode::INTERNAL_SERVER_ERROR, response);
        return;
    };
    let body: ShellFileOpRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            let response = shell_file_error_response(
                "unknown".to_string(),
                String::new(),
                String::new(),
                None,
                format!("Invalid JSON: {}", e),
            );
            render_shell_file(res, &audit, StatusCode::BAD_REQUEST, response);
            return;
        }
    };
    let op = body.op.clone();
    let client_id = body.client_id.clone();
    let path = body.path.clone();
    let cwd = body.cwd.clone();
    let request_content = body.content.clone();
    let wait_timeout_secs = body.wait_timeout_secs;
    if let Err((status, e)) =
        assert_registry_runner_owner(&registry, auth.as_ref(), &client_id).await
    {
        let response = shell_file_error_response(op, client_id, path, cwd, e);
        render_shell_file(res, &audit, status, response);
        return;
    }
    let requested_by = requested_by_from_auth(auth.as_ref());
    let (request_id, rx) = match registry.enqueue_file_op(body, requested_by).await {
        Ok(result) => result,
        Err(e) => {
            let response = shell_file_error_response(op, client_id, path, cwd, e);
            render_shell_file(res, &audit, StatusCode::BAD_REQUEST, response);
            return;
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(wait_timeout_secs), rx).await {
        Ok(Ok(response)) => render_shell_file(
            res,
            &audit,
            StatusCode::OK,
            shell_file_response_from_run(op, path, cwd, request_content, response),
        ),
        Ok(Err(_closed)) => {
            let response = shell_file_error_response(
                op,
                client_id,
                path,
                cwd,
                "shell file request waiter was dropped".to_string(),
            );
            render_shell_file(res, &audit, StatusCode::INTERNAL_SERVER_ERROR, response);
        }
        Err(_elapsed) => {
            registry.cancel_request(&request_id).await;
            let response = shell_file_error_response(
                op,
                client_id,
                path,
                cwd,
                format!(
                    "timed out waiting {} seconds for shell file result",
                    wait_timeout_secs
                ),
            );
            render_shell_file(res, &audit, StatusCode::REQUEST_TIMEOUT, response);
        }
    }
}

fn shell_file_error_response(
    op: String,
    client_id: String,
    path: String,
    cwd: Option<String>,
    error: String,
) -> ShellFileOpResponse {
    ShellFileOpResponse {
        success: false,
        op,
        request_id: String::new(),
        client_id,
        path,
        cwd,
        content: None,
        entries: Vec::new(),
        bytes: None,
        sha256: None,
        stderr: None,
        error: Some(error),
    }
}

fn shell_job_error_response(op: String, error: String) -> ShellJobOpResponse {
    ShellJobOpResponse {
        success: false,
        op,
        job: None,
        jobs: Vec::new(),
        stdout: None,
        stderr: None,
        next_stdout_line: None,
        next_stderr_line: None,
        error: Some(error),
    }
}

fn shell_job_status_response_from_job(job: ShellJobInfo) -> RunnerJobStatusResponse {
    RunnerJobStatusResponse {
        success: true,
        job_id: Some(job.job_id.clone()),
        client_id: Some(job.client_id.clone()),
        kind: Some(job.kind.clone()),
        status: Some(job.status.clone()),
        elapsed_secs: job.elapsed_secs,
        exit_code: job.exit_code,
        result: job.result.clone(),
        job: Some(job),
        error: None,
    }
}

fn shell_job_status_error_response(error: String) -> RunnerJobStatusResponse {
    RunnerJobStatusResponse {
        success: false,
        job_id: None,
        client_id: None,
        kind: None,
        status: None,
        elapsed_secs: None,
        exit_code: None,
        result: None,
        job: None,
        error: Some(error),
    }
}

fn shell_job_log_error_response(error: String) -> RunnerJobLogResponse {
    RunnerJobLogResponse {
        success: false,
        job_id: None,
        client_id: None,
        stdout_tail: None,
        stderr_tail: None,
        next_stdout_line: None,
        next_stderr_line: None,
        job: None,
        error: Some(error),
    }
}

fn shell_job_stop_error_response(error: String) -> RunnerJobStopResponse {
    RunnerJobStopResponse {
        success: false,
        job_id: None,
        status: None,
        job: None,
        error: Some(error),
    }
}

fn shell_jobs_list_error_response(client_id: String, error: String) -> RunnerJobsListResponse {
    RunnerJobsListResponse {
        success: false,
        client_id,
        jobs: Vec::new(),
        error: Some(error),
    }
}

async fn authorize_job_access(
    registry: &RunnerRegistry,
    auth: Option<&crate::auth::AuthContext>,
    job_id: &str,
    requested_client_id: Option<&str>,
) -> Result<ShellJobInfo, (StatusCode, String)> {
    let job = registry
        .get_job(job_id)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if let Some(requested_client_id) = requested_client_id {
        if requested_client_id != job.client_id {
            return Err((
                StatusCode::FORBIDDEN,
                format!(
                    "job_id {} belongs to client {}, not {}",
                    job_id, job.client_id, requested_client_id
                ),
            ));
        }
    }
    assert_registry_runner_owner(registry, auth, &job.client_id).await?;
    Ok(job)
}

#[handler]
pub async fn shell_job(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/job", "runShellJob");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_job(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            shell_job_error_response(
                "unknown".to_string(),
                "Runner registry not configured".to_string(),
            ),
        );
        return;
    };
    let body: ShellJobOpRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_job(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                shell_job_error_response("unknown".to_string(), format!("Invalid JSON: {}", e)),
            );
            return;
        }
    };
    let op = body.op.clone();
    match op.as_str() {
        "start" => {
            let Some(client_id) = body.client_id.as_deref() else {
                render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, "client_id is required for op=start".to_string()),
                );
                return;
            };
            if let Err((status, e)) =
                assert_registry_runner_owner(&registry, auth.as_ref(), client_id).await
            {
                render_shell_job(res, &audit, status, shell_job_error_response(op, e));
                return;
            }
            let requested_by = requested_by_from_auth(auth.as_ref());
            match registry.start_job(body, requested_by).await {
                Ok(job) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::OK,
                    ShellJobOpResponse {
                        success: true,
                        op,
                        job: Some(job),
                        jobs: Vec::new(),
                        stdout: None,
                        stderr: None,
                        next_stdout_line: None,
                        next_stderr_line: None,
                        error: None,
                    },
                ),
                Err(e) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, e),
                ),
            }
        }
        "status" => {
            let Some(job_id) = body.job_id.as_deref() else {
                render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, "job_id is required for op=status".to_string()),
                );
                return;
            };
            match registry.get_job(job_id).await {
                Ok(job) => {
                    if let Err((status, e)) =
                        assert_registry_runner_owner(&registry, auth.as_ref(), &job.client_id).await
                    {
                        render_shell_job(res, &audit, status, shell_job_error_response(op, e));
                        return;
                    }
                    render_shell_job(
                        res,
                        &audit,
                        StatusCode::OK,
                        ShellJobOpResponse {
                            success: true,
                            op,
                            job: Some(job),
                            jobs: Vec::new(),
                            stdout: None,
                            stderr: None,
                            next_stdout_line: None,
                            next_stderr_line: None,
                            error: None,
                        },
                    )
                }
                Err(e) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, e),
                ),
            }
        }
        "list" => {
            let limit = body.limit.unwrap_or(20).clamp(1, 100);
            let mut jobs = Vec::new();
            let access = runner_access_from_auth(auth.as_ref());
            for job in registry.list_jobs(Some(100)).await {
                if auth.as_ref().map(|auth| auth.is_admin()).unwrap_or(false) {
                    jobs.push(job);
                    continue;
                }
                if registry
                    .assert_runner_access(access.as_ref(), &job.client_id)
                    .await
                    .is_ok()
                {
                    jobs.push(job);
                }
            }
            jobs.truncate(limit);
            render_shell_job(
                res,
                &audit,
                StatusCode::OK,
                ShellJobOpResponse {
                    success: true,
                    op,
                    job: None,
                    jobs,
                    stdout: None,
                    stderr: None,
                    next_stdout_line: None,
                    next_stderr_line: None,
                    error: None,
                },
            );
        }
        "log" => {
            let Some(job_id) = body.job_id.as_deref() else {
                render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, "job_id is required for op=log".to_string()),
                );
                return;
            };
            let job = match registry.get_job(job_id).await {
                Ok(job) => job,
                Err(e) => {
                    render_shell_job(
                        res,
                        &audit,
                        StatusCode::BAD_REQUEST,
                        shell_job_error_response(op, e),
                    );
                    return;
                }
            };
            if let Err((status, e)) =
                assert_registry_runner_owner(&registry, auth.as_ref(), &job.client_id).await
            {
                render_shell_job(res, &audit, status, shell_job_error_response(op, e));
                return;
            }
            match registry
                .job_log(
                    job_id,
                    body.since_stdout_line,
                    body.since_stderr_line,
                    body.tail_lines,
                )
                .await
            {
                Ok((job, stdout, stderr, next_stdout_line, next_stderr_line)) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::OK,
                    ShellJobOpResponse {
                        success: true,
                        op,
                        job: Some(job),
                        jobs: Vec::new(),
                        stdout,
                        stderr,
                        next_stdout_line: Some(next_stdout_line),
                        next_stderr_line: Some(next_stderr_line),
                        error: None,
                    },
                ),
                Err(e) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, e),
                ),
            }
        }
        "stop" => {
            let Some(job_id) = body.job_id.as_deref() else {
                render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, "job_id is required for op=stop".to_string()),
                );
                return;
            };
            let job = match registry.get_job(job_id).await {
                Ok(job) => job,
                Err(e) => {
                    render_shell_job(
                        res,
                        &audit,
                        StatusCode::BAD_REQUEST,
                        shell_job_error_response(op, e),
                    );
                    return;
                }
            };
            if let Err((status, e)) =
                assert_registry_runner_owner(&registry, auth.as_ref(), &job.client_id).await
            {
                render_shell_job(res, &audit, status, shell_job_error_response(op, e));
                return;
            }
            let requested_by = requested_by_from_auth(auth.as_ref());
            match registry.stop_job(job_id, requested_by).await {
                Ok(job) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::OK,
                    ShellJobOpResponse {
                        success: true,
                        op,
                        job: Some(job),
                        jobs: Vec::new(),
                        stdout: None,
                        stderr: None,
                        next_stdout_line: None,
                        next_stderr_line: None,
                        error: None,
                    },
                ),
                Err(e) => render_shell_job(
                    res,
                    &audit,
                    StatusCode::BAD_REQUEST,
                    shell_job_error_response(op, e),
                ),
            }
        }
        _ => render_shell_job(
            res,
            &audit,
            StatusCode::BAD_REQUEST,
            shell_job_error_response(
                op,
                "op must be one of start, status, log, stop, list".to_string(),
            ),
        ),
    }
}

#[handler]
pub async fn shell_job_status(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(
        req,
        depot,
        "/api/shell/jobs/status",
        "getShellClientJobStatus",
    );
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_job_status(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            shell_job_status_error_response("Runner registry not configured".to_string()),
        );
        return;
    };
    let body: RunnerJobStatusRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_job_status(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                shell_job_status_error_response(format!("Invalid JSON: {}", e)),
            );
            return;
        }
    };
    match authorize_job_access(
        &registry,
        auth.as_ref(),
        &body.job_id,
        body.client_id.as_deref(),
    )
    .await
    {
        Ok(job) => render_shell_job_status(
            res,
            &audit,
            StatusCode::OK,
            shell_job_status_response_from_job(job),
        ),
        Err((status, e)) => {
            render_shell_job_status(res, &audit, status, shell_job_status_error_response(e))
        }
    }
}

#[handler]
pub async fn shell_job_log(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/jobs/log", "getShellClientJobLog");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_job_log(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            shell_job_log_error_response("Runner registry not configured".to_string()),
        );
        return;
    };
    let body: RunnerJobLogRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_job_log(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                shell_job_log_error_response(format!("Invalid JSON: {}", e)),
            );
            return;
        }
    };
    let job = match authorize_job_access(
        &registry,
        auth.as_ref(),
        &body.job_id,
        body.client_id.as_deref(),
    )
    .await
    {
        Ok(job) => job,
        Err((status, e)) => {
            render_shell_job_log(res, &audit, status, shell_job_log_error_response(e));
            return;
        }
    };
    match registry
        .job_log(
            &body.job_id,
            body.since_stdout_line,
            body.since_stderr_line,
            body.tail_lines,
        )
        .await
    {
        Ok((job, stdout_tail, stderr_tail, next_stdout_line, next_stderr_line)) => {
            render_shell_job_log(
                res,
                &audit,
                StatusCode::OK,
                RunnerJobLogResponse {
                    success: true,
                    job_id: Some(job.job_id.clone()),
                    client_id: Some(job.client_id.clone()),
                    stdout_tail,
                    stderr_tail,
                    next_stdout_line: Some(next_stdout_line),
                    next_stderr_line: Some(next_stderr_line),
                    job: Some(job),
                    error: None,
                },
            );
        }
        Err(e) => render_shell_job_log(
            res,
            &audit,
            StatusCode::BAD_REQUEST,
            shell_job_log_error_response(e),
        ),
    }
    let _ = job;
}

#[handler]
pub async fn shell_job_stop(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/jobs/stop", "stopShellClientJob");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_job_stop_response(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            shell_job_stop_error_response("Runner registry not configured".to_string()),
        );
        return;
    };
    let body: RunnerJobStopRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_job_stop_response(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                shell_job_stop_error_response(format!("Invalid JSON: {}", e)),
            );
            return;
        }
    };
    if let Err((status, e)) = authorize_job_access(
        &registry,
        auth.as_ref(),
        &body.job_id,
        body.client_id.as_deref(),
    )
    .await
    {
        render_shell_job_stop_response(res, &audit, status, shell_job_stop_error_response(e));
        return;
    }
    let requested_by = requested_by_from_auth(auth.as_ref());
    match registry.stop_job(&body.job_id, requested_by).await {
        Ok(job) => render_shell_job_stop_response(
            res,
            &audit,
            StatusCode::OK,
            RunnerJobStopResponse {
                success: true,
                job_id: Some(job.job_id.clone()),
                status: Some(job.status.clone()),
                job: Some(job),
                error: None,
            },
        ),
        Err(e) => render_shell_job_stop_response(
            res,
            &audit,
            StatusCode::BAD_REQUEST,
            shell_job_stop_error_response(e),
        ),
    }
}

#[handler]
pub async fn shell_jobs_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let audit = ActionAudit::start(req, depot, "/api/shell/jobs/list", "listShellClientJobs");
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let Some(registry) = get_registry(depot) else {
        render_shell_jobs_list(
            res,
            &audit,
            StatusCode::INTERNAL_SERVER_ERROR,
            shell_jobs_list_error_response(
                String::new(),
                "Runner registry not configured".to_string(),
            ),
        );
        return;
    };
    let body: RunnerJobsListRequest = match req.parse_json().await {
        Ok(body) => body,
        Err(e) => {
            render_shell_jobs_list(
                res,
                &audit,
                StatusCode::BAD_REQUEST,
                shell_jobs_list_error_response(String::new(), format!("Invalid JSON: {}", e)),
            );
            return;
        }
    };
    let client_id = body.client_id.clone();
    if let Err((status, e)) =
        assert_registry_runner_owner(&registry, auth.as_ref(), &client_id).await
    {
        render_shell_jobs_list(
            res,
            &audit,
            status,
            shell_jobs_list_error_response(client_id, e),
        );
        return;
    }
    match registry
        .list_jobs_for_runner(
            &client_id,
            body.status.as_deref(),
            Some(body.limit.unwrap_or(20).clamp(1, 100)),
        )
        .await
    {
        Ok(jobs) => render_shell_jobs_list(
            res,
            &audit,
            StatusCode::OK,
            RunnerJobsListResponse {
                success: true,
                client_id,
                jobs,
                error: None,
            },
        ),
        Err(e) => render_shell_jobs_list(
            res,
            &audit,
            StatusCode::BAD_REQUEST,
            shell_jobs_list_error_response(client_id, e),
        ),
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
