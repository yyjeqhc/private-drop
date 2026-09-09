use serde_json::json;
use std::time::Duration;

use super::helpers::{
    bounded_tail, command_failed_message, command_outcome_unknown_message,
    command_rejected_message, command_timeout_message, project_relative_runner_cwd,
    resolve_runner_cwd, COMMAND_STDIO_TAIL_CHARS,
};
use super::shell::{
    command_execution_state_name, dispatch_uncertainty_lifecycle, runner_command_lifecycle,
};
use super::structured_execution::{
    await_hidden_structured_job, HiddenStructuredJobWait, StructuredExecutionBudget,
};
use super::tool_audit::{assertion_validation_identity, run_process_validation_identity};
use super::{ExecutionPurpose, ToolResult, ToolRuntime};
use crate::auth::AuthContext;
use crate::runner_http::{
    process_preview, RunnerFeature, ShellJobStartMetadata, ShellJobVisibility,
    StructuredJobExecution, DETACHED_IDEMPOTENCY_CONFLICT, DETACHED_IDEMPOTENCY_RECOVERY_PREFIX,
};
use crate::runner_protocol::{
    validate_process_argv, ShellCommandExecutionState, ShellJobInfo, ShellJobOpRequest,
    ShellProcessArgv, PROCESS_CWD_MAX_BYTES, PROCESS_STDIN_MAX_BYTES,
    STRUCTURED_EXECUTION_DIRECT_SYNC_TIMEOUT_MAX_SECS,
};

fn command_started(state: ShellCommandExecutionState) -> bool {
    !matches!(state, ShellCommandExecutionState::NotStarted)
}

fn command_completed(state: ShellCommandExecutionState) -> bool {
    matches!(state, ShellCommandExecutionState::Completed)
}

pub(crate) fn success_output(
    exit_code: i32,
    stdout: String,
    stderr: String,
    duration_ms: Option<u64>,
) -> serde_json::Value {
    let (stdout_tail, stdout_truncated) = bounded_tail(&stdout, COMMAND_STDIO_TAIL_CHARS);
    let (stderr_tail, stderr_truncated) = bounded_tail(&stderr, COMMAND_STDIO_TAIL_CHARS);
    json!({
        "exit_code": exit_code,
        "stdout_tail": stdout_tail,
        "stderr_tail": stderr_tail,
        "stdout_lines": stdout.lines().count(),
        "stderr_lines": stderr.lines().count(),
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
        "duration_ms": duration_ms,
        "command_started": true,
        "command_completed": true,
        "command_ok": true,
        "execution_state": command_execution_state_name(ShellCommandExecutionState::Completed),
        "failure_kind": null,
        "tool_failure": false,
    })
}

pub(crate) fn command_failure_result(
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    duration_ms: Option<u64>,
    timeout_secs: u64,
    state: ShellCommandExecutionState,
) -> ToolResult {
    let (stdout_tail, stdout_truncated) = bounded_tail(&stdout, COMMAND_STDIO_TAIL_CHARS);
    let (stderr_tail, stderr_truncated) = bounded_tail(&stderr, COMMAND_STDIO_TAIL_CHARS);
    let timed_out = state == ShellCommandExecutionState::TimedOut;
    let output = json!({
        "exit_code": exit_code,
        "duration_ms": duration_ms,
        "stdout_tail": stdout_tail,
        "stderr_tail": stderr_tail,
        "stdout_lines": stdout.lines().count(),
        "stderr_lines": stderr.lines().count(),
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
        "command_started": command_started(state),
        "command_completed": command_completed(state),
        "command_ok": false,
        "execution_state": command_execution_state_name(state),
        "failure_kind": if timed_out { "timeout" } else { "command_exit_nonzero" },
        "tool_failure": false,
    });
    ToolResult {
        success: false,
        error: Some(if timed_out {
            command_timeout_message(timeout_secs, &stdout_tail, &stderr_tail)
        } else {
            command_failed_message(exit_code, &stdout_tail, &stderr_tail)
        }),
        output,
    }
}

