use super::access_control::{assert_runner_access, runner_visible_to_access};
use super::jobs::{begin_job_recovery, mark_job_lost, offline_last_seen};
use super::project_inventory::{
    expire_staging, pending_inventory_state, preserve_authoritative_pending,
};
use super::reconciliation::{
    preflight_inventory_locked, reconcile_inventory_locked, terminate_instance_jobs_locked,
    validate_job_inventory_without_project_membership,
};
use super::requests::resolve_disconnected_sync_requests_locked;
use super::state::{
    JobLifecycleState, JobRecoveryReason, NotifierEntry, RunnerRecord, RunnerRegistryInner,
    RunnerSemanticView,
};
use super::validation::{
    normalize_tool_providers, trim_string, validate_id, validate_optional_field,
    validate_runner_instance_id,
};
use super::{
    now_ts, AcceptedRunnerProtocol, RunnerFeature, RunnerFeatureSet, RunnerRegistry,
    RunnerTransport, MAX_RETIRED_INSTANCES_PER_RUNNER, RUNNER_ONLINE_WINDOW_SECS,
};
use crate::RunnerAccessGroup;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::{watch, Notify};
use webcodex_core::coding_agent::{
    validate_coding_agent_run_snapshot, validate_provider_id as validate_coding_agent_provider_id,
    validate_provider_instance_id as validate_coding_agent_provider_instance_id,
    CodingAgentProvider, CodingAgentRunInventory, CodingAgentRunSnapshot,
    CODING_AGENT_MAX_INVENTORY_RUNS, CODING_AGENT_MAX_PROVIDERS,
    CODING_AGENT_MAX_PROVIDER_NAME_BYTES,
};
use webcodex_core::mcp_gateway::validate_providers;
use webcodex_core::runner_protocol::{
    RunnerRegisterRequest, RunnerView, RUNNER_JOB_CONCURRENCY_MAX, RUNNER_JOB_CONCURRENCY_MIN,
};

fn validate_coding_agent_registration(
    client_id: &str,
    capability: bool,
    providers: Option<&[CodingAgentProvider]>,
    inventory: Option<&CodingAgentRunInventory>,
) -> Result<(), String> {
    match (capability, providers, inventory) {
        (false, None, None) => return Ok(()),
        (false, _, _) => {
            return Err(
                "coding-agent provider/inventory metadata requires coding_agent_runs capability"
                    .to_string(),
            )
        }
        (true, Some(providers), Some(inventory)) if !providers.is_empty() => {
            if providers.len() > CODING_AGENT_MAX_PROVIDERS {
                return Err("coding-agent provider inventory exceeds bounded limit".to_string());
            }
            if inventory.runs.len() > CODING_AGENT_MAX_INVENTORY_RUNS {
                return Err("coding-agent Run inventory exceeds bounded limit".to_string());
            }
            let mut provider_ids = HashSet::new();
            let mut provider_instances = HashSet::new();
            for provider in providers {
                validate_coding_agent_provider_id(&provider.provider_id)
                    .map_err(|error| format!("invalid coding-agent provider id: {error}"))?;
                validate_coding_agent_provider_instance_id(&provider.provider_instance_id)
                    .map_err(|error| format!("invalid coding-agent provider instance: {error}"))?;
                if provider.name.trim().is_empty()
                    || provider.name.len() > CODING_AGENT_MAX_PROVIDER_NAME_BYTES
                    || provider.name.chars().any(char::is_control)
                {
                    return Err("invalid coding-agent provider name".to_string());
                }
                if !provider_ids.insert(provider.provider_id.as_str())
                    || !provider_instances.insert(provider.provider_instance_id.as_str())
                {
                    return Err("duplicate coding-agent provider identity".to_string());
                }
            }
            let expected_project_prefix = format!("agent:{client_id}:");
            let mut run_ids = HashSet::new();
            for run in &inventory.runs {
                validate_coding_agent_run_snapshot(run)
                    .map_err(|error| format!("invalid coding-agent Run snapshot: {error}"))?;
                if !run_ids.insert(run.run_id.as_str()) {
                    return Err("duplicate coding-agent Run id in inventory".to_string());
                }
                if !run.runtime_project_id.starts_with(&expected_project_prefix) {
                    return Err(
                        "coding-agent Run inventory references another Runner project namespace"
                            .to_string(),
                    );
                }
                if !run.state.terminal()
                    && !providers.iter().any(|provider| {
                        provider.provider_id == run.provider_id
                            && provider.provider_instance_id == run.provider_instance_id
                    })
                {
                    return Err(
                        "active coding-agent Run references a stale provider instance".to_string(),
                    );
                }
            }
            Ok(())
        }
        (true, _, _) => Err(
            "coding_agent_runs capability requires non-empty provider inventory and Run inventory"
                .to_string(),
        ),
    }
}

fn reject_same_instance_feature_downgrade(
    existing: Option<&RunnerRecord>,
    runner_instance_id: &str,
    incoming: &RunnerFeatureSet,
    feature: RunnerFeature,
) -> Result<(), String> {
    if existing.is_some_and(|existing| {
        existing.runner_instance_id == runner_instance_id
            && existing.runner_features.supports(feature)
            && !incoming.supports(feature)
    }) {
        return Err(format!(
            "same runner instance cannot downgrade {} capability",
            feature.as_wire_name()
        ));
    }
    Ok(())
}

struct StreamingSessionRegistration {
    connection_id: String,
    transport: RunnerTransport,
    notify: Arc<Notify>,
    cancel: watch::Sender<bool>,
}

impl RunnerRegistry {
    #[cfg(any(test, feature = "root-test-support"))]
    pub async fn register(&self, body: RunnerRegisterRequest) -> Result<RunnerView, String> {
        self.register_with_auth(body, None).await
    }

    pub async fn register_with_auth(
        &self,
        body: RunnerRegisterRequest,
        auth: Option<&crate::RunnerAccess>,
    ) -> Result<RunnerView, String> {
        self.register_session(body, auth, None).await
    }

