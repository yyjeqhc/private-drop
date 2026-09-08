use super::access_control::assert_runner_access;
use super::jobs::{
    command_preview, ensure_dispatch_supported_locked, ensure_queue_capacity_locked,
    request_preview, PendingRequestEnqueueError,
};
#[cfg(test)]
use super::projects::RunnerLookupError;
use super::state::{
    CodingAgentDispatchFence, PendingShellRequest, PluginGatewayDispatchFence, RunnerRegistryInner,
    SkillStoreDispatchFence,
};
use super::validation::{
    validate_file_request, validate_id, validate_process_request, validate_run_request,
    validate_script_enqueue_request,
};
use super::{now_ts, RunnerFeature, RunnerRegistry, RUNNER_ONLINE_WINDOW_SECS};
use std::fmt;
use tokio::sync::oneshot;
use uuid::Uuid;
use webcodex_core::coding_agent::{
    validate_request as validate_coding_agent_request, CodingAgentDispatchState,
    CodingAgentRequest, CodingAgentResponse,
};
use webcodex_core::lsp_bridge::{RunnerLspPayload, RunnerLspRequest};
use webcodex_core::mcp_gateway::{
    validate_request as validate_mcp_gateway_request, McpGatewayDispatchState, McpGatewayRequest,
    McpGatewayResponse,
};
use webcodex_core::plugin::{
    validate_request as validate_plugin_gateway_request, PluginDispatchState, PluginGatewayRequest,
    PluginGatewayResponse,
};
use webcodex_core::runner_operation::{
    RunnerComputerOperation, RunnerComputerOperationKind, RunnerFileOperation,
    RunnerInvocationMetadata, RunnerOperation, RunnerPersistentShellOperation,
    RunnerProcessOperation, RunnerProjectOperation, RunnerProjectOperationKind,
    RunnerScriptOperation, RunnerShellOperation,
};
use webcodex_core::runner_protocol::{
    shell_computer_request_payload_max_bytes, PersistentShellRequest, PersistentShellResult,
    RunnerConfigOperationRequest, RunnerRequest, ShellFileOpRequest, ShellJobContext,
    ShellProcessArgv, ShellRunRequest, ShellRunResponse, ShellScriptPayload,
    RAW_SHELL_COMMAND_MAX_BYTES, RUNNER_CAPABILITY_APPLY_PATCH,
    RUNNER_CAPABILITY_APPLY_PATCH_MATCHING_MODE, RUNNER_CAPABILITY_APPLY_PATCH_MATCH_METADATA,
    RUNNER_CAPABILITY_APPLY_TEXT_EDIT_LINE_SCOPE, RUNNER_CAPABILITY_APPLY_TEXT_EDIT_OCCURRENCE,
    RUNNER_CAPABILITY_ARTIFACT_EXPORT_CHUNK_READ,
    RUNNER_CAPABILITY_ARTIFACT_EXPORT_STREAMING_METADATA, RUNNER_CAPABILITY_FILE_READ,
    RUNNER_CAPABILITY_FILE_WRITE, RUNNER_CAPABILITY_INTERNAL_POSIX_SCRIPT,
    RUNNER_CAPABILITY_PERSISTENT_SHELL, RUNNER_CAPABILITY_SSH_PERSISTENT_SHELL,
    RUNNER_CAPABILITY_STRUCTURED_FILE_DELETE, RUNNER_CAPABILITY_STRUCTURED_PROCESS_ARGV,
    RUNNER_CAPABILITY_STRUCTURED_SCRIPT_PAYLOAD, RUNNER_CONFIG_REQUEST_MAX_BYTES,
};
use webcodex_core::skill_store::SkillStoreRequest;
use webcodex_core::ssh_resource::{SshResourceRequest, SSH_RESOURCE_REQUEST_MAX_BYTES};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueLspError {
    InvalidRequest {
        message: String,
    },
    UnknownRunner {
        client_id: String,
    },
    RunnerOffline {
        client_id: String,
    },
    UnsupportedCapability {
        client_id: String,
        capability: &'static str,
    },
    QueueFull {
        client_id: String,
        limit: usize,
    },
}

impl fmt::Display for EnqueueLspError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest { message } => formatter.write_str(message),
            Self::UnknownRunner { client_id } => {
                write!(formatter, "unknown shell client: {client_id}")
            }
            Self::RunnerOffline { client_id } => write!(
                formatter,
                "runner {client_id} is offline (no keepalive within \
                 {RUNNER_ONLINE_WINDOW_SECS}s); reconnect the Runner before retrying"
            ),
            Self::UnsupportedCapability {
                client_id,
                capability,
            } => write!(
                formatter,
                "runner {client_id} does not support {capability}"
            ),
            Self::QueueFull { client_id, limit } => write!(
                formatter,
                "too many pending requests for runner {client_id} (limit {limit})"
            ),
        }
    }
}

impl std::error::Error for EnqueueLspError {}

impl From<PendingRequestEnqueueError> for EnqueueLspError {
    fn from(error: PendingRequestEnqueueError) -> Self {
        match error {
            PendingRequestEnqueueError::InvalidOperation { message } => {
                Self::InvalidRequest { message }
            }
            PendingRequestEnqueueError::UnknownRunner { client_id } => {
                Self::UnknownRunner { client_id }
            }
            PendingRequestEnqueueError::RunnerOffline { client_id } => {
                Self::RunnerOffline { client_id }
            }
            PendingRequestEnqueueError::QueueFull { client_id, limit } => {
                Self::QueueFull { client_id, limit }
            }
        }
    }
}

#[cfg(test)]
impl From<RunnerLookupError> for EnqueueLspError {
    fn from(error: RunnerLookupError) -> Self {
        match error {
            RunnerLookupError::UnknownRunner { client_id } => Self::UnknownRunner { client_id },
        }
    }
}

pub(super) fn next_request_id() -> String {
    Uuid::new_v4().to_string()
}

pub(super) fn notify_runner_locked(inner: &RunnerRegistryInner, client_id: &str) {
    if let Some(entry) = inner.notifiers.get(client_id) {
        entry.notify.notify_one();
    }
}

pub(super) fn enqueue_pending_request_locked(
    telemetry: &dyn crate::RunnerRegistryTelemetry,
    inner: &mut RunnerRegistryInner,
    client_id: &str,
    request_id: String,
    request: RunnerRequest,
    waiter: Option<oneshot::Sender<ShellRunResponse>>,
    job_id: Option<String>,
) -> Result<(), PendingRequestEnqueueError> {
    let operation = request
        .decode_operation()
        .map_err(|message| PendingRequestEnqueueError::InvalidOperation { message })?;
    ensure_dispatch_supported_locked(inner, client_id)?;
    ensure_queue_capacity_locked(inner, client_id)?;
    let runner = inner.runners.get(client_id);
    telemetry.request_enqueued(
        &request,
        &request_id,
        client_id,
        &request.kind,
        request.job_id.as_deref().or(job_id.as_deref()),
        runner.map(|record| record.runner_instance_id.as_str()),
        runner.map(|record| record.transport.as_str()),
        runner
            .and_then(|record| record.build.as_ref())
            .and_then(|build| build.version.as_deref()),
        runner
            .and_then(|record| record.build.as_ref())
            .and_then(|build| build.git_commit.as_deref()),
    );
    inner
        .queues_by_runner
        .entry(client_id.to_string())
        .or_default()
        .push_back(request_id.clone());
    inner.pending_by_id.insert(
        request_id,
        PendingShellRequest {
            request,
            operation,
            waiter,
            job_id,
            expected_runner_owner: None,
            expected_project_id: None,
            expected_project_cwd: None,
            expected_mcp_gateway_runner_instance_id: None,
            expected_mcp_gateway_provider_id: None,
            expected_mcp_gateway_provider_instance_id: None,
            expected_ssh_resource_runner_instance_id: None,
            expected_runner_config_runner_instance_id: None,
            skill_store_fence: None,
            dispatched: false,
        },
    );
    Ok(())
}

