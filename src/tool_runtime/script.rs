use serde_json::json;
use std::time::Duration;

use super::helpers::{command_rejected_message, project_relative_runner_cwd, resolve_runner_cwd};
use super::process::{
    add_structured_continuation_facts, classify_process_failure, command_failure_result,
    outcome_unknown_result, process_tool_failure_result, success_output,
    terminal_structured_job_result,
};
use super::shell::{dispatch_uncertainty_lifecycle, runner_command_lifecycle};
use super::structured_execution::{
    await_hidden_structured_job, HiddenStructuredJobWait, StructuredExecutionBudget,
};
use super::tool_audit::{assertion_validation_identity, run_script_validation_identity};
use super::{ExecutionPurpose, ToolResult, ToolRuntime};
use crate::auth::AuthContext;
use crate::runner_http::{
    script_preview, ShellJobStartMetadata, ShellJobVisibility, StructuredJobExecution,
};
use crate::runner_protocol::{
    validate_script_request, ShellCommandExecutionState, ShellJobOpRequest, ShellScriptLanguage,
    ShellScriptPayload,
};

fn decorate(
    output: &mut serde_json::Value,
    purpose: ExecutionPurpose,
    summary: &str,
    language: ShellScriptLanguage,
    cwd: &str,
    executor: &str,
) {
    output["execution_source"] = json!("run_script");
    output["purpose"] = json!(purpose.as_str());
    output["script_summary"] = json!(summary);
    output["language"] = json!(language.as_str());
    output["cwd"] = json!(cwd);
    output["executor"] = json!(executor);
}

impl ToolRuntime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_script_with_contract_for_resource(
        &self,
        project: String,
        language: ShellScriptLanguage,
        script: String,
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
        let budget =
            match StructuredExecutionBudget::resolve_with_sync_wait(timeout_secs, sync_wait_secs) {
            Ok(budget) => budget,
            Err(error) => {
                return process_tool_failure_result(
                    command_rejected_message(
                        format!("run_script {error}"),
                        "pass positive timeout_secs/sync_wait_secs values or omit them for defaults; oversized values are clamped to the supported runtime and synchronous-wait ceilings.",
                    ),
                    "invalid_arguments",
                    ShellCommandExecutionState::NotStarted,
                )
            }
        };
        let timeout = budget.effective_timeout_secs;
        let payload = ShellScriptPayload {
            language,
            script,
            args,
        };
        if let Err(error) =
            validate_script_request(&payload, stdin.as_deref(), cwd.as_deref(), timeout)
        {
            return process_tool_failure_result(
                command_rejected_message(
                    error,
                    "correct the typed script fields and retry; use run_shell only when an explicit command-string or SSH shell is required.",
                ),
                "invalid_arguments",
                ShellCommandExecutionState::NotStarted,
            );
        }
        if ssh_resource.is_some() {
            return process_tool_failure_result(
                command_rejected_message(
                    "named Session SSH resources do not support typed script payloads",
                    "use run_shell explicitly for this SSH resource, or run_script against the Runner-host project.",
                ),
                "unsupported_resource",
                ShellCommandExecutionState::NotStarted,
            );
        }
        let summary = script_preview(language.as_str(), payload.script.len(), payload.args.len());
        let declared_purpose = purpose.unwrap_or_default();
        let validation_identity = run_script_validation_identity(
            language.as_str(),
            &payload.script,
            &payload.args,
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
        // Generation-2 admission guarantees async typed structured Jobs;
        // sync_wait_secs now controls projection timing, not Runner compatibility.
        let async_handoff_available = true;
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
                        shell: Some(language.as_str().to_string()),
                        visibility: ShellJobVisibility::HiddenUntilHandoff,
                        structured_execution: Some(StructuredJobExecution::Script(payload)),
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
                                "confirm the Runner is connected and advertises structured_execution_jobs, then retry only if target state proves no script started.",
                            ),
                            classify_process_failure(&error),
                            ShellCommandExecutionState::NotStarted,
                        );
                    decorate(
                        &mut result.output,
                        declared_purpose,
                        &summary,
                        language,
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
                    "the durable script Job could not be observed during handoff: {error}"
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
                language,
                &resolved_cwd,
                "agent",
            );
            return result;
        }
        let wait_timeout = timeout;
        let (request_id, receiver) = match self
            .runner_registry
            .enqueue_script(
                client_id,
                Some(effective_cwd),
                payload,
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
                        match language {
                            ShellScriptLanguage::Javascript => "confirm the Runner is connected and advertises structured_script_payload plus structured_script_javascript, then retry only if target state proves no script started.",
                            ShellScriptLanguage::Typescript => "confirm the Runner is connected and advertises structured_script_payload plus structured_script_typescript, then retry only if target state proves no script started.",
                            _ => "confirm the Runner is connected and advertises structured_script_payload, then retry only if target state proves no script started.",
                        },
                    ),
                    classify_process_failure(&error),
                    ShellCommandExecutionState::NotStarted,
                )
            }
        };
        let mut result = match tokio::time::timeout(Duration::from_secs(wait_timeout + 2), receiver)
            .await
        {
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
                            .unwrap_or("Runner rejected the script before spawn");
                        process_tool_failure_result(
                                    command_rejected_message(
                                        reason,
                                        "inspect the rejection, correct language/script/args/cwd, then retry.",
                                    ),
                                    classify_process_failure(reason),
                                    state,
                                )
                    }
                    ShellCommandExecutionState::OutcomeUnknown => outcome_unknown_result(
                        response
                            .error
                            .as_deref()
                            .unwrap_or("the Runner did not return a trustworthy terminal result"),
                    ),
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
                            "script request waiter was dropped before Runner dispatch",
                            "check Runner connectivity, then retry.",
                        ),
                        "runtime_error",
                        ShellCommandExecutionState::NotStarted,
                    )
                } else {
                    outcome_unknown_result(
                        "script request waiter was dropped after dispatch may have occurred",
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
                        "timed out waiting {wait_timeout} seconds for the Runner script result"
                    ))
                }
            }
        };
        decorate(
            &mut result.output,
            declared_purpose,
            &summary,
            language,
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