pub(crate) fn process_tool_failure_result(
    message: impl Into<String>,
    failure_kind: &'static str,
    state: ShellCommandExecutionState,
) -> ToolResult {
    ToolResult::err_with_output(
        message.into(),
        json!({
            "command_started": command_started(state),
            "command_completed": command_completed(state),
            "command_ok": false,
            "exit_code": null,
            "execution_state": command_execution_state_name(state),
            "failure_kind": failure_kind,
            "tool_failure": true,
        }),
    )
}

pub(crate) fn outcome_unknown_result(reason: impl AsRef<str>) -> ToolResult {
    process_tool_failure_result(
        command_outcome_unknown_message(reason),
        "outcome_unknown",
        ShellCommandExecutionState::OutcomeUnknown,
    )
}

pub(crate) fn add_structured_continuation_facts(
    result: &mut ToolResult,
    effective_timeout_secs: u64,
    sync_wait_secs: u64,
    async_handoff_available: bool,
) {
    let terminal = result
        .output
        .get("execution_state")
        .and_then(serde_json::Value::as_str)
        != Some("outcome_unknown");
    result.output["promoted_to_job"] = json!(false);
    result.output["terminal"] = json!(terminal);
    result.output["job_id"] = serde_json::Value::Null;
    result.output["job_status"] = serde_json::Value::Null;
    result.output["effective_timeout_secs"] = json!(effective_timeout_secs);
    result.output["sync_wait_secs"] = json!(sync_wait_secs);
    result.output["async_handoff_available"] = json!(async_handoff_available);
}

pub(crate) fn terminal_structured_job_result(
    job: &ShellJobInfo,
    stdout: String,
    stderr: String,
    timeout_secs: u64,
) -> ToolResult {
    let state = job
        .command_execution_state
        .unwrap_or(ShellCommandExecutionState::OutcomeUnknown);
    let mut result = match state {
        ShellCommandExecutionState::NotStarted => {
            let reason = job
                .error
                .as_deref()
                .unwrap_or("Runner rejected the structured execution before child spawn");
            process_tool_failure_result(
                command_rejected_message(
                    reason,
                    "inspect the rejection, correct the typed request, then retry.",
                ),
                classify_process_failure(reason),
                state,
            )
        }
        ShellCommandExecutionState::OutcomeUnknown => outcome_unknown_result(
            job.error
                .as_deref()
                .unwrap_or("the Runner lost a trustworthy terminal process result"),
        ),
        ShellCommandExecutionState::TimedOut => command_failure_result(
            job.exit_code,
            stdout,
            stderr,
            job.duration_ms,
            timeout_secs,
            state,
        ),
        ShellCommandExecutionState::Completed
            if job.status == "completed" && job.exit_code == Some(0) && job.error.is_none() =>
        {
            ToolResult::ok(success_output(0, stdout, stderr, job.duration_ms))
        }
        ShellCommandExecutionState::Completed => command_failure_result(
            job.exit_code,
            stdout,
            stderr,
            job.duration_ms,
            timeout_secs,
            state,
        ),
    };
    if job.stdout_log_truncated {
        result.output["stdout_truncated"] = json!(true);
    }
    if job.stderr_log_truncated {
        result.output["stderr_truncated"] = json!(true);
    }
    result
}

