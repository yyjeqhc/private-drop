use super::{RunnerFeature, RunnerFeatureSet, RunnerTransport};
use crate::protocol::AcceptedRunnerProtocol;
use crate::RunnerAccessGroup;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::{oneshot, watch, Notify};
use webcodex_core::coding_agent::{
    CodingAgentProvider, CodingAgentResponse, CodingAgentRunInventory,
};
use webcodex_core::mcp_gateway::McpGatewayResponse;
use webcodex_core::plugin::PluginGatewayResponse;
pub(super) use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle as JobLifecycleState;
use webcodex_core::runner_operation::RunnerOperation;
use webcodex_core::runner_protocol::{
    PersistentShellResult, RunnerBuildInfo, RunnerHostContext, RunnerPolicySummary,
    RunnerProjectSummary, RunnerRequest, RunnerView, ShellCommandExecutionState, ShellJobActivity,
    ShellJobCodexMetadata, ShellJobStructuredExecutionMetadata, ShellJobValidationProgress,
    ShellProcessArgv, ShellProjectInventoryStatus, ShellRunResponse,
    JOB_INVENTORY_MAX_TERMINAL_JOBS, JOB_TERMINAL_RETENTION_SECS,
};

#[derive(Debug, Clone)]
pub(super) struct ProjectInventoryStaging {
    pub(super) generation: String,
    pub(super) snapshot_sequence: u64,
    pub(super) total_reported: usize,
    pub(super) next_page_index: u32,
    pub(super) projects: Vec<RunnerProjectSummary>,
    pub(super) seen_ids: HashSet<String>,
    pub(super) serialized_bytes: usize,
    pub(super) started_at: i64,
}

#[derive(Debug, Clone)]
pub(super) struct ProjectInventoryState {
    pub(super) status: ShellProjectInventoryStatus,
    pub(super) staging: Option<ProjectInventoryStaging>,
    pub(super) retired_generations: VecDeque<String>,
    /// Monotonic freshness fence for the active Runner process. Reset only
    /// when `agent_instance_id` changes.
    pub(super) highest_snapshot_sequence: u64,
    pub(super) last_page_generation: Option<String>,
    pub(super) last_page_index: Option<u32>,
    pub(super) last_page_digest: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct RunnerRecord {
    pub(super) client_id: String,
    /// Active Runner process identity (UUID). Replacing this value is the lease
    /// hand-off: once changed, the previous instance can no longer poll or
    /// submit results/job_updates.
    pub(super) runner_instance_id: String,
    pub(super) display_name: Option<String>,
    pub(super) owner: Option<String>,
    pub(super) hostname: Option<String>,
    /// Bounded Runner-configured planning metadata. This is not policy or live
    /// state and is replaced by each successful registration.
    pub(super) host_context: Option<RunnerHostContext>,
    /// Canonical Server capability truth normalized once from the required
    /// registration snapshot. It also owns the public wire projection, so the
    /// record has no second independently stored capability copy.
    pub(super) runner_features: RunnerFeatureSet,
    pub(super) projects: Vec<RunnerProjectSummary>,
    /// Authoritative project snapshot plus bounded in-progress staging. A
    /// staging failure never changes liveness or partially publishes projects.
    pub(super) project_inventory: ProjectInventoryState,
    pub(super) last_seen: i64,
    /// Supported protocol generation accepted once at registration ingress.
    pub(super) accepted_protocol: AcceptedRunnerProtocol,
    /// Authoritative transport from the concrete ingress path. External
    /// projections serialize this typed state as `polling`, `websocket`, or `quic`.
    pub(super) transport: RunnerTransport,
    /// Sanitized Runner policy summary reported at registration. `None` for
    /// older Runners that did not report a policy. Exposed in
    /// `runtime_status` / `list_runners`; never carries token/env/init_script.
    pub(super) policy: Option<RunnerPolicySummary>,
    /// Lightweight quick-start isolation group captured at registration. This
    /// is intentionally not exposed in `RunnerView`.
    pub(super) auth_group: Option<RunnerAccessGroup>,
    /// When the current Runner instance first registered under this client_id.
    /// Preserved across same-instance re-registrations (transport reconnects).
    pub(super) registered_at: i64,
    /// When the current transport connection was established (latest register
    /// for this instance).
    pub(super) connected_at: i64,
    /// Server-generated lease for one concrete WebSocket/QUIC connection.
    /// Polling registrations use `None`. This is internal and prevents a late
    /// disconnect from an older same-instance transport from tearing down the
    /// newer connection.
    pub(super) connection_id: Option<String>,
    /// When the server observed the last transport disconnect for the current
    /// instance. Cleared on re-register.
    pub(super) disconnected_at: Option<i64>,
    /// Runner-reported process start timestamp (register payload).
    pub(super) process_started_at: Option<i64>,
    /// Runner-reported build identity (register payload).
    pub(super) build: Option<RunnerBuildInfo>,
    /// Runner-reported effective static Job execution concurrency. This is
    /// safe operational metadata and remains unknown for older Runners.
    pub(super) job_concurrency_limit: Option<usize>,
    /// Sanitized startup-owned ACP providers for this exact Runner process.
    pub(super) coding_agent_providers: Vec<CodingAgentProvider>,
    /// Authoritative active/recent-terminal CodingAgentRun inventory from this
    /// Runner. Bodies/events are deliberately absent from this durable projection.
    pub(super) coding_agent_inventory: CodingAgentRunInventory,
    /// Same-Server evidence that a hidden structured terminal Job was already
    /// projected into its initiating tool result and deliberately discarded.
    /// This stays process-local and is preserved only across registrations by
    /// the same runner instance.
    pub(super) projected_structured_terminal_suppressions:
        VecDeque<ProjectedStructuredTerminalSuppression>,
}

/// Internal-only atomic observation of one active Runner record.
///
/// `view` preserves the existing compatibility/diagnostic projection while
/// feature decisions use the canonical set cloned from the same registry lock.
/// This type is never serialized or exposed through the wire protocol.
#[derive(Debug, Clone)]
pub struct RunnerSemanticView {
    pub view: RunnerView,
    pub(super) runner_features: RunnerFeatureSet,
}

impl RunnerSemanticView {
    pub fn supports(&self, feature: RunnerFeature) -> bool {
        self.runner_features.supports(feature)
    }

