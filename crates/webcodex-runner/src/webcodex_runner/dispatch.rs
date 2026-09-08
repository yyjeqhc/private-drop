use super::external_tools::ExternalRoute;
use super::lsp::{handle_lsp_operation, LspSupervisor};
use super::transport::ResultSubmission;
use super::validation::handle_validation_request;
use super::{
    handle_computer_operation, handle_prepare_managed_worktree_operation,
    handle_project_lifecycle_operation, handle_project_operation,
    handle_resolve_or_register_project_operation, handle_skill_store_request,
    run_internal_posix_script_with_profiles_and_execution_state,
    run_internal_search_script_with_profiles_and_execution_state,
    run_process_with_profiles_and_execution_state, run_script_with_profiles_and_execution_state,
    run_shell_with_profiles_and_execution_state, run_ssh_shell_with_execution_state, CommandResult,
    HotRunnerConfig, PersistentShellManager, ReloadableRunnerConfig, RunnerSink,
    ShellCommandResult, SubmitResultError,
};
use crate::runner_protocol::{
    PersistentShellResult, RunnerConfigAction, RunnerConfigOperationRequest,
    RunnerJobUpdateRequest, RunnerRequest, EXTERNAL_SEARCH_REQUEST_PREFIX,
    RUNNER_CONFIG_RESPONSE_MAX_BYTES,
};
use crate::{handle_file_operation, JobManager, PendingJobStart};
use std::path::Path;
use std::sync::atomic::Ordering;
use webcodex_core::runner_operation::{
    RunnerFileOperation, RunnerJobOperation, RunnerOperation, RunnerProjectOperation,
    RunnerProjectOperationKind, RunnerShellOperation,
};

fn internal_search_script(command: &str) -> Option<&str> {
    let rest = command.strip_prefix(EXTERNAL_SEARCH_REQUEST_PREFIX)?;
    let script = rest.strip_prefix('\n')?;
    (!script.is_empty()).then_some(script)
}

fn invalid_command(message: impl Into<String>) -> CommandResult {
    CommandResult {
        exit_code: None,
        stdout: None,
        stderr: None,
        duration_ms: Some(0),
        error: Some(message.into()),
    }
}

