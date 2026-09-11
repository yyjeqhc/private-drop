use super::activity::{ActivityRecorder, NoopActivityRecorder};
#[cfg(feature = "workspace-checkpoints")]
use super::checkpoint;
use super::observations::RuntimeObservations;
use super::permissions::PermissionEvaluator;
use super::runtime_info::RuntimeInfo;
use super::sessions;
use super::SessionShellRegistry;
use crate::runner_http::RunnerRegistry;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
#[cfg(test)]
use tokio::sync::Semaphore;
use uuid::Uuid;

fn new_git_diff_hunks_continuation_mac_key() -> Arc<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex.git-diff-hunks.runtime-mac-key.v1\0");
    hasher.update(Uuid::new_v4().as_bytes());
    hasher.update(Uuid::new_v4().as_bytes());
    Arc::new(hasher.finalize().into())
}

#[cfg(test)]
pub(crate) struct ValidationTerminalReconciliationTestHook {
    reconciliation_attempted: Semaphore,
    pause_next_after_snapshot: AtomicBool,
    snapshot_acquired: Semaphore,
    resume_snapshot: Semaphore,
    snapshot_acquisition_count: AtomicUsize,
}

#[cfg(test)]
impl Default for ValidationTerminalReconciliationTestHook {
    fn default() -> Self {
        Self {
            pause_next_after_snapshot: AtomicBool::new(false),
            reconciliation_attempted: Semaphore::new(0),
            snapshot_acquired: Semaphore::new(0),
            resume_snapshot: Semaphore::new(0),
            snapshot_acquisition_count: AtomicUsize::new(0),
        }
    }
}

#[cfg(test)]
impl ValidationTerminalReconciliationTestHook {
    pub(crate) fn before_reconciliation_lock(&self) {
        self.reconciliation_attempted.add_permits(1);
    }

    pub(crate) async fn wait_for_reconciliation_attempt(&self) {
        let permit = self
            .reconciliation_attempted
            .acquire()
            .await
            .expect("validation terminal reconciliation attempt semaphore closed");
        permit.forget();
    }

    pub(crate) fn pause_next_snapshot(&self) {
        assert!(
            !self.pause_next_after_snapshot.swap(true, Ordering::SeqCst),
            "validation terminal snapshot pause already armed"
        );
    }

    pub(crate) async fn after_snapshot_acquired(&self) {
        self.snapshot_acquisition_count
            .fetch_add(1, Ordering::SeqCst);
        self.snapshot_acquired.add_permits(1);
        if self.pause_next_after_snapshot.swap(false, Ordering::SeqCst) {
            let permit = self
                .resume_snapshot
                .acquire()
                .await
                .expect("validation terminal snapshot resume semaphore closed");
            permit.forget();
        }
    }

    pub(crate) async fn wait_for_snapshot_acquired(&self) {
        let permit = self
            .snapshot_acquired
            .acquire()
            .await
            .expect("validation terminal snapshot semaphore closed");
        permit.forget();
    }

    pub(crate) fn resume_snapshot(&self) {
        self.resume_snapshot.add_permits(1);
    }

