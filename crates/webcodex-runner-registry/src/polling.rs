use super::jobs::{
    assert_active_instance_locked, observe_job_terminal, replace_log_limited, request_preview,
    retain_ordinary_result_stream, retain_result_stream_to,
};
use super::requests::{remove_pending_request_locked, take_pending_request_locked};
use super::state::JobLifecycleState;
use super::validation::{validate_id, validate_runner_instance_id};
use super::{now_ts, RunnerFeature, RunnerRegistry};
use webcodex_core::coding_agent::{
    validate_response_for_request as validate_coding_agent_response, CodingAgentDispatchState,
    CodingAgentResponse,
};
use webcodex_core::mcp_gateway::{
    validate_response as validate_mcp_gateway_response, McpGatewayDispatchState, McpGatewayResponse,
};
use webcodex_core::plugin::{
    validate_response_for_request as validate_plugin_gateway_response, PluginDispatchState,
    PluginGatewayResponse,
};
use webcodex_core::runner_operation::{RunnerJobOperation, RunnerOperation};
use webcodex_core::runner_protocol::{
    RunnerPersistentShellResultRequest, RunnerPollRequest, RunnerRequest, RunnerResultPayload,
    ShellCommandExecutionState, ShellRunResponse,
};

impl RunnerRegistry {
    /// Polling-transport entry point. Polling registrations do not carry a
    /// server-internal `connection_id`, so this requires only the public
    /// `client_id` / `agent_instance_id` lease. The HTTP `/poll` handler uses
    /// this path; long-lived transports (WebSocket/QUIC) use
    /// [`RunnerRegistry::poll_for_connection`] instead so an older
    /// same-instance connection cannot steal requests from the current lease.
    pub async fn poll(&self, body: RunnerPollRequest) -> Result<Option<RunnerRequest>, String> {
        self.poll_checked(body, None).await
    }

    /// Connection-scoped entry point for long-lived transports. The pump for a
    /// concrete WebSocket/QUIC connection passes its captured `connection_id`
    /// so a stale same-instance connection (whose lease was replaced by a
    /// reconnect) is rejected before it can dequeue a request that belongs to
    /// the new connection.
    pub async fn poll_for_connection(
        &self,
        body: RunnerPollRequest,
        connection_id: &str,
    ) -> Result<Option<RunnerRequest>, String> {
        self.poll_checked(body, Some(connection_id)).await
    }