    #[cfg(any(test, feature = "root-test-support"))]
    pub async fn register_streaming_session(
        &self,
        body: RunnerRegisterRequest,
        auth: Option<&crate::RunnerAccess>,
        connection_id: &str,
        transport: RunnerTransport,
        notify: Arc<Notify>,
    ) -> Result<RunnerView, String> {
        let (cancel, _cancelled) = watch::channel(false);
        self.register_streaming_session_with_cancel_sender(
            body,
            auth,
            connection_id,
            transport,
            notify,
            cancel,
        )
        .await
    }

    /// Production streaming registration returns the receiver for the concrete
    /// connection cancellation lease. The receiver is created before
    /// validation but becomes authoritative only if `register_session` commits;
    /// a failed replacement cannot signal the currently active session.
    pub async fn register_streaming_session_with_cancel(
        &self,
        body: RunnerRegisterRequest,
        auth: Option<&crate::RunnerAccess>,
        connection_id: &str,
        transport: RunnerTransport,
        notify: Arc<Notify>,
    ) -> Result<(RunnerView, watch::Receiver<bool>), String> {
        let (cancel, cancelled) = watch::channel(false);
        let view = self
            .register_streaming_session_with_cancel_sender(
                body,
                auth,
                connection_id,
                transport,
                notify,
                cancel,
            )
            .await?;
        Ok((view, cancelled))
    }

    async fn register_streaming_session_with_cancel_sender(
        &self,
        body: RunnerRegisterRequest,
        auth: Option<&crate::RunnerAccess>,
        connection_id: &str,
        transport: RunnerTransport,
        notify: Arc<Notify>,
        cancel: watch::Sender<bool>,
    ) -> Result<RunnerView, String> {
        validate_id(connection_id, "connection_id")?;
        if transport == RunnerTransport::Polling {
            return Err("streaming Runner transport is unsupported".to_string());
        }
        self.register_session(
            body,
            auth,
            Some(StreamingSessionRegistration {
                connection_id: connection_id.to_string(),
                transport,
                notify,
                cancel,
            }),
        )
        .await
    }

    async fn register_session(
        &self,
        body: RunnerRegisterRequest,
        auth: Option<&crate::RunnerAccess>,
        streaming: Option<StreamingSessionRegistration>,
    ) -> Result<RunnerView, String> {
        validate_id(&body.client_id, "client_id")?;
        validate_runner_instance_id(&body.runner_instance_id)?;
        validate_optional_field(&body.display_name, "display_name")?;
        validate_optional_field(&body.owner, "owner")?;
        validate_optional_field(&body.hostname, "hostname")?;
        if body.job_concurrency_limit.is_some_and(|limit| {
            !(RUNNER_JOB_CONCURRENCY_MIN..=RUNNER_JOB_CONCURRENCY_MAX).contains(&limit)
        }) {
            return Err(format!(
                "job_concurrency_limit must be between {RUNNER_JOB_CONCURRENCY_MIN} and {RUNNER_JOB_CONCURRENCY_MAX}"
            ));
        }

        let client_id = body.client_id.trim().to_string();
        let runner_instance_id = body.runner_instance_id.trim().to_string();
        let accepted_protocol =
            AcceptedRunnerProtocol::try_from_registration(body.runner_protocol_generation)?;
        let runner_features = RunnerFeatureSet::try_from_registration(&body.capabilities)?;
        if runner_features.supports(RunnerFeature::ApplyPatchMatchMetadata)
            && !runner_features.supports(RunnerFeature::ApplyPatch)
        {
            return Err(
                "apply_patch_match_metadata capability requires apply_patch capability".to_string(),
            );
        }
        if runner_features.supports(RunnerFeature::ApplyPatchStrictMatching)
            && !runner_features.supports(RunnerFeature::ApplyPatchMatchMetadata)
        {
            return Err(
                "apply_patch_strict_matching capability requires apply_patch_match_metadata capability"
                    .to_string(),
            );
        }
        if runner_features.supports(RunnerFeature::ApplyPatchMatchingMode)
            && !runner_features.supports(RunnerFeature::ApplyPatchMatchMetadata)
        {
            return Err(
                "apply_patch_matching_mode capability requires apply_patch_match_metadata capability"
                    .to_string(),
            );
        }
        let job_inventory = body.job_inventory.clone();
        let coding_agent_providers = body.coding_agent_providers.clone();
        let coding_agent_inventory = body.coding_agent_inventory.clone();
        validate_coding_agent_registration(
            &client_id,
            runner_features.supports(RunnerFeature::CodingAgentRuns),
            coding_agent_providers.as_deref(),
            coding_agent_inventory.as_ref(),
        )?;
        let coding_agent_providers = coding_agent_providers.unwrap_or_default();
        let coding_agent_inventory = coding_agent_inventory.unwrap_or_default();
        let host_context = body
            .host_context
            .clone()
            .map(webcodex_core::runner_protocol::RunnerHostContext::normalized)
            .transpose()?;
        let mut policy = body.policy;
        if let Some(policy) = policy.as_mut() {
            policy.tool_providers = normalize_tool_providers(policy.tool_providers.take());
        }
        if let Some(providers) = policy
            .as_ref()
            .and_then(|policy| policy.mcp_gateway_providers.as_ref())
        {
            validate_providers(providers)
                .map_err(|error| format!("invalid MCP gateway provider inventory: {error}"))?;
        }
        let now = now_ts();
        // Registration establishes liveness only. Project routing becomes authoritative
        // exclusively through the bounded paged inventory protocol.
        let projects = Vec::new();
        let project_inventory = pending_inventory_state(0);
        let record = RunnerRecord {
            client_id: client_id.clone(),
            runner_instance_id: runner_instance_id.clone(),
            display_name: trim_string(body.display_name),
            owner: trim_string(body.owner),
            hostname: trim_string(body.hostname),
            host_context,
            runner_features: runner_features.clone(),
            projects,
            project_inventory,
            last_seen: now,
            transport: streaming
                .as_ref()
                .map(|session| session.transport)
                .unwrap_or(RunnerTransport::Polling),
            accepted_protocol,
            policy,
            auth_group: auth.and_then(|access| access.group.clone()),
            registered_at: now,
            connected_at: now,
            connection_id: streaming
                .as_ref()
                .map(|session| session.connection_id.clone()),
            disconnected_at: None,
            process_started_at: body.process_started_at,
            build: body.build,
            job_concurrency_limit: body.job_concurrency_limit,
            coding_agent_providers: coding_agent_providers.clone(),
            coding_agent_inventory,
            projected_structured_terminal_suppressions: VecDeque::new(),
        };
        match (
            runner_features.supports(RunnerFeature::JobStateReconciliation),
            job_inventory.as_ref(),
        ) {
            (true, Some(inventory)) => {
                // Registration inventory is always paged. Validate the bounded Job snapshot
                // and same-runner namespace now; exact project membership is established by
                // the authoritative project inventory before any project becomes routable.
                validate_job_inventory_without_project_membership(&client_id, inventory)?;
            }
            (true, None) => {
                return Err(
                    "job_state_reconciliation capability requires job_inventory".to_string()
                );
            }
            (false, Some(_)) => {
                return Err(
                    "job_inventory requires job_state_reconciliation capability".to_string()
                );
            }
            (false, None) => {}
        }
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);