pub(crate) fn classify_process_failure(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("interpreter_unavailable") || lower.contains("interpreter is unavailable") {
        "interpreter_unavailable"
    } else if lower.contains("script_setup_failed")
        || lower.contains("temporary script setup failed")
    {
        "script_setup_failed"
    } else if lower.contains("invalid_structured_script_request")
        || lower.starts_with("invalid_arguments:")
    {
        "invalid_arguments"
    } else if lower.contains("unsupported_executable_type") {
        "unsupported_executable_type"
    } else if lower.contains("capability_unavailable")
        || lower.contains("structured_process_argv")
        || lower.contains("does not support")
    {
        "capability_unavailable"
    } else if lower.contains("failed to spawn")
        || lower.contains("not found")
        || lower.contains("no such file")
        || lower.contains("cannot find")
        || lower.contains("executable is unavailable")
    {
        "spawn_failed"
    } else if lower.contains("offline")
        || lower.contains("not connected")
        || lower.contains("no connected")
        || lower.contains("unknown agent")
        || lower.contains("unknown_project")
    {
        "agent_offline"
    } else if lower.contains("permission")
        || lower.contains("denied")
        || lower.contains("outside")
        || lower.contains("not allowed")
        || lower.contains("sandbox")
    {
        "permission_denied"
    } else {
        "runtime_error"
    }
}

fn validate_process_input(
    process: &ShellProcessArgv,
    stdin: Option<&str>,
    cwd: Option<&str>,
) -> Result<(), String> {
    validate_process_argv(process)?;
    if let Some(stdin) = stdin {
        if stdin.len() > PROCESS_STDIN_MAX_BYTES {
            return Err(format!(
                "stdin is too large; maximum is {PROCESS_STDIN_MAX_BYTES} bytes"
            ));
        }
        if stdin.contains('\0') {
            return Err("stdin cannot contain NUL bytes".to_string());
        }
    }
    if let Some(cwd) = cwd {
        if cwd.len() > PROCESS_CWD_MAX_BYTES {
            return Err(format!(
                "cwd is too long; maximum is {PROCESS_CWD_MAX_BYTES} bytes"
            ));
        }
        if cwd.contains('\0') {
            return Err("cwd cannot contain NUL bytes".to_string());
        }
    }
    Ok(())
}

fn decorate(
    output: &mut serde_json::Value,
    purpose: ExecutionPurpose,
    summary: &str,
    cwd: &str,
    executor: &str,
) {
    output["execution_source"] = json!("run_process");
    output["purpose"] = json!(purpose.as_str());
    output["process_summary"] = json!(summary);
    output["cwd"] = json!(cwd);
    output["executor"] = json!(executor);
}