    #[cfg(any(test, feature = "root-test-support"))]
    pub fn from_public_view_for_test(view: RunnerView) -> Self {
        let runner_features = RunnerFeatureSet::from_wire_for_test(&view.capabilities);
        Self {
            view,
            runner_features,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectedStructuredTerminalSuppression {
    pub(super) client_id: String,
    pub(super) runner_instance_id: String,
    pub(super) job_id: String,
    pub(super) request_id: String,
    pub(super) expires_at: i64,
}

impl RunnerRecord {
    pub(super) fn prune_projected_structured_terminal_suppressions(&mut self, now: i64) {
        self.projected_structured_terminal_suppressions
            .retain(|suppression| suppression.expires_at > now);
    }

    pub(super) fn remember_projected_structured_terminal(
        &mut self,
        job_id: String,
        request_id: String,
        now: i64,
    ) {
        self.prune_projected_structured_terminal_suppressions(now);
        self.projected_structured_terminal_suppressions
            .retain(|suppression| {
                suppression.job_id != job_id || suppression.request_id != request_id
            });
        self.projected_structured_terminal_suppressions.push_back(
            ProjectedStructuredTerminalSuppression {
                client_id: self.client_id.clone(),
                runner_instance_id: self.runner_instance_id.clone(),
                job_id,
                request_id,
                expires_at: now.saturating_add(JOB_TERMINAL_RETENTION_SECS),
            },
        );
        while self.projected_structured_terminal_suppressions.len()
            > JOB_INVENTORY_MAX_TERMINAL_JOBS
        {
            self.projected_structured_terminal_suppressions.pop_front();
        }
    }

    pub(super) fn suppresses_projected_structured_terminal(
        &self,
        client_id: &str,
        runner_instance_id: &str,
        job_id: &str,
        request_id: &str,
        now: i64,
    ) -> bool {
        self.projected_structured_terminal_suppressions
            .iter()
            .any(|suppression| {
                suppression.expires_at > now
                    && suppression.client_id == client_id
                    && suppression.runner_instance_id == runner_instance_id
                    && suppression.job_id == job_id
                    && suppression.request_id == request_id
            })
    }
}

#[derive(Debug, Clone)]
pub(super) struct SkillStoreDispatchFence {
    pub(super) runner_instance_id: String,
    pub(super) management: bool,
}

#[derive(Debug)]
pub(super) struct PendingShellRequest {
    pub(super) request: RunnerRequest,
    /// Canonical semantic operation decoded exactly once when the V2 wire DTO is admitted.
    /// Registry fencing/result correlation must use this field rather than reinterpreting
    /// `request.kind + optional payloads`.
    pub(super) operation: RunnerOperation,
    pub(super) waiter: Option<oneshot::Sender<ShellRunResponse>>,
    pub(super) job_id: Option<String>,
    /// Optional Control-side project-placement fence for synchronous requests
    /// whose filesystem authority must still match the active registration at
    /// the instant the request is handed to the Runner.
    pub(super) expected_runner_owner: Option<String>,
    pub(super) expected_project_id: Option<String>,
    pub(super) expected_project_cwd: Option<String>,
    /// Exact Runner process lease captured for an MCP gateway request. This is
    /// revalidated under the registry lock immediately before dequeue so a
    /// replacement Runner cannot consume stale bridge work.
    pub(super) expected_mcp_gateway_runner_instance_id: Option<String>,
    /// Exact provider lease captured with the Runner lease. Both logical id
    /// and opaque provider instance must still match registration immediately
    /// before dequeue.
    pub(super) expected_mcp_gateway_provider_id: Option<String>,
    pub(super) expected_mcp_gateway_provider_instance_id: Option<String>,
    /// Exact Runner process lease captured for Runner-local managed SSH resource
    /// management. Revalidated at dequeue so a replacement Runner can never
    /// inherit a host-configuration mutation.
    pub(super) expected_ssh_resource_runner_instance_id: Option<String>,
    /// Exact Runner process lease captured for first-class config check/reload.
    /// Revalidated at dequeue so neither check nor reload can silently retarget
    /// a replacement process using the same client_id.
    pub(super) expected_runner_config_runner_instance_id: Option<String>,
    /// Exact Runner process lease plus read/manage mode captured for a
    /// Runner-global Skill store request. Revalidated at dequeue so a
    /// replacement process using the same client_id cannot inherit authority.
    pub(super) skill_store_fence: Option<SkillStoreDispatchFence>,
    pub(super) dispatched: bool,
}

#[derive(Debug, Clone)]
pub(super) struct CodingAgentDispatchFence {
    pub(super) runner_instance_id: String,
    pub(super) provider_id: String,
    pub(super) provider_instance_id: String,
}

#[derive(Debug, Clone)]
pub(super) struct PluginGatewayDispatchFence {
    pub(super) runner_instance_id: String,
    pub(super) provider: Option<(String, String)>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShellJobVisibility {
    #[default]
    Public,
    HiddenUntilHandoff,
    CleanupPending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JobRecoveryPhase {
    Recovering,
    Reconciled,
    LostAfterReconcile,
}

impl JobRecoveryPhase {
    pub(super) const fn as_wire(self) -> &'static str {
        match self {
            Self::Recovering => "recovering",
            Self::Reconciled => "reconciled",
            Self::LostAfterReconcile => "lost_after_reconcile",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JobRecoveryReason {
    RunnerTransportDisconnected,
    RunnerTransportStale,
    RunnerRequestNotDispatched,
    RunnerDisconnectedWithoutReconciliation,
    SharedKeyRunnerExpired,
    RunnerInventoryMissing,
    RunnerInstanceReplaced,
    RunnerRecoveryDeadlineExceeded,
    ServerRestartReconciliation,
    SameInstanceReconciliation,
    SameInstanceUpdateReconciliation,
    DetachedInstanceTransfer,
}

impl JobRecoveryReason {
    pub(super) const fn as_wire(self) -> &'static str {
        match self {
            Self::RunnerTransportDisconnected => "runner_transport_disconnected",
            Self::RunnerTransportStale => "runner_transport_stale",
            Self::RunnerRequestNotDispatched => "runner_request_not_dispatched",
            Self::RunnerDisconnectedWithoutReconciliation => {
                "runner_disconnected_without_reconciliation"
            }
            Self::SharedKeyRunnerExpired => "shared_key_runner_expired",
            Self::RunnerInventoryMissing => "runner_inventory_missing",
            Self::RunnerInstanceReplaced => "runner_instance_replaced",
            Self::RunnerRecoveryDeadlineExceeded => "runner_recovery_deadline_exceeded",
            Self::ServerRestartReconciliation => "server_restart_reconciliation",
            Self::SameInstanceReconciliation => "same_instance_reconciliation",
            Self::SameInstanceUpdateReconciliation => "same_instance_update_reconciliation",
            Self::DetachedInstanceTransfer => "detached_instance_transfer",
        }
    }

    pub(super) const fn implies_lost_after_reconcile(self) -> bool {
        matches!(
            self,
            Self::RunnerInventoryMissing
                | Self::RunnerInstanceReplaced
                | Self::RunnerRecoveryDeadlineExceeded
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct JobRecoveryState {
    pub(super) phase: Option<JobRecoveryPhase>,
    pub(super) recovered_after_server_restart: bool,
    pub(super) reconciled_at: Option<i64>,
    pub(super) reason: Option<JobRecoveryReason>,
    pub(super) recovering_since: Option<i64>,
}

impl JobRecoveryState {
    pub(super) fn recovering(&self) -> bool {
        self.phase == Some(JobRecoveryPhase::Recovering)
    }

    pub(super) fn public_state(&self) -> Option<&'static str> {
        self.phase.map(JobRecoveryPhase::as_wire)
    }

    pub(super) fn public_reason(&self) -> Option<&'static str> {
        self.reason.map(JobRecoveryReason::as_wire)
    }
}

#[derive(Debug, Clone)]
pub(super) struct JobObservationState {
    pub(super) epoch: Arc<str>,
    pub(super) revision: Arc<AtomicU64>,
    pub(super) notify: Arc<Notify>,
    /// First time this Server process observed the Job in a terminal execution
    /// lifecycle. Runner-reported `ended_at` remains the public execution time
    /// and never controls Server registry retention.
    pub(super) terminal_observed_at: Option<i64>,
}

impl JobObservationState {
    pub(super) fn new(epoch: Arc<str>) -> Self {
        Self {
            epoch,
            revision: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
            terminal_observed_at: None,
        }
    }
}

/// Server-process-local exact intent retained only to prove same-key detached
/// replays. It is never serialized into Runner protocol, durable state, audit,
/// or Session evidence; restart reconstruction deliberately restores `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DetachedIdempotencyIntent {
    pub(super) project_id: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) project_cwd: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) purpose: Option<String>,
    pub(super) shell: Option<String>,
    pub(super) process: ShellProcessArgv,
    pub(super) stdin: Option<String>,
    pub(super) timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub(super) struct ShellJobRecord {
    pub(super) job_id: String,
    pub(super) request_id: Option<String>,
    pub(super) client_id: String,
    /// Non-secret authorization partition captured when the Job is created.
    /// Shared-key runners store only the existing key hash group, never the
    /// plaintext key. Keeping this on the Job preserves authorization after
    /// the originating runner registration is removed.
    pub(super) auth_group: Option<RunnerAccessGroup>,
    /// Internal lease owner. Never exposed through public job tools.
    pub(super) runner_instance_id: String,
    pub(super) kind: String,
    pub(super) project_id: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) ssh_resource: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) project_cwd: Option<String>,
    pub(super) purpose: Option<String>,
    pub(super) shell: Option<String>,
    pub(super) command_preview: String,
    /// Exact detached replay intent is process-local only. Reconstructed Jobs
    /// keep this `None`, forcing exact logical-Job recovery instead of guessing
    /// that a resent body matches after Server restart.
    pub(super) detached_idempotency_intent: Option<DetachedIdempotencyIntent>,
    pub(super) lifecycle: JobLifecycleState,
    pub(super) created_at: i64,
    pub(super) started_at: Option<i64>,
    pub(super) ended_at: Option<i64>,
    pub(super) exit_code: Option<i32>,
    pub(super) duration_ms: Option<u64>,
    pub(super) stdout: ShellJobLogState,
    pub(super) stderr: ShellJobLogState,
    pub(super) error: Option<String>,
    pub(super) command_execution_state: Option<ShellCommandExecutionState>,
    pub(super) structured_execution: Option<ShellJobStructuredExecutionMetadata>,
    pub(super) codex: Option<ShellJobCodexMetadata>,
    pub(super) validation_steps: Vec<String>,
    pub(super) validation: Option<webcodex_core::runner_protocol::ShellJobValidationMetadata>,
    pub(super) validation_progress: Option<ShellJobValidationProgress>,
    /// Last Runner-authoritative bounded activity for an active Job. Cleared on
    /// terminal/recovery transitions; never used as execution authority.
    pub(super) activity: Option<ShellJobActivity>,
    pub(super) visibility: ShellJobVisibility,
    pub(super) last_update_seq: u64,
    pub(super) recovery: JobRecoveryState,
    /// Process-local observation ownership. It can wake readers but never grants
    /// execution authority or mutates lifecycle state.
    pub(super) observation: JobObservationState,
}

impl ShellJobRecord {
    /// Recovery is an orthogonal provenance/availability dimension, but it only
    /// controls active-Job behavior while the underlying execution lifecycle is
    /// still Runner-owned. A terminal lifecycle remains authoritative even when
    /// compatibility recovery metadata from the preceding disconnect is retained.
    pub(super) fn recovery_active(&self) -> bool {
        self.lifecycle.is_runner_active() && self.recovery.recovering()
    }

    pub(super) fn public_status(&self) -> &'static str {
        if self.recovery_active() {
            "recovering"
        } else {
            self.lifecycle.as_wire()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellJobLogState {
    pub(super) tail: String,
    pub(super) first_retained_line: usize,
    pub(super) next_line: usize,
    pub(super) truncated: bool,
}

impl Default for ShellJobLogState {
    fn default() -> Self {
        Self {
            tail: String::new(),
            first_retained_line: 1,
            next_line: 1,
            truncated: false,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct RunnerRegistryInner {
    pub(super) runners: HashMap<String, RunnerRecord>,
    pub(super) pending_by_id: HashMap<String, PendingShellRequest>,
    /// Waiters for explicit persistent-shell lifecycle results. Kept separate
    /// from synchronous `ShellRunResponse` waiters so PersistentShell never
    /// enters the Job/run_shell model.
    pub(super) persistent_waiters: HashMap<String, oneshot::Sender<PersistentShellResult>>,
    /// Waiters for the closed MCP gateway response contract. They remain
    /// separate from shell stdout/stderr so bridge calls cannot become a raw
    /// result tunnel.
    pub(super) mcp_gateway_waiters: HashMap<String, oneshot::Sender<McpGatewayResponse>>,
    /// Independent typed native Plugin gateway result channels. Provider
    /// identities stay internal and are fenced separately from MCP/ACP.
    pub(super) plugin_gateway_waiters: HashMap<String, oneshot::Sender<PluginGatewayResponse>>,
    pub(super) plugin_gateway_fences: HashMap<String, PluginGatewayDispatchFence>,
    /// Waiters and exact process/provider dispatch fences for CodingAgentRun
    /// operations. They are independent from shell/Job/MCP result channels.
    pub(super) coding_agent_waiters: HashMap<String, oneshot::Sender<CodingAgentResponse>>,
    pub(super) coding_agent_fences: HashMap<String, CodingAgentDispatchFence>,
    pub(super) queues_by_runner: HashMap<String, VecDeque<String>>,
    pub(super) jobs_by_id: HashMap<String, ShellJobRecord>,
    pub(super) request_to_job: HashMap<String, String>,
    /// Bounded stale-instance tombstones prevent a replaced runner process
    /// from reclaiming the same runner lease after the replacement later
    /// becomes stale.
    pub(super) retired_instances: HashMap<String, VecDeque<String>>,
    /// Runtime project ids temporarily fenced while unregister validates and
    /// removes the Runner registry entry. Job enqueue checks this set while
    /// holding the same registry mutex, closing the check/start TOCTOU window.
    pub(super) unregistering_projects: HashMap<String, usize>,
    /// Optional push notifiers for Runners connected over a long-lived
    /// transport (WebSocket/QUIC). When a request is enqueued for a runner that
    /// has a registered notifier, the server pumps the request immediately
    /// instead of waiting for the Runner to poll. Polling Runners never
    /// register a notifier and are unaffected.
    ///
    /// The stored instance and connection ids record which concrete transport
    /// owns the notifier. Disconnect cleanup is applied only when both leases
    /// still match, so neither a replaced process nor an older same-process
    /// socket can tear down the current notifier and jobs.
    pub(super) notifiers: HashMap<String, NotifierEntry>,
}

/// A registered push notifier plus the exact streaming connection lifecycle
/// that installed it. `cancel` is process-local and is signalled only after a
/// successful authoritative replacement has committed.
#[derive(Debug, Clone)]
pub(super) struct NotifierEntry {
    pub(super) notify: Arc<Notify>,
    pub(super) cancel: watch::Sender<bool>,
    pub(super) runner_instance_id: String,
    pub(super) connection_id: Option<String>,
}