    /// Unified dequeue path. Every check below — `client_id`,
    /// `agent_instance_id`, and (when provided) the current `connection_id`
    /// — runs while holding the same registry mutex as the queue mutation,
    /// `last_seen` update, project projection and `dispatched`/job-state
    /// transitions. A stale connection returns a stable error with no side
    /// effects: no `last_seen` refresh, no project update, no `pop_front()`,
    /// no `dispatched=true`, and no job-state change.
    async fn poll_checked(
        &self,
        body: RunnerPollRequest,
        expected_connection_id: Option<&str>,
    ) -> Result<Option<RunnerRequest>, String> {
        validate_id(&body.client_id, "client_id")?;
        validate_runner_instance_id(&body.runner_instance_id)?;
        let mut inner = self.inner.lock().await;
        {
            let Some(runner) = inner.runners.get_mut(&body.client_id) else {
                return Err(format!("unknown shell client: {}", body.client_id));
            };
            if runner.runner_instance_id != body.runner_instance_id {
                return Err(format!(
                    "runner {} is no longer the active instance (stale or replaced)",
                    body.client_id
                ));
            }
            if let Some(expected) = expected_connection_id {
                if runner.connection_id.as_deref() != Some(expected) {
                    return Err(format!(
                        "runner {} transport connection is no longer active",
                        body.client_id
                    ));
                }
            }
            runner.last_seen = now_ts();
        }
        loop {
            let request_id = {
                let Some(queue) = inner.queues_by_runner.get_mut(&body.client_id) else {
                    return Ok(None);
                };
                queue.pop_front()
            };
            let Some(request_id) = request_id else {
                return Ok(None);
            };
            let stale_runner_config_error =
                inner.pending_by_id.get(&request_id).and_then(|pending| {
                    match (
                        &pending.operation,
                        pending.expected_runner_config_runner_instance_id.as_deref(),
                    ) {
                        (RunnerOperation::RunnerConfig(_), Some(expected)) => {
                            let Some(runner) = inner.runners.get(&body.client_id) else {
                                return Some((
                                    "runner_replaced",
                                    "Exact Runner disappeared before config operation dispatch"
                                        .to_string(),
                                ));
                            };
                            if runner.runner_instance_id != expected {
                                return Some((
                                    "runner_replaced",
                                    "Exact Runner changed before config operation dispatch"
                                        .to_string(),
                                ));
                            }
                            (!runner
                                .runner_features
                                .supports(RunnerFeature::RunnerConfigControl))
                            .then_some((
                                "capability_unavailable",
                                "Runner config-control capability changed before dispatch"
                                    .to_string(),
                            ))
                        }
                        (RunnerOperation::RunnerConfig(_), None) => Some((
                            "runner_replaced",
                            "Runner config exact-process fence is missing".to_string(),
                        )),
                        (_, Some(_)) => Some((
                            "runner_replaced",
                            "Runner config exact-process fence is inconsistent".to_string(),
                        )),
                        _ => None,
                    }
                });
            if let Some((code, message)) = stale_runner_config_error {
                let Some(mut pending) = inner.pending_by_id.remove(&request_id) else {
                    continue;
                };
                if let Some(waiter) = pending.waiter.take() {
                    let _ = waiter.send(ShellRunResponse {
                        success: false,
                        request_id: request_id.clone(),
                        client_id: body.client_id.clone(),
                        cwd: None,
                        command_preview: String::new(),
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: None,
                        error: Some(format!("{code}: {message}")),
                        request_dispatched: Some(false),
                        command_execution_state: Some(ShellCommandExecutionState::NotStarted),
                    });
                }
                continue;
            }
            let stale_ssh_resource_error =
                inner.pending_by_id.get(&request_id).and_then(|pending| {
                    match (
                        &pending.operation,
                        pending.expected_ssh_resource_runner_instance_id.as_deref(),
                    ) {
                        (RunnerOperation::SshResource(_), Some(expected_runner)) => {
                            let Some(runner) = inner.runners.get(&body.client_id) else {
                                return Some((
                                    "runner_replaced",
                                    "Exact Runner disappeared before SSH resource dispatch"
                                        .to_string(),
                                ));
                            };
                            if runner.runner_instance_id != expected_runner {
                                return Some((
                                    "runner_replaced",
                                    "Exact Runner changed before SSH resource dispatch".to_string(),
                                ));
                            }
                            (!runner
                                .runner_features
                                .supports(RunnerFeature::ManagedSshResources))
                            .then_some((
                                "ssh_resource_registry_unavailable",
                                "Managed SSH resource capability changed before dispatch"
                                    .to_string(),
                            ))
                        }
                        (RunnerOperation::SshResource(_), None) => Some((
                            "runner_replaced",
                            "SSH resource exact Runner fence is missing".to_string(),
                        )),
                        (_, Some(_)) => Some((
                            "runner_replaced",
                            "SSH resource exact Runner fence is inconsistent".to_string(),
                        )),
                        _ => None,
                    }
                });
            if let Some((code, message)) = stale_ssh_resource_error {
                let Some(mut pending) = inner.pending_by_id.remove(&request_id) else {
                    continue;
                };
                if let Some(waiter) = pending.waiter.take() {
                    let _ = waiter.send(ShellRunResponse {
                        success: false,
                        request_id: request_id.clone(),
                        client_id: body.client_id.clone(),
                        cwd: None,
                        command_preview: String::new(),
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: None,
                        error: Some(format!("{code}: {message}")),
                        request_dispatched: Some(false),
                        command_execution_state: Some(ShellCommandExecutionState::NotStarted),
                    });
                }
                continue;
            }
            let stale_bridge_error =
                inner.pending_by_id.get(&request_id).and_then(|pending| {
                    match (
                        &pending.operation,
                        pending.expected_mcp_gateway_runner_instance_id.as_deref(),
                        pending.expected_mcp_gateway_provider_id.as_deref(),
                        pending.expected_mcp_gateway_provider_instance_id.as_deref(),
                    ) {
                        (
                            RunnerOperation::McpGateway(operation),
                            Some(expected_runner),
                            Some(expected_provider),
                            Some(expected_provider_instance),
                        ) => {
                            if operation.provider_id() != expected_provider
                                || operation.provider_instance_id() != expected_provider_instance
                            {
                                return Some((
                                "stale_provider",
                                "stale_mcp_gateway: pending exact-provider fence is inconsistent"
                                    .to_string(),
                            ));
                            }
                            let Some(runner) = inner.runners.get(&body.client_id) else {
                                return Some((
                                    "stale_runner",
                                    "stale_mcp_gateway: target Runner disappeared before dispatch"
                                        .to_string(),
                                ));
                            };
                            if runner.runner_instance_id != expected_runner {
                                return Some((
                                    "stale_runner",
                                    "stale_mcp_gateway: target Runner changed before dispatch"
                                        .to_string(),
                                ));
                            }
                            let provider_is_current = runner
                                .policy
                                .as_ref()
                                .and_then(|policy| policy.mcp_gateway_providers.as_ref())
                                .is_some_and(|providers| {
                                    providers.iter().any(|provider| {
                                        provider.provider_id == expected_provider
                                            && provider.provider_instance_id
                                                == expected_provider_instance
                                    })
                                });
                            (!provider_is_current).then_some((
                                "stale_provider",
                                "stale_mcp_gateway: target provider changed before dispatch"
                                    .to_string(),
                            ))
                        }
                        (operation, None, None, None)
                            if !matches!(operation, RunnerOperation::McpGateway(_)) =>
                        {
                            None
                        }
                        _ => Some((
                            "stale_provider",
                            "stale_mcp_gateway: pending exact bridge fence is incomplete"
                                .to_string(),
                        )),
                    }
                });
            if let Some((bridge_code, error)) = stale_bridge_error {
                let Some(mut pending) = inner.pending_by_id.remove(&request_id) else {
                    continue;
                };
                if let Some(waiter) = pending.waiter.take() {
                    let response = ShellRunResponse {
                        success: false,
                        request_id: request_id.clone(),
                        client_id: body.client_id.clone(),
                        cwd: pending.request.cwd.clone(),
                        command_preview: request_preview(&pending.request),
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: None,
                        error: Some(error.clone()),
                        request_dispatched: Some(false),
                        command_execution_state: Some(ShellCommandExecutionState::NotStarted),
                    };
                    let _ = waiter.send(response);
                }
                if let Some(waiter) = inner.mcp_gateway_waiters.remove(&request_id) {
                    let _ = waiter.send(McpGatewayResponse::error(
                        McpGatewayDispatchState::NotStarted,
                        bridge_code,
                        if bridge_code == "stale_runner" {
                            "Exact Runner changed before bridge dispatch; request was not started"
                        } else {
                            "Exact provider changed before bridge dispatch; request was not started"
                        },
                    ));
                }
                inner.persistent_waiters.remove(&request_id);
                continue;
            }
            let stale_plugin_error = inner.pending_by_id.get(&request_id).and_then(|pending| {
                let RunnerOperation::PluginGateway(operation) = &pending.operation else {
                    return None;
                };
                let Some(fence) = inner.plugin_gateway_fences.get(&request_id) else {
                    return Some((
                        "stale_plugin_fence",
                        "native Plugin exact dispatch fence is missing".to_string(),
                    ));
                };
                let operation_binding = operation.provider_binding();
                let fence_binding = fence
                    .provider
                    .as_ref()
                    .map(|(provider, instance)| (provider.as_str(), instance.as_str()));
                if operation_binding != fence_binding {
                    return Some((
                        "stale_plugin_provider",
                        "native Plugin provider binding changed before dispatch".to_string(),
                    ));
                }
                let Some(runner) = inner.runners.get(&body.client_id) else {
                    return Some((
                        "stale_runner",
                        "native Plugin target Runner disappeared before dispatch".to_string(),
                    ));
                };
                if runner.runner_instance_id != fence.runner_instance_id {
                    return Some((
                        "stale_runner",
                        "native Plugin target Runner changed before dispatch".to_string(),
                    ));
                }
                if !runner
                    .runner_features
                    .supports(RunnerFeature::NativeToolPlugins)
                {
                    return Some((
                        "plugin_capability_unavailable",
                        "native Plugin capability changed before dispatch".to_string(),
                    ));
                }
                None
            });
            if let Some((code, message)) = stale_plugin_error {
                let Some(mut pending) = inner.pending_by_id.remove(&request_id) else {
                    continue;
                };
                if let Some(waiter) = pending.waiter.take() {
                    let _ = waiter.send(ShellRunResponse {
                        success: false,
                        request_id: request_id.clone(),
                        client_id: body.client_id.clone(),
                        cwd: pending.request.cwd.clone(),
                        command_preview: request_preview(&pending.request),
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: None,
                        error: Some(message.clone()),
                        request_dispatched: Some(false),
                        command_execution_state: Some(ShellCommandExecutionState::NotStarted),
                    });
                }
                if let Some(waiter) = inner.plugin_gateway_waiters.remove(&request_id) {
                    let _ = waiter.send(PluginGatewayResponse::error(
                        PluginDispatchState::NotStarted,
                        code,
                        "Exact native Plugin dispatch target changed before dispatch; request was not started",
                    ));
                }
                inner.plugin_gateway_fences.remove(&request_id);
                inner.mcp_gateway_waiters.remove(&request_id);
                inner.coding_agent_waiters.remove(&request_id);
                inner.coding_agent_fences.remove(&request_id);
                inner.persistent_waiters.remove(&request_id);
                continue;
            }
            let stale_skill_store_error =
                inner.pending_by_id.get(&request_id).and_then(|pending| {
                    match (&pending.operation, pending.skill_store_fence.as_ref()) {
                        (RunnerOperation::SkillStore(_), Some(fence)) if !fence.configured_roots => {
                            let Some(runner) = inner.runners.get(&body.client_id) else {
                                return Some(
                                    "stale_runner: Skill store target Runner disappeared before dispatch"
                                        .to_string(),
                                );
                            };
                            if runner.runner_instance_id != fence.runner_instance_id {
                                return Some(
                                    "stale_runner: Skill store target Runner changed before dispatch"
                                        .to_string(),
                                );
                            }
                            let required = if fence.management {
                                RunnerFeature::SkillStoreManage
                            } else {
                                RunnerFeature::SkillStoreRead
                            };
                            (!runner.runner_features.supports(required)).then(|| {
                                format!(
                                    "skill_store_capability_unavailable: exact Runner no longer advertises {} before dispatch",
                                    required.as_wire_name()
                                )
                            })
                        }
                        (RunnerOperation::ConfiguredSkillRoots(_), Some(fence))
                            if fence.configured_roots && !fence.management =>
                        {
                            let Some(runner) = inner.runners.get(&body.client_id) else {
                                return Some(
                                    "stale_runner: configured Skill roots target Runner disappeared before dispatch"
                                        .to_string(),
                                );
                            };
                            if runner.runner_instance_id != fence.runner_instance_id {
                                return Some(
                                    "stale_runner: configured Skill roots target Runner changed before dispatch"
                                        .to_string(),
                                );
                            }
                            (!runner
                                .runner_features
                                .supports(RunnerFeature::ConfiguredSkillRootsRead))
                            .then(|| {
                                "configured_skill_roots_capability_unavailable: exact Runner no longer advertises configured_skill_roots_read before dispatch"
                                    .to_string()
                            })
                        }
                        (RunnerOperation::SkillStore(_) | RunnerOperation::ConfiguredSkillRoots(_), None) => Some(
                            "stale_runner: Skill source exact dispatch fence is missing".to_string(),
                        ),
                        (RunnerOperation::SkillStore(_) | RunnerOperation::ConfiguredSkillRoots(_), Some(_)) => Some(
                            "stale_runner: Skill source dispatch fence mode does not match the request kind"
                                .to_string(),
                        ),
                        (_, Some(_)) => Some(
                            "stale_runner: Skill source dispatch fence is attached to the wrong request kind"
                                .to_string(),
                        ),
                        (_, None) => None,
                    }
                });
            if let Some(error) = stale_skill_store_error {
                let Some(mut pending) = inner.pending_by_id.remove(&request_id) else {
                    continue;
                };
                if let Some(waiter) = pending.waiter.take() {
                    let response = ShellRunResponse {
                        success: false,
                        request_id: request_id.clone(),
                        client_id: body.client_id.clone(),
                        cwd: None,
                        command_preview: request_preview(&pending.request),
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: None,
                        error: Some(error),
                        request_dispatched: Some(false),
                        command_execution_state: Some(ShellCommandExecutionState::NotStarted),
                    };
                    let _ = waiter.send(response);
                }
                inner.persistent_waiters.remove(&request_id);
                continue;
            }
            let stale_coding_agent_error =
                inner.pending_by_id.get(&request_id).and_then(|pending| {
                    if !matches!(&pending.operation, RunnerOperation::CodingAgent(_)) {
                        return None;
                    }
                    let Some(fence) = inner.coding_agent_fences.get(&request_id) else {
                        return Some((
                            "stale_coding_agent_fence",
                            "CodingAgentRun exact dispatch fence is missing".to_string(),
                        ));
                    };
                    let Some(runner) = inner.runners.get(&body.client_id) else {
                        return Some((
                            "stale_runner",
                            "CodingAgentRun target Runner disappeared before dispatch".to_string(),
                        ));
                    };
                    if runner.runner_instance_id != fence.runner_instance_id {
                        return Some((
                            "stale_runner",
                            "CodingAgentRun target Runner changed before dispatch".to_string(),
                        ));
                    }
                    if !runner.coding_agent_providers.iter().any(|provider| {
                        provider.provider_id == fence.provider_id
                            && provider.provider_instance_id == fence.provider_instance_id
                    }) {
                        return Some((
                            "stale_provider",
                            "CodingAgentRun target ACP provider changed before dispatch"
                                .to_string(),
                        ));
                    }
                    None
                });
            if let Some((code, message)) = stale_coding_agent_error {
                inner.pending_by_id.remove(&request_id);
                if let Some(waiter) = inner.coding_agent_waiters.remove(&request_id) {
                    let _ = waiter.send(CodingAgentResponse::error(
                        CodingAgentDispatchState::NotStarted,
                        code,
                        message,
                        Some("stale_state"),
                        Some("reobserve"),
                    ));
                }
                inner.coding_agent_fences.remove(&request_id);
                inner.mcp_gateway_waiters.remove(&request_id);
                inner.persistent_waiters.remove(&request_id);
                continue;
            }
            let stale_project_error = inner.pending_by_id.get(&request_id).and_then(|pending| {
                match (
                    pending.expected_project_id.as_deref(),
                    pending.expected_project_cwd.as_deref(),
                ) {
                    (Some(project_id), Some(project_cwd)) => match inner.runners.get(&body.client_id) {
                        Some(runner) if runner.owner != pending.expected_runner_owner => Some(
                            "stale_authority: target Runner owner changed before dispatch".to_string(),
                        ),
                        Some(runner)
                            if !runner.runner_features.supports(RunnerFeature::FileWrite) =>
                        {
                            Some(
                            "stale_authority: target Runner no longer advertises file_write before dispatch"
                                .to_string(),
                            )
                        }
                        Some(runner)
                            if runner.projects.iter().any(|project| {
                                !project.disabled
                                    && project.id == project_id
                                    && project.path == project_cwd
                            }) => None,
                        Some(_) | None => Some(format!(
                            "stale_project: target project {project_id} is no longer registered at the resolved path"
                        )),
                    },
                    (None, None) => None,
                    _ => Some(
                        "stale_project: pending project placement fence is incomplete".to_string(),
                    ),
                }
            });
            if let Some(error) = stale_project_error {
                let Some(mut pending) = inner.pending_by_id.remove(&request_id) else {
                    continue;
                };
                if let Some(waiter) = pending.waiter.take() {
                    let response = ShellRunResponse {
                        success: false,
                        request_id: request_id.clone(),
                        client_id: body.client_id.clone(),
                        cwd: pending.request.cwd.clone(),
                        command_preview: request_preview(&pending.request),
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: None,
                        error: Some(error),
                        request_dispatched: Some(false),
                        command_execution_state: Some(ShellCommandExecutionState::NotStarted),
                    };
                    let _ = waiter.send(response);
                }
                inner.persistent_waiters.remove(&request_id);
                continue;
            }
            let Some((request, operation, job_id)) =
                inner.pending_by_id.get_mut(&request_id).map(|pending| {
                    pending.dispatched = true;
                    (
                        pending.request.clone(),
                        pending.operation.clone(),
                        pending.job_id.clone(),
                    )
                })
            else {
                continue;
            };
            if matches!(
                &operation,
                RunnerOperation::Job(RunnerJobOperation::Stop { .. })
            ) {
                inner.pending_by_id.remove(&request_id);
                return Ok(Some(request));
            }
            if let Some(job_id) = job_id {
                if let Some(job) = inner.jobs_by_id.get_mut(&job_id) {
                    if job.lifecycle == JobLifecycleState::Queued {
                        job.lifecycle = JobLifecycleState::RunnerQueued;
                        // Dispatch proves only that the Runner accepted the
                        // Job request. A typed structured Job becomes started
                        // only when the Runner reports `running` after a
                        // successful child spawn.
                        if job.structured_execution.is_none() {
                            job.started_at = Some(now_ts());
                        }
                        super::jobs::notify_job_update(job);
                    }
                }
            }
            return Ok(Some(request));
        }
    }