impl ToolRuntime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_process_with_contract_for_resource(
        &self,
        project: String,
        executable: String,
        args: Vec<String>,
        stdin: Option<String>,
        timeout_secs: Option<u64>,
        sync_wait_secs: Option<u64>,
        cwd: Option<String>,
        purpose: Option<ExecutionPurpose>,
        ssh_resource: Option<&str>,
        session_id: Option<String>,
        validation_assertion_name: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.run_process_with_contract_mode(
            project,
            executable,
            args,
            stdin,
            timeout_secs,
            sync_wait_secs,
            cwd,
            purpose,
            ssh_resource,
            session_id,
            auth,
            validation_assertion_name,
            true,
        )
        .await
    }

    /// Admit one explicitly detached native process through the existing durable
    /// Job identity/admission path. This never falls back to ordinary process,
    /// shell, script, local execution, or a retrying respawn path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_detached_process_with_contract(
        &self,
        project: String,
        idempotency_key: String,
        executable: String,
        args: Vec<String>,
        stdin: Option<String>,
        timeout_secs: Option<u64>,
        cwd: Option<String>,
        purpose: Option<ExecutionPurpose>,
        ssh_resource: Option<&str>,
        session_id: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let budget = match StructuredExecutionBudget::resolve(timeout_secs) {
            Ok(budget) => budget,
            Err(error) => {
                return process_tool_failure_result(
                    command_rejected_message(
                        format!("run_detached_process {error}"),
                        "pass timeout_secs between 1 and 3600, or omit it for the default of 60 seconds.",
                    ),
                    "invalid_arguments",
                    ShellCommandExecutionState::NotStarted,
                )
            }
        };
        let timeout = budget.effective_timeout_secs;
        let process = ShellProcessArgv { executable, args };
        if let Err(error) = validate_process_input(&process, stdin.as_deref(), cwd.as_deref()) {
            return process_tool_failure_result(
                command_rejected_message(
                    error,
                    "correct the structured process fields and retry; detached execution accepts native argv only.",
                ),
                "invalid_arguments",
                ShellCommandExecutionState::NotStarted,
            );
        }
        if ssh_resource.is_some() {
            return process_tool_failure_result(
                command_rejected_message(
                    "named Session SSH resources do not support detached native argv ownership",
                    "run the detached process against the Runner-host project; use run_shell explicitly for remote shell semantics.",
                ),
                "unsupported_resource",
                ShellCommandExecutionState::NotStarted,
            );
        }
        let summary = format!("detached process ({} args)", process.args.len());
        let declared_purpose = purpose.unwrap_or_default();
        let proj = match self.resolve_project(&project).await {
            Ok(project) => project,
            Err(error) => {
                return process_tool_failure_result(
                    command_rejected_message(
                        error.to_message(),
                        "verify the project id with list_projects, then retry with a registered Runner project.",
                    ),
                    "agent_offline",
                    ShellCommandExecutionState::NotStarted,
                )
            }
        };
        let client_id = proj.client_id.clone();
        let effective_cwd = match resolve_runner_cwd(&proj, cwd.as_deref()) {
            Ok(cwd) => cwd,
            Err(error) => {
                return process_tool_failure_result(
                    command_rejected_message(
                        error,
                        "choose '.', an existing project-relative cwd, or an absolute path inside the registered project root.",
                    ),
                    "permission_denied",
                    ShellCommandExecutionState::NotStarted,
                )
            }
        };
        let resolved_cwd =
            project_relative_runner_cwd(&proj, &effective_cwd).unwrap_or_else(|_| ".".to_string());
        let features = match self
            .runner_registry
            .get_runner_feature_set(&client_id)
            .await
        {
            Ok(features) => features,
            Err(error) => {
                return process_tool_failure_result(
                    command_rejected_message(
                        error.to_string(),
                        "confirm the Runner is registered and connected, then retry.",
                    ),
                    "agent_offline",
                    ShellCommandExecutionState::NotStarted,
                )
            }
        };
        if !features.supports(RunnerFeature::DetachedProcessJobs) {
            return process_tool_failure_result(
                command_rejected_message(
                    "capability_unavailable: this Runner does not advertise the complete detached_process_jobs structured Job contract",
                    "upgrade or select a Runner that explicitly advertises detached_process_jobs; do not retry as ordinary run_process.",
                ),
                "capability_unavailable",
                ShellCommandExecutionState::NotStarted,
            );
        }
        let access = crate::runner_http::runner_access_from_auth(auth);
        let detached_initiator = match crate::runner_http::detached_initiator_identity_from_auth(auth)
        {
            Ok(identity) => identity,
            Err(error) => {
                return process_tool_failure_result(
                    command_rejected_message(
                        &error,
                        "observe the deterministic logical Job if an initiating response may have been lost; do not retry with a fresh key unless no Job was admitted.",
                    ),
                    classify_process_failure(&error),
                    ShellCommandExecutionState::NotStarted,
                )
            }
        };
        match self
            .runner_registry
            .start_job_with_metadata_for_access(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some(client_id),
                    cwd: Some(effective_cwd),
                    command: Some(String::new()),
                    timeout_secs: Some(timeout),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tool_runtime".to_string(),
                ShellJobStartMetadata {
                    project_id: Some(project.clone()),
                    session_id,
                    project_cwd: Some(resolved_cwd.clone()),
                    purpose: Some(declared_purpose.as_str().to_string()),
                    shell: Some("direct_argv".to_string()),
                    visibility: ShellJobVisibility::Public,
                    structured_execution: Some(StructuredJobExecution::DetachedProcess(process)),
                    stdin,
                    detached_idempotency_key: Some(idempotency_key),
                    ..Default::default()
                },
                access.as_ref(),
                Some(&detached_initiator),
            )
            .await
        {
            Ok(job) => ToolResult::ok(json!({
                "job_id": job.job_id,
                "kind": job.kind,
                "status": job.status,
                "project": project,
                "execution_source": "run_detached_process",
                "purpose": declared_purpose.as_str(),
                "process_summary": summary,
                "cwd": resolved_cwd,
                "shell": "direct_argv",
                "executor": "agent",
                "execution_state": "pending",
                "command_started": false,
                "command_completed": false,
                "terminal": false,
                "effective_timeout_secs": timeout,
                "created_at": job.created_at,
                "observation_token": job.observation_token,
                "last_update_seq": job.last_update_seq,
            })),
            Err(error) => {
                if let Some(job_id) = error.strip_prefix(DETACHED_IDEMPOTENCY_RECOVERY_PREFIX) {
                    return ToolResult::err_with_output(
                        "detached initiation key already identifies a durable logical Job reconstructed after Server restart; the resent body was not redispatched. Observe the returned job_id instead of retrying execution.".to_string(),
                        json!({
                            "job_id": job_id,
                            "execution_state": "not_started",
                            "command_started": false,
                            "command_completed": false,
                            "command_ok": false,
                            "failure_kind": "idempotency_recovery_required",
                            "redispatched": false,
                            "tool_failure": true,
                        }),
                    );
                }
                if error.starts_with(DETACHED_IDEMPOTENCY_CONFLICT) {
                    return process_tool_failure_result(
                        command_rejected_message(
                            &error,
                            "use a fresh idempotency_key for a different detached process intent; the existing logical Job was left unchanged.",
                        ),
                        "idempotency_conflict",
                        ShellCommandExecutionState::NotStarted,
                    );
                }
                process_tool_failure_result(
                    command_rejected_message(
                        &error,
                        "observe the deterministic logical Job if an initiating response may have been lost; do not retry with a fresh key unless no Job was admitted.",
                    ),
                    classify_process_failure(&error),
                    ShellCommandExecutionState::NotStarted,
                )
            }
        }
    }

    /// Execute one server-owned fixed process synchronously without exposing the
    /// model-facing structured-execution Job handoff. Effectful internal tools
    /// must not report success while their fixed mutation is still running.
    pub(super) async fn run_internal_process_sync(
        &self,
        project: String,
        executable: String,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> ToolResult {
        self.run_process_with_contract_mode(
            project,
            executable,
            args,
            None,
            Some(timeout_secs),
            None,
            None,
            Some(ExecutionPurpose::Operation),
            None,
            None,
            None,
            None,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_process_with_contract_mode(
        &self,
        project: String,
        executable: String,
        args: Vec<String>,
        stdin: Option<String>,
        timeout_secs: Option<u64>,
        sync_wait_secs: Option<u64>,
        cwd: Option<String>,
        purpose: Option<ExecutionPurpose>,
        ssh_resource: Option<&str>,
        session_id: Option<String>,
        auth: Option<&AuthContext>,
        validation_assertion_name: Option<&str>,
        allow_async_handoff: bool,
    ) -> ToolResult {
        let budget =
            match StructuredExecutionBudget::resolve_with_sync_wait(timeout_secs, sync_wait_secs) {
            Ok(budget) => budget,
            Err(error) => {
                return process_tool_failure_result(
                    command_rejected_message(
                        format!("run_process {error}"),
                        "pass timeout_secs between 1 and 3600 and sync_wait_secs between 1 and 60 without exceeding timeout_secs; both may be omitted for their defaults.",
                    ),
                    "invalid_arguments",
                    ShellCommandExecutionState::NotStarted,
                )
            }
        };
        let timeout = budget.effective_timeout_secs;
        let process = ShellProcessArgv { executable, args };
        if let Err(error) = validate_process_input(&process, stdin.as_deref(), cwd.as_deref()) {
            return process_tool_failure_result(
                command_rejected_message(
                    error,
                    "correct the structured process fields and retry; use run_shell only when shell syntax is required.",
                ),
                "invalid_arguments",
                ShellCommandExecutionState::NotStarted,
            );
        }
        if ssh_resource.is_some() {
            return process_tool_failure_result(
                command_rejected_message(
                    "named Session SSH resources do not support native structured argv",
                    "use run_shell explicitly for this SSH resource, or run_process against the Runner-host project.",
                ),
                "unsupported_resource",
                ShellCommandExecutionState::NotStarted,
            );
        }
        let summary = process_preview(&process.executable, process.args.iter().map(String::as_str));
        let declared_purpose = purpose.unwrap_or_default();
        let validation_identity = run_process_validation_identity(
            &process.executable,
            &process.args,
            stdin.as_deref(),
            cwd.as_deref(),
            Some(declared_purpose.as_str()),
        )
        .map(|mut identity| {
            if let Some(assertion_name) = validation_assertion_name {
                identity.identity = assertion_validation_identity(assertion_name);
            }
            identity
        });
        let proj = match self.resolve_project(&project).await {
            Ok(project) => project,
            Err(error) => {
                return process_tool_failure_result(
                    command_rejected_message(
                        error.to_message(),
                        "verify the project id with list_projects, then retry with a registered project.",
                    ),
                    "agent_offline",
                    ShellCommandExecutionState::NotStarted,
                )
            }
        };
        let client_id = proj.client_id.clone();
        let effective_cwd = match resolve_runner_cwd(&proj, cwd.as_deref()) {
                Ok(cwd) => cwd,
                Err(error) => {
                    return process_tool_failure_result(
                        command_rejected_message(
                            error,
                            "choose '.', an existing project-relative cwd, or an absolute path inside the registered project root.",
                        ),
                        "permission_denied",
                        ShellCommandExecutionState::NotStarted,
                    )
                }
            };
        let resolved_cwd =
            project_relative_runner_cwd(&proj, &effective_cwd).unwrap_or_else(|_| ".".to_string());
        // Generation-2 admission guarantees the complete typed structured
        // execution baseline. Only server-owned internal mutations may opt
        // out of model-facing durable Job handoff.
        let async_handoff_available = allow_async_handoff;
        if !async_handoff_available && timeout > STRUCTURED_EXECUTION_DIRECT_SYNC_TIMEOUT_MAX_SECS {
            let mut result = process_tool_failure_result(
                command_rejected_message(
                    "internal synchronous structured execution cannot hand off to a durable Job",
                    "keep this internal operation at timeout_secs <= 120.",
                ),
                "capability_unavailable",
                ShellCommandExecutionState::NotStarted,
            );
            decorate(
                &mut result.output,
                declared_purpose,
                &summary,
                &resolved_cwd,
                "agent",
            );
            add_structured_continuation_facts(
                &mut result,
                timeout,
                STRUCTURED_EXECUTION_DIRECT_SYNC_TIMEOUT_MAX_SECS,
                false,
            );
            return result;
        }
        if async_handoff_available && timeout > budget.sync_wait_secs {
            let job = self
                .runner_registry
                .start_job_with_metadata_for_access(
                    ShellJobOpRequest {
                        op: "start".to_string(),
                        client_id: Some(client_id),
                        cwd: Some(effective_cwd),
                        command: Some(String::new()),
                        timeout_secs: Some(timeout),
                        job_id: None,
                        since_stdout_line: None,
                        since_stderr_line: None,
                        tail_lines: None,
                        limit: None,
                        codex: None,
                    },
                    "tool_runtime".to_string(),
                    ShellJobStartMetadata {
                        project_id: Some(project.clone()),
                        session_id,
                        project_cwd: Some(resolved_cwd.clone()),
                        purpose: Some(declared_purpose.as_str().to_string()),
                        shell: Some("direct_argv".to_string()),
                        visibility: ShellJobVisibility::HiddenUntilHandoff,
                        structured_execution: Some(StructuredJobExecution::Process(process)),
                        validation_identity: validation_identity
                            .as_ref()
                            .map(|identity| identity.identity.clone()),
                        validation_tool: validation_identity
                            .as_ref()
                            .and_then(|identity| identity.validation_tool.map(str::to_string)),
                        assertion_name: validation_identity
                            .as_ref()
                            .filter(|identity| identity.identity.starts_with("assertion:"))
                            .and_then(|_| validation_assertion_name.map(str::to_string)),
                        stdin,
                        ..Default::default()
                    },
                    crate::runner_http::runner_access_from_auth(auth).as_ref(),
                    None,
                )
                .await;
            let job = match job {
                Ok(job) => job,
                Err(error) => {
                    let mut result = process_tool_failure_result(
                            command_rejected_message(
                                &error,
                                "confirm the Runner is connected and advertises structured_execution_jobs, then retry only if target state proves no process started.",
                            ),
                            classify_process_failure(&error),
                            ShellCommandExecutionState::NotStarted,
                        );
                    decorate(
                        &mut result.output,
                        declared_purpose,
                        &summary,
                        &resolved_cwd,
                        "agent",
                    );
                    add_structured_continuation_facts(
                        &mut result,
                        timeout,
                        budget.sync_wait_secs,
                        true,
                    );
                    return result;
                }
            };
            let wait = self
                .structured_execution_sync_wait
                .min(Duration::from_secs(budget.sync_wait_secs));
            let handoff = await_hidden_structured_job(
                self.runner_registry.clone(),
                job.job_id.clone(),
                wait,
                auth.cloned(),
            )
            .await;
            let mut result = match handoff {
                Ok(HiddenStructuredJobWait::Terminal {
                    job,
                    stdout,
                    stderr,
                }) => {
                    let result = terminal_structured_job_result(&job, stdout, stderr, timeout);
                    self.runner_registry
                        .remove_projected_hidden_structured_job_record(&job.job_id)
                        .await;
                    result
                }
                Ok(HiddenStructuredJobWait::Continued {
                    observation,
                    execution_state,
                    command_started,
                }) => {
                    let detected_summary =
                        crate::tool_runtime::jobs::detected_job_summary_with_activity(
                            Some(&summary),
                            Some(declared_purpose.as_str()),
                            &observation.job.status,
                            observation.job.exit_code.map(i64::from),
                            &observation.stdout_tail,
                            &observation.stderr_tail,
                            observation.job.activity.as_ref(),
                        );
                    ToolResult::ok(json!({
                        "execution_state": execution_state,
                        "command_started": command_started,
                        "command_completed": false,
                        "command_ok": false,
                        "exit_code": null,
                        "failure_kind": null,
                        "tool_failure": false,
                        "promoted_to_job": true,
                        "terminal": false,
                        "job_id": observation.job.job_id,
                        "job_status": observation.job.status,
                        "observation_token": observation.job.observation_token,
                        "activity": observation.job.activity,
                        "effective_timeout_secs": timeout,
                        "sync_wait_secs": budget.sync_wait_secs,
                        "async_handoff_available": true,
                        "stdout_tail": observation.stdout_tail,
                        "stderr_tail": observation.stderr_tail,
                        "stdout_lines": observation.stdout_lines,
                        "stderr_lines": observation.stderr_lines,
                        "stdout_truncated": observation.stdout_truncated,
                        "stderr_truncated": observation.stderr_truncated,
                        "detected_summary": detected_summary,
                    }))
                }
                Err(error) => outcome_unknown_result(format!(
                    "the durable process Job could not be observed during handoff: {error}"
                )),
            };
            if result.output["promoted_to_job"] != json!(true) {
                add_structured_continuation_facts(
                    &mut result,
                    timeout,
                    budget.sync_wait_secs,
                    true,
                );
            }
            decorate(
                &mut result.output,
                declared_purpose,
                &summary,
                &resolved_cwd,
                "agent",
            );
            return result;
        }
        let wait_timeout = timeout;
        let (request_id, receiver) = match self
                .runner_registry
                .enqueue_process(
                    client_id,
                    Some(effective_cwd),
                    process,
                    stdin,
                    timeout,
                    wait_timeout,
                    "tool_runtime".to_string(),
                )
                .await
            {
                Ok(enqueued) => enqueued,
                Err(error) => {
                    return process_tool_failure_result(
                        command_rejected_message(
                            &error,
                            "confirm the Runner is connected and advertises structured_process_argv, then retry only if target state proves no process started.",
                        ),
                        classify_process_failure(&error),
                        ShellCommandExecutionState::NotStarted,
                    )
                }
            };
        let mut result =
            match tokio::time::timeout(Duration::from_secs(wait_timeout + 2), receiver).await {
                Ok(Ok(response)) => {
                    let state = runner_command_lifecycle(&response, timeout);
                    let exit_code = response.exit_code;
                    let stdout = response.stdout.unwrap_or_default();
                    let stderr = response.stderr.unwrap_or_default();
                    match state {
                        ShellCommandExecutionState::NotStarted => {
                            let reason = response
                                .error
                                .as_deref()
                                .unwrap_or("Runner rejected the process before spawn");
                            process_tool_failure_result(
                            command_rejected_message(
                                reason,
                                "inspect the rejection, correct executable/argv/cwd, then retry.",
                            ),
                            classify_process_failure(reason),
                            state,
                        )
                        }
                        ShellCommandExecutionState::OutcomeUnknown => {
                            outcome_unknown_result(response.error.as_deref().unwrap_or(
                                "the Runner did not return a trustworthy terminal result",
                            ))
                        }
                        ShellCommandExecutionState::TimedOut => command_failure_result(
                            exit_code,
                            stdout,
                            stderr,
                            response.duration_ms,
                            timeout,
                            state,
                        ),
                        ShellCommandExecutionState::Completed
                            if response.error.is_none() && exit_code == Some(0) =>
                        {
                            ToolResult::ok(success_output(0, stdout, stderr, response.duration_ms))
                        }
                        ShellCommandExecutionState::Completed => command_failure_result(
                            exit_code,
                            stdout,
                            stderr,
                            response.duration_ms,
                            timeout,
                            state,
                        ),
                    }
                }
                Ok(Err(_)) => {
                    let dispatch = self
                        .runner_registry
                        .cancel_request_dispatch_state(&request_id)
                        .await;
                    if dispatch == Some(false) {
                        process_tool_failure_result(
                            command_rejected_message(
                                "process request waiter was dropped before Runner dispatch",
                                "check Runner connectivity, then retry.",
                            ),
                            "runtime_error",
                            ShellCommandExecutionState::NotStarted,
                        )
                    } else {
                        outcome_unknown_result(
                            "process request waiter was dropped after dispatch may have occurred",
                        )
                    }
                }
                Err(_) => {
                    let dispatch = self
                        .runner_registry
                        .cancel_request_dispatch_state(&request_id)
                        .await;
                    let state = dispatch_uncertainty_lifecycle(dispatch);
                    if state == ShellCommandExecutionState::NotStarted {
                        process_tool_failure_result(
                            command_rejected_message(
                                format!(
                                "timed out waiting {wait_timeout} seconds before Runner dispatch"
                            ),
                                "check Runner connectivity and availability, then retry.",
                            ),
                            "timeout",
                            state,
                        )
                    } else {
                        outcome_unknown_result(format!(
                        "timed out waiting {wait_timeout} seconds for the Runner process result"
                    ))
                    }
                }
            };
        decorate(
            &mut result.output,
            declared_purpose,
            &summary,
            &resolved_cwd,
            "agent",
        );
        add_structured_continuation_facts(
            &mut result,
            timeout,
            if async_handoff_available {
                budget.sync_wait_secs
            } else {
                timeout
            },
            async_handoff_available,
        );
        result
    }
}