pub(super) fn take_pending_request_locked(
    inner: &mut RunnerRegistryInner,
    request_id: &str,
) -> Option<PendingShellRequest> {
    inner.pending_by_id.remove(request_id)
}

pub(super) fn remove_pending_request_locked(
    inner: &mut RunnerRegistryInner,
    request_id: &str,
) -> Option<PendingShellRequest> {
    let pending = take_pending_request_locked(inner, request_id);
    remove_request_from_queues_locked(inner, request_id);
    pending
}

fn remove_request_from_queues_locked(inner: &mut RunnerRegistryInner, request_id: &str) {
    for queue in inner.queues_by_runner.values_mut() {
        queue.retain(|id| id != request_id);
    }
}

/// Resolve every in-flight *synchronous* tool request (no `job_id`) owned by
/// `client_id` with a disconnect error, instead of leaving its oneshot waiter
/// parked until the calling tool's own timeout fires.
///
/// Sync requests (`enqueue_run`/`enqueue_file_op`/`enqueue_project_op`/
/// `enqueue_lsp`) carry a live `waiter` but `job_id: None`. The oneshot
/// `Sender` lives in the shared registry, not in the transport handler, so
/// aborting the connection task does not drop it — without this drain the
/// caller (e.g. an MCP `run_shell`/`read_file`) blocks for the full wait
/// timeout (tens of seconds) after the Runner goes away. Job-backed requests
/// are handled separately by `reconcile_disconnect` (they transition their job
/// to `lost`) and are intentionally skipped here.
pub(super) fn resolve_disconnected_sync_requests_locked(
    inner: &mut RunnerRegistryInner,
    client_id: &str,
    error: &str,
) {
    let request_ids: Vec<String> = inner
        .pending_by_id
        .iter()
        .filter(|(_, pending)| pending.job_id.is_none() && pending.request.client_id == client_id)
        .map(|(request_id, _)| request_id.clone())
        .collect();
    for request_id in request_ids {
        let Some(mut pending) = inner.pending_by_id.remove(&request_id) else {
            continue;
        };
        if let Some(queue) = inner.queues_by_runner.get_mut(client_id) {
            queue.retain(|id| id != &request_id);
        }
        if let Some(waiter) = pending.waiter.take() {
            let response = ShellRunResponse {
                success: false,
                request_id: request_id.clone(),
                client_id: client_id.to_string(),
                cwd: pending.request.cwd.clone(),
                command_preview: request_preview(&pending.request),
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: None,
                error: Some(error.to_string()),
                request_dispatched: Some(pending.dispatched),
                command_execution_state: None,
            };
            // The receiver may already be gone if the caller timed out first;
            // a failed send is expected and safe to ignore.
            let _ = waiter.send(response);
        }
        if let Some(waiter) = inner.mcp_gateway_waiters.remove(&request_id) {
            let state = if pending.dispatched {
                McpGatewayDispatchState::OutcomeUnknown
            } else {
                McpGatewayDispatchState::NotStarted
            };
            let _ = waiter.send(McpGatewayResponse::error(
                state,
                "runner_unavailable",
                if pending.dispatched {
                    "Runner transport failed after dispatch; downstream outcome is unknown and the call must not be retried automatically"
                } else {
                    "Runner transport failed before dispatch; provider request was not started"
                },
            ));
        }
        if let Some(waiter) = inner.plugin_gateway_waiters.remove(&request_id) {
            let state = if pending.dispatched {
                PluginDispatchState::OutcomeUnknown
            } else {
                PluginDispatchState::NotStarted
            };
            let _ = waiter.send(PluginGatewayResponse::error(
                state,
                "runner_unavailable",
                if pending.dispatched {
                    "Runner transport failed after Plugin dispatch; downstream outcome is unknown and the call must not be retried automatically"
                } else {
                    "Runner transport failed before Plugin dispatch; request was not started"
                },
            ));
        }
        if let Some(waiter) = inner.coding_agent_waiters.remove(&request_id) {
            let state = if pending.dispatched {
                CodingAgentDispatchState::OutcomeUnknown
            } else {
                CodingAgentDispatchState::NotStarted
            };
            let _ = waiter.send(CodingAgentResponse::error(
                state,
                "runner_unavailable",
                if pending.dispatched {
                    "Runner transport failed after CodingAgentRun dispatch; reconcile the same run_id before any new initiation"
                } else {
                    "Runner transport failed before CodingAgentRun dispatch; request was not started"
                },
                Some("unavailable"),
                Some("reobserve"),
            ));
        }
        inner.coding_agent_fences.remove(&request_id);
        inner.plugin_gateway_fences.remove(&request_id);
        inner.persistent_waiters.remove(&request_id);
    }
}

fn apply_text_edits_capability_requirements(body: &ShellFileOpRequest) -> (bool, bool) {
    if body.op != "apply_text_edits" {
        return (false, false);
    }
    let Some(content) = body.content.as_deref() else {
        return (false, false);
    };
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(content) else {
        // Invalid JSON cannot become a valid Runner mutation. Preserve the
        // existing generic-ingress behavior and let the Runner reject it.
        return (false, false);
    };
    let Some(changes) = payload.get("changes").and_then(serde_json::Value::as_array) else {
        return (false, false);
    };

    let mut requires_occurrence = false;
    let mut requires_line_scope = false;
    for edit in changes
        .iter()
        .filter_map(|change| change.get("edits").and_then(serde_json::Value::as_array))
        .flatten()
    {
        requires_occurrence |= edit.get("occurrence").is_some_and(|value| !value.is_null());
        requires_line_scope |= edit.get("line_scope").is_some_and(|value| !value.is_null());
        if requires_occurrence && requires_line_scope {
            break;
        }
    }
    (requires_occurrence, requires_line_scope)
}

fn encode_runner_operation(
    request_id: &str,
    client_id: &str,
    requested_by: String,
    operation: RunnerOperation,
) -> Result<RunnerRequest, String> {
    RunnerRequest::from_operation(
        RunnerInvocationMetadata {
            request_id: request_id.to_string(),
            client_id: client_id.to_string(),
            requested_by,
            created_at: now_ts(),
        },
        operation,
    )
}

fn encode_file_operation(
    request_id: &str,
    body: &ShellFileOpRequest,
    requested_by: String,
) -> Result<RunnerRequest, String> {
    let operation = RunnerFileOperation::from_server_request(body)?;
    encode_runner_operation(
        request_id,
        &body.client_id,
        requested_by,
        RunnerOperation::File(operation),
    )
}