    /// Polling-transport result entry point. Requires the public
    /// `client_id` / `agent_instance_id` lease and refreshes `last_seen` for
    /// the active instance. Used by the HTTP `/result` handler.
    pub async fn complete(&self, payload: impl Into<RunnerResultPayload>) -> Result<(), String> {
        self.complete_checked(payload.into(), None).await
    }

    /// Connection-scoped result entry point for long-lived transports. A
    /// late, legitimately-dispatched result arriving on a stale
    /// same-instance connection (the request was polled before the
    /// transport reconnect) is still accepted — it belongs to the same
    /// Runner instance and is gated by request/job ownership — but it must
    /// not refresh the new connection's `last_seen` liveness. Only the
    /// connection that currently holds the lease refreshes liveness.
    pub async fn complete_for_connection(
        &self,
        payload: RunnerResultPayload,
        connection_id: &str,
    ) -> Result<(), String> {
        self.complete_checked(payload, Some(connection_id)).await
    }

    async fn complete_checked(
        &self,
        payload: RunnerResultPayload,
        expected_connection_id: Option<&str>,
    ) -> Result<(), String> {
        let body = &payload.result;
        validate_id(&body.client_id, "client_id")?;
        validate_id(&body.request_id, "request_id")?;
        validate_runner_instance_id(&body.runner_instance_id)?;
        let mut inner = self.inner.lock().await;
        // Reject results from a stale/replaced instance before refreshing
        // liveness: a dead process must not update the active lease's
        // `last_seen` or resolve its waiters.
        assert_active_instance_locked(&inner, &body.client_id, &body.runner_instance_id)?;
        // Refresh liveness only for the connection that currently holds the
        // transport lease. A late result on a stale same-instance connection
        // is still processed below, but it must not make the new connection
        // appear online.
        if expected_connection_id.is_none()
            || inner
                .runners
                .get(&body.client_id)
                .is_some_and(|runner| runner.connection_id.as_deref() == expected_connection_id)
        {
            if let Some(runner) = inner.runners.get_mut(&body.client_id) {
                runner.last_seen = now_ts();
            }
        }
        let Some(pending) = inner.pending_by_id.get(&body.request_id) else {
            return Err(format!(
                "unknown or expired shell request: {}",
                body.request_id
            ));
        };
        if pending.request.client_id != body.client_id {
            return Err("request_id does not belong to client_id".to_string());
        }
        if !matches!(&pending.operation, RunnerOperation::CodingAgent(_))
            && payload.coding_agent.is_some()
        {
            return Err("unexpected CodingAgentRun result for non-coding request".to_string());
        }
        if !matches!(&pending.operation, RunnerOperation::McpGateway(_))
            && payload.mcp_gateway.is_some()
        {
            return Err("unexpected MCP gateway result for non-bridge request".to_string());
        }
        if !matches!(&pending.operation, RunnerOperation::PluginGateway(_))
            && payload.plugin_gateway.is_some()
        {
            return Err("unexpected Plugin gateway result for non-Plugin request".to_string());
        }
        self.telemetry
            .runner_result_accepted(&body.request_id, &payload);
        let trace_request_id = body.request_id.clone();
        let RunnerResultPayload {
            result: body,
            command_execution_state,
            mcp_gateway,
            plugin_gateway,
            coding_agent,
        } = payload;
        let Some(mut pending) = take_pending_request_locked(&mut inner, &body.request_id) else {
            return Err(format!(
                "unknown or expired shell request: {}",
                body.request_id
            ));
        };
        if matches!(&pending.operation, RunnerOperation::McpGateway(_)) {
            let response = match mcp_gateway {
                Some(response)
                    if command_execution_state.is_none()
                        && body.exit_code.is_none()
                        && body.stdout.is_none()
                        && body.stderr.is_none()
                        && body.duration_ms.is_none()
                        && body.error.is_none()
                        && validate_mcp_gateway_response(&response).is_ok() =>
                {
                    response
                }
                _ => McpGatewayResponse::error(
                    if pending.dispatched {
                        McpGatewayDispatchState::OutcomeUnknown
                    } else {
                        McpGatewayDispatchState::NotStarted
                    },
                    "invalid_runner_response",
                    if pending.dispatched {
                        "Runner returned an invalid bridge response after dispatch; downstream outcome is unknown and must not be retried automatically"
                    } else {
                        "Runner returned an invalid bridge response before provider dispatch"
                    },
                ),
            };
            let waiter = inner.mcp_gateway_waiters.remove(&body.request_id);
            if let Some(waiter) = waiter {
                let _ = waiter.send(response);
            }
            self.telemetry.runner_result_finalized(&trace_request_id);
            return Ok(());
        }
        if let RunnerOperation::PluginGateway(plugin_request) = &pending.operation {
            let response = match plugin_gateway {
                Some(response)
                    if command_execution_state.is_none()
                        && mcp_gateway.is_none()
                        && coding_agent.is_none()
                        && body.exit_code.is_none()
                        && body.stdout.is_none()
                        && body.stderr.is_none()
                        && body.duration_ms.is_none()
                        && body.error.is_none()
                        && validate_plugin_gateway_response(plugin_request, &response).is_ok() =>
                {
                    response
                }
                _ => PluginGatewayResponse::error(
                    if pending.dispatched {
                        PluginDispatchState::OutcomeUnknown
                    } else {
                        PluginDispatchState::NotStarted
                    },
                    "invalid_runner_response",
                    if pending.dispatched {
                        "Runner returned an invalid Plugin response after dispatch; downstream outcome is unknown and must not be retried automatically"
                    } else {
                        "Runner returned an invalid Plugin response before provider dispatch"
                    },
                ),
            };
            let waiter = inner.plugin_gateway_waiters.remove(&body.request_id);
            inner.plugin_gateway_fences.remove(&body.request_id);
            if let Some(waiter) = waiter {
                let _ = waiter.send(response);
            }
            self.telemetry.runner_result_finalized(&trace_request_id);
            return Ok(());
        }
        if let RunnerOperation::CodingAgent(coding_request) = &pending.operation {
            let response = match coding_agent {
                Some(response)
                    if command_execution_state.is_none()
                        && mcp_gateway.is_none()
                        && body.exit_code.is_none()
                        && body.stdout.is_none()
                        && body.stderr.is_none()
                        && body.duration_ms.is_none()
                        && body.error.is_none()
                        && validate_coding_agent_response(coding_request, &response).is_ok() =>
                {
                    response
                }
                _ => CodingAgentResponse::error(
                    if pending.dispatched {
                        CodingAgentDispatchState::OutcomeUnknown
                    } else {
                        CodingAgentDispatchState::NotStarted
                    },
                    "invalid_runner_response",
                    if pending.dispatched {
                        "Runner returned an invalid CodingAgentRun response after dispatch; reconcile the same run_id before any new initiation"
                    } else {
                        "Runner returned an invalid CodingAgentRun response before dispatch"
                    },
                    Some("protocol"),
                    Some("reobserve"),
                ),
            };
            let waiter = inner.coding_agent_waiters.remove(&body.request_id);
            inner.coding_agent_fences.remove(&body.request_id);
            if let Some(waiter) = waiter {
                let _ = waiter.send(response);
            }
            self.telemetry.runner_result_finalized(&trace_request_id);
            return Ok(());
        }
        let request_id = body.request_id.clone();
        let client_id = body.client_id.clone();
        let error = body.error.clone();
        // Ordinary command responses and durable Job logs are distinct retained
        // projections of the same Runner result. Keep the raw streams until both
        // projections have been derived so either retention contract can evolve
        // independently without silently constraining the other first.
        let raw_stdout = body.stdout;
        let raw_stderr = body.stderr;
        let stdout = if pending.operation.is_large_native_image_request() {
            retain_result_stream_to(
                raw_stdout.clone(),
                webcodex_core::artifact_policy::MAX_MCP_IMAGE_RESPONSE_BYTES,
            )
        } else {
            retain_ordinary_result_stream(raw_stdout.clone())
        };
        let stderr = retain_ordinary_result_stream(raw_stderr.clone());
        let success = matches!(
            command_execution_state,
            None | Some(ShellCommandExecutionState::Completed)
        ) && error.is_none()
            && body.exit_code == Some(0);
        let terminal_job_id = pending.job_id.clone();
        if let Some(job_id) = terminal_job_id.as_deref() {
            inner.request_to_job.remove(&request_id);
            if let Some(job) = inner.jobs_by_id.get_mut(job_id) {
                let terminal_now = now_ts();
                job.lifecycle = if success {
                    JobLifecycleState::Completed
                } else {
                    JobLifecycleState::Failed
                };
                observe_job_terminal(job, terminal_now);
                job.ended_at = Some(terminal_now);
                job.exit_code = body.exit_code;
                job.duration_ms = body.duration_ms;
                replace_log_limited(&mut job.stdout, raw_stdout);
                replace_log_limited(&mut job.stderr, raw_stderr);
                job.error = error.clone();
                super::jobs::notify_job_update(job);
            }
        }
        let request_preview = request_preview(&pending.request);
        let response = ShellRunResponse {
            success,
            request_id,
            client_id,
            cwd: pending.request.cwd,
            command_preview: request_preview,
            exit_code: body.exit_code,
            stdout,
            stderr,
            duration_ms: body.duration_ms,
            error,
            request_dispatched: Some(pending.dispatched),
            command_execution_state,
        };
        if let Some(waiter) = pending.waiter.take() {
            let _ = waiter.send(response);
        }
        if let Some(job_id) = terminal_job_id.as_deref() {
            self.telemetry
                .runner_job_finalized(Some(&trace_request_id), job_id);
        } else {
            self.telemetry.runner_result_finalized(&trace_request_id);
        }
        Ok(())
    }