        if inner
            .runners
            .get(&client_id)
            .is_some_and(|existing| existing.auth_group != record.auth_group)
        {
            return Err("runner identity is unavailable".to_string());
        }
        if let Some(RunnerAccessGroup::SharedKey(group)) = record.auth_group.as_ref() {
            let is_new_runner = !inner.runners.contains_key(&client_id);
            if is_new_runner {
                let group_count = inner
                    .runners
                    .values()
                    .filter(|runner| {
                        matches!(
                            runner.auth_group.as_ref(),
                            Some(RunnerAccessGroup::SharedKey(existing_group))
                                if existing_group == group
                        )
                    })
                    .count();
                if group_count >= self.shared_key_limits.per_group {
                    return Err(format!(
                        "shared-key runner group limit reached (maximum {} runners)",
                        self.shared_key_limits.per_group
                    ));
                }
                let global_count = inner
                    .runners
                    .values()
                    .filter(|runner| {
                        matches!(runner.auth_group, Some(RunnerAccessGroup::SharedKey(_)))
                    })
                    .count();
                if global_count >= self.shared_key_limits.global {
                    return Err(format!(
                        "shared-key runner global limit reached (maximum {} runners)",
                        self.shared_key_limits.global
                    ));
                }
            }
        }
        if inner
            .retired_instances
            .get(&client_id)
            .is_some_and(|retired| retired.iter().any(|id| id == &runner_instance_id))
        {
            return Err(format!(
                "runner {} instance was replaced and cannot reclaim the lease",
                client_id
            ));
        }
        reject_same_instance_feature_downgrade(
            inner.runners.get(&client_id),
            &runner_instance_id,
            &runner_features,
            RunnerFeature::JobStateReconciliation,
        )?;
        reject_same_instance_feature_downgrade(
            inner.runners.get(&client_id),
            &runner_instance_id,
            &runner_features,
            RunnerFeature::CodingAgentRuns,
        )?;
        reject_same_instance_feature_downgrade(
            inner.runners.get(&client_id),
            &runner_instance_id,
            &runner_features,
            RunnerFeature::StructuredCargoTestExecutionPolicy,
        )?;
        reject_same_instance_feature_downgrade(
            inner.runners.get(&client_id),
            &runner_instance_id,
            &runner_features,
            RunnerFeature::NativeToolPlugins,
        )?;
        if inner.runners.get(&client_id).is_some_and(|existing| {
            existing.runner_instance_id == runner_instance_id
                && existing.coding_agent_providers != coding_agent_providers
        }) {
            return Err(
                "same runner instance cannot change ACP coding-agent provider inventory"
                    .to_string(),
            );
        }
        if inner.runners.get(&client_id).is_some_and(|existing| {
            existing.runner_instance_id == runner_instance_id
                && existing
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.mcp_gateway_providers.as_ref())
                    != record
                        .policy
                        .as_ref()
                        .and_then(|policy| policy.mcp_gateway_providers.as_ref())
        }) {
            return Err(
                "same runner instance cannot change MCP gateway provider inventory".to_string(),
            );
        }
        // A successful different-instance registration is an explicit lease
        // takeover. `last_seen` remains a passive liveness grace for temporary
        // network gaps; it must not turn a deliberate process restart into a
        // 60-second registration lock. The replacement path below retires the
        // old instance before the new lease becomes authoritative.
        let replaced_instance = inner
            .runners
            .get(&client_id)
            .map(|existing| existing.runner_instance_id != runner_instance_id)
            .unwrap_or(false);
        let replaced_instance_id = replaced_instance.then(|| {
            inner
                .runners
                .get(&client_id)
                .expect("replacement requires existing runner")
                .runner_instance_id
                .clone()
        });
        if let Some(inventory) = job_inventory.as_ref() {
            preflight_inventory_locked(&inner, &client_id, &runner_instance_id, inventory)?;
        }
        // All fallible validation/preflight is complete. Capture the previous
        // streaming session's cancellation sender now, but do not signal it
        // until the new authoritative record/notifier/connection lease is fully
        // committed and the registry mutex has been released.
        let replaced_streaming_cancel = inner
            .notifiers
            .get(&client_id)
            .map(|entry| entry.cancel.clone());
        if replaced_instance {
            let replaced_instance_id = replaced_instance_id
                .as_deref()
                .expect("replacement id captured");
            terminate_instance_jobs_locked(
                &mut inner,
                &client_id,
                replaced_instance_id,
                job_inventory.as_ref(),
                now,
            );
            // A different `agent_instance_id` is a new Runner process; pending
            // synchronous requests were admitted for the process that existed
            // when they were queued, so they must not be inherited by or
            // dispatched to the replacement. Fail and drain them here, under
            // the same lock and before the new lease is installed, preserving
            // `request_dispatched` truth. Job-backed requests are governed by
            // `terminate_instance_jobs_locked` above and are never drained as
            // synchronous requests.
            resolve_disconnected_sync_requests_locked(
                &mut inner,
                &client_id,
                "runner instance was replaced before the request completed; the request was not handed to the replacement instance",
            );
            let retired = inner
                .retired_instances
                .entry(client_id.clone())
                .or_default();
            if !retired.iter().any(|id| id == replaced_instance_id) {
                retired.push_back(replaced_instance_id.to_string());
                while retired.len() > MAX_RETIRED_INSTANCES_PER_RUNNER {
                    retired.pop_front();
                }
            }
        }

        // Same-instance re-register is a transport reconnect: keep the
        // original registration time; `connected_at` reflects the new
        // connection. A new instance starts a fresh lifecycle.
        let mut record = record;
        if let Some(existing) = inner.runners.get(&client_id) {
            if existing.runner_instance_id == runner_instance_id {
                record.registered_at = existing.registered_at;
                record.projected_structured_terminal_suppressions =
                    existing.projected_structured_terminal_suppressions.clone();
                record.prune_projected_structured_terminal_suppressions(now);
            }
            // Identity/liveness registration is independent from inventory, but
            // routing authority is not transferable across Runner processes. A
            // same-instance transport reconnect may keep its last complete
            // authoritative project projection while a fresh paged snapshot is
            // pending/degraded. A different `agent_instance_id` must use the
            // projection built from its own registration (empty for V2 paged
            // registration) until that instance completes an atomic snapshot.
            if existing.runner_instance_id == runner_instance_id {
                record.projects = existing.projects.clone();
                record.project_inventory = preserve_authoritative_pending(
                    &existing.project_inventory,
                    record.projects.len(),
                );
            }
        }

        // Every successful registration replaces the concrete transport
        // lease, including a same-instance reconnect. Streaming transport,
        // connection lease, and notifier are committed under this same lock so
        // the registry never exposes a half-registered long-lived session.
        inner.notifiers.remove(&client_id);
        inner.runners.insert(client_id.clone(), record);
        if let Some(streaming) = streaming {
            inner.notifiers.insert(
                client_id.clone(),
                NotifierEntry {
                    notify: streaming.notify,
                    cancel: streaming.cancel,
                    runner_instance_id: runner_instance_id.clone(),
                    connection_id: Some(streaming.connection_id),
                },
            );
        }
        if let Some(inventory) = job_inventory.as_ref() {
            let auth_group = inner
                .runners
                .get(&client_id)
                .and_then(|runner| runner.auth_group.clone());
            let reconciliation = reconcile_inventory_locked(
                &mut inner,
                &client_id,
                &runner_instance_id,
                auth_group,
                self.observation_epoch.clone(),
                inventory,
                now,
            );
            let process_started_at = inner
                .runners
                .get(&client_id)
                .and_then(|runner| runner.process_started_at);
            tracing::info!(
                target: "webcodex::job_reconciliation",
                client_id = %client_id,
                runner_instance_id = %runner_instance_id,
                process_started_at = ?process_started_at,
                inventory_active = reconciliation.inventory_active,
                inventory_terminal = reconciliation.inventory_terminal,
                reconstructed = reconciliation.reconstructed,
                updated = reconciliation.updated,
                missing = reconciliation.missing,
                suppressed_terminal = reconciliation.suppressed_terminal,
                "runner job inventory reconciled"
            );
        }
        let view = Self::runner_view_locked(&inner, &client_id).expect("runner just inserted");
        drop(inner);
        if let Some(cancel) = replaced_streaming_cancel {
            let _ = cancel.send(true);
        }
        Ok(view)
    }

    /// Test-only hook for observability projections that need to model a
    /// transport without opening a real streaming connection. Production
    /// streaming registration commits its transport atomically.
    #[cfg(any(test, feature = "root-test-support"))]
    pub async fn set_transport(
        &self,
        client_id: &str,
        transport: RunnerTransport,
    ) -> Result<(), String> {
        self.set_transport_checked(client_id, None, None, transport)
            .await
    }

    #[cfg(any(test, feature = "root-test-support"))]
    async fn set_transport_checked(
        &self,
        client_id: &str,
        runner_instance_id: Option<&str>,
        connection_id: Option<&str>,
        transport: RunnerTransport,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get_mut(client_id) else {
            return Err(format!("unknown shell client: {}", client_id));
        };
        if runner_instance_id.is_some_and(|id| runner.runner_instance_id != id)
            || connection_id.is_some_and(|id| runner.connection_id.as_deref() != Some(id))
        {
            return Err(format!(
                "runner {} transport connection is no longer active",
                client_id
            ));
        }
        runner.transport = transport;
        Ok(())
    }

    /// Refresh `last_seen` for a registered Runner to "now" without performing
    /// any business operation. Used by keepalive traffic to keep active
    /// long-lived transports inside the online window. Polling Runners have no
    /// server-internal connection lease and use this path directly; long-lived
    /// transports use [`Self::touch_runner_for_connection`] instead so a stale
    /// same-instance connection cannot revive the new lease.
    #[cfg(test)]
    pub async fn touch_runner(
        &self,
        client_id: &str,
        runner_instance_id: &str,
    ) -> Result<(), String> {
        validate_runner_instance_id(runner_instance_id)?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get_mut(client_id) else {
            return Err(format!("unknown shell client: {}", client_id));
        };
        if runner.runner_instance_id != runner_instance_id {
            return Err(format!(
                "runner {} is no longer the active instance (stale or replaced)",
                client_id
            ));
        }
        runner.last_seen = now_ts();
        Ok(())
    }

    /// Connection-scoped keepalive refresh for long-lived transports
    /// (WebSocket/QUIC Ping/Pong). Validates `client_id`,
    /// `agent_instance_id`, and the current `connection_id` under the same
    /// registry mutex that owns `last_seen`. A delayed Ping/Pong from a stale
    /// same-instance connection (replaced by a reconnect) returns a stable
    /// error and must not refresh the new connection's `last_seen` or revive
    /// an already-disconnected runner. Polling keepalive keeps using
    /// [`RunnerRegistry::touch_runner`].
    pub async fn touch_runner_for_connection(
        &self,
        client_id: &str,
        runner_instance_id: &str,
        connection_id: &str,
    ) -> Result<(), String> {
        validate_runner_instance_id(runner_instance_id)?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get_mut(client_id) else {
            return Err(format!("unknown shell client: {}", client_id));
        };
        if runner.runner_instance_id != runner_instance_id {
            return Err(format!(
                "runner {} is no longer the active instance (stale or replaced)",
                client_id
            ));
        }
        if runner.connection_id.as_deref() != Some(connection_id) {
            return Err(format!(
                "runner {} transport connection is no longer active",
                client_id
            ));
        }
        runner.last_seen = now_ts();
        Ok(())
    }

    /// Apply sanitized provider metadata to the active Runner record. Optional
    /// metadata is best-effort: malformed/unknown state is ignored by the
    /// normalizer and never changes transport or tool completion semantics.
    /// Polling-transport `RuntimeMetadata` uses this path.
    pub async fn update_tool_providers(
        &self,
        client_id: &str,
        runner_instance_id: &str,
        status: Option<webcodex_core::runner_protocol::ToolProvidersStatus>,
    ) -> Result<(), String> {
        self.update_tool_providers_checked(client_id, runner_instance_id, None, status)
            .await
    }

    /// Connection-scoped `RuntimeMetadata` for long-lived transports. A stale
    /// same-instance connection must not overwrite the current connection's
    /// provider metadata or refresh its liveness: when the connection no
    /// longer holds the lease the update is rejected with a stable error.
    pub async fn update_tool_providers_for_connection(
        &self,
        client_id: &str,
        runner_instance_id: &str,
        connection_id: &str,
        status: Option<webcodex_core::runner_protocol::ToolProvidersStatus>,
    ) -> Result<(), String> {
        self.update_tool_providers_checked(
            client_id,
            runner_instance_id,
            Some(connection_id),
            status,
        )
        .await
    }

    async fn update_tool_providers_checked(
        &self,
        client_id: &str,
        runner_instance_id: &str,
        expected_connection_id: Option<&str>,
        status: Option<webcodex_core::runner_protocol::ToolProvidersStatus>,
    ) -> Result<(), String> {
        let Some(status) = normalize_tool_providers(status) else {
            return Ok(());
        };
        validate_runner_instance_id(runner_instance_id)?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get_mut(client_id) else {
            return Err(format!("unknown shell client: {}", client_id));
        };
        if runner.runner_instance_id != runner_instance_id {
            return Err(format!(
                "runner {} is no longer the active instance (stale or replaced)",
                client_id
            ));
        }
        if let Some(expected) = expected_connection_id {
            if runner.connection_id.as_deref() != Some(expected) {
                return Err(format!(
                    "runner {} transport connection is no longer active",
                    client_id
                ));
            }
        }
        if let Some(policy) = runner.policy.as_mut() {
            policy.tool_providers = Some(status);
        }
        runner.last_seen = now_ts();
        Ok(())
    }

    /// Test-only hook to force a runner's `last_seen` so liveness/stale
    /// behavior can be exercised without sleeping for the full online window.
    #[cfg(any(test, feature = "root-test-support"))]
    pub async fn set_last_seen_for_test(&self, client_id: &str, ts: i64) {
        let mut inner = self.inner.lock().await;
        if let Some(runner) = inner.runners.get_mut(client_id) {
            runner.last_seen = ts;
        }
    }

    pub(super) fn prune_expired_shared_key_runners_locked(
        &self,
        inner: &mut RunnerRegistryInner,
        now: i64,
    ) {
        let expired = inner
            .runners
            .iter()
            .filter_map(|(client_id, runner)| {
                if !matches!(runner.auth_group, Some(RunnerAccessGroup::SharedKey(_))) {
                    return None;
                }
                let transport_connected = inner.notifiers.contains_key(client_id);
                let recently_seen =
                    now.saturating_sub(runner.last_seen) <= RUNNER_ONLINE_WINDOW_SECS;
                if transport_connected || recently_seen {
                    return None;
                }
                let offline_since = runner.disconnected_at.unwrap_or(runner.last_seen);
                (now.saturating_sub(offline_since) > self.shared_key_limits.offline_ttl_secs)
                    .then(|| client_id.clone())
            })
            .collect::<Vec<_>>();
        if expired.is_empty() {
            return;
        }

        let expired_set = expired.iter().cloned().collect::<HashSet<_>>();
        let request_ids = inner
            .pending_by_id
            .iter()
            .filter(|(_, pending)| expired_set.contains(&pending.request.client_id))
            .map(|(request_id, _)| request_id.clone())
            .collect::<HashSet<_>>();
        let job_ids = inner
            .jobs_by_id
            .iter()
            .filter(|(_, job)| expired_set.contains(&job.client_id))
            .map(|(job_id, _)| job_id.clone())
            .collect::<HashSet<_>>();

        for client_id in &expired {
            resolve_disconnected_sync_requests_locked(
                inner,
                client_id,
                "runner went offline: shared-key runner registration expired",
            );
        }
        for job_id in &job_ids {
            if let Some(job) = inner.jobs_by_id.get_mut(job_id) {
                mark_job_lost(
                    job,
                    now,
                    JobRecoveryReason::SharedKeyRunnerExpired,
                    "shared-key runner registration expired after being offline",
                );
            }
        }
        for request_id in &request_ids {
            inner.pending_by_id.remove(request_id);
            inner.persistent_waiters.remove(request_id);
        }
        inner.request_to_job.retain(|request_id, job_id| {
            !request_ids.contains(request_id) && !job_ids.contains(job_id)
        });
        for client_id in &expired {
            inner.runners.remove(client_id);
            inner.queues_by_runner.remove(client_id);
            inner.notifiers.remove(client_id);
            inner.retired_instances.remove(client_id);
            let project_prefix = format!("agent:{client_id}:");
            inner
                .unregistering_projects
                .retain(|project_id, _| !project_id.starts_with(&project_prefix));
        }
        self.cleanup_intents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|job_id, _| !job_ids.contains(job_id));
    }

    pub(super) async fn prune_expired_shared_key_runners(&self) {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
    }

    /// Test-only hook for instance-lease notifier regressions that intentionally
    /// exercise notifier replacement independently. Production streaming
    /// registration installs its notifier atomically with the runner record.
    #[cfg(test)]
    pub async fn register_notifier(
        &self,
        client_id: &str,
        runner_instance_id: &str,
        notify: Arc<Notify>,
    ) -> Result<(), String> {
        self.register_notifier_checked(client_id, runner_instance_id, None, notify)
            .await
    }

    #[cfg(test)]
    async fn register_notifier_checked(
        &self,
        client_id: &str,
        runner_instance_id: &str,
        connection_id: Option<&str>,
        notify: Arc<Notify>,
    ) -> Result<(), String> {
        validate_runner_instance_id(runner_instance_id)?;
        let mut inner = self.inner.lock().await;
        let Some(runner) = inner.runners.get(client_id) else {
            return Err(format!("unknown shell client: {}", client_id));
        };
        if runner.runner_instance_id != runner_instance_id {
            return Err(format!(
                "runner {} is no longer the active instance (stale or replaced)",
                client_id
            ));
        }
        if connection_id.is_some_and(|id| runner.connection_id.as_deref() != Some(id)) {
            return Err(format!(
                "runner {} transport connection is no longer active",
                client_id
            ));
        }
        let (cancel, _cancelled) = watch::channel(false);
        inner.notifiers.insert(
            client_id.to_string(),
            NotifierEntry {
                notify,
                cancel,
                runner_instance_id: runner_instance_id.to_string(),
                connection_id: connection_id.map(str::to_string),
            },
        );
        Ok(())
    }

    /// Reconcile state after a Runner transport disconnects or sends a
    /// graceful offline notice.
    #[cfg(any(test, feature = "root-test-support"))]
    pub async fn reconcile_disconnect(&self, client_id: &str, runner_instance_id: &str) {
        let _ = self
            .reconcile_disconnect_checked(client_id, runner_instance_id, None, None)
            .await;
    }

    /// Apply a graceful polling shutdown only when this exact process still
    /// owns the active polling lease. A delayed notice from a replaced process,
    /// or from an old polling request after the same instance moved to a
    /// streaming connection, is an idempotent no-op.
    pub async fn reconcile_polling_disconnect(
        &self,
        client_id: &str,
        runner_instance_id: &str,
    ) -> Result<bool, String> {
        validate_id(client_id, "client_id")?;
        validate_runner_instance_id(runner_instance_id)?;
        Ok(self
            .reconcile_disconnect_checked(
                client_id,
                runner_instance_id,
                None,
                Some(RunnerTransport::Polling),
            )
            .await)
    }

    pub async fn reconcile_disconnect_for_connection(
        &self,
        client_id: &str,
        runner_instance_id: &str,
        connection_id: &str,
    ) {
        let _ = self
            .reconcile_disconnect_checked(client_id, runner_instance_id, Some(connection_id), None)
            .await;
    }

    async fn reconcile_disconnect_checked(
        &self,
        client_id: &str,
        runner_instance_id: &str,
        connection_id: Option<&str>,
        expected_transport: Option<RunnerTransport>,
    ) -> bool {
        let mut inner = self.inner.lock().await;
        let is_active = inner
            .runners
            .get(client_id)
            .map(|runner| {
                runner.runner_instance_id == runner_instance_id
                    && connection_id
                        .map(|id| runner.connection_id.as_deref() == Some(id))
                        .unwrap_or(true)
                    && expected_transport
                        .map(|transport| runner.transport == transport)
                        .unwrap_or(true)
            })
            .unwrap_or(false);
        if !is_active {
            return false;
        }
        if inner
            .notifiers
            .get(client_id)
            .map(|entry| {
                entry.runner_instance_id == runner_instance_id
                    && connection_id
                        .map(|id| entry.connection_id.as_deref() == Some(id))
                        .unwrap_or(true)
            })
            .unwrap_or(false)
        {
            inner.notifiers.remove(client_id);
        }
        let now = now_ts();
        let recoverable = inner.runners.get(client_id).is_some_and(|runner| {
            runner
                .runner_features
                .supports(RunnerFeature::JobStateReconciliation)
        });
        if let Some(runner) = inner.runners.get_mut(client_id) {
            runner.last_seen = offline_last_seen(now);
            runner.disconnected_at = Some(now);
        }
        let affected_job_ids: Vec<String> = inner
            .jobs_by_id
            .iter()
            .filter_map(|(job_id, job)| {
                if job.client_id != client_id {
                    return None;
                }
                if job.lifecycle.is_terminal()
                    || !(job.lifecycle == JobLifecycleState::Queued
                        || job.lifecycle.is_runner_active())
                {
                    return None;
                }
                Some(job_id.clone())
            })
            .collect();
        for job_id in affected_job_ids {
            let request_id = inner
                .jobs_by_id
                .get(&job_id)
                .and_then(|j| j.request_id.clone());
            if let Some(job) = inner.jobs_by_id.get_mut(&job_id) {
                if recoverable && job.lifecycle != JobLifecycleState::Queued {
                    begin_job_recovery(job, now, JobRecoveryReason::RunnerTransportDisconnected);
                } else {
                    let (reason, message) = if job.lifecycle == JobLifecycleState::Queued {
                        (
                            JobRecoveryReason::RunnerRequestNotDispatched,
                            "Runner transport disconnected before the queued request was dispatched",
                        )
                    } else {
                        (
                            JobRecoveryReason::RunnerDisconnectedWithoutReconciliation,
                            "Runner transport disconnected without reconciliation support",
                        )
                    };
                    mark_job_lost(job, now, reason, message);
                }
            }
            if let Some(request_id) = request_id {
                inner.pending_by_id.remove(&request_id);
                inner.request_to_job.remove(&request_id);
                if let Some(queue) = inner.queues_by_runner.get_mut(client_id) {
                    queue.retain(|id| id != &request_id);
                }
            }
        }
        // Drop any additional job-backed control request (for example an
        // undispatched stop request). Reconciliation restores executor truth;
        // it never replays a stale server queue after reconnect.
        let extra_job_requests = inner
            .pending_by_id
            .iter()
            .filter(|(_, pending)| {
                pending.request.client_id == client_id && pending.job_id.is_some()
            })
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        for request_id in extra_job_requests {
            inner.pending_by_id.remove(&request_id);
            inner.request_to_job.remove(&request_id);
            if let Some(queue) = inner.queues_by_runner.get_mut(client_id) {
                queue.retain(|id| id != &request_id);
            }
        }
        // Synchronous tool requests (run_shell/read_file/write/lsp/project ops)
        // carry a live oneshot waiter but no job_id, so the job loop above skips
        // them. Fail them fast here; otherwise the calling tool blocks until its
        // own wait timeout (tens of seconds) after the Runner has already gone,
        // which surfaces as an unresponsive MCP `tools/call`.
        resolve_disconnected_sync_requests_locked(
            &mut inner,
            client_id,
            "runner went offline: transport disconnected before returning a result",
        );
        true
    }

    #[cfg(any(test, feature = "root-test-support"))]
    pub async fn list_runners(&self) -> Vec<RunnerView> {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
        for runner in inner.runners.values_mut() {
            expire_staging(runner, now);
        }
        let mut ids = inner.runners.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| Self::runner_view_locked(&inner, &id))
            .collect()
    }

    pub async fn list_runners_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
    ) -> Vec<RunnerView> {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
        for runner in inner.runners.values_mut() {
            expire_staging(runner, now);
        }
        let mut ids = inner.runners.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        ids.into_iter()
            .filter(|id| {
                inner
                    .runners
                    .get(id)
                    .map(|runner| runner_visible_to_access(auth, runner))
                    .unwrap_or(false)
            })
            .filter_map(|id| Self::runner_view_locked(&inner, &id))
            .collect()
    }

    pub async fn has_connected_shared_key_group(&self, shared_key_hash: &str) -> bool {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
        inner.runners.values().any(|runner| {
            matches!(
                runner.auth_group.as_ref(),
                Some(RunnerAccessGroup::SharedKey(group)) if group == shared_key_hash
            ) && now.saturating_sub(runner.last_seen) <= RUNNER_ONLINE_WINDOW_SECS
        })
    }

    /// Return canonical feature snapshots for currently-online Runners in one
    /// exact shared-key authorization group. Callers must evaluate each set as
    /// a whole; combining features across different Runners would overstate
    /// executable authority.
    pub async fn connected_shared_key_group_feature_sets(
        &self,
        shared_key_hash: &str,
    ) -> Vec<RunnerFeatureSet> {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
        inner
            .runners
            .values()
            .filter(|runner| {
                matches!(
                    runner.auth_group.as_ref(),
                    Some(RunnerAccessGroup::SharedKey(group)) if group == shared_key_hash
                ) && now.saturating_sub(runner.last_seen) <= RUNNER_ONLINE_WINDOW_SECS
            })
            .map(|runner| runner.runner_features.clone())
            .collect()
    }

    pub async fn list_runner_semantic_views_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
    ) -> Vec<RunnerSemanticView> {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
        for runner in inner.runners.values_mut() {
            expire_staging(runner, now);
        }
        let mut ids = inner.runners.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        ids.into_iter()
            .filter(|id| {
                inner
                    .runners
                    .get(id)
                    .map(|runner| runner_visible_to_access(auth, runner))
                    .unwrap_or(false)
            })
            .filter_map(|id| Self::runner_semantic_view_locked(&inner, &id))
            .collect()
    }

    /// Return a complete canonical Runner/Project observation only when both
    /// caller-supplied cardinality bounds hold. `None` means the observation is
    /// incomplete and must never support a negative authority conclusion.
    pub async fn list_bounded_runner_semantic_views_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
        max_runners: usize,
        max_projects: usize,
    ) -> Option<Vec<RunnerSemanticView>> {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
        for runner in inner.runners.values_mut() {
            expire_staging(runner, now);
        }
        Self::bounded_runner_semantic_views_for_auth_locked(&inner, auth, max_runners, max_projects)
    }

    /// Hold one bounded authoritative Runner/Project registry observation stable
    /// while a synchronous Control operation decides against it. `None` is an
    /// incomplete observation; callers must fail closed. Callers must not await
    /// inside `f`.
    pub async fn with_bounded_runner_semantic_views_for_auth_locked<R>(
        &self,
        auth: Option<&crate::RunnerAccess>,
        max_runners: usize,
        max_projects: usize,
        f: impl FnOnce(Option<Vec<RunnerSemanticView>>) -> R,
    ) -> R {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
        for runner in inner.runners.values_mut() {
            expire_staging(runner, now);
        }
        let views = Self::bounded_runner_semantic_views_for_auth_locked(
            &inner,
            auth,
            max_runners,
            max_projects,
        );
        let result = f(views);
        drop(inner);
        result
    }

    pub async fn get_runner_view(&self, client_id: &str) -> Option<RunnerView> {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
        if let Some(runner) = inner.runners.get_mut(client_id) {
            expire_staging(runner, now);
        }
        Self::runner_view_locked(&inner, client_id)
    }

    #[cfg(any(test, feature = "root-test-support"))]
    pub async fn get_runner_view_for_connection(
        &self,
        client_id: &str,
        runner_instance_id: &str,
        connection_id: &str,
    ) -> Option<RunnerView> {
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now_ts());
        let runner = inner.runners.get(client_id)?;
        if runner.runner_instance_id != runner_instance_id
            || runner.connection_id.as_deref() != Some(connection_id)
        {
            return None;
        }
        Self::runner_view_locked(&inner, client_id)
    }

    pub async fn get_runner_view_for_auth(
        &self,
        client_id: &str,
        auth: Option<&crate::RunnerAccess>,
    ) -> Option<RunnerView> {
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now_ts());
        let runner = inner.runners.get(client_id)?;
        if !runner_visible_to_access(auth, runner) {
            return None;
        }
        Self::runner_view_locked(&inner, client_id)
    }

    pub async fn get_runner_semantic_view(&self, client_id: &str) -> Option<RunnerSemanticView> {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
        if let Some(runner) = inner.runners.get_mut(client_id) {
            expire_staging(runner, now);
        }
        Self::runner_semantic_view_locked(&inner, client_id)
    }

    pub async fn get_runner_semantic_view_for_auth(
        &self,
        client_id: &str,
        auth: Option<&crate::RunnerAccess>,
    ) -> Option<RunnerSemanticView> {
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now_ts());
        let runner = inner.runners.get(client_id)?;
        if !runner_visible_to_access(auth, runner) {
            return None;
        }
        Self::runner_semantic_view_locked(&inner, client_id)
    }

    pub async fn get_runner_semantic_view_checked_for_auth(
        &self,
        client_id: &str,
        auth: Option<&crate::RunnerAccess>,
    ) -> Result<RunnerSemanticView, String> {
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now_ts());
        let runner = inner
            .runners
            .get(client_id)
            .ok_or_else(|| format!("unknown shell client: {client_id}"))?;
        assert_runner_access(auth, runner)?;
        Self::runner_semantic_view_locked(&inner, client_id)
            .ok_or_else(|| format!("unknown shell client: {client_id}"))
    }

    pub async fn coding_agent_run_for_runner_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
        client_id: &str,
        run_id: &str,
    ) -> Option<(RunnerView, CodingAgentRunSnapshot)> {
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now_ts());
        let runner = inner.runners.get(client_id)?;
        if !runner_visible_to_access(auth, runner) {
            return None;
        }
        let run = runner
            .coding_agent_inventory
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .cloned()?;
        let view = Self::runner_view_locked(&inner, client_id)?;
        Some((view, run))
    }

    pub async fn coding_agent_run_for_auth(
        &self,
        auth: Option<&crate::RunnerAccess>,
        run_id: &str,
    ) -> Option<(RunnerView, CodingAgentRunSnapshot)> {
        let now = now_ts();
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now);
        let mut ids = inner.runners.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        let mut matched = None;
        for client_id in ids {
            let Some(runner) = inner.runners.get(&client_id) else {
                continue;
            };
            if !runner_visible_to_access(auth, runner) {
                continue;
            }
            let Some(run) = runner
                .coding_agent_inventory
                .runs
                .iter()
                .find(|run| run.run_id == run_id)
                .cloned()
            else {
                continue;
            };
            if matched.is_some() {
                // A Server restart has no process-local binding to disambiguate
                // duplicate run ids. Fail closed instead of choosing a Runner by
                // registry iteration order and silently retargeting provenance.
                return None;
            }
            let view = Self::runner_view_locked(&inner, &client_id)?;
            matched = Some((view, run));
        }
        matched
    }

    pub async fn assert_runner_access(
        &self,
        auth: Option<&crate::RunnerAccess>,
        client_id: &str,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        self.prune_expired_shared_key_runners_locked(&mut inner, now_ts());
        let runner = inner
            .runners
            .get(client_id)
            .ok_or_else(|| format!("unknown shell client: {}", client_id))?;
        assert_runner_access(auth, runner)
    }

    fn bounded_runner_semantic_views_for_auth_locked(
        inner: &RunnerRegistryInner,
        auth: Option<&crate::RunnerAccess>,
        max_runners: usize,
        max_projects: usize,
    ) -> Option<Vec<RunnerSemanticView>> {
        // Check the backing registry before allocating/sorting identifiers. This
        // is conservative for non-admin callers, but lifecycle tools are
        // administrator-only and a conservative incomplete result is fail-closed.
        if inner.runners.len() > max_runners {
            return None;
        }
        let mut ids = Vec::with_capacity(inner.runners.len());
        let mut project_count = 0usize;
        for (id, runner) in &inner.runners {
            if !runner_visible_to_access(auth, runner) {
                continue;
            }
            project_count = project_count.checked_add(runner.projects.len())?;
            if project_count > max_projects {
                return None;
            }
            ids.push(id.clone());
        }
        ids.sort();
        ids.into_iter()
            .map(|id| Self::runner_semantic_view_locked(inner, &id))
            .collect()
    }

    fn runner_semantic_view_locked(
        inner: &RunnerRegistryInner,
        client_id: &str,
    ) -> Option<RunnerSemanticView> {
        let runner_features = inner.runners.get(client_id)?.runner_features.clone();
        let view = Self::runner_view_locked(inner, client_id)?;
        Some(RunnerSemanticView {
            view,
            runner_features,
        })
    }

    fn runner_view_locked(inner: &RunnerRegistryInner, client_id: &str) -> Option<RunnerView> {
        let runner = inner.runners.get(client_id)?;
        let pending_requests = inner
            .queues_by_runner
            .get(client_id)
            .map(VecDeque::len)
            .unwrap_or(0);
        let age = now_ts().saturating_sub(runner.last_seen);
        let connected = age <= RUNNER_ONLINE_WINDOW_SECS;
        Some(RunnerView {
            client_id: runner.client_id.clone(),
            runner_instance_id: runner.runner_instance_id.clone(),
            display_name: runner.display_name.clone(),
            owner: runner.owner.clone(),
            hostname: runner.hostname.clone(),
            host_context: runner.host_context.clone(),
            status: if connected { "online" } else { "stale" }.to_string(),
            connected,
            last_seen: runner.last_seen,
            capabilities: runner.runner_features.wire_capabilities().clone(),
            coding_agent_providers: (!runner.coding_agent_providers.is_empty())
                .then(|| runner.coding_agent_providers.clone()),
            pending_requests,
            projects: runner.projects.clone(),
            project_inventory: Some(runner.project_inventory.status.clone()),
            runner_protocol_generation: runner.accepted_protocol.generation(),
            transport: runner.transport.as_str().to_string(),
            policy: runner.policy.clone(),
            registered_at: runner.registered_at,
            connected_at: runner.connected_at,
            disconnected_at: runner.disconnected_at,
            process_started_at: runner.process_started_at,
            build: runner.build.clone(),
            job_concurrency_limit: runner.job_concurrency_limit,
        })
    }
}