impl RunnerRegistry {
    pub async fn enqueue_file_op(
        &self,
        body: ShellFileOpRequest,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_file_request(&body)?;
        if matches!(
            body.op.as_str(),
            "read_project_artifact_export_chunk" | "skill_list_packages" | "skill_read_file"
        ) {
            return Err(format!(
                "{} is internal-only; generic file-op enqueue is forbidden",
                body.op
            ));
        }
        let (requires_occurrence, requires_line_scope) =
            apply_text_edits_capability_requirements(&body);
        if requires_line_scope {
            return self
                .enqueue_apply_text_edits_with_line_scope(body, requested_by, requires_occurrence)
                .await;
        }
        self.enqueue_validated_file_op(body, requested_by).await
    }

    /// Enqueue one Phase-3 read-only Skill filesystem primitive. Generic
    /// `/api/shell/file` callers cannot reach these ops through
    /// `enqueue_file_op`; ToolRuntime calls this internal method only after the
    /// authoritative model-surface/project gates have resolved.
    pub async fn enqueue_skill_file_op(
        &self,
        body: ShellFileOpRequest,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_file_request(&body)?;
        if !matches!(body.op.as_str(), "skill_list_packages" | "skill_read_file") {
            return Err(format!(
                "Skill file-op enqueue only accepts project Skill runtime ops (got {})",
                body.op
            ));
        }
        self.enqueue_validated_file_op(body, requested_by).await
    }