    pub async fn complete_persistent_shell(
        &self,
        body: RunnerPersistentShellResultRequest,
    ) -> Result<(), String> {
        self.complete_persistent_shell_checked(body, None).await
    }

    pub async fn complete_persistent_shell_for_connection(
        &self,
        body: RunnerPersistentShellResultRequest,
        connection_id: &str,
    ) -> Result<(), String> {
        self.complete_persistent_shell_checked(body, Some(connection_id))
            .await
    }

    async fn complete_persistent_shell_checked(
        &self,
        mut body: RunnerPersistentShellResultRequest,
        expected_connection_id: Option<&str>,
    ) -> Result<(), String> {
        validate_id(&body.client_id, "client_id")?;
        validate_id(&body.request_id, "request_id")?;
        validate_runner_instance_id(&body.runner_instance_id)?;
        normalize_persistent_shell_result(&mut body.result)?;
        let mut inner = self.inner.lock().await;
        assert_active_instance_locked(&inner, &body.client_id, &body.runner_instance_id)?;
        if expected_connection_id.is_none()
            || inner
                .runners
                .get(&body.client_id)
                .is_some_and(|runner| runner.connection_id.as_deref() == expected_connection_id)
        {
            if let Some(runner) = inner.runners.get_mut(&body.client_id) {
                runner.last_seen = now_ts();
            }
        }
        let Some(pending) = inner.pending_by_id.get(&body.request_id) else {
            return Err(format!(
                "unknown or expired persistent shell request: {}",
                body.request_id
            ));
        };
        if pending.request.client_id != body.client_id {
            return Err("request_id does not belong to client_id".to_string());
        }
        let RunnerOperation::PersistentShell(operation) = &pending.operation else {
            return Err("request_id does not belong to a persistent shell request".to_string());
        };
        let expected = &operation.request;
        if expected.shell_id != body.result.shell_id
            || expected.workflow_session_id != body.result.workflow_session_id
            || expected.runtime_project_id != body.result.runtime_project_id
        {
            remove_pending_request_locked(&mut inner, &body.request_id);
            inner.persistent_waiters.remove(&body.request_id);
            return Err("persistent shell result identity mismatch".to_string());
        }
        take_pending_request_locked(&mut inner, &body.request_id)
            .expect("persistent shell request remained present after identity validation");
        let waiter = inner.persistent_waiters.remove(&body.request_id);
        if let Some(waiter) = waiter {
            let _ = waiter.send(body.result);
        }
        Ok(())
    }
}