    pub(crate) fn snapshot_acquisition_count(&self) -> usize {
        self.snapshot_acquisition_count.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub struct ToolRuntime {
    pub runner_registry: Arc<RunnerRegistry>,
    pub(crate) mcp_gateway: Arc<crate::mcp_gateway::McpGatewayRuntime>,
    pub(crate) plugin_gateway: Arc<crate::plugin_gateway::PluginGatewayRuntime>,
    pub(crate) ssh_resource_gateway: Arc<crate::ssh_resource_gateway::SshResourceGatewayRuntime>,
    pub(crate) coding_agent_runs: Arc<super::coding_agent::CodingAgentServerState>,
    pub runtime_info: Arc<RuntimeInfo>,
    runtime_exposure: crate::model_surface::RuntimeExposure,
    #[cfg(feature = "workspace-checkpoints")]
    pub(crate) checkpoint_store: checkpoint::CheckpointStore,
    pub(crate) sessions: sessions::SessionStore,
    pub(crate) session_shells: SessionShellRegistry,
    pub(crate) semantic_navigation_probe_timeout: Duration,
    pub(crate) repository_overview_probe_timeout: Duration,
    /// One deadline shared by every item in a `read_files` batch.
    pub(crate) read_files_deadline: Duration,
    /// One deadline shared by every query in a `search_project_texts` batch.
    pub(crate) search_project_texts_deadline: Duration,
    /// Internal synchronous wait window for a read-only structured validation
    /// before it promotes to a Job. Defaults to `SYNC_VALIDATION_WAIT_SECS`;
    /// tests shrink it so the handoff path can be exercised without sleeping.
    pub(crate) validation_sync_wait: Duration,
    /// Orders authoritative terminal-Job snapshot acquisition through Session
    /// marker/evidence materialization. Marker eviction interprets absence from
    /// that snapshot as retention exit, so a later snapshot must never commit
    /// before an earlier snapshot has finished using its eviction authority.
    /// Cloned runtimes share this mutex; restart drops all in-flight snapshots.
    pub(crate) validation_terminal_reconciliation: Arc<Mutex<()>>,
    #[cfg(test)]
    pub(crate) validation_terminal_reconciliation_test_hook:
        Arc<ValidationTerminalReconciliationTestHook>,
    /// Runtime cap for the model-requested synchronous grace before typed
    /// process/script Job handoff. Production permits the public maximum;
    /// each call's StructuredExecutionBudget selects the actual wait. Tests
    /// shrink this cap to exercise handoff without sleeping.
    pub(crate) structured_execution_sync_wait: Duration,
    /// Per-runtime secret used only to authenticate opaque committed-range
    /// git_diff_hunks continuation state. Clones share the same key; a runtime
    /// restart intentionally invalidates old committed continuations fail-closed.
    pub(crate) git_diff_hunks_continuation_mac_key: Arc<[u8; 32]>,
    /// Authoritative permission evaluator for this runtime instance.
    /// Resolved once at construction (`WEBCODEX_AUTHORITY_MODE`); dispatch
    /// evaluates once per tool request before mutation.
    pub(crate) permission_evaluator: PermissionEvaluator,
    /// Sink for the workspace activity ledger (mutating tool executions).
    /// No-op unless the host injects a durable recorder.
    pub(crate) activity: Arc<dyn ActivityRecorder>,
    /// Cross-surface connection observations (connector endpoint activity,
    /// last successful meaningful tool call). Shared with the connector
    /// runtime; never stores payloads or secrets.
    pub(crate) observations: Arc<RuntimeObservations>,
    /// Process-local payload-free view of currently in-flight MCP Window
    /// requests. It is observability only and intentionally resets on restart.
    pub(crate) window_activity: Arc<super::window_activity::WindowActivityRegistry>,
    /// Fail-open metrics projection. This consumes canonical runtime/transport
    /// facts and has no authority over execution or persistence.
    pub(crate) metrics: Arc<dyn super::runtime_metrics::RuntimeMetrics>,
    /// Durable ActionAudit-backed Window activity query handle. This shares the
    /// normal Server SQLite database and never becomes an authorization store.
    pub(crate) window_activity_db: Option<Arc<crate::Database>>,
    /// Optional Control-owned durable project Memory store. It is injected by
    /// the server from the existing webcodex.db handle; Runner-native project
    /// filesystems never own Memory v1 persistence.
    pub(crate) memory_db: Option<Arc<crate::Database>>,
    /// Optional Control-owned durable Agent and Conversation store. It shares
    /// the Server SQLite handle with other durable domains but owns independent tables.
    pub(crate) communication_db: Option<Arc<crate::Database>>,
    /// Optional process-local Host continuation registry/controller. It is
    /// created only when the durable communication database is injected and is
    /// intentionally empty again after process restart.
    pub(crate) agent_continuations: Option<crate::agent_wake::AgentContinuationController>,
}

impl ToolRuntime {
    pub fn new(runner_registry: Arc<RunnerRegistry>, runtime_info: Arc<RuntimeInfo>) -> Self {
        Self {
            runner_registry,
            mcp_gateway: Arc::new(crate::mcp_gateway::McpGatewayRuntime::default()),
            plugin_gateway: Arc::new(crate::plugin_gateway::PluginGatewayRuntime::default()),
            ssh_resource_gateway: Arc::new(
                crate::ssh_resource_gateway::SshResourceGatewayRuntime::default(),
            ),
            coding_agent_runs: Arc::new(super::coding_agent::CodingAgentServerState::default()),
            runtime_info,
            runtime_exposure: crate::model_surface::RuntimeExposure::Runtime(
                crate::model_surface::ModelSurface::LocalCoding,
            ),
            #[cfg(feature = "workspace-checkpoints")]
            checkpoint_store: checkpoint::CheckpointStore::default(),
            sessions: sessions::SessionStore::default(),
            session_shells: SessionShellRegistry::default(),
            semantic_navigation_probe_timeout:
                super::semantic_navigation::DEFAULT_SEMANTIC_NAVIGATION_PROBE_TIMEOUT,
            repository_overview_probe_timeout:
                super::coding_task::DEFAULT_REPOSITORY_OVERVIEW_PROBE_TIMEOUT,
            read_files_deadline: super::read_files::DEFAULT_READ_FILES_DEADLINE,
            search_project_texts_deadline:
                super::search_project_texts::DEFAULT_SEARCH_PROJECT_TEXTS_DEADLINE,
            validation_sync_wait: Duration::from_secs(super::helpers::SYNC_VALIDATION_WAIT_SECS),
            validation_terminal_reconciliation: Arc::new(Mutex::new(())),
            #[cfg(test)]
            validation_terminal_reconciliation_test_hook: Arc::new(
                ValidationTerminalReconciliationTestHook::default(),
            ),
            structured_execution_sync_wait: Duration::from_secs(
                super::structured_execution::STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS,
            ),
            git_diff_hunks_continuation_mac_key: new_git_diff_hunks_continuation_mac_key(),
            permission_evaluator: PermissionEvaluator::from_env(),
            activity: Arc::new(NoopActivityRecorder),
            observations: Arc::new(RuntimeObservations::default()),
            window_activity: Arc::new(super::window_activity::WindowActivityRegistry::default()),
            metrics: Arc::new(super::runtime_metrics::TracingRuntimeMetrics),
            window_activity_db: None,
            memory_db: None,
            communication_db: None,
            agent_continuations: None,
        }
    }

    pub(crate) fn with_runtime_exposure(
        mut self,
        runtime_exposure: crate::model_surface::RuntimeExposure,
    ) -> Self {
        self.runtime_exposure = runtime_exposure;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_model_surface(
        self,
        model_surface: crate::model_surface::ModelSurface,
    ) -> Self {
        self.with_runtime_exposure(crate::model_surface::RuntimeExposure::Runtime(
            model_surface,
        ))
    }

    pub(crate) fn runtime_exposure(&self) -> crate::model_surface::RuntimeExposure {
        self.runtime_exposure
    }

    pub(crate) fn model_surface(&self) -> Option<crate::model_surface::ModelSurface> {
        self.runtime_exposure.model_surface()
    }

    /// Attach a durable workspace-activity recorder (server wiring).
    pub fn with_activity_recorder(mut self, recorder: Arc<dyn ActivityRecorder>) -> Self {
        self.activity = recorder;
        self
    }

    pub(crate) fn with_window_activity_database(mut self, db: Arc<crate::Database>) -> Self {
        self.window_activity_db = Some(db);
        self
    }

    pub(crate) fn window_activity_registry(
        &self,
    ) -> Arc<super::window_activity::WindowActivityRegistry> {
        self.window_activity.clone()
    }

    pub(crate) fn with_memory_database(mut self, db: Arc<crate::Database>) -> Self {
        self.memory_db = Some(db);
        self
    }

    pub(crate) fn with_communication_database(mut self, db: Arc<crate::Database>) -> Self {
        self.agent_continuations = Some(crate::agent_wake::AgentContinuationController::new(
            db.clone(),
        ));
        self.communication_db = Some(db);
        self
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests() -> Self {
        Self::new_for_tests_with_runner_registry(Arc::new(RunnerRegistry::default()))
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests_with_runner_registry(runner_registry: Arc<RunnerRegistry>) -> Self {
        Self::new(runner_registry, Arc::new(RuntimeInfo::default()))
    }

    pub fn with_session_ledger(mut self, path: impl Into<PathBuf>) -> Self {
        self.sessions = sessions::SessionStore::with_persistence(
            path,
            sessions::DEFAULT_MAX_SESSIONS,
            sessions::DEFAULT_MAX_EVENTS_PER_SESSION,
        );
        self
    }

    pub(crate) fn workflow_sessions_console_list(
        &self,
        project: &str,
        limit: Option<usize>,
    ) -> sessions::WorkflowSessionConsoleList {
        self.sessions
            .console_list_for_project(project, limit, sessions::console_validation_hooks())
    }

    pub(crate) fn workflow_session_console_detail(
        &self,
        project: &str,
        session_id: &str,
        limit: Option<usize>,
    ) -> Option<sessions::WorkflowSessionConsoleDetail> {
        self.sessions.console_detail_for_project(
            project,
            session_id,
            limit,
            sessions::console_validation_hooks(),
        )
    }

    #[cfg(test)]
    pub(crate) fn with_semantic_navigation_probe_timeout(mut self, timeout: Duration) -> Self {
        self.semantic_navigation_probe_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_repository_overview_probe_timeout(mut self, timeout: Duration) -> Self {
        self.repository_overview_probe_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_read_files_deadline(mut self, deadline: Duration) -> Self {
        self.read_files_deadline = deadline;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_search_project_texts_deadline(mut self, deadline: Duration) -> Self {
        self.search_project_texts_deadline = deadline;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_validation_sync_wait(mut self, wait: Duration) -> Self {
        self.validation_sync_wait = wait;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_structured_execution_sync_wait(mut self, wait: Duration) -> Self {
        self.structured_execution_sync_wait = wait;
        self
    }

    /// Replace the permission evaluator (tests: mode matrix / single-eval counters).
    #[cfg(test)]
    pub(crate) fn with_permission_evaluator(mut self, evaluator: PermissionEvaluator) -> Self {
        self.permission_evaluator = evaluator;
        self
    }
}
