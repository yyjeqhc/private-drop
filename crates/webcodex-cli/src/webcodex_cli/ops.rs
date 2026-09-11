use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use webcodex_admin::ServerHttpOptions;

use super::{call_runtime_tool_status, http_post_json_status, resolve_user_api_token};

const DEFAULT_EXPECTED_TOOL_COUNT: u64 = 66;
pub(crate) const DEFAULT_RUNNER_REQUEST_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpsCommonOptions {
    pub(crate) server_url: String,
    pub(crate) server_http: ServerHttpOptions,
    pub(crate) env_file: Option<PathBuf>,
    pub(crate) token_file: Option<PathBuf>,
    pub(crate) token: Option<String>,
    pub(crate) json: bool,
    pub(crate) strict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpsSmokePreflightOptions {
    pub(crate) common: OpsCommonOptions,
    pub(crate) project: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpsWindowsOptions {
    pub(crate) common: OpsCommonOptions,
    pub(crate) project: String,
    pub(crate) limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpsRunnerOptions {
    pub(crate) common: OpsCommonOptions,
    pub(crate) client_id: String,
    pub(crate) request_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpsCommand {
    Status(OpsCommonOptions),
    Runners(OpsCommonOptions),
    Runner(OpsRunnerOptions),
    Projects(OpsCommonOptions),
    Windows(OpsWindowsOptions),
    SmokePreflight(OpsSmokePreflightOptions),
}

impl OpsCommand {
    pub(crate) fn strict(&self) -> bool {
        match self {
            OpsCommand::Status(opts) | OpsCommand::Runners(opts) | OpsCommand::Projects(opts) => {
                opts.strict
            }
            OpsCommand::Runner(opts) => opts.common.strict,
            OpsCommand::Windows(opts) => opts.common.strict,
            OpsCommand::SmokePreflight(opts) => opts.common.strict,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpsCommandOutput {
    pub(crate) stdout: String,
    pub(crate) status: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpsVerdict {
    pub(crate) status: &'static str,
    pub(crate) blocking: bool,
    pub(crate) blocking_reasons: Vec<String>,
    pub(crate) warning_reasons: Vec<String>,
    pub(crate) suggested_next_actions: Vec<String>,
}

impl OpsVerdict {
    fn pass() -> Self {
        Self {
            status: "pass",
            blocking: false,
            blocking_reasons: Vec::new(),
            warning_reasons: Vec::new(),
            suggested_next_actions: Vec::new(),
        }
    }

    fn fail_reason(&mut self, reason: impl Into<String>, action: impl Into<String>) {
        self.status = "fail";
        self.blocking = true;
        push_unique_string(&mut self.blocking_reasons, reason.into());
        push_unique_string(&mut self.suggested_next_actions, action.into());
    }

    fn warn_reason(&mut self, reason: impl Into<String>, action: impl Into<String>) {
        if self.status != "fail" {
            self.status = "warn";
        }
        push_unique_string(&mut self.warning_reasons, reason.into());
        push_unique_string(&mut self.suggested_next_actions, action.into());
    }

    fn finish(mut self) -> Self {
        if self.suggested_next_actions.is_empty() {
            self.suggested_next_actions
                .push("no action needed".to_string());
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpsReport {
    pub(crate) verdict: OpsVerdict,
    pub(crate) summary: Value,
    pub(crate) source: Value,
}

pub(crate) async fn run_ops_command(command: OpsCommand) -> Result<OpsCommandOutput, String> {
    match command {
        OpsCommand::Status(opts) => {
            let token = resolve_ops_token(&opts)?;
            let report = match fetch_ops_json_output(
                &opts.server_url,
                &opts.server_http,
                "/api/runtime/status",
                token.as_deref(),
                json!({}),
            )
            .await
            {
                Ok(runtime) => ops_status_report(&opts.server_url, &Some(runtime)),
                Err(failure) => ops_http_failure_report(
                    &opts.server_url,
                    "runtime_status",
                    failure,
                    token.is_some(),
                ),
            };
            render_ops_command_output(report, opts.json, render_ops_status)
        }
        OpsCommand::Runners(opts) => {
            let token = resolve_ops_token(&opts)?;
            let report = match fetch_ops_json_output(
                &opts.server_url,
                &opts.server_http,
                "/api/runtime/status",
                token.as_deref(),
                json!({}),
            )
            .await
            {
                Ok(runtime) => ops_runners_report(&opts.server_url, &Some(runtime)),
                Err(failure) => ops_http_failure_report(
                    &opts.server_url,
                    "runtime_status",
                    failure,
                    token.is_some(),
                ),
            };
            render_ops_command_output(report, opts.json, render_ops_runners)
        }
        OpsCommand::Runner(opts) => {
            let token = resolve_ops_token(&opts.common)?;
            let report = match fetch_ops_json_output_bounded(
                &opts.common.server_url,
                &opts.common.server_http,
                "/api/runtime/status",
                token.as_deref(),
                json!({"client_id": opts.client_id}),
                opts.request_timeout_ms,
            )
            .await
            {
                Ok(runtime) => {
                    ops_runner_report(&opts.common.server_url, &opts.client_id, &Some(runtime))
                }
                Err(failure) => ops_http_failure_report(
                    &opts.common.server_url,
                    "runtime_status",
                    failure,
                    token.is_some(),
                ),
            };
            render_ops_command_output(report, opts.common.json, render_ops_runner)
        }
        OpsCommand::Projects(opts) => {
            let token = resolve_ops_token(&opts)?;
            let report =
                match fetch_projects(&opts.server_url, &opts.server_http, token.as_deref()).await {
                    Ok(projects) => ops_projects_report(&opts.server_url, Some(&projects)),
                    Err(failure) => ops_http_failure_report(
                        &opts.server_url,
                        "list_projects",
                        failure,
                        token.is_some(),
                    ),
                };
            render_ops_command_output(report, opts.json, render_ops_projects)
        }
        OpsCommand::Windows(opts) => {
            let token = resolve_ops_token(&opts.common)?;
            let report = match fetch_ops_json_output(
                &opts.common.server_url,
                &opts.common.server_http,
                "/api/runtime-console/windows",
                token.as_deref(),
                json!({"project": opts.project, "limit": opts.limit}),
            )
            .await
            {
                Ok(windows) => ops_windows_report(&opts.common.server_url, windows),
                Err(failure) => ops_http_failure_report(
                    &opts.common.server_url,
                    "runtime_console_windows",
                    failure,
                    token.is_some(),
                ),
            };
            render_ops_command_output(report, opts.common.json, render_ops_windows)
        }
        OpsCommand::SmokePreflight(opts) => {
            let token = resolve_ops_token(&opts.common)?;
            let runtime_status = match fetch_ops_json_output(
                &opts.common.server_url,
                &opts.common.server_http,
                "/api/runtime/status",
                token.as_deref(),
                json!({}),
            )
            .await
            {
                Ok(runtime) => runtime,
                Err(failure) => {
                    let report = ops_http_failure_report(
                        &opts.common.server_url,
                        "runtime_status",
                        failure,
                        token.is_some(),
                    );
                    return render_ops_command_output(
                        report,
                        opts.common.json,
                        render_ops_smoke_preflight,
                    );
                }
            };
            let projects = match fetch_projects(
                &opts.common.server_url,
                &opts.common.server_http,
                token.as_deref(),
            )
            .await
            {
                Ok(projects) => projects,
                Err(failure) => {
                    let report = ops_http_failure_report(
                        &opts.common.server_url,
                        "list_projects",
                        failure,
                        token.is_some(),
                    );
                    return render_ops_command_output(
                        report,
                        opts.common.json,
                        render_ops_smoke_preflight,
                    );
                }
            };
            let target_state =
                smoke_preflight_target_ready(find_project(Some(&projects), &opts.project));
            let (show_changes, hygiene) = if target_state.ready {
                let show_changes = match call_runtime_tool(
                    &opts.common.server_url,
                    &opts.common.server_http,
                    token.as_deref(),
                    "show_changes",
                    json!({"project": opts.project, "include_diff": false}),
                )
                .await
                {
                    Ok(value) => value,
                    Err(failure) => {
                        let report = ops_http_failure_report(
                            &opts.common.server_url,
                            "show_changes",
                            failure,
                            token.is_some(),
                        );
                        return render_ops_command_output(
                            report,
                            opts.common.json,
                            render_ops_smoke_preflight,
                        );
                    }
                };
                let hygiene = match call_runtime_tool(
                    &opts.common.server_url,
                    &opts.common.server_http,
                    token.as_deref(),
                    "workspace_hygiene_check",
                    json!({"project": opts.project}),
                )
                .await
                {
                    Ok(value) => value,
                    Err(failure) => {
                        let report = ops_http_failure_report(
                            &opts.common.server_url,
                            "workspace_hygiene_check",
                            failure,
                            token.is_some(),
                        );
                        return render_ops_command_output(
                            report,
                            opts.common.json,
                            render_ops_smoke_preflight,
                        );
                    }
                };
                (show_changes, hygiene)
            } else {
                (None, None)
            };
            let report = ops_smoke_preflight_report(
                &opts.common.server_url,
                &opts.project,
                Some(&runtime_status),
                Some(&projects),
                show_changes.as_ref(),
                hygiene.as_ref(),
            );
            render_ops_command_output(report, opts.common.json, render_ops_smoke_preflight)
        }
    }
}

pub(crate) fn ops_exit_code(strict: bool, status: &str) -> i32 {
    if strict && status == "fail" {
        2
    } else {
        0
    }
}

fn render_ops_command_output(
    report: OpsReport,
    json_output: bool,
    render: fn(&OpsReport, bool) -> Result<String, String>,
) -> Result<OpsCommandOutput, String> {
    let status = report.verdict.status;
    let stdout = render(&report, json_output)?;
    Ok(OpsCommandOutput { stdout, status })
}

fn resolve_ops_token(opts: &OpsCommonOptions) -> Result<Option<String>, String> {
    resolve_user_api_token(&opts.token, &opts.token_file, &opts.env_file)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpsHttpFailureKind {
    Unauthorized,
    Forbidden,
    Unreachable,
    RequestTimeout,
    ServerError,
    NonJson,
    Malformed,
    Other,
}

impl OpsHttpFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            OpsHttpFailureKind::Unauthorized => "unauthorized",
            OpsHttpFailureKind::Forbidden => "forbidden",
            OpsHttpFailureKind::Unreachable => "runtime_unreachable",
            OpsHttpFailureKind::RequestTimeout => "request_timeout",
            OpsHttpFailureKind::ServerError => "server_error",
            OpsHttpFailureKind::NonJson => "non_json_response",
            OpsHttpFailureKind::Malformed => "malformed_response",
            OpsHttpFailureKind::Other => "http_error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpsHttpFailure {
    kind: OpsHttpFailureKind,
    status_code: Option<u16>,
    content_type: Option<String>,
}

impl OpsHttpFailure {
    fn from_response(status: u16, content_type: String, parsed_json: bool) -> Self {
        let kind = if status == 401 {
            OpsHttpFailureKind::Unauthorized
        } else if status == 403 {
            OpsHttpFailureKind::Forbidden
        } else if (500..600).contains(&status) {
            OpsHttpFailureKind::ServerError
        } else if !content_type_is_json(&content_type) {
            OpsHttpFailureKind::NonJson
        } else if !parsed_json {
            OpsHttpFailureKind::Malformed
        } else {
            OpsHttpFailureKind::Other
        };
        Self {
            kind,
            status_code: Some(status),
            content_type: Some(content_type),
        }
    }

    fn from_transport_error(error: String) -> Self {
        let kind = if error.starts_with("request failed:") {
            OpsHttpFailureKind::Unreachable
        } else if error.starts_with("failed to read response:") {
            OpsHttpFailureKind::Malformed
        } else {
            OpsHttpFailureKind::Other
        };
        Self {
            kind,
            status_code: None,
            content_type: None,
        }
    }

    fn request_timeout() -> Self {
        Self {
            kind: OpsHttpFailureKind::RequestTimeout,
            status_code: None,
            content_type: None,
        }
    }
}

async fn fetch_ops_json_output(
    server_url: &str,
    server_http: &ServerHttpOptions,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> Result<Value, OpsHttpFailure> {
    match http_post_json_status(server_url, server_http, path, token, body).await {
        Ok((status, _content_type, Some(value))) if (200..300).contains(&status) => {
            Ok(output_payload(value))
        }
        Ok((status, content_type, value)) => Err(OpsHttpFailure::from_response(
            status,
            content_type,
            value.is_some(),
        )),
        Err(e) => Err(OpsHttpFailure::from_transport_error(e)),
    }
}

async fn fetch_ops_json_output_bounded(
    server_url: &str,
    server_http: &ServerHttpOptions,
    path: &str,
    token: Option<&str>,
    body: Value,
    request_timeout_ms: u64,
) -> Result<Value, OpsHttpFailure> {
    match tokio::time::timeout(
        Duration::from_millis(request_timeout_ms),
        fetch_ops_json_output(server_url, server_http, path, token, body),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(OpsHttpFailure::request_timeout()),
    }
}

async fn fetch_projects(
    server_url: &str,
    server_http: &ServerHttpOptions,
    token: Option<&str>,
) -> Result<Value, OpsHttpFailure> {
    fetch_ops_json_output(
        server_url,
        server_http,
        "/api/projects/list",
        token,
        json!({}),
    )
    .await
}

fn ops_http_failure_report(
    server_url: &str,
    endpoint: &str,
    failure: OpsHttpFailure,
    token_present: bool,
) -> OpsReport {
    let mut verdict = OpsVerdict::pass();
    verdict.fail_reason(
        ops_http_failure_reason(&failure, token_present),
        ops_http_failure_action(failure.kind),
    );
    let http = ops_http_failure_json(endpoint, &failure);
    OpsReport {
        verdict: verdict.finish(),
        summary: json!({
            "runtime_reachable": !matches!(
                failure.kind,
                OpsHttpFailureKind::Unreachable | OpsHttpFailureKind::RequestTimeout
            ),
            "http": http.clone(),
        }),
        source: json!({
            "server_url": server_url,
            "runtime_commit": Value::Null,
            "tool": endpoint,
            "http": http,
        }),
    }
}

async fn call_runtime_tool(
    server_url: &str,
    server_http: &ServerHttpOptions,
    token: Option<&str>,
    tool: &str,
    params: Value,
) -> Result<Option<Value>, OpsHttpFailure> {
    match call_runtime_tool_status(server_url, server_http, token, tool, params).await {
        Ok((status, _content_type, Some(value))) if (200..300).contains(&status) => {
            Ok(Some(output_payload(value)))
        }
        Ok((status, content_type, value)) => Err(OpsHttpFailure::from_response(
            status,
            content_type,
            value.is_some(),
        )),
        Err(error) => Err(OpsHttpFailure::from_transport_error(error)),
    }
}

fn output_payload(value: Value) -> Value {
    value.get("output").cloned().unwrap_or(value)
}

fn ops_http_failure_reason(failure: &OpsHttpFailure, token_present: bool) -> &'static str {
    match failure.kind {
        OpsHttpFailureKind::Unauthorized if !token_present => "auth_required",
        OpsHttpFailureKind::Unauthorized => "unauthorized",
        OpsHttpFailureKind::Forbidden => "forbidden",
        OpsHttpFailureKind::Unreachable => "runtime_unreachable",
        OpsHttpFailureKind::RequestTimeout => "request_timeout",
        OpsHttpFailureKind::ServerError => "server_error",
        OpsHttpFailureKind::NonJson => "non_json_response",
        OpsHttpFailureKind::Malformed => "malformed_response",
        OpsHttpFailureKind::Other => "http_error",
    }
}

fn ops_http_failure_action(kind: OpsHttpFailureKind) -> &'static str {
    match kind {
        OpsHttpFailureKind::Unauthorized => {
            "provide a user token/PAT or bearer token accepted by the WebCodex server"
        }
        OpsHttpFailureKind::Forbidden => {
            "use a bearer token with the required runtime, project, or job scope"
        }
        OpsHttpFailureKind::Unreachable => "check --server-url, DNS, firewall, and server process",
        OpsHttpFailureKind::RequestTimeout => {
            "retry the exact Runner observation within its bounded request deadline"
        }
        OpsHttpFailureKind::ServerError => {
            "inspect WebCodex server logs for the failing ops request"
        }
        OpsHttpFailureKind::NonJson => {
            "check the reverse proxy and server route; expected a JSON response"
        }
        OpsHttpFailureKind::Malformed => {
            "check server logs and proxy output; the ops response was not valid JSON"
        }
        OpsHttpFailureKind::Other => "check HTTP status, content-type, and server logs",
    }
}

fn ops_http_failure_json(endpoint: &str, failure: &OpsHttpFailure) -> Value {
    json!({
        "endpoint": endpoint,
        "failure_kind": failure.kind.as_str(),
        "status": failure.status_code,
        "content_type": failure.content_type.clone(),
    })
}

fn content_type_is_json(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("application/json"))
}

pub(crate) fn ops_status_report(server_url: &str, runtime: &Option<Value>) -> OpsReport {
    let mut verdict = OpsVerdict::pass();
    let Some(runtime) = runtime.as_ref() else {
        verdict.fail_reason(
            "runtime_unreachable",
            "check --server-url, network reachability, and bearer token",
        );
        return OpsReport {
            verdict: verdict.finish(),
            summary: json!({"runtime_reachable": false}),
            source: source_json(server_url, None, "runtime_status"),
        };
    };
    let runtime_commit_value = runtime_commit(runtime);
    if runtime.get("service").and_then(Value::as_str).is_none() {
        verdict.fail_reason(
            "malformed_runtime_status",
            "rerun runtime_status directly and inspect server logs",
        );
    }

    let dirty = runtime.pointer("/build/git_dirty").and_then(Value::as_bool);
    if dirty != Some(false) {
        verdict.warn_reason(
            "server_build_dirty",
            "deploy a clean server build before release smoke",
        );
    }

    let tools_count = runtime.pointer("/tools/count").and_then(Value::as_u64);
    if tools_count != Some(DEFAULT_EXPECTED_TOOL_COUNT) {
        verdict.warn_reason(
            format!(
                "tools_count_unexpected:{}",
                tools_count
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            "confirm runtime tool registry count before deployment",
        );
    }

    let active_jobs = runtime
        .pointer("/jobs/active_count")
        .and_then(Value::as_u64);
    if active_jobs.unwrap_or(0) > 0 {
        verdict.warn_reason(
            format!("active_jobs:{}", active_jobs.unwrap_or(0)),
            "wait for active jobs to finish before smoke validation",
        );
    }

    let online = runner_count(runtime, "online_count", "online");
    let stale = runner_count(runtime, "stale_count", "stale");
    if online == 0 {
        verdict.fail_reason(
            "no_online_agents",
            "start a webcodex-runner and rerun ops status",
        );
    } else {
        if stale > 0 {
            verdict.warn_reason(
                format!("stale_agents:{}", stale),
                "inspect stale Runners with ops runners",
            );
        }
    }

    let effective_status = str_pointer(runtime, "/projects/effective/status");
    if effective_status.as_deref() != Some("ok") {
        verdict.fail_reason(
            format!(
                "projects_effective_status:{}",
                effective_status.unwrap_or_else(|| "unknown".to_string())
            ),
            "inspect Project registration and connected Runners",
        );
    }

    let summary = json!({
        "runtime_reachable": true,
        "server": {
            "version": runtime.get("version").cloned().unwrap_or(Value::Null),
            "commit": runtime_commit_value,
            "dirty": dirty,
        },
        "tools": {
            "count": tools_count,
        },
        "jobs": {
            "active_count": active_jobs,
        },
        "agents": {
            "online_count": online,
            "stale_count": stale,
            "clients": runner_clients(runtime),
        },
        "projects": {
            "effective": {
                "status": str_pointer(runtime, "/projects/effective/status"),
                "count": runtime.pointer("/projects/effective/count").and_then(Value::as_u64),
            },
        },
    });
    OpsReport {
        verdict: verdict.finish(),
        summary,
        source: source_json(server_url, runtime_commit_value, "runtime_status"),
    }
}

pub(crate) fn ops_runners_report(server_url: &str, runtime: &Option<Value>) -> OpsReport {
    let mut verdict = OpsVerdict::pass();
    let Some(runtime) = runtime.as_ref() else {
        verdict.fail_reason(
            "runtime_unreachable",
            "check --server-url, network reachability, and bearer token",
        );
        return OpsReport {
            verdict: verdict.finish(),
            summary: json!({"runtime_reachable": false, "agents": []}),
            source: source_json(server_url, None, "runtime_status"),
        };
    };
    let clients = runner_clients(runtime);
    let online = runner_count(runtime, "online_count", "online");
    let stale = runner_count(runtime, "stale_count", "stale");
    let active_jobs = clients
        .iter()
        .filter_map(|client| client.get("active_jobs").and_then(Value::as_u64))
        .sum::<u64>();
    if online == 0 {
        verdict.fail_reason(
            "no_online_agents",
            "start a webcodex-runner and rerun ops runners",
        );
    }
    if stale > 0 {
        verdict.warn_reason(
            format!("stale_agents:{}", stale),
            "inspect stale Runners and transport health",
        );
    }
    if active_jobs > 0 {
        verdict.warn_reason(
            format!("active_agent_jobs:{}", active_jobs),
            "wait for active Runner jobs to finish before smoke validation",
        );
    }
    let summary = json!({
        "runtime_reachable": true,
        "online_count": online,
        "stale_count": stale,
        "active_jobs": active_jobs,
        "agents": clients,
    });
    OpsReport {
        verdict: verdict.finish(),
        summary,
        source: source_json(server_url, runtime_commit(runtime), "runtime_status"),
    }
}

pub(crate) fn ops_runner_report(
    server_url: &str,
    expected_client_id: &str,
    runtime: &Option<Value>,
) -> OpsReport {
    let mut verdict = OpsVerdict::pass();
    let Some(runtime) = runtime.as_ref() else {
        verdict.fail_reason(
            "runtime_unreachable",
            "check --server-url, network reachability, and bearer token",
        );
        return OpsReport {
            verdict: verdict.finish(),
            summary: json!({
                "runtime_reachable": false,
                "client_id": expected_client_id,
            }),
            source: source_json(server_url, None, "runtime_status"),
        };
    };
    let focus = runtime.get("focus").filter(|value| value.is_object());
    let Some(focus) = focus else {
        verdict.fail_reason(
            "runner_not_observed",
            "verify the exact Runner client_id is registered and caller-visible",
        );
        return OpsReport {
            verdict: verdict.finish(),
            summary: json!({
                "runtime_reachable": true,
                "client_id": expected_client_id,
                "connected": false,
            }),
            source: source_json(server_url, runtime_commit(runtime), "runtime_status"),
        };
    };
    if focus.get("client_id").and_then(Value::as_str) != Some(expected_client_id) {
        verdict.fail_reason(
            "unexpected_runner_identity",
            "retry the exact client_id query and inspect Server runtime registration",
        );
    }
    let summary = json!({
        "runtime_reachable": true,
        "client_id": focus.get("client_id").cloned().unwrap_or(Value::Null),
        "connected": focus.get("connected").cloned().unwrap_or(Value::Null),
        "status": focus.get("status").cloned().unwrap_or(Value::Null),
        "agent_instance_id": focus.get("agent_instance_id").cloned().unwrap_or(Value::Null),
        "build": focus.get("build").cloned().unwrap_or(Value::Null),
        "compatibility_status": focus.get("compatibility_status").cloned().unwrap_or(Value::Null),
        "source_alignment": focus.get("source_alignment").cloned().unwrap_or(Value::Null),
    });
    OpsReport {
        verdict: verdict.finish(),
        summary,
        source: source_json(server_url, runtime_commit(runtime), "runtime_status"),
    }
}

pub(crate) fn ops_windows_report(server_url: &str, windows: Value) -> OpsReport {
    OpsReport {
        verdict: OpsVerdict::pass().finish(),
        summary: output_payload(windows),
        source: source_json(server_url, None, "runtime_console_windows"),
    }
}

pub(crate) fn ops_projects_report(server_url: &str, projects: Option<&Value>) -> OpsReport {
    let mut verdict = OpsVerdict::pass();
    let projects_list = project_entries(projects);
    let total = projects_list.len();
    let online = projects_list
        .iter()
        .filter(|project| project_online(project))
        .count();
    let disconnected = projects_list
        .iter()
        .filter(|project| !project_connected(project))
        .count();
    let stale = projects_list
        .iter()
        .filter(|project| project_runner_status(project).as_deref() == Some("stale"))
        .count();
    let recommended = projects_list
        .iter()
        .filter(|project| project_bool(project, "/capabilities/recommended_for_smoke"))
        .count();
    let recommended_smoke_offline = projects_list
        .iter()
        .filter(|project| {
            project_bool(project, "/capabilities/recommended_for_smoke")
                && (!project_online(project))
        })
        .count();
    if total == 0 {
        verdict.fail_reason(
            "no_projects",
            "register a Runner Project and rerun ops projects",
        );
    } else if online == 0 {
        verdict.fail_reason(
            "no_online_projects",
            "start the owning Runner for at least one Project",
        );
    }
    if total > 0 && recommended == 0 {
        verdict.warn_reason(
            "no_recommended_smoke_project",
            "choose a git-backed safe smoke project or register one",
        );
    }
    if disconnected > 0 {
        verdict.warn_reason(
            format!("disconnected_projects:{}", disconnected),
            "inspect disconnected Projects and restart their owning Runners if needed",
        );
    }
    if stale > 0 {
        verdict.warn_reason(
            format!("stale_projects:{}", stale),
            "inspect stale Projects with ops runners",
        );
    }
    if recommended_smoke_offline > 0 {
        verdict.warn_reason(
            format!("recommended_smoke_offline:{}", recommended_smoke_offline),
            "start the recommended smoke Project Runner or select another connected git Project",
        );
    }
    let summary = json!({
        "count": total,
        "online_count": online,
        "disconnected_count": disconnected,
        "stale_count": stale,
        "recommended_for_smoke_count": recommended,
        "recommended_smoke_offline_count": recommended_smoke_offline,
        "projects": compact_projects(&projects_list),
    });
    OpsReport {
        verdict: verdict.finish(),
        summary,
        source: source_json(server_url, None, "list_projects"),
    }
}

pub(crate) fn ops_smoke_preflight_report(
    server_url: &str,
    project_id: &str,
    runtime: Option<&Value>,
    projects: Option<&Value>,
    show_changes: Option<&Value>,
    hygiene: Option<&Value>,
) -> OpsReport {
    let mut verdict = OpsVerdict::pass();
    let projects_list = project_entries(projects);
    let project = projects_list
        .iter()
        .find(|project| project.get("id").and_then(Value::as_str) == Some(project_id));
    let target_state = smoke_preflight_target_ready(project);
    let active_jobs = runtime
        .and_then(|value| value.pointer("/jobs/active_count"))
        .and_then(Value::as_u64);
    if active_jobs.unwrap_or(0) > 0 {
        verdict.fail_reason(
            format!("active_jobs:{}", active_jobs.unwrap_or(0)),
            "wait for active jobs to finish before deploy smoke preflight",
        );
    }

    match project {
        Some(project) => {
            if !target_state.connected {
                verdict.fail_reason(
                    "project_disconnected",
                    "start the owning Runner and wait for the Project to reconnect",
                );
            }
            if !target_state.runner_online {
                verdict.fail_reason(
                    "project_offline",
                    "start the owning Runner and wait for the Project status to become online",
                );
            }
            if !target_state.git_available {
                verdict.fail_reason(
                    "project_git_unavailable",
                    "select a git-backed project for deploy smoke preflight",
                );
            }
            if target_state.ready {
                if !project_bool(project, "/capabilities/recommended_for_smoke") {
                    verdict.warn_reason(
                        "project_not_recommended_for_smoke",
                        "prefer a project with recommended_for_smoke=true",
                    );
                }
                if !project_bool(project, "/capabilities/safe_smoke_project") {
                    verdict.warn_reason(
                        "project_not_safe_smoke_project",
                        "prefer a project with safe_smoke_project=true",
                    );
                }
            }
        }
        None => verdict.fail_reason(
            "project_missing",
            "run ops projects and pass an existing runtime project id",
        ),
    }

    let show_clean = show_changes
        .and_then(|value| value.get("clean"))
        .and_then(Value::as_bool);
    let show_verdict = verdict_status(show_changes);
    if target_state.ready {
        if show_clean != Some(true) {
            verdict.fail_reason(
                "workspace_dirty",
                "review show_changes output and clean or commit workspace changes",
            );
        }
        if show_verdict.as_deref() == Some("fail") || show_verdict.is_none() {
            verdict.fail_reason(
                format!(
                    "show_changes_verdict:{}",
                    show_verdict.as_deref().unwrap_or("unknown")
                ),
                "rerun show_changes directly and inspect the failure",
            );
        }
    }

    let hygiene_clean = hygiene
        .and_then(|value| value.get("clean"))
        .and_then(Value::as_bool);
    let hygiene_verdict = verdict_status(hygiene);
    if target_state.ready {
        if hygiene_clean != Some(true) {
            verdict.fail_reason(
                "hygiene_not_clean",
                "review workspace_hygiene_check findings before deploy smoke",
            );
        }
        if hygiene_verdict.as_deref() == Some("fail") || hygiene_verdict.is_none() {
            verdict.fail_reason(
                format!(
                    "hygiene_verdict:{}",
                    hygiene_verdict.as_deref().unwrap_or("unknown")
                ),
                "rerun workspace_hygiene_check directly and inspect the failure",
            );
        } else if hygiene_verdict.as_deref() == Some("warn") {
            verdict.warn_reason(
                "hygiene_warnings_present",
                "review low-severity hygiene findings before deployment",
            );
        }
    }

    let project_summary = project.map(compact_project).unwrap_or_else(|| {
        json!({
            "id": project_id,
            "exists": false,
        })
    });
    let summary = json!({
        "project": project_summary,
        "workspace": {
            "clean": show_clean,
            "show_changes_verdict_status": show_verdict,
        },
        "hygiene": {
            "clean": hygiene_clean,
            "verdict_status": hygiene_verdict,
        },
        "jobs": {
            "active_count": active_jobs,
        },
    });
    OpsReport {
        verdict: verdict.finish(),
        summary,
        source: smoke_preflight_source_json(
            server_url,
            runtime.and_then(runtime_commit),
            show_changes.is_some(),
            hygiene.is_some(),
        ),
    }
}

pub(crate) fn render_ops_status(report: &OpsReport, json_output: bool) -> Result<String, String> {
    if json_output {
        return render_ops_json(report);
    }
    let mut out = render_overall_header(report);
    out.push_str(&render_http_failure(report));
    let server = &report.summary["server"];
    out.push_str("Server:\n");
    out.push_str(&format!(
        "  version: {}\n",
        display_value(&server["version"])
    ));
    out.push_str(&format!("  commit: {}\n", display_value(&server["commit"])));
    out.push_str(&format!("  dirty: {}\n", display_value(&server["dirty"])));
    out.push_str("Tools:\n");
    out.push_str(&format!(
        "  count: {}\n",
        display_value(&report.summary["tools"]["count"])
    ));
    out.push_str("Jobs:\n");
    out.push_str(&format!(
        "  active_count: {}\n",
        display_value(&report.summary["jobs"]["active_count"])
    ));
    out.push_str("Runners:\n");
    out.push_str(&format!(
        "  online/stale: {}/{}\n",
        display_value(&report.summary["agents"]["online_count"]),
        display_value(&report.summary["agents"]["stale_count"])
    ));
    for client in report.summary["agents"]["clients"]
        .as_array()
        .into_iter()
        .flatten()
    {
        out.push_str(&format!(
            "  - client_id={} status={} transport={} projects_count={} active_jobs={} last_seen_age_secs={}\n",
            display_value(&client["client_id"]),
            display_value(&client["status"]),
            display_value(&client["transport"]),
            display_value(&client["projects_count"]),
            display_value(&client["active_jobs"]),
            display_value(&client["last_seen_age_secs"])
        ));
    }
    out.push_str("Projects:\n");
    out.push_str(&format!(
        "  effective.status: {}\n",
        display_value(&report.summary["projects"]["effective"]["status"])
    ));
    out.push_str(&format!(
        "  effective.count: {}\n",
        display_value(&report.summary["projects"]["effective"]["count"])
    ));
    out.push_str(&render_reasons(report));
    Ok(out)
}

pub(crate) fn render_ops_runners(report: &OpsReport, json_output: bool) -> Result<String, String> {
    if json_output {
        return render_ops_json(report);
    }
    let mut out = render_overall_header(report);
    out.push_str(&render_http_failure(report));
    out.push_str("Runners:\n");
    out.push_str("  client_id status transport projects_count active_jobs pending_requests last_seen_age_secs\n");
    for client in report.summary["agents"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {} {} {} {} {} {} {}\n",
            display_value(&client["client_id"]),
            display_value(&client["status"]),
            display_value(&client["transport"]),
            display_value(&client["projects_count"]),
            display_value(&client["active_jobs"]),
            display_value(&client["pending_requests"]),
            display_value(&client["last_seen_age_secs"])
        ));
    }
    out.push_str(&render_reasons(report));
    Ok(out)
}

pub(crate) fn render_ops_runner(report: &OpsReport, json_output: bool) -> Result<String, String> {
    if json_output {
        return render_ops_json(report);
    }
    let mut out = render_overall_header(report);
    out.push_str(&render_http_failure(report));
    out.push_str("Runner:\n");
    for field in [
        "client_id",
        "connected",
        "status",
        "agent_instance_id",
        "compatibility_status",
    ] {
        out.push_str(&format!(
            "  {field}: {}\n",
            display_value(&report.summary[field])
        ));
    }
    out.push_str("Build:\n");
    for field in ["version", "git_commit", "git_dirty", "built_at"] {
        out.push_str(&format!(
            "  {field}: {}\n",
            display_value(&report.summary["build"][field])
        ));
    }
    out.push_str(&render_reasons(report));
    Ok(out)
}

pub(crate) fn render_ops_projects(report: &OpsReport, json_output: bool) -> Result<String, String> {
    if json_output {
        return render_ops_json(report);
    }
    let mut out = render_overall_header(report);
    out.push_str(&render_http_failure(report));
    out.push_str("Projects:\n");
    out.push_str("  id client_id runner_status connected git_available recommended_for_smoke safe_smoke_project allow_patch path\n");
    for project in report.summary["projects"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {} {} {} {} {} {} {} {} {}\n",
            display_value(&project["id"]),
            display_value(&project["client_id"]),
            display_value(&project["agent_status"]),
            display_value(&project["connected"]),
            display_value(&project["git_available"]),
            display_value(&project["recommended_for_smoke"]),
            display_value(&project["safe_smoke_project"]),
            display_value(&project["allow_patch"]),
            display_value(&project["path"])
        ));
    }
    out.push_str(&render_reasons(report));
    Ok(out)
}

pub(crate) fn render_ops_windows(report: &OpsReport, json_output: bool) -> Result<String, String> {
    if json_output {
        return render_ops_json(report);
    }
    let mut out = render_overall_header(report);
    out.push_str(&render_http_failure(report));
    out.push_str("Windows:\n");
    out.push_str(&format!(
        "  returned: {}\n  total: {}\n  truncated: {}\n",
        display_value(&report.summary["returned"]),
        display_value(&report.summary["total"]),
        display_value(&report.summary["truncated"]),
    ));
    for window in report.summary["windows"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  - last_seen_at_ms={} last_meaningful_activity_at_ms={} active_count={}\n",
            display_value(&window["last_seen_at_ms"]),
            display_value(&window["last_meaningful_activity_at_ms"]),
            display_value(&window["active_count"]),
        ));
    }
    out.push_str(&render_reasons(report));
    Ok(out)
}

pub(crate) fn render_ops_smoke_preflight(
    report: &OpsReport,
    json_output: bool,
) -> Result<String, String> {
    if json_output {
        return render_ops_json(report);
    }
    let mut out = render_overall_header(report);
    out.push_str(&render_http_failure(report));
    out.push_str("Project:\n");
    out.push_str(&format!(
        "  id: {}\n",
        display_value(&report.summary["project"]["id"])
    ));
    out.push_str(&format!(
        "  online: {}\n",
        display_value(&report.summary["project"]["connected"])
    ));
    out.push_str(&format!(
        "  git_available: {}\n",
        display_value(&report.summary["project"]["git_available"])
    ));
    out.push_str(&format!(
        "  recommended_for_smoke: {}\n",
        display_value(&report.summary["project"]["recommended_for_smoke"])
    ));
    out.push_str("Workspace:\n");
    out.push_str(&format!(
        "  clean: {}\n",
        display_value(&report.summary["workspace"]["clean"])
    ));
    out.push_str(&format!(
        "  show_changes.verdict.status: {}\n",
        display_value(&report.summary["workspace"]["show_changes_verdict_status"])
    ));
    out.push_str("Hygiene:\n");
    out.push_str(&format!(
        "  clean: {}\n",
        display_value(&report.summary["hygiene"]["clean"])
    ));
    out.push_str(&format!(
        "  verdict.status: {}\n",
        display_value(&report.summary["hygiene"]["verdict_status"])
    ));
    out.push_str("Jobs:\n");
    out.push_str(&format!(
        "  active_count: {}\n",
        display_value(&report.summary["jobs"]["active_count"])
    ));
    out.push_str(&render_reasons(report));
    Ok(out)
}

fn render_ops_json(report: &OpsReport) -> Result<String, String> {
    serde_json::to_string_pretty(&json!({
        "status": report.verdict.status,
        "blocking": report.verdict.blocking,
        "blocking_reasons": report.verdict.blocking_reasons,
        "warning_reasons": report.verdict.warning_reasons,
        "suggested_next_actions": report.verdict.suggested_next_actions,
        "summary": report.summary,
        "source": report.source,
    }))
    .map_err(|e| e.to_string())
}

fn render_overall_header(report: &OpsReport) -> String {
    format!("Overall: {}\n", report.verdict.status.to_ascii_uppercase())
}

fn render_http_failure(report: &OpsReport) -> String {
    let http = &report.summary["http"];
    if !http.is_object() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("HTTP:\n");
    out.push_str(&format!(
        "  endpoint: {}\n",
        display_value(&http["endpoint"])
    ));
    out.push_str(&format!("  status: {}\n", display_value(&http["status"])));
    out.push_str(&format!(
        "  content_type: {}\n",
        display_value(&http["content_type"])
    ));
    out.push_str(&format!(
        "  failure: {}\n",
        display_value(&http["failure_kind"])
    ));
    out
}

fn render_reasons(report: &OpsReport) -> String {
    let mut out = String::new();
    out.push_str("Warnings:\n");
    if report.verdict.warning_reasons.is_empty() {
        out.push_str("  none\n");
    } else {
        for reason in &report.verdict.warning_reasons {
            out.push_str(&format!("  - {}\n", reason));
        }
    }
    if !report.verdict.blocking_reasons.is_empty() {
        out.push_str("Blocking:\n");
        for reason in &report.verdict.blocking_reasons {
            out.push_str(&format!("  - {}\n", reason));
        }
    }
    out.push_str("Next actions:\n");
    for action in &report.verdict.suggested_next_actions {
        out.push_str(&format!("  - {}\n", action));
    }
    out
}

fn source_json(server_url: &str, runtime_commit: Option<String>, tool: &str) -> Value {
    json!({
        "server_url": server_url,
        "runtime_commit": runtime_commit,
        "tool": tool,
    })
}

fn smoke_preflight_source_json(
    server_url: &str,
    runtime_commit: Option<String>,
    called_show_changes: bool,
    called_hygiene: bool,
) -> Value {
    let mut tools = vec![json!("runtime_status"), json!("list_projects")];
    if called_show_changes {
        tools.push(json!("show_changes"));
    }
    if called_hygiene {
        tools.push(json!("workspace_hygiene_check"));
    }
    json!({
        "server_url": server_url,
        "runtime_commit": runtime_commit,
        "tool": "runtime_status",
        "tools": tools,
    })
}

fn runtime_commit(runtime: &Value) -> Option<String> {
    runtime
        .pointer("/build/git_commit")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
}

fn str_pointer(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn runner_count(runtime: &Value, field: &str, summary_field: &str) -> u64 {
    // `/agents` remains the stable runtime_status JSON compatibility path.
    runtime
        .pointer(&format!("/agents/{field}"))
        .and_then(Value::as_u64)
        .or_else(|| {
            runtime
                .pointer(&format!("/agents/summary/{summary_field}"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(0)
}

fn runner_clients(runtime: &Value) -> Vec<Value> {
    // `/agents` remains the stable runtime_status JSON compatibility path.
    let clients = runtime
        .pointer("/agents/summary/clients")
        .or_else(|| runtime.pointer("/agents/clients"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    clients
        .into_iter()
        .map(|client| {
            json!({
                "client_id": client.get("client_id").cloned().unwrap_or(Value::Null),
                "status": client.get("status").cloned().unwrap_or(Value::Null),
                "transport": client.get("transport").cloned().unwrap_or(Value::Null),
                "projects_count": client.get("projects_count").cloned().unwrap_or(Value::Null),
                "active_jobs": client.get("active_jobs").cloned().unwrap_or(Value::Null),
                "pending_requests": client.get("pending_requests").cloned().unwrap_or(Value::Null),
                "last_seen_age_secs": client.get("last_seen_age_secs").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

fn project_entries(projects: Option<&Value>) -> Vec<Value> {
    projects
        .and_then(|value| value.get("projects"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn find_project<'a>(projects: Option<&'a Value>, project_id: &str) -> Option<&'a Value> {
    projects
        .and_then(|value| value.get("projects"))
        .and_then(Value::as_array)
        .and_then(|projects| {
            projects
                .iter()
                .find(|project| project.get("id").and_then(Value::as_str) == Some(project_id))
        })
}

fn compact_projects(projects: &[Value]) -> Vec<Value> {
    projects.iter().map(compact_project).collect()
}

fn compact_project(project: &Value) -> Value {
    json!({
        "id": project.get("id").cloned().unwrap_or(Value::Null),
        "client_id": project.get("client_id").cloned().unwrap_or(Value::Null),
        "agent_status": project.get("agent_status").cloned().unwrap_or(Value::Null),
        "connected": project.get("connected").cloned().unwrap_or(Value::Null),
        "git_available": project.pointer("/capabilities/git_available").cloned().unwrap_or(Value::Null),
        "recommended_for_smoke": project.pointer("/capabilities/recommended_for_smoke").cloned().unwrap_or(Value::Null),
        "safe_smoke_project": project.pointer("/capabilities/safe_smoke_project").cloned().unwrap_or(Value::Null),
        "allow_patch": project.get("allow_patch").cloned().unwrap_or(Value::Null),
        "path": project.get("path").cloned().unwrap_or(Value::Null),
    })
}

fn project_connected(project: &Value) -> bool {
    project
        .get("connected")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn project_runner_status(project: &Value) -> Option<String> {
    // `agent_status` remains the stable list_projects JSON compatibility key.
    project
        .get("agent_status")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn project_runner_online(project: &Value) -> bool {
    project_runner_status(project).as_deref() == Some("online")
}

fn project_online(project: &Value) -> bool {
    project_connected(project) && project_runner_online(project)
}

fn project_bool(project: &Value, pointer: &str) -> bool {
    project
        .pointer(pointer)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn verdict_status(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|value| value.pointer("/verdict/status"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SmokePreflightTargetState {
    ready: bool,
    connected: bool,
    runner_online: bool,
    git_available: bool,
}

fn smoke_preflight_target_ready(project: Option<&Value>) -> SmokePreflightTargetState {
    let Some(project) = project else {
        return SmokePreflightTargetState {
            ready: false,
            connected: false,
            runner_online: false,
            git_available: false,
        };
    };
    let connected = project_connected(project);
    let runner_online = project_runner_online(project);
    let git_available = project_bool(project, "/capabilities/git_available");
    SmokePreflightTargetState {
        ready: connected && runner_online && git_available,
        connected,
        runner_online,
        git_available,
    }
}

fn display_value(value: &Value) -> String {
    match value {
        Value::Null => "unknown".to_string(),
        Value::String(s) if s.is_empty() => "unknown".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn push_unique_string(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}