fn normalize_persistent_shell_result(
    result: &mut webcodex_core::runner_protocol::PersistentShellResult,
) -> Result<(), String> {
    validate_id(&result.shell_id, "shell_id")?;
    validate_id(&result.workflow_session_id, "workflow_session_id")?;
    if result.runtime_project_id.is_empty()
        || result.runtime_project_id.len() > 1024
        || result.runtime_project_id.chars().any(char::is_control)
    {
        return Err("invalid runtime_project_id in persistent shell result".to_string());
    }
    if !matches!(
        result.shell_state.as_str(),
        "opening" | "running" | "exited" | "closed" | "poisoned" | "lost" | "unknown"
    ) {
        return Err("invalid shell_state in persistent shell result".to_string());
    }
    const MAX_METADATA_BYTES: usize = 8 * 1024;
    for (name, value) in [
        ("execution_state", Some(&result.execution_state)),
        ("cwd", result.cwd.as_ref()),
        ("initial_cwd", result.initial_cwd.as_ref()),
        ("shell", result.shell.as_ref()),
        ("profile", result.profile.as_ref()),
        ("close_reason", result.close_reason.as_ref()),
        ("error_code", result.error_code.as_ref()),
        ("error", result.error.as_ref()),
    ] {
        if value.is_some_and(|value| value.len() > MAX_METADATA_BYTES) {
            return Err(format!(
                "{name} exceeds persistent shell result metadata limit"
            ));
        }
    }
    if truncate_persistent_shell_stream(&mut result.stdout) {
        result.stdout_truncated = true;
    }
    if truncate_persistent_shell_stream(&mut result.stderr) {
        result.stderr_truncated = true;
    }
    Ok(())
}