fn handle_runner_config_operation(
    runtime: &ReloadableRunnerConfig,
    operation: &RunnerConfigOperationRequest,
) -> CommandResult {
    let invalid = || {
        invalid_command(
            "invalid_runner_config_request: bounded typed config operation is required; request was not started",
        )
    };
    let response = match operation.action {
        RunnerConfigAction::Check => runtime.check_config(),
        RunnerConfigAction::Reload => {
            let Some(expected_generation) = operation.expected_generation else {
                return invalid();
            };
            runtime.reload_config(expected_generation)
        }
    };
    if response.validate().is_err() {
        return invalid_command(
            "invalid_runner_config_response: Runner config operation result was rejected",
        );
    }
    let Ok(stdout) = serde_json::to_string(&response) else {
        return invalid();
    };
    if stdout.len() > RUNNER_CONFIG_RESPONSE_MAX_BYTES {
        return invalid();
    }
    CommandResult {
        exit_code: Some(0),
        stdout: Some(stdout),
        stderr: None,
        duration_ms: Some(0),
        error: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunnerDispatchOutcome {
    pub(crate) handled: bool,
    pub(crate) project_cache_invalidation_required: bool,
}

pub(super) fn runner_tool_trace_enabled() -> bool {
    std::env::var("WEBCODEX_TOOL_REQUEST_TRACE")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| {
            matches!(
                value.as_str(),
                "1" | "true" | "yes" | "on" | "metadata" | "full"
            )
        })
}

#[allow(clippy::too_many_arguments)]
fn run_native_shell_or_internal_search(
    config: &HotRunnerConfig,
    runtime: &ReloadableRunnerConfig,
    jobs: &JobManager,
    project_registry_dir: &Path,
    operation: &RunnerShellOperation,
) -> ShellCommandResult {
    if operation.command.lines().next() == Some(EXTERNAL_SEARCH_REQUEST_PREFIX) {
        let Some(script) = internal_search_script(&operation.command) else {
            return ShellCommandResult::not_started(invalid_command(
                "invalid_internal_search_request: generated search script is missing; command was not started",
            ));
        };
        return run_internal_search_script_with_profiles_and_execution_state(
            config.generation,
            &config.policy,
            &config.shell,
            project_registry_dir,
            &jobs.prepared_profiles,
            operation.cwd.as_deref(),
            script,
            operation.timeout_secs,
            Some(runtime.shutdown_flag()),
        );
    }
    run_shell_with_profiles_and_execution_state(
        config.generation,
        &config.policy,
        &config.shell,
        project_registry_dir,
        &jobs.prepared_profiles,
        operation.cwd.as_deref(),
        &operation.command,
        operation.stdin.as_deref(),
        operation.timeout_secs,
        Some(runtime.shutdown_flag()),
    )
}

fn invalid_persistent_shell_result(
    request: &RunnerRequest,
    message: String,
) -> Option<PersistentShellResult> {
    let operation = request.persistent_shell.as_ref()?;
    Some(PersistentShellResult {
        shell_id: operation.shell_id.clone(),
        workflow_session_id: operation.workflow_session_id.clone(),
        runtime_project_id: operation.runtime_project_id.clone(),
        shell_state: "unknown".to_string(),
        execution_state: "rejected".to_string(),
        command_started: false,
        command_completed: false,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        duration_ms: 0,
        cwd: None,
        initial_cwd: None,
        shell: None,
        profile: None,
        created_at: None,
        last_activity_at: None,
        busy: false,
        already_closed: false,
        close_reason: None,
        error_code: Some("persistent_shell_invalid_request".to_string()),
        error: Some(message),
    })
}

fn submit_invalid_job_start(sink: &RunnerSink, request: &RunnerRequest, error: String) -> bool {
    let Some(job_id) = request.job_id.clone() else {
        return false;
    };
    let error = if error.contains("requires job_context") {
        "job start request is missing recovery context".to_string()
    } else {
        format!("invalid Runner Job request: {error}")
    };
    let command_execution_state = crate::structured_prestart_lifecycle(request);
    let _ = sink.send_job_update(&RunnerJobUpdateRequest {
        client_id: sink.client_id().to_string(),
        runner_instance_id: sink.runner_instance_id().to_string(),
        job_id,
        request_id: Some(request.request_id.clone()),
        update_seq: Some(1),
        status: "failed".to_string(),
        stdout_chunk: None,
        stderr_chunk: None,
        stdout_tail: None,
        stderr_tail: None,
        log_snapshot: None,
        exit_code: None,
        duration_ms: Some(0),
        error: Some(error),
        command_execution_state,
        validation_progress: None,
        activity: None,
        finished: true,
    });
    true
}

/// Decoder failures are handled at the V2 boundary. The small amount of `kind`
/// matching here selects the existing result channel only; it never dispatches
/// execution semantics or recovers an operation from optional payload fields.
fn submit_decode_failure(
    sink: &RunnerSink,
    config: &HotRunnerConfig,
    runtime: &ReloadableRunnerConfig,
    request: RunnerRequest,
    error: String,
) -> Result<bool, SubmitResultError> {
    let request_id = request.request_id.clone();
    match request.kind.as_str() {
        "start_job"
        | "start_validation_job"
        | "start_process_job"
        | "start_detached_process_job"
        | "start_script_job" => {
            if submit_invalid_job_start(sink, &request, error.clone()) {
                Ok(true)
            } else {
                sink.submit_result_with_metadata(
                    request_id,
                    invalid_command(format!(
                        "invalid_runner_operation: {error}; command was not started"
                    )),
                    config,
                    runtime,
                )
                .map(|_| true)
            }
        }
        "coding_agent" => sink
            .submit_coding_agent_result(
                request_id,
                webcodex_core::coding_agent::CodingAgentResponse::error(
                    webcodex_core::coding_agent::CodingAgentDispatchState::NotStarted,
                    "invalid_coding_agent_request",
                    error,
                    Some("invalid_input"),
                    Some("fix_input"),
                ),
            )
            .map(|_| true),
        "mcp_gateway" => sink
            .submit_mcp_gateway_result(
                request_id,
                crate::mcp_gateway::McpGatewayResponse::error(
                    crate::mcp_gateway::McpGatewayDispatchState::NotStarted,
                    "invalid_bridge_request",
                    error,
                ),
            )
            .map(|_| true),
        "plugin_gateway" => sink
            .submit_plugin_gateway_result(
                request_id,
                webcodex_core::plugin::PluginGatewayResponse::error(
                    webcodex_core::plugin::PluginDispatchState::NotStarted,
                    "invalid_plugin_request",
                    error,
                ),
            )
            .map(|_| true),
        "persistent_shell" => {
            if let Some(result) = invalid_persistent_shell_result(&request, error.clone()) {
                sink.submit_persistent_shell_result(request_id, result)
                    .map(|_| true)
            } else {
                sink.submit_result_with_metadata(
                    request_id,
                    invalid_command(format!(
                        "invalid_request: {error}; command was not started"
                    )),
                    config,
                    runtime,
                )
                .map(|_| true)
            }
        }
        "run_shell" => sink
            .submit_shell_result_with_metadata(
                request_id,
                ShellCommandResult::not_started(invalid_command(format!(
                    "invalid_raw_shell_request: {error}; command was not started"
                ))),
                config,
                runtime,
            )
            .map(|_| true),
        "run_process" | "run_script" | "run_internal_posix_script" => sink
            .submit_shell_result_with_metadata(
                request_id,
                ShellCommandResult::not_started(invalid_command(format!(
                    "invalid_runner_operation: {error}; command was not started"
                ))),
                config,
                runtime,
            )
            .map(|_| true),
        kind if kind.starts_with("file_") && !RunnerFileOperation::is_wire_kind(kind) => sink
            .submit_result_with_metadata(
                request_id,
                invalid_command(
                    "unsupported_file_request_kind: unsupported file request kind; command was not started",
                ),
                config,
                runtime,
            )
            .map(|_| true),
        _ => sink
            .submit_result_with_metadata(
                request_id,
                invalid_command(format!(
                    "invalid_runner_operation: {error}; command was not started"
                )),
                config,
                runtime,
            )
            .map(|_| true),
    }
}

/// Execute one V2 request through the transport-neutral typed Runner ingress.
/// Polling, WebSocket, and QUIC all reach this function with the same wire DTO;
/// operation semantics are decoded exactly once before this exhaustive match.
pub(crate) fn dispatch_request_with_outcome(
    sink: &RunnerSink,
    config: &HotRunnerConfig,
    runtime: &ReloadableRunnerConfig,
    jobs: &JobManager,
    persistent_shells: &PersistentShellManager,
    project_registry_dir: &Path,
    lsp: &LspSupervisor,
    request: RunnerRequest,
) -> Result<RunnerDispatchOutcome, SubmitResultError> {
    if runner_tool_trace_enabled() {
        tracing::info!(
            event = "runner_tool_dispatch_started",
            runner_request_id = %request.request_id,
            runner_client_id = %request.client_id,
            runner_request_kind = %request.kind,
            runner_job_id = request.job_id.as_deref().unwrap_or("-"),
            runner_agent_instance_id = sink.runner_instance_id(),
            "runner_tool_dispatch_started"
        );
    }
    if runtime.shutdown_flag().load(Ordering::SeqCst) {
        return Ok(RunnerDispatchOutcome {
            handled: false,
            project_cache_invalidation_required: false,
        });
    }

    let invocation = match request.decode_invocation() {
        Ok(invocation) => invocation,
        Err(error) => {
            return submit_decode_failure(sink, config, runtime, request, error).map(|handled| {
                RunnerDispatchOutcome {
                    handled,
                    project_cache_invalidation_required: false,
                }
            })
        }
    };
    let project_cache_invalidation_required =
        matches!(&invocation.operation, RunnerOperation::Project(_));
    let request_id = invocation.metadata.request_id.clone();
    let client_id = invocation.metadata.client_id.clone();
    let policy = &config.policy;
    let shell = &config.shell;

    let dispatch_result = match invocation.operation {
        RunnerOperation::CodingAgent(operation) => {
            let response = match runtime.coding_agents() {
                Some(manager) => manager.handle(operation, project_registry_dir),
                None => webcodex_core::coding_agent::CodingAgentResponse::error(
                    webcodex_core::coding_agent::CodingAgentDispatchState::NotStarted,
                    "coding_agent_unavailable",
                    "Runner ACP coding-agent execution is not configured/available",
                    Some("unavailable"),
                    Some("reobserve"),
                ),
            };
            sink.submit_coding_agent_result(request_id, response)
                .map(|_| true)
        }
        RunnerOperation::McpGateway(operation) => sink
            .submit_mcp_gateway_result(request_id, runtime.mcp_gateway().handle(operation))
            .map(|_| true),
        RunnerOperation::PluginGateway(operation) => sink
            .submit_plugin_gateway_result(request_id, runtime.plugins().handle(operation))
            .map(|_| true),
        RunnerOperation::RunnerConfig(operation) => {
            let result = handle_runner_config_operation(runtime, &operation);
            // A reload may have replaced the snapshot passed into dispatch_request.
            let current = runtime.snapshot();
            sink.submit_result_with_metadata(request_id, result, &current, runtime)
                .map(|_| true)
        }
        RunnerOperation::SshResource(operation) => {
            let response = runtime.managed_ssh().handle(&config.static_ssh, operation);
            let result = CommandResult {
                exit_code: Some(0),
                stdout: Some(
                    serde_json::to_string(&response)
                        .expect("managed SSH resource response serialization is infallible"),
                ),
                stderr: None,
                duration_ms: Some(0),
                error: None,
            };
            sink.submit_result_with_metadata(request_id, result, config, runtime)
                .map(|_| true)
        }
        RunnerOperation::SkillStore(operation) => {
            let result = handle_skill_store_request(
                runtime.client_id(),
                runtime.server_url(),
                policy,
                operation,
            );
            sink.submit_result_with_metadata(request_id, result, config, runtime)
                .map(|_| true)
        }
        RunnerOperation::Computer(operation) => {
            let result = handle_computer_operation(&operation);
            sink.submit_result_with_metadata(request_id, result, config, runtime)
                .map(|_| true)
        }
        RunnerOperation::RunProcess(operation) => {
            let result = run_process_with_profiles_and_execution_state(
                config.generation,
                policy,
                shell,
                project_registry_dir,
                &jobs.prepared_profiles,
                operation.cwd.as_deref(),
                &operation.process.executable,
                &operation.process.args,
                operation.stdin.as_deref(),
                operation.timeout_secs,
                Some(runtime.shutdown_flag()),
            );
            sink.submit_shell_result_with_metadata(request_id, result, config, runtime)
                .map(|_| true)
        }
        RunnerOperation::RunScript(operation) => {
            let result = run_script_with_profiles_and_execution_state(
                config.generation,
                policy,
                shell,
                project_registry_dir,
                &jobs.prepared_profiles,
                operation.cwd.as_deref(),
                &operation.script,
                operation.stdin.as_deref(),
                operation.timeout_secs,
                Some(runtime.shutdown_flag()),
            );
            sink.submit_shell_result_with_metadata(request_id, result, config, runtime)
                .map(|_| true)
        }
        RunnerOperation::RunInternalPosixScript(operation) => {
            let result = run_internal_posix_script_with_profiles_and_execution_state(
                config.generation,
                policy,
                shell,
                project_registry_dir,
                &jobs.prepared_profiles,
                operation.cwd.as_deref(),
                &operation.script.script,
                operation.timeout_secs,
                Some(runtime.shutdown_flag()),
            );
            sink.submit_shell_result_with_metadata(request_id, result, config, runtime)
                .map(|_| true)
        }
        RunnerOperation::PersistentShell(operation) => {
            let identity = (
                operation.request.shell_id.clone(),
                operation.request.workflow_session_id.clone(),
                operation.request.runtime_project_id.clone(),
            );
            let result = persistent_shells.handle_operation(
                policy,
                shell,
                &config.ssh,
                config.generation,
                project_registry_dir,
                &client_id,
                &operation,
            );
            let submitted = sink.submit_persistent_shell_result(request_id, result);
            if !matches!(&submitted, Ok(ResultSubmission::Accepted)) {
                let _ = persistent_shells.close_exact(
                    &identity.0,
                    &identity.1,
                    &identity.2,
                    "persistent_shell_result_not_accepted",
                );
            }
            submitted.map(|_| true)
        }
        RunnerOperation::RunShell(operation) => {
            let ssh_resource = operation
                .job_context
                .as_ref()
                .and_then(|context| context.ssh_resource.as_deref());
            let ssh_session_id = operation
                .job_context
                .as_ref()
                .and_then(|context| context.workflow_session_id.as_deref());
            if let Some(resource) = ssh_resource {
                let result = match ssh_session_id {
                    Some(session_id) => run_ssh_shell_with_execution_state(
                        &jobs.ssh_pool,
                        config.generation,
                        &config.ssh,
                        policy,
                        resource,
                        session_id,
                        operation.cwd.as_deref(),
                        &operation.command,
                        operation.stdin.as_deref(),
                        operation.timeout_secs,
                        Some(runtime.shutdown_flag()),
                    ),
                    None => ShellCommandResult::not_started(invalid_command(
                        "ssh_session_required: an SSH resource requires a Workflow Session id; command was not started",
                    )),
                };
                sink.submit_shell_result_with_metadata(request_id, result, config, runtime)
                    .map(|_| true)
            } else {
                match config.external_tools.route_with_shutdown(
                    policy,
                    &operation,
                    Some(runtime.shutdown_flag()),
                ) {
                    ExternalRoute::Handled(result) => sink
                        .submit_result_with_metadata(request_id, result, config, runtime)
                        .map(|_| true),
                    ExternalRoute::NativeFallback(fallback) => {
                        let result = run_native_shell_or_internal_search(
                            config,
                            runtime,
                            jobs,
                            project_registry_dir,
                            &operation,
                        );
                        config
                            .external_tools
                            .complete_native_fallback(fallback, &result.result);
                        sink.submit_shell_result_with_metadata(request_id, result, config, runtime)
                            .map(|_| true)
                    }
                    ExternalRoute::Native => {
                        let result = run_native_shell_or_internal_search(
                            config,
                            runtime,
                            jobs,
                            project_registry_dir,
                            &operation,
                        );
                        sink.submit_shell_result_with_metadata(request_id, result, config, runtime)
                            .map(|_| true)
                    }
                }
            }
        }
        RunnerOperation::File(operation) => {
            let result = handle_file_operation(policy, &operation);
            sink.submit_result_with_metadata(request_id, result, config, runtime)
                .map(|_| true)
        }
        RunnerOperation::Project(operation) => {
            let kind = operation.kind;
            let result = match kind {
                RunnerProjectOperationKind::Register | RunnerProjectOperationKind::Create => {
                    handle_project_operation(policy, project_registry_dir, &client_id, &operation)
                }
                RunnerProjectOperationKind::ResolveOrRegister => {
                    handle_resolve_or_register_project_operation(
                        policy,
                        project_registry_dir,
                        &client_id,
                        &operation,
                    )
                }
                RunnerProjectOperationKind::PrepareManagedWorktree => {
                    handle_prepare_managed_worktree_operation(
                        policy,
                        project_registry_dir,
                        &client_id,
                        &operation,
                    )
                }
                RunnerProjectOperationKind::LifecycleEnable
                | RunnerProjectOperationKind::LifecycleDisable
                | RunnerProjectOperationKind::LifecycleUnregister => {
                    handle_project_lifecycle_operation(policy, project_registry_dir, &operation)
                }
            };
            if result.exit_code == Some(0) && kind.disables_execution() {
                if let Some(project_id) = lifecycle_project_id(&operation) {
                    let runtime_project_id = format!("agent:{client_id}:{project_id}");
                    persistent_shells
                        .close_project(&runtime_project_id, "project_execution_disabled");
                }
            }
            sink.submit_result_with_metadata(request_id, result, config, runtime)
                .map(|_| true)
        }
        RunnerOperation::Lsp {
            payload,
            timeout_secs,
        } => {
            let result =
                handle_lsp_operation(policy, project_registry_dir, lsp, &payload, timeout_secs);
            sink.submit_result_with_metadata(request_id, result, config, runtime)
                .map(|_| true)
        }
        RunnerOperation::Validation { payload, .. } => {
            let result = handle_validation_request(
                policy,
                project_registry_dir,
                &payload,
                Some(runtime.shutdown_flag()),
            );
            sink.submit_result_with_metadata(request_id, result, config, runtime)
                .map(|_| true)
        }
        RunnerOperation::Job(operation) => match operation {
            RunnerJobOperation::Stop { job_id } => {
                jobs.install_sink(sink.clone());
                if let Err(error) = jobs.stop(&job_id) {
                    eprintln!("webcodex-runner stop_job error: {error}");
                }
                Ok(true)
            }
            RunnerJobOperation::StartShell(_)
            | RunnerJobOperation::StartValidation(_)
            | RunnerJobOperation::StartProcess(_)
            | RunnerJobOperation::StartDetachedProcess(_)
            | RunnerJobOperation::StartScript(_) => {
                // Commit 4 removes this temporary wire handoff inside JobManager;
                // semantic dispatch has already been closed here.
                jobs.enqueue(
                    sink.clone(),
                    PendingJobStart {
                        generation: config.generation,
                        policy: policy.clone(),
                        shell: shell.clone(),
                        ssh: config.ssh.clone(),
                        project_registry_dir: project_registry_dir.to_path_buf(),
                        request,
                    },
                );
                Ok(true)
            }
        },
    };
    dispatch_result.map(|handled| RunnerDispatchOutcome {
        handled,
        project_cache_invalidation_required,
    })
}

#[cfg(test)]
pub(crate) fn dispatch_request(
    sink: &RunnerSink,
    config: &HotRunnerConfig,
    runtime: &ReloadableRunnerConfig,
    jobs: &JobManager,
    persistent_shells: &PersistentShellManager,
    project_registry_dir: &Path,
    lsp: &LspSupervisor,
    request: RunnerRequest,
) -> Result<bool, SubmitResultError> {
    dispatch_request_with_outcome(
        sink,
        config,
        runtime,
        jobs,
        persistent_shells,
        project_registry_dir,
        lsp,
        request,
    )
    .map(|outcome| outcome.handled)
}

fn lifecycle_project_id(operation: &RunnerProjectOperation) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&operation.payload)
        .ok()
        .and_then(|payload| {
            payload
                .get("project_id")
                .and_then(|project_id| project_id.as_str())
                .map(str::to_string)
        })
}