    async fn enqueue_validated_file_op(
        &self,
        body: ShellFileOpRequest,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_file_operation(&request_id, &body, requested_by)?;
        let mut inner = self.inner.lock().await;
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &body.client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &body.client_id);
        Ok((request_id, rx))
    }

    /// Enqueue an apply_text_edits request containing at least one occurrence
    /// selector. The capability check and pending admission are intentionally
    /// performed under the same registry lock: an older or replacement Runner
    /// must never receive a selector it could silently ignore.
    pub async fn enqueue_apply_text_edits_with_occurrence(
        &self,
        body: ShellFileOpRequest,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_file_request(&body)?;
        if body.op != "apply_text_edits" {
            return Err(format!(
                "occurrence-fenced edit enqueue only accepts op=apply_text_edits (got {})",
                body.op
            ));
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_file_operation(&request_id, &body, requested_by)?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&body.client_id) else {
            return Err(format!("unknown shell client: {}", body.client_id));
        };
        if !runner
            .runner_features
            .supports(RunnerFeature::ApplyTextEditOccurrence)
        {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_APPLY_TEXT_EDIT_OCCURRENCE}",
                body.client_id
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &body.client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &body.client_id);
        Ok((request_id, rx))
    }

    /// Enqueue an apply_text_edits request containing at least one line_scope.
    /// The additive line-scope capability (and occurrence capability when the
    /// same payload also uses occurrence) is checked under the same registry
    /// lock as pending admission so an older/replacement Runner can never
    /// receive a safety fence it could silently ignore.
    pub async fn enqueue_apply_text_edits_with_line_scope(
        &self,
        body: ShellFileOpRequest,
        requested_by: String,
        requires_occurrence: bool,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_file_request(&body)?;
        if body.op != "apply_text_edits" {
            return Err(format!(
                "line-scoped edit enqueue only accepts op=apply_text_edits (got {})",
                body.op
            ));
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_file_operation(&request_id, &body, requested_by)?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&body.client_id) else {
            return Err(format!("unknown shell client: {}", body.client_id));
        };
        if !runner
            .runner_features
            .supports(RunnerFeature::ApplyTextEditLineScope)
        {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_APPLY_TEXT_EDIT_LINE_SCOPE}",
                body.client_id
            ));
        }
        if requires_occurrence
            && !runner
                .runner_features
                .supports(RunnerFeature::ApplyTextEditOccurrence)
        {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_APPLY_TEXT_EDIT_OCCURRENCE}",
                body.client_id
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &body.client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &body.client_id);
        Ok((request_id, rx))
    }

    /// Enqueue one Codex-compatible patch request only when the exact accepted
    /// Runner advertises both apply_patch and the current 0.4 match-metadata
    /// success contract. Capability admission and queue insertion share one
    /// registry lock so an older/replacement Runner cannot receive the mutation.
    pub async fn enqueue_apply_patch(
        &self,
        body: ShellFileOpRequest,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_file_request(&body)?;
        if body.op != "apply_patch" {
            return Err(format!(
                "apply_patch enqueue only accepts op=apply_patch (got {})",
                body.op
            ));
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_file_operation(&request_id, &body, requested_by)?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&body.client_id) else {
            return Err(format!("unknown shell client: {}", body.client_id));
        };
        if !runner.runner_features.supports(RunnerFeature::ApplyPatch) {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_APPLY_PATCH}",
                body.client_id
            ));
        }
        if !runner
            .runner_features
            .supports(RunnerFeature::ApplyPatchMatchMetadata)
        {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_APPLY_PATCH_MATCH_METADATA}",
                body.client_id
            ));
        }
        if !runner
            .runner_features
            .supports(RunnerFeature::ApplyPatchMatchingMode)
        {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_APPLY_PATCH_MATCHING_MODE}",
                body.client_id
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &body.client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &body.client_id);
        Ok((request_id, rx))
    }

    /// Enqueue the create-only artifact write used by computer_save_snapshot.
    /// Target ownership and file_write are rechecked under the same registry
    /// lock as pending admission so a concurrent Runner replacement cannot
    /// receive a write it no longer advertises.
    pub async fn enqueue_computer_snapshot_artifact(
        &self,
        body: ShellFileOpRequest,
        expected_project_id: &str,
        expected_project_cwd: &str,
        requested_by: String,
        auth: Option<&crate::RunnerAccess>,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_file_request(&body)?;
        if body.op != "save_project_artifact" {
            return Err(format!(
                "computer snapshot artifact enqueue only accepts op=save_project_artifact (got {})",
                body.op
            ));
        }
        if expected_project_id.is_empty()
            || expected_project_cwd.is_empty()
            || body.cwd.as_deref().map(str::trim) != Some(expected_project_cwd)
        {
            return Err(
                "computer snapshot artifact target project identity is invalid".to_string(),
            );
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_file_operation(&request_id, &body, requested_by)?;
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now_ts());
        let current = inner
            .runners
            .get(&body.client_id)
            .ok_or_else(|| format!("unknown shell client: {}", body.client_id))?;
        assert_runner_access(auth, current)?;
        if !current.runner_features.supports(RunnerFeature::FileWrite) {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_FILE_WRITE}",
                body.client_id
            ));
        }
        if !current.projects.iter().any(|project| {
            !project.disabled
                && project.id == expected_project_id
                && project.path == expected_project_cwd
        }) {
            return Err(format!(
                "stale_project: target project {expected_project_id} is no longer registered at the resolved path"
            ));
        }
        let expected_runner_owner = current.owner.clone();
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &body.client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        let pending = inner
            .pending_by_id
            .get_mut(&request_id)
            .expect("computer snapshot artifact request was just enqueued");
        pending.expected_runner_owner = expected_runner_owner;
        pending.expected_project_id = Some(expected_project_id.to_string());
        pending.expected_project_cwd = Some(expected_project_cwd.to_string());
        notify_runner_locked(&inner, &body.client_id);
        Ok((request_id, rx))
    }

    /// Enqueue the export-only large-file metadata read. The generation-2
    /// baseline checks and admission share the registry lock so request dispatch
    /// cannot outlive the exact accepted Runner capability snapshot.
    pub async fn enqueue_artifact_export_metadata(
        &self,
        body: ShellFileOpRequest,
        requested_by: String,
        auth: Option<&crate::RunnerAccess>,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_file_request(&body)?;
        if body.op != "read_project_artifact_metadata" {
            return Err(format!(
                "artifact export metadata enqueue only accepts op=read_project_artifact_metadata (got {})",
                body.op
            ));
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_file_operation(&request_id, &body, requested_by)?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&body.client_id) else {
            return Err(format!("unknown shell client: {}", body.client_id));
        };
        assert_runner_access(auth, runner)?;
        if !runner.runner_features.supports(RunnerFeature::FileRead) {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_FILE_READ}",
                body.client_id
            ));
        }
        if !runner
            .runner_features
            .supports(RunnerFeature::ArtifactExportChunkRead)
        {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_ARTIFACT_EXPORT_CHUNK_READ}",
                body.client_id
            ));
        }
        if !runner
            .runner_features
            .supports(RunnerFeature::ArtifactExportStreamingMetadata)
        {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_ARTIFACT_EXPORT_STREAMING_METADATA}",
                body.client_id
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &body.client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &body.client_id);
        Ok((request_id, rx))
    }

    /// Enqueue the internal artifact-export segment read. The generation-2
    /// baseline checks and pending admission share the registry lock. A baseline
    /// miss is an invariant failure and never selects a second read implementation.
    pub async fn enqueue_artifact_export_chunk(
        &self,
        body: ShellFileOpRequest,
        requested_by: String,
        auth: Option<&crate::RunnerAccess>,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_file_request(&body)?;
        if body.op != "read_project_artifact_export_chunk" {
            return Err(format!(
                "artifact export chunk enqueue only accepts op=read_project_artifact_export_chunk (got {})",
                body.op
            ));
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_file_operation(&request_id, &body, requested_by)?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&body.client_id) else {
            return Err(format!("unknown shell client: {}", body.client_id));
        };
        assert_runner_access(auth, runner)?;
        if !runner.runner_features.supports(RunnerFeature::FileRead) {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_FILE_READ}",
                body.client_id
            ));
        }
        if !runner
            .runner_features
            .supports(RunnerFeature::ArtifactExportChunkRead)
        {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_ARTIFACT_EXPORT_CHUNK_READ}",
                body.client_id
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &body.client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &body.client_id);
        Ok((request_id, rx))
    }

    /// Enqueue a structured `delete_project_files` Runner file op. The
    /// generation-2 `structured_file_delete` baseline check and pending-request
    /// admission happen under the same registry lock. A baseline miss queues
    /// nothing and is reported as an invariant failure; there is no shell fallback.
    pub async fn enqueue_structured_file_delete(
        &self,
        body: ShellFileOpRequest,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_file_request(&body)?;
        if body.op != "delete_project_files" {
            return Err(format!(
                "structured file delete only accepts op=delete_project_files (got {})",
                body.op
            ));
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_file_operation(&request_id, &body, requested_by)?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&body.client_id) else {
            return Err(format!("unknown shell client: {}", body.client_id));
        };
        if !runner
            .runner_features
            .supports(RunnerFeature::StructuredFileDelete)
        {
            return Err(format!(
                "capability_unavailable: runner {} does not support {RUNNER_CAPABILITY_STRUCTURED_FILE_DELETE}",
                body.client_id
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &body.client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &body.client_id);
        Ok((request_id, rx))
    }

    pub async fn enqueue_run(
        &self,
        body: ShellRunRequest,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        self.enqueue_run_with_ssh(body, requested_by, None, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_process(
        &self,
        client_id: String,
        cwd: Option<String>,
        process: ShellProcessArgv,
        stdin: Option<String>,
        timeout_secs: u64,
        wait_timeout_secs: u64,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_process_request(
            &client_id,
            cwd.as_deref(),
            &process,
            stdin.as_deref(),
            timeout_secs,
            wait_timeout_secs,
        )?;
        let normalized_cwd = cwd.map(|cwd| cwd.trim().to_string());
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_runner_operation(
            &request_id,
            &client_id,
            requested_by,
            RunnerOperation::RunProcess(RunnerProcessOperation {
                cwd: normalized_cwd,
                process,
                stdin,
                timeout_secs,
            }),
        )?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&client_id) else {
            return Err(format!("unknown shell client: {client_id}"));
        };
        if !runner
            .runner_features
            .supports(RunnerFeature::StructuredProcessArgv)
        {
            return Err(format!(
                "capability_unavailable: runner {client_id} does not support {RUNNER_CAPABILITY_STRUCTURED_PROCESS_ARGV}"
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &client_id);
        Ok((request_id, rx))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_script(
        &self,
        client_id: String,
        cwd: Option<String>,
        script: ShellScriptPayload,
        stdin: Option<String>,
        timeout_secs: u64,
        wait_timeout_secs: u64,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_script_enqueue_request(
            &client_id,
            cwd.as_deref(),
            &script,
            stdin.as_deref(),
            timeout_secs,
            wait_timeout_secs,
        )?;
        let normalized_cwd = cwd.map(|cwd| cwd.trim().to_string());
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_runner_operation(
            &request_id,
            &client_id,
            requested_by,
            RunnerOperation::RunScript(RunnerScriptOperation {
                cwd: normalized_cwd,
                script,
                stdin,
                timeout_secs,
            }),
        )?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&client_id) else {
            return Err(format!("unknown shell client: {client_id}"));
        };
        if !runner
            .runner_features
            .supports(RunnerFeature::StructuredScriptPayload)
        {
            return Err(format!(
                "capability_unavailable: runner {client_id} does not support {RUNNER_CAPABILITY_STRUCTURED_SCRIPT_PAYLOAD}"
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &client_id);
        Ok((request_id, rx))
    }

    /// Enqueue one WebCodex-generated POSIX program through the dedicated
    /// Runner runtime. This is not a caller shell escape hatch: the server
    /// selects the language and request kind, arguments are unavailable, and
    /// enqueue retains an atomic generation-2 baseline invariant check.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_internal_posix_script(
        &self,
        client_id: String,
        cwd: Option<String>,
        script: String,
        timeout_secs: u64,
        wait_timeout_secs: u64,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        webcodex_core::runner_protocol::validate_raw_shell_wire_command(&script)?;
        let script = ShellScriptPayload {
            language: webcodex_core::runner_protocol::ShellScriptLanguage::Sh,
            script,
            args: Vec::new(),
        };
        validate_script_enqueue_request(
            &client_id,
            cwd.as_deref(),
            &script,
            None,
            timeout_secs,
            wait_timeout_secs,
        )?;
        let normalized_cwd = cwd.map(|cwd| cwd.trim().to_string());
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_runner_operation(
            &request_id,
            &client_id,
            requested_by,
            RunnerOperation::RunInternalPosixScript(RunnerScriptOperation {
                cwd: normalized_cwd,
                script,
                stdin: None,
                timeout_secs,
            }),
        )?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&client_id) else {
            return Err(format!("unknown shell client: {client_id}"));
        };
        if !runner
            .runner_features
            .supports(RunnerFeature::InternalPosixScript)
        {
            return Err(format!(
                "capability_unavailable: runner {client_id} does not support {RUNNER_CAPABILITY_INTERNAL_POSIX_SCRIPT}"
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &client_id);
        Ok((request_id, rx))
    }

    /// Internal Session execution context for a named Runner-local SSH
    /// resource. Public shell endpoints do not accept this field directly.
    pub async fn enqueue_run_with_ssh(
        &self,
        body: ShellRunRequest,
        requested_by: String,
        ssh_resource: Option<String>,
        ssh_session_id: Option<String>,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_run_request(&body)?;
        // A Workflow Session may execute locally without an SSH resource.
        // Only remote execution without the Session that owns the resource is
        // invalid.
        if ssh_resource.is_some() && ssh_session_id.is_none() {
            return Err(
                "ssh_session_required: an SSH resource requires a Workflow Session id".to_string(),
            );
        }
        let normalized_cwd = body.cwd.clone().map(|cwd| cwd.trim().to_string());
        let ssh_context = ssh_resource
            .zip(ssh_session_id)
            .map(|(resource, session_id)| ShellJobContext {
                runtime_project_id: None,
                workflow_session_id: Some(session_id),
                ssh_resource: Some(resource),
                project_cwd: None,
                cwd: normalized_cwd.clone(),
                purpose: None,
                shell: None,
                command_preview: command_preview(&body.command),
                validation_steps: Vec::new(),
                validation: None,
                structured_execution: None,
            });
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let has_ssh_context = ssh_context
            .as_ref()
            .is_some_and(|context| context.ssh_resource.is_some());
        let request = encode_runner_operation(
            &request_id,
            &body.client_id,
            requested_by,
            RunnerOperation::RunShell(RunnerShellOperation {
                cwd: normalized_cwd,
                command: body.command.clone(),
                stdin: body.stdin.clone(),
                timeout_secs: body.timeout_secs,
                job_context: ssh_context,
            }),
        )?;
        let mut inner = self.inner.lock().await;
        if has_ssh_context {
            let Some(runner) = inner.runners.get(&body.client_id) else {
                return Err(format!("unknown shell client: {}", body.client_id));
            };
            if !runner.runner_features.supports(RunnerFeature::SshShell) {
                return Err(format!(
                    "agent_capability_unavailable: runner {} does not support ssh_shell",
                    body.client_id
                ));
            }
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &body.client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &body.client_id);
        Ok((request_id, rx))
    }

    /// Cancel a pending synchronous request and report whether a Runner had
    /// already polled it. This lets timeout callers distinguish queue timeout
    /// from an actually started command without retaining expired requests.
    pub async fn cancel_request(&self, request_id: &str) -> bool {
        self.cancel_request_dispatch_state(request_id)
            .await
            .unwrap_or(false)
    }

    /// Remove synchronous pending requests whose response receiver has already
    /// been dropped by the caller. A closed waiter proves there is no remaining
    /// observer for the response, so retaining the queue/registry entry can only
    /// consume bounded pending-request capacity until a late result or disconnect.
    pub async fn cancel_abandoned_sync_requests(&self) -> usize {
        let mut inner = self.inner.lock().await;
        let abandoned = inner
            .pending_by_id
            .iter()
            .filter(|(_, pending)| {
                pending.job_id.is_none()
                    && (pending
                        .waiter
                        .as_ref()
                        .is_some_and(tokio::sync::oneshot::Sender::is_closed)
                        || inner
                            .mcp_gateway_waiters
                            .get(&pending.request.request_id)
                            .is_some_and(tokio::sync::oneshot::Sender::is_closed)
                        || inner
                            .plugin_gateway_waiters
                            .get(&pending.request.request_id)
                            .is_some_and(tokio::sync::oneshot::Sender::is_closed)
                        || inner
                            .coding_agent_waiters
                            .get(&pending.request.request_id)
                            .is_some_and(tokio::sync::oneshot::Sender::is_closed))
            })
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        for request_id in &abandoned {
            inner.persistent_waiters.remove(request_id);
            inner.mcp_gateway_waiters.remove(request_id);
            inner.plugin_gateway_waiters.remove(request_id);
            inner.plugin_gateway_fences.remove(request_id);
            inner.coding_agent_waiters.remove(request_id);
            inner.coding_agent_fences.remove(request_id);
            remove_pending_request_locked(&mut inner, request_id);
        }
        abandoned.len()
    }

    /// Cancel a pending synchronous request while preserving the distinction
    /// between an undispatched request and one whose registry record was
    /// already consumed. A missing record cannot prove that execution did not
    /// start, so lifecycle-sensitive callers must treat `None` conservatively.
    pub async fn cancel_request_dispatch_state(&self, request_id: &str) -> Option<bool> {
        let mut inner = self.inner.lock().await;
        inner.persistent_waiters.remove(request_id);
        inner.mcp_gateway_waiters.remove(request_id);
        inner.plugin_gateway_waiters.remove(request_id);
        inner.plugin_gateway_fences.remove(request_id);
        inner.coding_agent_waiters.remove(request_id);
        inner.coding_agent_fences.remove(request_id);
        remove_pending_request_locked(&mut inner, request_id).map(|pending| pending.dispatched)
    }

    /// Enqueue one closed Runner-global Skill store operation for one exact
    /// live Runner process. Read and management capabilities are independent;
    /// the exact process lease and capability are revalidated again at dequeue.
    pub async fn enqueue_skill_store(
        &self,
        client_id: &str,
        expected_runner_instance_id: &str,
        operation: SkillStoreRequest,
        auth: Option<&crate::RunnerAccess>,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        let management = operation.requires_management_capability();
        let content = serde_json::to_string(&operation)
            .map_err(|_| "invalid Skill store request".to_string())?;
        if content.len() > 32 * 1024 {
            return Err("invalid Skill store request".to_string());
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_runner_operation(
            &request_id,
            client_id,
            requested_by,
            RunnerOperation::SkillStore(operation),
        )
        .map_err(|_| "invalid Skill store request".to_string())?;
        let mut inner = self.inner.lock().await;
        let runner = inner
            .runners
            .get(client_id)
            .ok_or_else(|| "exact Runner is unavailable".to_string())?;
        assert_runner_access(auth, runner)
            .map_err(|_| "exact Runner is unavailable".to_string())?;
        if runner.runner_instance_id != expected_runner_instance_id {
            return Err(
                "stale Runner identity; Skill store request was not dispatched".to_string(),
            );
        }
        let required = if management {
            RunnerFeature::SkillStoreManage
        } else {
            RunnerFeature::SkillStoreRead
        };
        if !runner.runner_features.supports(required) {
            return Err(format!(
                "skill_store_capability_unavailable: exact Runner does not support {}",
                required.as_wire_name()
            ));
        }
        if now_ts().saturating_sub(runner.last_seen) > RUNNER_ONLINE_WINDOW_SECS {
            return Err(
                "exact Runner is offline; Skill store request was not dispatched".to_string(),
            );
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        let pending = inner
            .pending_by_id
            .get_mut(&request_id)
            .expect("Skill store request was just enqueued");
        pending.skill_store_fence = Some(SkillStoreDispatchFence {
            runner_instance_id: expected_runner_instance_id.to_string(),
            management,
        });
        notify_runner_locked(&inner, client_id);
        Ok((request_id, rx))
    }

    /// Enqueue one typed MCP gateway operation for an exact live Runner
    /// instance. Authorization and lease identity are rechecked atomically at
    /// admission, so discovery is never authoritative and a stale bridge id
    /// cannot silently route to a replacement Runner.
    pub async fn enqueue_mcp_gateway(
        &self,
        client_id: &str,
        expected_runner_instance_id: &str,
        operation: McpGatewayRequest,
        auth: Option<&crate::RunnerAccess>,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<McpGatewayResponse>), String> {
        validate_mcp_gateway_request(&operation)
            .map_err(|_| "invalid MCP gateway request".to_string())?;
        let expected_provider_id = operation.provider_id().to_string();
        let expected_provider_instance_id = operation.provider_instance_id().to_string();
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_runner_operation(
            &request_id,
            client_id,
            requested_by,
            RunnerOperation::McpGateway(operation),
        )?;
        let mut inner = self.inner.lock().await;
        let runner = inner
            .runners
            .get(client_id)
            .ok_or_else(|| "exact Runner is unavailable".to_string())?;
        assert_runner_access(auth, runner)
            .map_err(|_| "exact Runner is unavailable".to_string())?;
        if runner.runner_instance_id != expected_runner_instance_id {
            return Err("stale Runner identity; request was not started".to_string());
        }
        let provider_is_current = runner
            .policy
            .as_ref()
            .and_then(|policy| policy.mcp_gateway_providers.as_ref())
            .is_some_and(|providers| {
                providers.iter().any(|provider| {
                    provider.provider_id == expected_provider_id
                        && provider.provider_instance_id == expected_provider_instance_id
                })
            });
        if !provider_is_current {
            return Err("stale provider identity; request was not started".to_string());
        }
        if now_ts().saturating_sub(runner.last_seen) > super::RUNNER_ONLINE_WINDOW_SECS {
            return Err("exact Runner is offline; request was not started".to_string());
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            client_id,
            request_id.clone(),
            request,
            None,
            None,
        )?;
        let pending = inner
            .pending_by_id
            .get_mut(&request_id)
            .expect("MCP gateway request was just enqueued");
        pending.expected_mcp_gateway_runner_instance_id =
            Some(expected_runner_instance_id.to_string());
        pending.expected_mcp_gateway_provider_id = Some(expected_provider_id);
        pending.expected_mcp_gateway_provider_instance_id = Some(expected_provider_instance_id);
        inner.mcp_gateway_waiters.insert(request_id.clone(), tx);
        notify_runner_locked(&inner, client_id);
        Ok((request_id, rx))
    }

    /// Enqueue one closed native Plugin operation for one exact live Runner.
    /// Provider instance identity stays opaque Runner-owned state; the Runner
    /// validates it against its current committed provider set before dispatch.
    pub async fn enqueue_plugin_gateway(
        &self,
        client_id: &str,
        expected_runner_instance_id: &str,
        operation: PluginGatewayRequest,
        auth: Option<&crate::RunnerAccess>,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<PluginGatewayResponse>), String> {
        validate_plugin_gateway_request(&operation)
            .map_err(|_| "invalid Plugin gateway request".to_string())?;
        let provider_binding =
            operation
                .provider_binding()
                .map(|(provider_id, provider_instance_id)| {
                    (provider_id.to_string(), provider_instance_id.to_string())
                });
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_runner_operation(
            &request_id,
            client_id,
            requested_by,
            RunnerOperation::PluginGateway(operation),
        )?;
        let mut inner = self.inner.lock().await;
        let runner = inner
            .runners
            .get(client_id)
            .ok_or_else(|| "exact Runner is unavailable".to_string())?;
        assert_runner_access(auth, runner)
            .map_err(|_| "exact Runner is unavailable".to_string())?;
        if runner.runner_instance_id != expected_runner_instance_id {
            return Err("stale Runner identity; request was not started".to_string());
        }
        if !runner
            .runner_features
            .supports(RunnerFeature::NativeToolPlugins)
        {
            return Err("exact Runner does not support native Tool Plugins".to_string());
        }
        if now_ts().saturating_sub(runner.last_seen) > super::RUNNER_ONLINE_WINDOW_SECS {
            return Err("exact Runner is offline; request was not started".to_string());
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            client_id,
            request_id.clone(),
            request,
            None,
            None,
        )?;
        inner.plugin_gateway_fences.insert(
            request_id.clone(),
            PluginGatewayDispatchFence {
                runner_instance_id: expected_runner_instance_id.to_string(),
                provider: provider_binding,
            },
        );
        inner.plugin_gateway_waiters.insert(request_id.clone(), tx);
        notify_runner_locked(&inner, client_id);
        Ok((request_id, rx))
    }

    /// Enqueue one closed Runner-local managed SSH resource operation for one
    /// exact live Runner process. The destination may be present in the transient
    /// request body for `register`, but is never projected into registry metadata.
    pub async fn enqueue_ssh_resource(
        &self,
        client_id: &str,
        expected_runner_instance_id: &str,
        operation: SshResourceRequest,
        auth: Option<&crate::RunnerAccess>,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        operation
            .validate()
            .map_err(|_| "ssh_resource_invalid: request was not started".to_string())?;
        let content = serde_json::to_string(&operation)
            .map_err(|_| "ssh_resource_invalid: request was not started".to_string())?;
        if content.len() > SSH_RESOURCE_REQUEST_MAX_BYTES {
            return Err("ssh_resource_invalid: request was not started".to_string());
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_runner_operation(
            &request_id,
            client_id,
            requested_by,
            RunnerOperation::SshResource(operation),
        )
        .map_err(|_| "ssh_resource_invalid: request was not started".to_string())?;
        let mut inner = self.inner.lock().await;
        let runner = inner
            .runners
            .get(client_id)
            .ok_or_else(|| "exact Runner is unavailable".to_string())?;
        assert_runner_access(auth, runner)
            .map_err(|_| "exact Runner is unavailable".to_string())?;
        if runner.runner_instance_id != expected_runner_instance_id {
            return Err("runner_replaced: request was not started".to_string());
        }
        if !runner
            .runner_features
            .supports(RunnerFeature::ManagedSshResources)
        {
            return Err(
                "ssh_resource_registry_unavailable: Runner does not support managed SSH resources"
                    .to_string(),
            );
        }
        if now_ts().saturating_sub(runner.last_seen) > RUNNER_ONLINE_WINDOW_SECS {
            return Err("exact Runner is offline; request was not started".to_string());
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        inner
            .pending_by_id
            .get_mut(&request_id)
            .expect("SSH resource request was just enqueued")
            .expected_ssh_resource_runner_instance_id =
            Some(expected_runner_instance_id.to_string());
        notify_runner_locked(&inner, client_id);
        Ok((request_id, rx))
    }

    /// Enqueue one bounded first-class config operation against one exact live
    /// Runner process. No filesystem path is accepted or placed on the wire.
    pub async fn enqueue_runner_config(
        &self,
        client_id: &str,
        expected_runner_instance_id: &str,
        operation: RunnerConfigOperationRequest,
        auth: Option<&crate::RunnerAccess>,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        operation
            .validate()
            .map_err(|_| "invalid_runner_config_request: request was not started".to_string())?;
        let content = serde_json::to_string(&operation)
            .map_err(|_| "invalid_runner_config_request: request was not started".to_string())?;
        if content.len() > RUNNER_CONFIG_REQUEST_MAX_BYTES {
            return Err("invalid_runner_config_request: request was not started".to_string());
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_runner_operation(
            &request_id,
            client_id,
            requested_by,
            RunnerOperation::RunnerConfig(operation),
        )
        .map_err(|_| "invalid_runner_config_request: request was not started".to_string())?;
        let mut inner = self.inner.lock().await;
        let runner = inner
            .runners
            .get(client_id)
            .ok_or_else(|| "exact Runner is unavailable".to_string())?;
        assert_runner_access(auth, runner)
            .map_err(|_| "exact Runner is unavailable".to_string())?;
        if runner.runner_instance_id != expected_runner_instance_id {
            return Err("runner_replaced: request was not started".to_string());
        }
        if !runner
            .runner_features
            .supports(RunnerFeature::RunnerConfigControl)
        {
            return Err("capability_unavailable: Runner config control is unsupported".to_string());
        }
        if now_ts().saturating_sub(runner.last_seen) > RUNNER_ONLINE_WINDOW_SECS {
            return Err("exact Runner is offline; request was not started".to_string());
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        inner
            .pending_by_id
            .get_mut(&request_id)
            .expect("Runner config request was just enqueued")
            .expected_runner_config_runner_instance_id =
            Some(expected_runner_instance_id.to_string());
        notify_runner_locked(&inner, client_id);
        Ok((request_id, rx))
    }

    /// Enqueue one closed CodingAgentRun operation for one exact Runner/provider
    /// process lease. The caller supplies only WebCodex typed Run semantics; raw
    /// ACP method/params never enter this registry.
    pub async fn enqueue_coding_agent(
        &self,
        client_id: &str,
        expected_runner_instance_id: &str,
        expected_provider_id: &str,
        expected_provider_instance_id: &str,
        operation: CodingAgentRequest,
        auth: Option<&crate::RunnerAccess>,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<CodingAgentResponse>), String> {
        validate_coding_agent_request(&operation)
            .map_err(|error| format!("invalid CodingAgentRun request: {error}"))?;
        if let Some((provider_id, provider_instance_id)) = operation.provider_binding() {
            if provider_id != expected_provider_id
                || provider_instance_id != expected_provider_instance_id
            {
                return Err(
                    "CodingAgentRun provider binding does not match exact dispatch fence"
                        .to_string(),
                );
            }
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_runner_operation(
            &request_id,
            client_id,
            requested_by,
            RunnerOperation::CodingAgent(operation),
        )?;
        let mut inner = self.inner.lock().await;
        let runner = inner
            .runners
            .get(client_id)
            .ok_or_else(|| "exact Runner is unavailable".to_string())?;
        assert_runner_access(auth, runner)
            .map_err(|_| "exact Runner is unavailable".to_string())?;
        if !runner
            .runner_features
            .supports(RunnerFeature::CodingAgentRuns)
        {
            return Err("exact Runner does not support CodingAgentRun".to_string());
        }
        if runner.runner_instance_id != expected_runner_instance_id {
            return Err("stale Runner identity; CodingAgentRun was not dispatched".to_string());
        }
        let provider_is_current = runner.coding_agent_providers.iter().any(|provider| {
            provider.provider_id == expected_provider_id
                && provider.provider_instance_id == expected_provider_instance_id
        });
        if !provider_is_current {
            return Err(
                "stale ACP provider identity; CodingAgentRun was not dispatched".to_string(),
            );
        }
        if now_ts().saturating_sub(runner.last_seen) > super::RUNNER_ONLINE_WINDOW_SECS {
            return Err("exact Runner is offline; CodingAgentRun was not dispatched".to_string());
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            client_id,
            request_id.clone(),
            request,
            None,
            None,
        )?;
        inner.coding_agent_fences.insert(
            request_id.clone(),
            CodingAgentDispatchFence {
                runner_instance_id: expected_runner_instance_id.to_string(),
                provider_id: expected_provider_id.to_string(),
                provider_instance_id: expected_provider_instance_id.to_string(),
            },
        );
        inner.coding_agent_waiters.insert(request_id.clone(), tx);
        notify_runner_locked(&inner, client_id);
        Ok((request_id, rx))
    }

    /// Enqueue one explicit persistent-shell lifecycle operation. Capability
    /// absence is a hard failure; there is no fallback to `run_shell`.
    ///
    /// `job_context` carries safe Session/resource metadata so the Runner can
    /// route an SSH persistent shell to its bound resource; it is `None` for
    /// local persistent shells. SSH persistent shells require
    /// `persistent_shell` plus the additive `ssh_persistent_shell` capability;
    /// `ssh_shell` remains the separate one-shot/background SSH capability.
    pub async fn enqueue_persistent_shell(
        &self,
        client_id: String,
        request: PersistentShellRequest,
        job_context: Option<ShellJobContext>,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<PersistentShellResult>), String> {
        validate_id(&client_id, "client_id")?;
        validate_id(&request.shell_id, "shell_id")?;
        validate_id(&request.workflow_session_id, "workflow_session_id")?;
        if request.runtime_project_id.trim().is_empty() {
            return Err("runtime_project_id is required".to_string());
        }
        if !matches!(
            request.action.as_str(),
            "open" | "exec" | "status" | "close"
        ) {
            return Err(format!(
                "unsupported persistent shell action: {}",
                request.action
            ));
        }
        if request
            .command
            .as_deref()
            .is_some_and(|command| command.contains('\0'))
        {
            return Err("persistent shell command cannot contain NUL".to_string());
        }
        if request
            .command
            .as_deref()
            .is_some_and(|command| command.len() > RAW_SHELL_COMMAND_MAX_BYTES)
        {
            return Err(format!(
                "persistent shell command too long (max {RAW_SHELL_COMMAND_MAX_BYTES} bytes)"
            ));
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let wire_request = encode_runner_operation(
            &request_id,
            &client_id,
            requested_by,
            RunnerOperation::PersistentShell(RunnerPersistentShellOperation {
                request,
                job_context: job_context.clone(),
            }),
        )?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(&client_id) else {
            return Err(format!("unknown shell client: {client_id}"));
        };
        if !runner
            .runner_features
            .supports(RunnerFeature::PersistentShell)
        {
            return Err(format!(
                "agent_capability_unavailable: runner {client_id} does not support {RUNNER_CAPABILITY_PERSISTENT_SHELL}"
            ));
        }
        // `persistent_shell` is checked above. A named SSH persistent shell
        // additionally requires only `ssh_persistent_shell`; it intentionally
        // does not depend on the one-shot/background `ssh_shell` capability.
        if job_context
            .as_ref()
            .is_some_and(|ctx| ctx.ssh_resource.is_some())
            && !runner
                .runner_features
                .supports(RunnerFeature::SshPersistentShell)
        {
            return Err(format!(
                "agent_capability_unavailable: runner {client_id} does not support {RUNNER_CAPABILITY_SSH_PERSISTENT_SHELL}"
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &client_id,
            request_id.clone(),
            wire_request,
            None,
            None,
        )?;
        inner.persistent_waiters.insert(request_id.clone(), tx);
        notify_runner_locked(&inner, &client_id);
        Ok((request_id, rx))
    }

    /// Enqueue a project-management Runner request (`register_project`,
    /// `create_project`, or the internal path resolver). The JSON payload is
    /// carried in `stdin` so the
    /// Runner can parse it without shell interpolation. The `command` field is
    /// empty (unused for these kinds); the Runner dispatches on `kind` and
    /// reads the payload from `stdin`. Returns a oneshot receiver for the
    /// `ShellRunResponse` (the Runner returns structured JSON in `stdout`).
    pub async fn enqueue_project_op(
        &self,
        client_id: String,
        kind: &str,
        payload: String,
        requested_by: String,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_id(&client_id, "client_id")?;
        if !matches!(
            kind,
            "register_project"
                | "create_project"
                | "resolve_or_register_project"
                | "prepare_managed_worktree"
                | "project_lifecycle_enable"
                | "project_lifecycle_disable"
                | "project_lifecycle_unregister"
        ) {
            return Err(format!("unsupported project op kind: {}", kind));
        }
        if payload.contains('\0') {
            return Err("project op payload must not contain NUL".to_string());
        }
        let required_feature = match kind {
            "resolve_or_register_project" => Some(RunnerFeature::ProjectPathRegistration),
            "prepare_managed_worktree" => Some(RunnerFeature::ManagedWorktree),
            "project_lifecycle_enable"
            | "project_lifecycle_disable"
            | "project_lifecycle_unregister" => Some(RunnerFeature::ProjectLifecycle),
            "register_project" | "create_project" => None,
            _ => unreachable!("project op kind validated above"),
        };
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let operation_kind = RunnerProjectOperationKind::from_wire(kind)
            .ok_or_else(|| format!("unsupported project op kind: {kind}"))?;
        let request = encode_runner_operation(
            &request_id,
            &client_id,
            requested_by,
            RunnerOperation::Project(RunnerProjectOperation {
                kind: operation_kind,
                payload,
            }),
        )?;
        let mut inner = self.inner.lock().await;
        if let Some(required_feature) = required_feature {
            let runner = inner
                .runners
                .get(&client_id)
                .ok_or_else(|| format!("unknown shell client: {client_id}"))?;
            if !runner.runner_features.supports(required_feature) {
                return Err(format!(
                    "capability_unavailable: runner {client_id} does not support {}",
                    required_feature.as_wire_name()
                ));
            }
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )?;
        notify_runner_locked(&inner, &client_id);
        Ok((request_id, rx))
    }

    /// Enqueue one typed bounded computer request. The payload is bounded JSON
    /// in stdin and command is always empty. Owner/auth and the exact per-kind
    /// computer capability requirements are rechecked under the registry lock so a
    /// concurrent re-registration cannot create a TOCTOU escape.
    pub async fn enqueue_computer(
        &self,
        client_id: String,
        kind: &'static str,
        payload: String,
        requested_by: String,
        auth: Option<&crate::RunnerAccess>,
        timeout_secs: u64,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), String> {
        validate_id(&client_id, "client_id")?;
        let required_features: &[RunnerFeature] = match kind {
            "computer_list_applications" => &[RunnerFeature::ComputerApplicationDiscovery],
            "computer_launch_application" => &[RunnerFeature::ComputerApplicationLaunch],
            "computer_list_displays" | "computer_snapshot_display" => {
                &[RunnerFeature::ComputerDisplayObserve]
            }
            "computer_read_clipboard" => &[RunnerFeature::ComputerClipboardRead],
            "computer_write_clipboard" => &[RunnerFeature::ComputerClipboardWrite],
            "computer_list_windows" | "computer_snapshot" => &[RunnerFeature::ComputerObserve],
            "computer_snapshot_region" => &[
                RunnerFeature::ComputerObserve,
                RunnerFeature::ComputerSnapshotRegion,
            ],
            "computer_accessibility_status" | "computer_accessibility_tree" => {
                &[RunnerFeature::ComputerAccessibilityObserve]
            }
            "computer_element_state" => &[RunnerFeature::ComputerElementState],
            "computer_control" => &[RunnerFeature::ComputerControl],
            "computer_scroll_to_element" => &[RunnerFeature::ComputerScrollToElement],
            "computer_key_input" => &[RunnerFeature::ComputerKeyInput],
            "computer_pointer_move" | "computer_pointer_click" => {
                &[RunnerFeature::ComputerPointerControl]
            }
            "computer_activate_window" => &[RunnerFeature::ComputerWindowActivate],
            "computer_input_text" => &[RunnerFeature::ComputerTextInput],
            _ => return Err("invalid computer request kind".to_string()),
        };
        if payload.len() > shell_computer_request_payload_max_bytes(kind) || payload.contains('\0')
        {
            return Err("computer request payload is invalid or too large".to_string());
        }
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let operation_kind = RunnerComputerOperationKind::from_wire(kind)
            .ok_or_else(|| "invalid computer request kind".to_string())?;
        let request = encode_runner_operation(
            &request_id,
            &client_id,
            requested_by,
            RunnerOperation::Computer(RunnerComputerOperation {
                kind: operation_kind,
                payload,
                timeout_secs: timeout_secs.max(1),
            }),
        )?;
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now_ts());
        let current = inner
            .runners
            .get(&client_id)
            .ok_or_else(|| format!("unknown shell client: {client_id}"))?;
        assert_runner_access(auth, current)?;
        if let Some(required_feature) = required_features
            .iter()
            .copied()
            .find(|feature| !current.runner_features.supports(*feature))
        {
            return Err(format!(
                "runner {client_id} does not support {}",
                required_feature.as_wire_name()
            ));
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )
        .map_err(|error| error.to_string())?;
        notify_runner_locked(&inner, &client_id);
        Ok((request_id, rx))
    }

    /// Enqueue a typed read-only LSP navigation request. Never falls through
    /// to shell execution: the Runner dispatches exclusively on `kind = "lsp"`
    /// with a structured `lsp` payload.
    pub async fn enqueue_lsp(
        &self,
        client_id: String,
        payload: RunnerLspPayload,
        requested_by: String,
        timeout_secs: u64,
    ) -> Result<(String, oneshot::Receiver<ShellRunResponse>), EnqueueLspError> {
        validate_id(&client_id, "client_id")
            .map_err(|message| EnqueueLspError::InvalidRequest { message })?;
        // Capability gate before enqueue so old Runners never receive unknown
        // LSP kinds that could fall into shell fallback.
        let required_feature = if matches!(&payload.request, RunnerLspRequest::CallHierarchy { .. })
        {
            RunnerFeature::LspCallHierarchy
        } else {
            RunnerFeature::LspReadOnlyNavigation
        };
        let request_id = next_request_id();
        let (tx, rx) = oneshot::channel();
        let request = encode_runner_operation(
            &request_id,
            &client_id,
            requested_by,
            RunnerOperation::Lsp {
                payload,
                timeout_secs: timeout_secs.max(1),
            },
        )
        .map_err(|message| EnqueueLspError::InvalidRequest { message })?;
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now_ts());
        // This check is the authoritative TOCTOU fence: capability validation
        // and pending-request admission happen under the same registry lock.
        let current =
            inner
                .runners
                .get(&client_id)
                .ok_or_else(|| EnqueueLspError::UnknownRunner {
                    client_id: client_id.clone(),
                })?;
        if !current.runner_features.supports(required_feature) {
            return Err(EnqueueLspError::UnsupportedCapability {
                client_id,
                capability: required_feature.as_wire_name(),
            });
        }
        enqueue_pending_request_locked(
            self.telemetry.as_ref(),
            &mut inner,
            &client_id,
            request_id.clone(),
            request,
            Some(tx),
            None,
        )
        .map_err(EnqueueLspError::from)?;
        notify_runner_locked(&inner, &client_id);
        Ok((request_id, rx))
    }
}