fn truncate_persistent_shell_stream(value: &mut String) -> bool {
    if value.len() <= crate::registry::PERSISTENT_SHELL_STREAM_RETENTION_BYTES {
        return false;
    }
    let mut start = value.len() - crate::registry::PERSISTENT_SHELL_STREAM_RETENTION_BYTES;
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    *value = value[start..].to_string();
    true
}

#[cfg(test)]
mod retention_contract_tests {
    use super::*;

    #[test]
    fn server_stream_retention_contracts_remain_256_kib_and_independent() {
        assert_eq!(
            crate::registry::ORDINARY_RESULT_STREAM_RETENTION_BYTES,
            256 * 1024
        );
        assert_eq!(crate::registry::LIVE_JOB_STREAM_RETENTION_BYTES, 256 * 1024);
        assert_eq!(
            crate::registry::PERSISTENT_SHELL_STREAM_RETENTION_BYTES,
            256 * 1024
        );

        let limit = crate::registry::PERSISTENT_SHELL_STREAM_RETENTION_BYTES;
        let mut exact = "x".repeat(limit);
        assert!(!truncate_persistent_shell_stream(&mut exact));
        assert_eq!(exact.len(), limit);

        let mut oversized = format!("🙂{}", "x".repeat(limit));
        assert!(truncate_persistent_shell_stream(&mut oversized));
        assert_eq!(oversized.len(), limit);
        assert!(oversized.bytes().all(|byte| byte == b'x'));
    }
}
