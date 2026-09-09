use crate::activity::{ActivityEventKind, ActivityLevel, ActivityLog};
use crate::deadline::Deadline;
use crate::error::{DesktopError, DesktopResult};
use crate::models::{
    aggregate_readiness, ChatGptActivitySnapshot, DesktopOperationKind, DesktopStateSnapshot,
    Enrollment, Experience, Exposure, ExposureReadiness, OpenAiTunnelConfigSnapshot,
    ProjectReadiness, ProjectSelection,
    QuickShareState, ReadinessNextActionKind, ReadinessSummaryKind, RegularConnectionPreference,
    RegularTunnelState, RegularTunnelStatus, RunnerReadiness, RunnerTopology, RuntimeTopology,
    ServerReadiness, ServerTopology, StoredDesktopConfig, StoredRuntime, TunnelProxyConfig,
    TunnelProxyMode, TunnelProxySnapshot,
};
use crate::operation::{
    cancelled_error, CancellationContext, CancellationSignal, OperationAdmission,
    OperationController,
};
use crate::process::{MachineEventReceiver, ProcessKind, ProcessPhase, ProcessSupervisor};
use crate::webcodex::{
    inspect_project_path, ProjectRuntimeIdentity, QuickShareReadyEvent, RegularTunnelReadyEvent,
    WebCodexAdapter,
};
use serde_json::Value;
#[cfg(unix)]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex;

const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(20);
const RUNNER_READY_TIMEOUT: Duration = Duration::from_secs(30);
const PROJECT_READY_TIMEOUT: Duration = Duration::from_secs(20);
const QUICK_SHARE_READY_TIMEOUT: Duration = Duration::from_secs(90);
const REGULAR_TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(90);
const POLL_INTERVAL: Duration = Duration::from_millis(300);
const READINESS_CLEANUP_SLACK: Duration = Duration::from_secs(2);
const SHUTDOWN_OPERATION_WAIT: Duration = Duration::from_secs(5);
const DESKTOP_STATE_MAX_BYTES: u64 = 256 * 1024;
const DESKTOP_SERVER_ENV_MAX_BYTES: u64 = 256 * 1024;
const DESKTOP_MCP_MODEL_SURFACE: &str = "adaptive-runtime-v1";
const DESKTOP_MCP_COMPACT_SCHEMAS: &str = "true";
static NEXT_STATE_TEMP_ID: AtomicU64 = AtomicU64::new(1);

type SharedSupervisor = Arc<Mutex<ProcessSupervisor>>;

#[derive(Debug, Clone)]
struct ChatGptActivityProbe {
    identity: ProjectRuntimeIdentity,
    webcodex: PathBuf,
}

pub struct AppState {
    core: Mutex<Option<DesktopCore>>,
    published: Arc<RwLock<DesktopStateSnapshot>>,
    supervisor: SharedSupervisor,
    activity: ActivityLog,
    operations: OperationController,
    shutdown_signal: CancellationSignal,
    shutdown_started: AtomicBool,
}

impl AppState {
    pub fn new(data_dir: PathBuf, resource_dir: PathBuf) -> DesktopResult<Self> {
        let core = DesktopCore::new(data_dir, resource_dir)?;
        let published = Arc::clone(&core.published);
        let supervisor = Arc::clone(&core.supervisor);
        let activity = core.activity.clone();
        Ok(Self {
            core: Mutex::new(Some(core)),
            published,
            supervisor,
            operations: OperationController::new(activity.clone()),
            activity,
            shutdown_signal: CancellationSignal::new(),
            shutdown_started: AtomicBool::new(false),
        })
    }

    pub fn get_state(&self) -> DesktopStateSnapshot {
        let mut snapshot = self
            .published
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if snapshot.regular_tunnel.is_some() {
            if let Ok(mut supervisor) = self.supervisor.try_lock() {
                let active =
                    supervisor
                        .snapshot(ProcessKind::RegularTunnel)
                        .is_some_and(|process| {
                            matches!(
                                process.phase,
                                ProcessPhase::Starting | ProcessPhase::Running
                            )
                        });
                if let Some(exposure) =
                    regular_tunnel_exposure(&mut snapshot.regular_tunnel, active)
                {
                    snapshot.readiness = aggregate_readiness(
                        snapshot.readiness.server.clone(),
                        snapshot.readiness.runner.clone(),
                        exposure.clone(),
                        snapshot.readiness.project.clone(),
                    );
                    apply_regular_tunnel_next_action(&mut snapshot, &exposure);
                }
            }
        }
        snapshot.current_operation = self.operations.current();
        snapshot.activity_sequence = self.activity.latest_sequence();
        apply_openai_tunnel_configuration(&mut snapshot);
        snapshot.regular_tunnel_available = true;
        snapshot.powershell_runtime = crate::platform::powershell_runtime_snapshot();
        snapshot
    }

    pub fn activity(&self) -> Vec<crate::activity::ActivityEntry> {
        self.activity.snapshot()
    }

    pub async fn inspect_project(&self, path: &str) -> DesktopResult<ProjectSelection> {
        inspect_project_path(path).await
    }

    pub async fn refresh_runtime_status(&self) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::RuntimeRefresh, true)
            .await?;
        let result = core.refresh_runtime_status(&cancellation).await;
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    pub async fn observe_chatgpt_activity(&self) -> DesktopResult<DesktopStateSnapshot> {
        if self.shutdown_signal.is_cancelled() {
            return Err(cancelled_error());
        }
        if self.operations.current().is_some() {
            return Ok(self.get_state());
        }
        let cancellation = CancellationContext::new(
            CancellationSignal::new(),
            self.shutdown_signal.clone(),
        );
        let probe = {
            let slot = self.core.lock().await;
            let Some(core) = slot.as_ref() else {
                return Ok(self.get_state());
            };
            let Some(probe) = core.chatgpt_activity_probe() else {
                return Ok(self.get_state());
            };
            probe
        };
        let observation = WebCodexAdapter::chatgpt_activity_with_binary(
            &probe.webcodex,
            &probe.identity,
            &cancellation,
        )
        .await;
        cancellation.check()?;
        let Ok(last_meaningful_activity_at_ms) = observation else {
            return Ok(self.get_state());
        };
        if self.operations.current().is_some() {
            return Ok(self.get_state());
        }
        let mut slot = self.core.lock().await;
        let Some(core) = slot.as_mut() else {
            return Ok(self.get_state());
        };
        core.apply_chatgpt_activity_observation(&probe.identity, last_meaningful_activity_at_ms)
    }

    pub async fn resume_saved_runtime(&self) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::RuntimeResume, true)
            .await?;
        let result = core.resume_saved_runtime(&cancellation).await;
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    pub async fn update_tunnel_proxy(
        &self,
        mode: TunnelProxyMode,
        custom_url: Option<&str>,
    ) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::TunnelProxyUpdate, false)
            .await?;
        let result = core
            .update_tunnel_proxy(mode, custom_url, &cancellation)
            .await;
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    pub async fn configure_local_setup(
        &self,
        project_path: Option<&str>,
    ) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::LocalSetup, true)
            .await?;
        let result = core
            .configure_local_setup(project_path, &cancellation)
            .await;
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    pub async fn configure_remote_setup(
        &self,
        server_url: &str,
        pairing_code: &str,
        project_path: &str,
    ) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::RemoteSetup, true)
            .await?;
        let result = core
            .configure_remote_setup(server_url, pairing_code, project_path, &cancellation)
            .await;
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    pub async fn start_quick_share(
        &self,
        project_path: &str,
        provider: &str,
    ) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::QuickShareStart, true)
            .await?;
        let result = core
            .start_quick_share(project_path, provider, &cancellation)
            .await;
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    pub async fn stop_quick_share(&self) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::QuickShareStop, false)
            .await?;
        let result = core.stop_quick_share(&cancellation).await;
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    pub async fn start_regular_tunnel(&self) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::RegularTunnelStart, true)
            .await?;
        let result = core.start_regular_tunnel(&cancellation).await;
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    pub async fn stop_regular_tunnel(&self) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::RegularTunnelStop, false)
            .await?;
        let result = core.stop_regular_tunnel(&cancellation).await;
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    pub async fn stop_local_runtime(&self) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::LocalRuntimeStop, false)
            .await?;
        let result = core.stop_local_runtime(&cancellation).await;
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    pub fn cancel_operation(&self, operation_id: &str) -> DesktopResult<DesktopStateSnapshot> {
        self.operations.cancel(operation_id)?;
        Ok(self.get_state())
    }

    pub async fn shutdown(&self) {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.shutdown_signal.cancel();
        self.operations.cancel_active_for_shutdown();
        self.supervisor.lock().await.stop_all().await;
        let _ = self
            .operations
            .wait_until_idle(tokio::time::Instant::now() + SHUTDOWN_OPERATION_WAIT)
            .await;
    }

    async fn begin_operation(
        &self,
        kind: DesktopOperationKind,
        cancellable: bool,
    ) -> DesktopResult<(
        OperationAdmission,
        CancellationContext,
        DesktopCore,
        ProcessBaseline,
    )> {
        if self.shutdown_signal.is_cancelled() {
            return Err(cancelled_error());
        }
        let operation = self.operations.admit(kind, cancellable)?;
        let cancellation =
            CancellationContext::new(operation.cancellation.clone(), self.shutdown_signal.clone());
        let baseline = self.capture_process_baseline().await;
        let core = {
            let mut slot = self.core.lock().await;
            slot.take()
        };
        let Some(core) = core else {
            let error = DesktopError::new(
                "desktop_operation_busy",
                "Desktop mutation state is already in use",
                "Wait for the current operation to finish.",
            );
            let result: DesktopResult<()> = Err(error.clone());
            self.operations.finish(&operation.id, &result);
            return Err(error);
        };
        Ok((operation, cancellation, core, baseline))
    }

    async fn finish_operation(
        &self,
        operation: OperationAdmission,
        cancellation: CancellationContext,
        mut core: DesktopCore,
        baseline: ProcessBaseline,
        mut result: DesktopResult<DesktopStateSnapshot>,
    ) -> DesktopResult<DesktopStateSnapshot> {
        if result.is_ok() && cancellation.is_cancelled() {
            result = Err(cancelled_error());
        }
        if result.is_err() {
            let cancelled = result
                .as_ref()
                .err()
                .is_some_and(|error| error.code == "desktop_operation_cancelled");
            let cleanup = self.cleanup_new_owned_processes(&baseline).await;
            core.reconcile_after_operation_failure(operation.kind, &baseline, cleanup, cancelled);
            core.publish_snapshot();
        }
        {
            let mut slot = self.core.lock().await;
            *slot = Some(core);
        }
        self.operations.finish(&operation.id, &result);
        match result {
            Ok(_) => Ok(self.get_state()),
            Err(error) => Err(error),
        }
    }

    async fn capture_process_baseline(&self) -> ProcessBaseline {
        let snapshot = self
            .published
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut supervisor = self.supervisor.lock().await;
        ProcessBaseline {
            local_server: process_is_active(supervisor.snapshot(ProcessKind::LocalServer)),
            local_runner: process_is_active(supervisor.snapshot(ProcessKind::LocalRunner)),
            quick_share: process_is_active(supervisor.snapshot(ProcessKind::QuickShare)),
            regular_tunnel: process_is_active(supervisor.snapshot(ProcessKind::RegularTunnel)),
            snapshot,
        }
    }

    async fn cleanup_new_owned_processes(&self, baseline: &ProcessBaseline) -> ProcessCleanup {
        let mut supervisor = self.supervisor.lock().await;
        let mut cleanup = ProcessCleanup::default();
        for (kind, existed) in [
            (ProcessKind::QuickShare, baseline.quick_share),
            (ProcessKind::RegularTunnel, baseline.regular_tunnel),
            (ProcessKind::LocalRunner, baseline.local_runner),
            (ProcessKind::LocalServer, baseline.local_server),
        ] {
            if !existed && supervisor.snapshot(kind).is_some() {
                supervisor.stop(kind).await;
                cleanup.mark_stopped(kind);
            }
        }
        cleanup
    }

    #[cfg(test)]
    async fn hold_test_operation(
        &self,
        started: tokio::sync::oneshot::Sender<String>,
        cleanup_release: tokio::sync::oneshot::Receiver<()>,
    ) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, core, baseline) = self
            .begin_operation(DesktopOperationKind::LocalSetup, true)
            .await?;
        let _ = started.send(operation.id.clone());
        cancellation.cancelled().await;
        let _ = cleanup_release.await;
        let result: DesktopResult<DesktopStateSnapshot> = Err(cancelled_error());
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }

    #[cfg(test)]
    async fn run_test_one_shot_operation(
        &self,
        executable: PathBuf,
        args: Vec<String>,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> DesktopResult<DesktopStateSnapshot> {
        let (operation, cancellation, mut core, baseline) = self
            .begin_operation(DesktopOperationKind::RuntimeRefresh, true)
            .await?;
        let result = crate::webcodex::run_test_bounded(
            &executable,
            &args,
            Some(&payload),
            &cancellation,
            timeout,
        )
        .await
        .map(|_| core.publish_snapshot());
        self.finish_operation(operation, cancellation, core, baseline, result)
            .await
    }
}

#[derive(Clone)]
struct ProcessBaseline {
    local_server: bool,
    local_runner: bool,
    quick_share: bool,
    regular_tunnel: bool,
    snapshot: DesktopStateSnapshot,
}

#[derive(Clone, Copy, Default)]
struct ProcessCleanup {
    local_server: bool,
    local_runner: bool,
    quick_share: bool,
    regular_tunnel: bool,
}

impl ProcessCleanup {
    fn mark_stopped(&mut self, kind: ProcessKind) {
        match kind {
            ProcessKind::LocalServer => self.local_server = true,
            ProcessKind::LocalRunner => self.local_runner = true,
            ProcessKind::QuickShare => self.quick_share = true,
            ProcessKind::RegularTunnel => self.regular_tunnel = true,
        }
    }
}

fn project_not_loaded_error() -> DesktopError {
    readiness_timeout_error(
        "project_not_loaded",
        "The selected project did not become ready on the current Runner",
        "Retry project activation after checking Runner and project diagnostics.",
    )
}

fn process_is_active(snapshot: Option<crate::process::ProcessSnapshot>) -> bool {
    snapshot.is_some_and(|process| {
        matches!(
            process.phase,
            ProcessPhase::Starting | ProcessPhase::Running | ProcessPhase::Stopping
        )
    })
}

fn can_refresh_legacy_runner(snapshot: Option<crate::process::ProcessSnapshot>) -> bool {
    snapshot.is_some_and(|process| {
        process.owned_by_desktop
            && matches!(
                process.phase,
                ProcessPhase::Starting | ProcessPhase::Running | ProcessPhase::Stopping
            )
    })
}

pub struct DesktopCore {
    data_dir: PathBuf,
    default_project_dir: PathBuf,
    config_path: PathBuf,
    config: StoredDesktopConfig,
    snapshot: DesktopStateSnapshot,
    adapter: WebCodexAdapter,
    supervisor: SharedSupervisor,
    activity: ActivityLog,
    published: Arc<RwLock<DesktopStateSnapshot>>,
}

impl DesktopCore {
    fn new(data_dir: PathBuf, resource_dir: PathBuf) -> DesktopResult<Self> {
        let activity = ActivityLog::default();
        let default_project_dir = default_management_project_dir(&data_dir, &resource_dir);
        let config_path = data_dir.join("desktop-state.json");
        let config = load_config(&config_path, &activity)?;
        let mut snapshot = DesktopStateSnapshot::default();
        snapshot.topology = config.topology.clone();
        snapshot.project = project_snapshot(&config);
        apply_openai_tunnel_configuration(&mut snapshot);
        snapshot.regular_tunnel_available = true;
        snapshot.powershell_runtime = crate::platform::powershell_runtime_snapshot();
        apply_config_projection(&mut snapshot, &config);
        let published = Arc::new(RwLock::new(snapshot.clone()));
        let supervisor = Arc::new(Mutex::new(ProcessSupervisor::new(activity.clone())));
        Ok(Self {
            data_dir,
            default_project_dir,
            config_path,
            config,
            snapshot,
            adapter: WebCodexAdapter::new(Some(resource_dir.join("webcodex-runtime"))),
            supervisor,
            activity,
            published,
        })
    }

    pub async fn get_state(&mut self) -> DesktopResult<DesktopStateSnapshot> {
        apply_openai_tunnel_configuration(&mut self.snapshot);
        self.snapshot.regular_tunnel_available = true;
        apply_config_projection(&mut self.snapshot, &self.config);
        if self.snapshot.regular_tunnel.is_some() {
            let active = self
                .process_snapshot(ProcessKind::RegularTunnel)
                .await
                .is_some_and(|process| {
                    matches!(
                        process.phase,
                        ProcessPhase::Starting | ProcessPhase::Running
                    )
                });
            if let Some(exposure) =
                regular_tunnel_exposure(&mut self.snapshot.regular_tunnel, active)
            {
                self.snapshot.readiness = aggregate_readiness(
                    self.snapshot.readiness.server.clone(),
                    self.snapshot.readiness.runner.clone(),
                    exposure.clone(),
                    self.snapshot.readiness.project.clone(),
                );
                apply_regular_tunnel_next_action(&mut self.snapshot, &exposure);
                self.snapshot.topology = effective_topology(
                    self.config.topology.as_ref(),
                    self.snapshot.regular_tunnel.is_some(),
                );
            }
        }
        Ok(self.publish_snapshot())
    }

    fn chatgpt_activity_probe(&self) -> Option<ChatGptActivityProbe> {
        if !self.snapshot.readiness.runtime_ready
            || self
                .snapshot
                .chatgpt_activity
                .as_ref()
                .is_some_and(|activity| activity.observed)
        {
            return None;
        }
        let identity = identity_from_config(&self.config)?;
        let webcodex = self.adapter.binaries().ok()?.webcodex.clone();
        Some(ChatGptActivityProbe { identity, webcodex })
    }

    fn apply_chatgpt_activity_observation(
        &mut self,
        expected_identity: &ProjectRuntimeIdentity,
        last_meaningful_activity_at_ms: Option<i64>,
    ) -> DesktopResult<DesktopStateSnapshot> {
        if !self.snapshot.readiness.runtime_ready
            || identity_from_config(&self.config).as_ref() != Some(expected_identity)
            || self
                .snapshot
                .chatgpt_activity
                .as_ref()
                .is_some_and(|activity| activity.observed)
        {
            return Ok(self.publish_snapshot());
        }
        self.snapshot.chatgpt_activity = Some(ChatGptActivitySnapshot {
            observed: last_meaningful_activity_at_ms.is_some(),
            last_meaningful_activity_at_ms,
        });
        Ok(self.publish_snapshot())
    }

    fn stage_project_scope(&mut self, project: ProjectSelection) {
        self.snapshot.project = Some(project);
        // ChatGPT activity is evidence for one exact runtime Project. Never carry
        // an observation from the previously displayed Project into a new setup
        // or Quick Share scope; the new Project must earn its own observation.
        self.snapshot.chatgpt_activity = None;
    }

    pub async fn refresh_runtime_status(
        &mut self,
        cancellation: &CancellationContext,
    ) -> DesktopResult<DesktopStateSnapshot> {
        cancellation.check()?;
        if self.snapshot.quick_share.is_some() {
            self.snapshot.chatgpt_activity = None;
            let active = self
                .process_snapshot(ProcessKind::QuickShare)
                .await
                .is_some_and(|process| {
                    matches!(
                        process.phase,
                        ProcessPhase::Starting | ProcessPhase::Running
                    )
                });
            if !active {
                self.snapshot.readiness = aggregate_readiness(
                    ServerReadiness::Stopped,
                    RunnerReadiness::Stopped,
                    ExposureReadiness::Error,
                    ProjectReadiness::Configured,
                );
                self.snapshot.readiness.summary_kind = ReadinessSummaryKind::QuickShareStopped;
                self.snapshot.readiness.next_action_kind =
                    Some(ReadinessNextActionKind::RestartQuickShare);
                self.snapshot.readiness.summary = "Quick Share stopped".to_string();
                self.snapshot.readiness.next_action = Some("Start Quick Share again.".to_string());
            }
            return self.get_state().await;
        }

        let Some(identity) = identity_from_config(&self.config) else {
            self.snapshot.chatgpt_activity = None;
            self.snapshot.topology = self.config.topology.clone();
            self.snapshot.project = project_snapshot(&self.config);
            return self.get_state().await;
        };
        self.adapter.ensure_binaries(cancellation).await.ok();
        cancellation.check()?;
        if let Ok(binaries) = self.adapter.binaries() {
            self.snapshot.binaries = Some(binaries.info());
        }
        let server = match self
            .adapter
            .server_status(
                Some(&identity.server_url),
                self.config
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.server_env_file.as_deref()),
                Some(&identity.user_token_file),
                cancellation,
            )
            .await
        {
            Ok(status) if status.http_reachable => ServerReadiness::Ready,
            Ok(_) => ServerReadiness::Error,
            Err(_) => ServerReadiness::Unknown,
        };
        cancellation.check()?;
        let runner = match self.adapter.runner_ready(&identity, cancellation).await {
            Ok(true) => RunnerReadiness::Ready,
            Ok(false) => RunnerReadiness::Connecting,
            Err(_) => RunnerReadiness::Unknown,
        };
        cancellation.check()?;
        let project = match self.adapter.project_ready(&identity, cancellation).await {
            Ok(true) => ProjectReadiness::Ready,
            Ok(false) => ProjectReadiness::ReloadRequired,
            Err(_) => ProjectReadiness::Unknown,
        };
        cancellation.check()?;
        self.snapshot.chatgpt_activity = if server == ServerReadiness::Ready
            && project == ProjectReadiness::Ready
        {
            match self.adapter.chatgpt_activity(&identity, cancellation).await {
                Ok(last_meaningful_activity_at_ms) => Some(ChatGptActivitySnapshot {
                    observed: last_meaningful_activity_at_ms.is_some(),
                    last_meaningful_activity_at_ms,
                }),
                Err(_) => None,
            }
        } else {
            None
        };
        cancellation.check()?;
        let tunnel_active = self
            .process_snapshot(ProcessKind::RegularTunnel)
            .await
            .is_some_and(|process| {
                matches!(
                    process.phase,
                    ProcessPhase::Starting | ProcessPhase::Running
                )
            });
        let regular_tunnel_expected = self.snapshot.regular_tunnel.is_some();
        let exposure = regular_tunnel_exposure(&mut self.snapshot.regular_tunnel, tunnel_active)
            .unwrap_or_else(|| exposure_readiness(self.config.topology.as_ref()));
        self.snapshot.readiness = aggregate_readiness(server, runner, exposure.clone(), project);
        if regular_tunnel_expected && exposure == ExposureReadiness::Error {
            self.snapshot.readiness.next_action_kind =
                Some(ReadinessNextActionKind::RestartSecureTunnel);
            self.snapshot.readiness.next_action = Some("Restart the secure tunnel.".to_string());
        } else if regular_tunnel_expected && exposure == ExposureReadiness::Degraded {
            self.snapshot.readiness.next_action_kind =
                Some(ReadinessNextActionKind::RestoreClipboardHandoff);
            self.snapshot.readiness.next_action = Some(
                "Restore clipboard access, then restart the secure tunnel handoff.".to_string(),
            );
        }
        self.snapshot.topology = effective_topology(
            self.config.topology.as_ref(),
            self.snapshot.regular_tunnel.is_some(),
        );
        self.snapshot.project = project_snapshot(&self.config);
        self.get_state().await
    }

    fn publish_snapshot(&mut self) -> DesktopStateSnapshot {
        self.snapshot.current_operation = None;
        self.snapshot.activity_sequence = self.activity.latest_sequence();
        apply_openai_tunnel_configuration(&mut self.snapshot);
        self.snapshot.regular_tunnel_available = true;
        self.snapshot.powershell_runtime = crate::platform::powershell_runtime_snapshot();
        apply_config_projection(&mut self.snapshot, &self.config);
        let snapshot = self.snapshot.clone();
        *self
            .published
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot.clone();
        snapshot
    }

    fn reconcile_after_operation_failure(
        &mut self,
        kind: DesktopOperationKind,
        baseline: &ProcessBaseline,
        cleanup: ProcessCleanup,
        cancelled: bool,
    ) {
        let observed_binaries = self.snapshot.binaries.clone();
        match kind {
            DesktopOperationKind::RuntimeResume => {
                // Resume is a desired-state replay of an already committed setup.
                // If any step fails or is cancelled, newly owned processes have
                // already been reclaimed above; restore the last published view
                // rather than leaving a synthetic "starting" state behind.
                self.snapshot = baseline.snapshot.clone();
            }
            DesktopOperationKind::QuickShareStart => {
                // Quick Share is intentionally ephemeral. Any failed start has
                // already stopped (or will have cleanup stop) the newly owned
                // foreground process, so return the public topology to the
                // last committed runtime instead of leaving "starting" behind.
                self.snapshot = baseline.snapshot.clone();
            }
            DesktopOperationKind::RegularTunnelStart if cancelled => {
                // User cancellation is not a tunnel failure. Restore the last
                // observed full-runtime state after the exact owned tunnel is
                // reclaimed rather than publishing a synthetic tunnel error.
                self.snapshot = baseline.snapshot.clone();
            }
            DesktopOperationKind::RuntimeRefresh if cancelled => {
                // A cancelled observation must not partially overwrite the
                // last published control-plane state.
                self.snapshot = baseline.snapshot.clone();
            }
            DesktopOperationKind::LocalSetup => {
                let server = if cleanup.local_server {
                    ServerReadiness::Stopped
                } else if self.snapshot.readiness.server == ServerReadiness::Starting {
                    baseline.snapshot.readiness.server.clone()
                } else {
                    self.snapshot.readiness.server.clone()
                };
                let runner = if cleanup.local_runner {
                    RunnerReadiness::Stopped
                } else if self.snapshot.readiness.runner == RunnerReadiness::Connecting {
                    baseline.snapshot.readiness.runner.clone()
                } else {
                    self.snapshot.readiness.runner.clone()
                };
                let project = if cleanup.local_server || cleanup.local_runner {
                    self.snapshot
                        .project
                        .as_ref()
                        .map(|_| ProjectReadiness::Configured)
                        .unwrap_or(ProjectReadiness::None)
                } else {
                    self.snapshot.readiness.project.clone()
                };
                self.snapshot.readiness = aggregate_readiness(
                    server,
                    runner,
                    self.snapshot.readiness.exposure.clone(),
                    project,
                );
            }
            DesktopOperationKind::RemoteSetup => {
                let runner = if cleanup.local_runner {
                    RunnerReadiness::Stopped
                } else if self.snapshot.readiness.runner == RunnerReadiness::Connecting {
                    baseline.snapshot.readiness.runner.clone()
                } else {
                    self.snapshot.readiness.runner.clone()
                };
                let project = if cleanup.local_runner {
                    self.snapshot
                        .project
                        .as_ref()
                        .map(|_| ProjectReadiness::Configured)
                        .unwrap_or(ProjectReadiness::None)
                } else {
                    self.snapshot.readiness.project.clone()
                };
                self.snapshot.readiness = aggregate_readiness(
                    self.snapshot.readiness.server.clone(),
                    runner,
                    self.snapshot.readiness.exposure.clone(),
                    project,
                );
            }
            _ => {}
        }
        if observed_binaries.is_some() {
            self.snapshot.binaries = observed_binaries;
        }
    }

    pub async fn resume_saved_runtime(
        &mut self,
        cancellation: &CancellationContext,
    ) -> DesktopResult<DesktopStateSnapshot> {
        cancellation.check()?;
        let Some(topology) = self.config.topology.clone() else {
            return self.get_state().await;
        };
        if topology.experience != Experience::Full {
            return self.get_state().await;
        }
        let project_path = self
            .config
            .project
            .as_ref()
            .map(|project| project.path.clone());
        match topology.server {
            ServerTopology::Local => {
                self.configure_local_setup(project_path.as_deref(), cancellation)
                    .await
            }
            ServerTopology::Remote { url } => {
                let project_path = project_path.ok_or_else(|| {
                    DesktopError::new(
                        "project_not_ready",
                        "The saved remote Desktop runtime no longer has a project selection",
                        "Choose a project or change the Desktop runtime setup.",
                    )
                })?;
                self.configure_remote_setup(&url, "", &project_path, cancellation)
                    .await
            }
        }
    }

    pub async fn update_tunnel_proxy(
        &mut self,
        mode: TunnelProxyMode,
        custom_url: Option<&str>,
        cancellation: &CancellationContext,
    ) -> DesktopResult<DesktopStateSnapshot> {
        cancellation.check()?;
        let custom_url = match mode {
            TunnelProxyMode::Custom => Some(validate_tunnel_proxy_url(custom_url.unwrap_or(""))?),
            TunnelProxyMode::Auto | TunnelProxyMode::Direct => None,
        };
        self.config.tunnel_proxy = TunnelProxyConfig { mode, custom_url };
        self.save_config().await?;
        self.get_state().await
    }

    pub async fn configure_local_setup(
        &mut self,
        project_path: Option<&str>,
        cancellation: &CancellationContext,
    ) -> DesktopResult<DesktopStateSnapshot> {
        cancellation.check()?;
        let project = match project_path.map(str::trim).filter(|path| !path.is_empty()) {
            Some(path) => self.adapter.inspect_project(path).await?,
            None => {
                tokio::fs::create_dir_all(&self.default_project_dir)
                    .await
                    .map_err(|error| {
                        DesktopError::new(
                            "default_project_unavailable",
                            "Desktop could not prepare its default management project",
                            "Check that the WebCodex Desktop install directory is writable, or choose another project from Change runtime mode.",
                        )
                        .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
                    })?;
                let mut project = self
                    .adapter
                    .inspect_project(&self.default_project_dir.to_string_lossy())
                    .await?;
                // The automatically registered management project should not
                // also grant authority to register sibling installation folders.
                // Equality is a valid allowed-root boundary, so keep this
                // implicit setup scoped to the installation directory itself.
                project.allowed_root = project.path.clone();
                project
            }
        };
        cancellation.check()?;
        let binaries = self.adapter.ensure_binaries(cancellation).await?.clone();
        self.snapshot.binaries = Some(binaries.info());
        self.activity.push(
            ActivityEventKind::LocalSetupPreparing,
            "desktop",
            ActivityLevel::Info,
            "Preparing WebCodex on this computer",
        );
        self.snapshot.topology = Some(RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Local,
            runner: RunnerTopology::Local,
            exposure: Exposure::None,
            enrollment: Enrollment::ManagedPairing,
        });
        self.stage_project_scope(project.clone());
        self.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Starting,
            RunnerReadiness::Stopped,
            ExposureReadiness::Disabled,
            ProjectReadiness::Configured,
        );
        self.publish_snapshot();

        let local_dir = self.data_dir.join("runtime").join("local");
        let env_file = local_dir.join("webcodex.env");
        let data_dir = local_dir.join("data");
        tokio::fs::create_dir_all(&local_dir).await.map_err(|_| {
            DesktopError::new(
                "desktop_state_unavailable",
                "Desktop could not create its local runtime directory",
                "Check local app-data permissions and retry.",
            )
        })?;
        cancellation.check()?;

        let server_url = if env_file.is_file() {
            ensure_desktop_server_defaults(&env_file)?;
            self.adapter
                .server_status(None, Some(&env_file), None, cancellation)
                .await?
                .probe_url
        } else {
            let listen = reserve_loopback_address()?;
            let status = self
                .adapter
                .init_local_server(&listen, &data_dir, &env_file, cancellation)
                .await?;
            ensure_desktop_server_defaults(&env_file)?;
            status.probe_url
        };
        let reusable_identity = identity_from_config(&self.config)
            .filter(|identity| same_server(&identity.server_url, &server_url));
        let saved_runner_client_id = stored_runner_client_id(&self.config);
        cancellation.check()?;

        let server_deadline = Deadline::after(SERVER_READY_TIMEOUT);
        let running = self
            .adapter
            .server_status_until(
                Some(&server_url),
                Some(&env_file),
                None,
                cancellation,
                server_deadline,
            )
            .await
            .is_ok_and(|status| status.http_reachable);
        cancellation.check()?;
        let server_started = if !running {
            if server_deadline.is_elapsed() {
                return Err(readiness_timeout_error(
                    "server_unreachable",
                    "WebCodex Service did not become ready",
                    "Check the local Service diagnostics and retry.",
                ));
            }
            let command = self.adapter.local_server_command(&env_file)?;
            self.spawn_owned(ProcessKind::LocalServer, command, false, cancellation)
                .await?;
            true
        } else {
            false
        };
        self.wait_for_server(
            &server_url,
            Some(&env_file),
            None,
            cancellation,
            server_deadline,
            server_started,
        )
        .await?;
        self.snapshot.readiness.server = ServerReadiness::Ready;
        self.publish_snapshot();

        let reusable_observation = match reusable_identity.as_ref() {
            Some(identity) => self
                .adapter
                .observe_runner_connection(
                    identity,
                    saved_runner_client_id.as_deref(),
                    cancellation,
                )
                .await
                .ok(),
            None => None,
        };
        let (mut identity, identity_replaced, runner_client_id) =
            match (reusable_identity, reusable_observation) {
                (Some(identity), Some(observation)) => (identity, false, observation.client_id),
                _ => {
                    // Login publishes with --overwrite. Keep the saved connection's
                    // files intact until activation and config persistence succeed.
                    let connections_dir = local_enrollment_directory(&self.data_dir, &self.config);
                    let pairing_code = self
                        .adapter
                        .create_local_pairing(&server_url, &env_file, cancellation)
                        .await?;
                    let identity = self
                        .adapter
                        .login_with_pairing(
                            &server_url,
                            &pairing_code,
                            &connections_dir,
                            &project,
                            cancellation,
                        )
                        .await?;
                    drop(pairing_code);
                    cancellation.check()?;
                    let observation = self
                        .adapter
                        .observe_runner_connection(&identity, None, cancellation)
                        .await?;
                    (identity, true, observation.client_id)
                }
            };

        let replacing_owned_runner = identity_replaced
            && process_is_active(self.process_snapshot(ProcessKind::LocalRunner).await);
        let runner_deadline = Deadline::after(RUNNER_READY_TIMEOUT);
        let activation: DesktopResult<bool> = async {
            if replacing_owned_runner {
                // A Desktop-owned Runner can only serve the exact config it was
                // started with. Replace that owned process transactionally while
                // keeping the local Server alive; never broad-kill unrelated Runners.
                self.stop_process_until(ProcessKind::LocalRunner, runner_deadline)
                    .await;
                if runner_deadline.is_elapsed() {
                    return Err(readiness_timeout_error(
                        "runner_offline",
                        "Desktop could not stop its previous Runner before changing projects",
                        "Retry project setup. Desktop will only replace the Runner it owns.",
                    ));
                }
                cancellation.check()?;
            }

            let runner_ready = if identity_replaced {
                // A fresh Desktop enrollment may reuse the same client_id while
                // changing the Runner token or project registry. An older Runner
                // with that client_id is not proof that this exact config is active.
                false
            } else {
                self.adapter
                    .runner_ready_until(&identity, cancellation, runner_deadline)
                    .await
                    .unwrap_or(false)
            };
            cancellation.check()?;
            let mut runner_started = if !runner_ready {
                if runner_deadline.is_elapsed() {
                    return Err(readiness_timeout_error(
                        "runner_offline",
                        "Runner did not become connected",
                        "Retry project setup to restart Desktop's Runner.",
                    ));
                }
                self.snapshot.readiness.runner = RunnerReadiness::Connecting;
                self.publish_snapshot();
                let command = self.adapter.local_runner_command(&identity.runner_config)?;
                self.spawn_owned(ProcessKind::LocalRunner, command, false, cancellation)
                    .await?;
                true
            } else {
                false
            };
            self.wait_for_runner(&identity, cancellation, runner_deadline, runner_started)
                .await?;
            self.snapshot.readiness.runner = RunnerReadiness::Ready;
            self.publish_snapshot();
            identity = match self
                .adapter
                .activate_project(&identity, &runner_client_id, &project, cancellation)
                .await
            {
                Ok(identity) => identity,
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "project_activation_capability_unavailable"
                            | "project_activation_restart_required"
                    ) =>
                {
                    if !can_refresh_legacy_runner(
                        self.process_snapshot(ProcessKind::LocalRunner).await,
                    ) {
                        return Err(DesktopError::new(
                            "project_activation_legacy_runner",
                            "This Runner needs to be refreshed before the new project can be activated",
                            "Refresh the Runner, then select this project again.",
                        ));
                    }
                    let legacy_identity = self
                        .adapter
                        .legacy_register_project(
                            &identity,
                            &runner_client_id,
                            &project,
                            cancellation,
                        )
                        .await?;
                    let legacy_deadline = Deadline::after(RUNNER_READY_TIMEOUT);
                    self.stop_process_until(ProcessKind::LocalRunner, legacy_deadline)
                        .await;
                    if legacy_deadline.is_elapsed() {
                        return Err(readiness_timeout_error(
                            "runner_offline",
                            "Desktop could not refresh its legacy Runner",
                            "Retry project setup after checking Runner diagnostics.",
                        ));
                    }
                    let command = self
                        .adapter
                        .local_runner_command(&legacy_identity.runner_config)?;
                    self.spawn_owned(ProcessKind::LocalRunner, command, false, cancellation)
                        .await?;
                    self.wait_for_runner(&legacy_identity, cancellation, legacy_deadline, true)
                        .await?;
                    runner_started = true;
                    legacy_identity
                }
                Err(error) => return Err(error),
            };
            self.wait_for_project(&identity, cancellation).await?;
            cancellation.check()?;
            Ok(runner_started)
        }
        .await;
        let runner_started = match activation {
            Ok(runner_started) => runner_started,
            Err(error) => {
                if replacing_owned_runner {
                    self.stop_process_until(
                        ProcessKind::LocalRunner,
                        Deadline::after(READINESS_CLEANUP_SLACK),
                    )
                    .await;
                    self.snapshot.topology = self.config.topology.clone();
                    self.snapshot.project = project_snapshot(&self.config);
                    self.snapshot.readiness = aggregate_readiness(
                        ServerReadiness::Ready,
                        RunnerReadiness::Stopped,
                        exposure_readiness(self.config.topology.as_ref()),
                        self.config
                            .project
                            .as_ref()
                            .map(|_| ProjectReadiness::Configured)
                            .unwrap_or(ProjectReadiness::None),
                    );
                    self.publish_snapshot();
                }
                return Err(error);
            }
        };

        // Commit the visible/saved project only after the new Runner and exact
        // project have both reached readiness. Until this point the previous
        // stored project remains the recovery authority.
        let previous_config = self.config.clone();
        let mut committed_project = project.clone();
        committed_project.runtime_project_id = Some(identity.runtime_project_id.clone());
        self.config.topology = self.snapshot.topology.clone();
        self.config.project = Some(committed_project.clone());
        self.config.runtime_autostart = Some(true);
        self.config.runtime = Some(StoredRuntime {
            server_url: identity.server_url.clone(),
            server_env_file: Some(env_file.clone()),
            runner_config: Some(identity.runner_config.clone()),
            user_token_file: Some(identity.user_token_file.clone()),
            runner_client_id: Some(runner_client_id.clone()),
            project_id: Some(identity.project_id.clone()),
            runtime_project_id: Some(identity.runtime_project_id.clone()),
        });
        if let Err(error) = self.save_config().await {
            self.config = previous_config;
            self.snapshot.topology = self.config.topology.clone();
            self.snapshot.project = project_snapshot(&self.config);
            if runner_started || replacing_owned_runner {
                self.stop_process_until(
                    ProcessKind::LocalRunner,
                    Deadline::after(READINESS_CLEANUP_SLACK),
                )
                .await;
            }
            if server_started {
                self.stop_process_until(
                    ProcessKind::LocalServer,
                    Deadline::after(READINESS_CLEANUP_SLACK),
                )
                .await;
            }
            self.snapshot.readiness = aggregate_readiness(
                if server_started {
                    ServerReadiness::Stopped
                } else {
                    ServerReadiness::Ready
                },
                if runner_started || replacing_owned_runner {
                    RunnerReadiness::Stopped
                } else {
                    RunnerReadiness::Ready
                },
                ExposureReadiness::Disabled,
                self.config
                    .project
                    .as_ref()
                    .map(|_| ProjectReadiness::Configured)
                    .unwrap_or(ProjectReadiness::None),
            );
            self.publish_snapshot();
            return Err(error);
        }
        self.snapshot.project = Some(committed_project);
        self.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            ExposureReadiness::LocalReady,
            ProjectReadiness::Ready,
        );
        self.activity.push(
            ActivityEventKind::LocalRuntimeReady,
            "desktop",
            ActivityLevel::Info,
            "Local WebCodex runtime is ready",
        );
        self.get_state().await
    }

    pub async fn configure_remote_setup(
        &mut self,
        server_url: &str,
        pairing_code: &str,
        project_path: &str,
        cancellation: &CancellationContext,
    ) -> DesktopResult<DesktopStateSnapshot> {
        cancellation.check()?;
        let server_url = crate::webcodex::validate_server_url(server_url)?;
        let project = self.adapter.inspect_project(project_path).await?;
        cancellation.check()?;
        let binaries = self.adapter.ensure_binaries(cancellation).await?.clone();
        self.snapshot.binaries = Some(binaries.info());
        let exposure = if server_url.starts_with("https://") {
            Exposure::ExistingHttps {
                url: server_url.clone(),
            }
        } else {
            Exposure::None
        };
        let topology = RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Remote {
                url: server_url.clone(),
            },
            runner: RunnerTopology::Local,
            exposure,
            enrollment: Enrollment::ManagedPairing,
        };
        self.snapshot.topology = Some(topology.clone());
        self.stage_project_scope(project.clone());
        self.config.topology = Some(topology.clone());
        self.config.runtime_autostart = Some(true);
        self.config.preferred_connection = Some(RegularConnectionPreference::NoChatGpt);
        self.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Starting,
            RunnerReadiness::Connecting,
            if server_url.starts_with("https://") {
                ExposureReadiness::Starting
            } else {
                ExposureReadiness::Degraded
            },
            ProjectReadiness::Configured,
        );
        self.activity.push(
            ActivityEventKind::RemoteConnecting,
            "desktop",
            ActivityLevel::Info,
            "Connecting this computer to the existing WebCodex Server",
        );
        self.publish_snapshot();

        let reusable_identity = identity_from_config(&self.config)
            .filter(|identity| same_server(&identity.server_url, &server_url));
        let saved_runner_client_id = stored_runner_client_id(&self.config);
        let reusable_observation = match reusable_identity.as_ref() {
            Some(identity) => self
                .adapter
                .observe_runner_connection(
                    identity,
                    saved_runner_client_id.as_deref(),
                    cancellation,
                )
                .await
                .ok(),
            None => None,
        };
        let (mut identity, identity_replaced, runner_client_id) = match (
            reusable_identity,
            reusable_observation,
        ) {
            (Some(identity), Some(observation)) => (identity, false, observation.client_id),
            _ => {
                if !pairing_code.starts_with("wc_pair_") {
                    return Err(DesktopError::new(
                            "pairing_code_invalid",
                            "The saved Runner identity is not reusable and no new WebCodex pairing code was provided",
                            "Refresh this Runner connection with a new wc_pair_… code.",
                        ));
                }
                let identity = self
                    .adapter
                    .login_with_pairing(
                        &server_url,
                        pairing_code,
                        &self.data_dir.join("connections"),
                        &project,
                        cancellation,
                    )
                    .await?;
                let observation = self
                    .adapter
                    .observe_runner_connection(&identity, None, cancellation)
                    .await?;
                // A remote pairing code is one-shot. Publish the newly
                // validated connection identity before Runner/project
                // activation so a later readiness failure can retry this
                // same credential instead of forcing another pairing.
                self.config.topology = Some(topology.clone());
                self.store_identity(
                    &project,
                    &identity,
                    None,
                    Some(observation.client_id.clone()),
                )
                .await?;
                cancellation.check()?;
                (identity, true, observation.client_id)
            }
        };

        let server_deadline = Deadline::after(SERVER_READY_TIMEOUT);
        let server_status = match self
            .adapter
            .server_status_until(
                Some(&server_url),
                None,
                Some(&identity.user_token_file),
                cancellation,
                server_deadline,
            )
            .await
        {
            Ok(status) => status,
            Err(error) => {
                cancellation.check()?;
                if server_deadline.is_elapsed() {
                    return Err(readiness_timeout_error(
                        "server_unreachable",
                        "The existing WebCodex Server did not respond before the readiness deadline",
                        "Check the Server URL and network path, then retry.",
                    ));
                }
                return Err(error);
            }
        };
        cancellation.check()?;
        if !server_status.http_reachable {
            return Err(DesktopError::new(
                "server_unreachable",
                "The existing WebCodex Server is not reachable",
                "Check the Server URL and network path, then retry.",
            ));
        }
        let runner_deadline = Deadline::after(RUNNER_READY_TIMEOUT);
        let replacing_owned_runner = identity_replaced
            && process_is_active(self.process_snapshot(ProcessKind::LocalRunner).await);
        if replacing_owned_runner {
            self.stop_process_until(ProcessKind::LocalRunner, runner_deadline)
                .await;
            cancellation.check()?;
        }
        let runner_ready = if identity_replaced {
            false
        } else {
            self.adapter
                .runner_ready_until(&identity, cancellation, runner_deadline)
                .await
                .unwrap_or(false)
        };
        cancellation.check()?;
        let runner_started = if !runner_ready {
            if runner_deadline.is_elapsed() {
                return Err(readiness_timeout_error(
                    "runner_offline",
                    "Runner did not become connected",
                    "Check Server reachability and Runner diagnostics, then retry.",
                ));
            }
            let command = self.adapter.local_runner_command(&identity.runner_config)?;
            self.spawn_owned(ProcessKind::LocalRunner, command, false, cancellation)
                .await?;
            true
        } else {
            false
        };
        self.wait_for_runner(&identity, cancellation, runner_deadline, runner_started)
            .await?;
        identity = match self
            .adapter
            .activate_project(&identity, &runner_client_id, &project, cancellation)
            .await
        {
            Ok(identity) => identity,
            Err(error)
                if matches!(
                    error.code.as_str(),
                    "project_activation_capability_unavailable"
                        | "project_activation_restart_required"
                ) =>
            {
                if !can_refresh_legacy_runner(self.process_snapshot(ProcessKind::LocalRunner).await)
                {
                    return Err(DesktopError::new(
                        "project_activation_legacy_runner",
                        "This Runner needs to be refreshed before the new project can be activated",
                        "Refresh the Runner, then select this project again.",
                    ));
                }
                let legacy_identity = self
                    .adapter
                    .legacy_register_project(&identity, &runner_client_id, &project, cancellation)
                    .await?;
                let legacy_deadline = Deadline::after(RUNNER_READY_TIMEOUT);
                self.stop_process_until(ProcessKind::LocalRunner, legacy_deadline)
                    .await;
                if legacy_deadline.is_elapsed() {
                    return Err(readiness_timeout_error(
                        "runner_offline",
                        "Desktop could not refresh its legacy Runner",
                        "Retry project setup after checking Runner diagnostics.",
                    ));
                }
                let command = self
                    .adapter
                    .local_runner_command(&legacy_identity.runner_config)?;
                self.spawn_owned(ProcessKind::LocalRunner, command, false, cancellation)
                    .await?;
                self.wait_for_runner(&legacy_identity, cancellation, legacy_deadline, true)
                    .await?;
                legacy_identity
            }
            Err(error) => return Err(error),
        };
        self.wait_for_project(&identity, cancellation).await?;
        cancellation.check()?;
        self.config.topology = Some(topology);
        self.store_identity(&project, &identity, None, Some(runner_client_id))
            .await?;
        self.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            if server_url.starts_with("https://") {
                // HTTPS proves transport shape, not that ChatGPT can reach and
                // authenticate to the MCP endpoint. D1 has no canonical remote
                // MCP/handoff probe, so keep this explicitly unverified.
                ExposureReadiness::Unknown
            } else {
                ExposureReadiness::Degraded
            },
            ProjectReadiness::Ready,
        );
        self.activity.push(
            ActivityEventKind::RemoteConnected,
            "desktop",
            ActivityLevel::Info,
            "This computer is connected to the existing WebCodex Server",
        );
        self.get_state().await
    }

    pub async fn start_quick_share(
        &mut self,
        project_path: &str,
        provider: &str,
        cancellation: &CancellationContext,
    ) -> DesktopResult<DesktopStateSnapshot> {
        cancellation.check()?;
        let project = self.adapter.inspect_project(project_path).await?;
        cancellation.check()?;
        let binaries = self.adapter.ensure_binaries(cancellation).await?.clone();
        self.snapshot.binaries = Some(binaries.info());
        if self
            .process_snapshot(ProcessKind::QuickShare)
            .await
            .is_some_and(|process| {
                matches!(
                    process.phase,
                    ProcessPhase::Starting | ProcessPhase::Running
                )
            })
        {
            return Err(DesktopError::new(
                "quick_share_already_running",
                "Quick Share is already running",
                "Stop the current share before starting another one.",
            ));
        }
        let deadline = Deadline::after(QUICK_SHARE_READY_TIMEOUT);
        let tunnel_proxy = effective_tunnel_proxy(&self.config.tunnel_proxy)?;
        let command = self.adapter.quick_share_command(
            Path::new(&project.path),
            provider,
            tunnel_proxy.url.as_deref(),
        )?;
        if deadline.is_elapsed() {
            return Err(readiness_timeout_error(
                "quick_share_not_ready",
                "Quick Share did not reach verified readiness",
                "Check Activity and Tunnel prerequisites, then retry.",
            ));
        }
        let mut events = self
            .spawn_owned(ProcessKind::QuickShare, command, true, cancellation)
            .await?
            .expect("machine stdout requested");
        self.snapshot.topology = Some(RuntimeTopology {
            experience: Experience::QuickShare,
            server: ServerTopology::Local,
            runner: RunnerTopology::Local,
            exposure: match provider {
                "cloudflare" => Exposure::Cloudflare,
                "openai" => Exposure::OpenAiTunnel,
                _ => Exposure::None,
            },
            enrollment: Enrollment::ExistingProfile {
                profile: "temporary_share".to_string(),
            },
        });
        self.stage_project_scope(project.clone());
        self.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Starting,
            RunnerReadiness::Connecting,
            ExposureReadiness::Starting,
            ProjectReadiness::Configured,
        );
        self.activity.push(
            ActivityEventKind::QuickShareStarting,
            "quick_share",
            ActivityLevel::Info,
            "Starting the temporary Quick Share runtime",
        );
        self.publish_snapshot();
        let event_wait = async {
            while let Some(value) = events.recv().await {
                match value.get("event").and_then(Value::as_str) {
                    Some("ready") => return Ok(Some(value)),
                    Some("machine_event_overflow") => return Err(value),
                    _ => {}
                }
            }
            Ok(None)
        };
        let event_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                self.stop_process_until(
                    ProcessKind::QuickShare,
                    Deadline::at(deadline.cleanup_deadline(READINESS_CLEANUP_SLACK)),
                ).await;
                return Err(cancelled_error());
            }
            result = tokio::time::timeout_at(deadline.instant(), event_wait) => {
                result
            }
        };
        let event_value = match event_result {
            Ok(Ok(Some(value))) => value,
            Ok(Err(overflow)) => {
                self.stop_process_until(
                    ProcessKind::QuickShare,
                    Deadline::at(deadline.cleanup_deadline(READINESS_CLEANUP_SLACK)),
                )
                .await;
                return Err(machine_event_overflow_error(&overflow));
            }
            Ok(Ok(None)) | Err(_) => {
                let logs = self.process_logs(ProcessKind::QuickShare).await;
                self.stop_process_until(
                    ProcessKind::QuickShare,
                    Deadline::at(deadline.cleanup_deadline(READINESS_CLEANUP_SLACK)),
                )
                .await;
                return Err(DesktopError::new(
                    "quick_share_not_ready",
                    "Quick Share did not reach verified readiness",
                    "Check Activity and Tunnel prerequisites, then retry.",
                )
                .with_details(serde_json::json!({
                    "category": "readiness_timeout",
                    "diagnostic_lines": logs,
                })));
            }
        };
        let event: QuickShareReadyEvent = match serde_json::from_value(event_value) {
            Ok(event) => event,
            Err(_) => {
                self.stop_process(ProcessKind::QuickShare).await;
                return Err(DesktopError::new(
                    "webcodex_contract_invalid",
                    "Quick Share returned an invalid readiness event",
                    "Verify that Desktop and WebCodex binaries come from the same source baseline.",
                ));
            }
        };
        if event.event != "ready"
            || event.schema_version != 1
            || event.experience != "quick_share"
            || event.project.trim().is_empty()
            || event.exposure.kind.trim().is_empty()
        {
            self.stop_process(ProcessKind::QuickShare).await;
            return Err(DesktopError::new(
                "webcodex_contract_invalid",
                "Quick Share readiness identity is incomplete",
                "Update Desktop and WebCodex together.",
            ));
        }
        cancellation.check()?;
        let clipboard_required = event.connection.clipboard_contains != "none";
        let handoff_available = !clipboard_required || event.connection.clipboard_state == "copied";
        let ready_for_chatgpt = event.ready_for_chatgpt && handoff_available;
        self.snapshot.quick_share = Some(QuickShareState {
            provider: provider.to_string(),
            project: project.path.clone(),
            mcp_url: event.connection.mcp_url,
            clipboard_state: event.connection.clipboard_state,
            clipboard_contains: event.connection.clipboard_contains,
            ready_for_chatgpt,
        });
        let exposure_readiness = match event.exposure.state.as_str() {
            "remote_ready" if handoff_available => ExposureReadiness::RemoteReady,
            "remote_ready" => ExposureReadiness::Degraded,
            "local_ready" => ExposureReadiness::LocalReady,
            _ => ExposureReadiness::Unknown,
        };
        self.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            exposure_readiness,
            ProjectReadiness::Ready,
        );
        if !handoff_available {
            self.snapshot.readiness.next_action_kind =
                Some(ReadinessNextActionKind::RestoreClipboardHandoff);
            self.snapshot.readiness.next_action =
                Some("Clipboard handoff is unavailable; restart Quick Share after clipboard access is restored.".to_string());
        }
        self.activity.push(
            ActivityEventKind::QuickShareReady,
            "quick_share",
            ActivityLevel::Info,
            "Quick Share reached verified readiness",
        );
        self.get_state().await
    }

    pub async fn stop_quick_share(
        &mut self,
        cancellation: &CancellationContext,
    ) -> DesktopResult<DesktopStateSnapshot> {
        self.stop_process(ProcessKind::QuickShare).await;
        self.snapshot.quick_share = None;
        self.snapshot.topology = self.config.topology.clone();
        self.snapshot.project = self.config.project.clone();
        self.activity.push(
            ActivityEventKind::QuickShareStopped,
            "quick_share",
            ActivityLevel::Info,
            "Quick Share stopped",
        );
        if self.config.runtime.is_some() {
            self.refresh_runtime_status(cancellation).await
        } else {
            self.snapshot.readiness = DesktopStateSnapshot::default().readiness;
            self.get_state().await
        }
    }

    pub async fn start_regular_tunnel(
        &mut self,
        cancellation: &CancellationContext,
    ) -> DesktopResult<DesktopStateSnapshot> {
        cancellation.check()?;
        if self.snapshot.quick_share.is_some() {
            return Err(DesktopError::new(
                "unsupported_topology",
                "Regular ChatGPT Connection is unavailable while Quick Share is active",
                "Stop Quick Share and start the Local Full Runtime first.",
            ));
        }
        let topology = self.config.topology.clone().ok_or_else(|| {
            DesktopError::new(
                "runtime_not_ready",
                "Local Full Runtime has not been configured",
                "Set up WebCodex on this computer before starting the secure tunnel.",
            )
        })?;
        if topology.experience != Experience::Full
            || !matches!(topology.server, ServerTopology::Local)
        {
            return Err(DesktopError::new(
                "unsupported_topology",
                "Regular OpenAI Secure Tunnel is only started for a local WebCodex Server",
                "Manage external exposure on the remote Server instead.",
            ));
        }
        if !openai_tunnel_configuration().is_configured() {
            return Err(DesktopError::new(
                "tunnel_unavailable",
                "OpenAI Secure Tunnel is not configured",
                "Configure the canonical Control Plane Tunnel environment, then retry.",
            ));
        }
        if self
            .process_snapshot(ProcessKind::RegularTunnel)
            .await
            .is_some_and(|process| {
                matches!(
                    process.phase,
                    ProcessPhase::Starting | ProcessPhase::Running
                )
            })
        {
            return Err(DesktopError::new(
                "regular_tunnel_already_running",
                "OpenAI Secure Tunnel is already running",
                "Stop the current secure tunnel before starting another one.",
            ));
        }

        let current = self.refresh_runtime_status(cancellation).await?;
        if !current.readiness.runtime_ready {
            return Err(DesktopError::new(
                "runtime_not_ready",
                "Local WebCodex runtime is not ready",
                "Restore the Server and Runner readiness before starting the secure tunnel.",
            ));
        }
        let runtime = self.config.runtime.clone().ok_or_else(|| {
            DesktopError::new(
                "runtime_not_ready",
                "Local runtime identity is incomplete",
                "Run Local Setup again.",
            )
        })?;
        let env_file = runtime
            .server_env_file
            .filter(|path| path.is_file())
            .ok_or_else(|| {
                DesktopError::new(
                    "server_unavailable",
                    "Local Server configuration is unavailable",
                    "Run Local Setup again.",
                )
            })?;
        let deadline = Deadline::after(REGULAR_TUNNEL_READY_TIMEOUT);
        let tunnel_proxy = effective_tunnel_proxy(&self.config.tunnel_proxy)?;
        let command = self
            .adapter
            .regular_tunnel_command(&env_file, tunnel_proxy.url.as_deref())?;
        if deadline.is_elapsed() {
            return Err(readiness_timeout_error(
                "tunnel_unavailable",
                "OpenAI Secure Tunnel did not reach verified readiness",
                "Check Activity and the canonical Tunnel prerequisites, then retry.",
            ));
        }
        let mut events = self
            .spawn_owned(ProcessKind::RegularTunnel, command, true, cancellation)
            .await?
            .expect("regular tunnel machine stdout requested");
        self.snapshot.regular_tunnel = Some(RegularTunnelState {
            provider: "openai".to_string(),
            status: RegularTunnelStatus::Starting,
            clipboard_state: "pending".to_string(),
            clipboard_contains: "tunnel_id".to_string(),
            ready_for_chatgpt: false,
        });
        self.snapshot.topology = effective_topology(self.config.topology.as_ref(), true);
        self.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            ExposureReadiness::Starting,
            current.readiness.project.clone(),
        );
        self.activity.push(
            ActivityEventKind::RegularTunnelStarting,
            "regular_tunnel",
            ActivityLevel::Info,
            "Starting the regular OpenAI Secure Tunnel",
        );
        self.publish_snapshot();
        let event_wait = async {
            while let Some(value) = events.recv().await {
                match value.get("event").and_then(Value::as_str) {
                    Some("ready") => return Ok(Some(value)),
                    Some("machine_event_overflow") => return Err(value),
                    _ => {}
                }
            }
            Ok(None)
        };
        let event_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                self.stop_process_until(
                    ProcessKind::RegularTunnel,
                    Deadline::at(deadline.cleanup_deadline(READINESS_CLEANUP_SLACK)),
                ).await;
                return Err(cancelled_error());
            }
            result = tokio::time::timeout_at(deadline.instant(), event_wait) => {
                result
            }
        };
        let event_value = match event_result {
            Ok(Ok(Some(value))) => value,
            Ok(Err(overflow)) => {
                self.stop_process_until(
                    ProcessKind::RegularTunnel,
                    Deadline::at(deadline.cleanup_deadline(READINESS_CLEANUP_SLACK)),
                )
                .await;
                self.snapshot.regular_tunnel = None;
                return Err(machine_event_overflow_error(&overflow));
            }
            Ok(Ok(None)) | Err(_) => {
                let logs = self.process_logs(ProcessKind::RegularTunnel).await;
                self.stop_process_until(
                    ProcessKind::RegularTunnel,
                    Deadline::at(deadline.cleanup_deadline(READINESS_CLEANUP_SLACK)),
                )
                .await;
                self.snapshot.regular_tunnel = Some(RegularTunnelState {
                    provider: "openai".to_string(),
                    status: RegularTunnelStatus::Error,
                    clipboard_state: "unavailable".to_string(),
                    clipboard_contains: "tunnel_id".to_string(),
                    ready_for_chatgpt: false,
                });
                self.snapshot.readiness = aggregate_readiness(
                    ServerReadiness::Ready,
                    RunnerReadiness::Ready,
                    ExposureReadiness::Error,
                    current.readiness.project.clone(),
                );
                apply_regular_tunnel_next_action(&mut self.snapshot, &ExposureReadiness::Error);
                return Err(DesktopError::new(
                    "tunnel_unavailable",
                    "OpenAI Secure Tunnel did not reach verified readiness",
                    "Check Activity and the canonical Tunnel prerequisites, then retry.",
                )
                .with_details(serde_json::json!({
                    "category": "readiness_timeout",
                    "diagnostic_lines": logs,
                })));
            }
        };
        let event: RegularTunnelReadyEvent = match serde_json::from_value(event_value) {
            Ok(event) => event,
            Err(_) => {
                self.stop_process(ProcessKind::RegularTunnel).await;
                self.snapshot.regular_tunnel = None;
                return Err(DesktopError::new(
                    "webcodex_contract_invalid",
                    "Regular Tunnel returned an invalid readiness event",
                    "Verify that Desktop and WebCodex binaries come from the same source baseline.",
                ));
            }
        };
        if event.event != "ready"
            || event.schema_version != 1
            || event.provider != "openai"
            || event.connection.kind != "openai_tunnel"
            || event.connection.clipboard_contains != "tunnel_id"
        {
            self.stop_process(ProcessKind::RegularTunnel).await;
            self.snapshot.regular_tunnel = None;
            return Err(DesktopError::new(
                "webcodex_contract_invalid",
                "Regular Tunnel readiness identity is incomplete",
                "Update Desktop and WebCodex together.",
            ));
        }
        cancellation.check()?;
        let handoff_available = event.connection.clipboard_state == "copied";
        self.snapshot.regular_tunnel = Some(RegularTunnelState {
            provider: event.provider,
            status: RegularTunnelStatus::Ready,
            clipboard_state: event.connection.clipboard_state,
            clipboard_contains: event.connection.clipboard_contains,
            ready_for_chatgpt: event.ready_for_chatgpt && handoff_available,
        });
        self.config.preferred_connection = Some(RegularConnectionPreference::OpenAiTunnel);
        self.save_config().await?;
        // The daemon and clipboard handoff prove only Desktop-managed Tunnel
        // readiness. Actual ChatGPT use is observed independently from canonical
        // Window activity and projected through `chatgpt_activity` on refresh.
        let exposure = if handoff_available {
            ExposureReadiness::LocalReady
        } else {
            ExposureReadiness::Degraded
        };
        self.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            exposure.clone(),
            current.readiness.project.clone(),
        );
        apply_regular_tunnel_next_action(&mut self.snapshot, &exposure);
        self.snapshot.topology = effective_topology(self.config.topology.as_ref(), true);
        self.snapshot.project = project_snapshot(&self.config);
        self.activity.push(
            ActivityEventKind::RegularTunnelReady,
            "regular_tunnel",
            ActivityLevel::Info,
            "Regular OpenAI Secure Tunnel reached local handoff readiness",
        );
        self.get_state().await
    }

    pub async fn stop_regular_tunnel(
        &mut self,
        cancellation: &CancellationContext,
    ) -> DesktopResult<DesktopStateSnapshot> {
        self.stop_process(ProcessKind::RegularTunnel).await;
        self.snapshot.regular_tunnel = None;
        self.snapshot.topology = self.config.topology.clone();
        self.config.preferred_connection = Some(RegularConnectionPreference::NoChatGpt);
        self.save_config().await?;
        self.activity.push(
            ActivityEventKind::RegularTunnelStopped,
            "regular_tunnel",
            ActivityLevel::Info,
            "Regular OpenAI Secure Tunnel stopped",
        );
        if self.config.runtime.is_some() {
            self.refresh_runtime_status(cancellation).await
        } else {
            self.get_state().await
        }
    }

    pub async fn stop_local_runtime(
        &mut self,
        _cancellation: &CancellationContext,
    ) -> DesktopResult<DesktopStateSnapshot> {
        self.stop_process(ProcessKind::RegularTunnel).await;
        self.snapshot.regular_tunnel = None;
        self.stop_process(ProcessKind::LocalRunner).await;
        self.config.runtime_autostart = Some(false);
        self.save_config().await?;
        self.stop_process(ProcessKind::LocalServer).await;
        self.snapshot.topology = self.config.topology.clone();
        let exposure = exposure_readiness(self.config.topology.as_ref());
        self.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Stopped,
            RunnerReadiness::Stopped,
            exposure,
            self.config
                .project
                .as_ref()
                .map(|_| ProjectReadiness::Configured)
                .unwrap_or(ProjectReadiness::None),
        );
        self.activity.push(
            ActivityEventKind::RuntimeStopped,
            "desktop",
            ActivityLevel::Info,
            "Desktop-managed local runtime stopped",
        );
        self.get_state().await
    }

    async fn process_snapshot(&self, kind: ProcessKind) -> Option<crate::process::ProcessSnapshot> {
        self.supervisor.lock().await.snapshot(kind)
    }

    async fn process_logs(&self, kind: ProcessKind) -> Vec<String> {
        self.supervisor.lock().await.logs(kind)
    }

    async fn spawn_owned(
        &self,
        kind: ProcessKind,
        command: std::process::Command,
        machine_stdout: bool,
        cancellation: &CancellationContext,
    ) -> DesktopResult<Option<MachineEventReceiver>> {
        cancellation.check()?;
        let mut supervisor = self.supervisor.lock().await;
        cancellation.check()?;
        supervisor.spawn_owned(kind, command, machine_stdout).await
    }

    async fn stop_process(&self, kind: ProcessKind) {
        self.supervisor.lock().await.stop(kind).await;
    }

    async fn stop_process_until(&self, kind: ProcessKind, deadline: Deadline) {
        self.supervisor
            .lock()
            .await
            .stop_until(kind, deadline)
            .await;
    }

    async fn wait_for_server(
        &mut self,
        server_url: &str,
        env_file: Option<&Path>,
        token_file: Option<&Path>,
        cancellation: &CancellationContext,
        deadline: Deadline,
        cleanup_owned_process: bool,
    ) -> DesktopResult<()> {
        loop {
            cancellation.check()?;
            if deadline.is_elapsed() {
                self.cleanup_readiness_process(
                    ProcessKind::LocalServer,
                    deadline,
                    cleanup_owned_process,
                )
                .await;
                return Err(readiness_timeout_error(
                    "server_unreachable",
                    "WebCodex Service did not become ready",
                    "Check the local Service diagnostics and retry.",
                ));
            }
            if let Some(process) = self.process_snapshot(ProcessKind::LocalServer).await {
                if matches!(process.phase, ProcessPhase::Exited | ProcessPhase::Failed) {
                    self.cleanup_readiness_process(
                        ProcessKind::LocalServer,
                        deadline,
                        cleanup_owned_process,
                    )
                    .await;
                    return Err(DesktopError::new(
                        "server_start_failed",
                        "The Desktop-owned WebCodex Server exited during startup",
                        "Open Activity for safe diagnostics and retry.",
                    ));
                }
            }
            if self
                .adapter
                .server_status_until(
                    Some(server_url),
                    env_file,
                    token_file,
                    cancellation,
                    deadline,
                )
                .await
                .is_ok_and(|status| status.http_reachable)
            {
                return Ok(());
            }
            cancellation.check()?;
            if deadline.is_elapsed() {
                self.cleanup_readiness_process(
                    ProcessKind::LocalServer,
                    deadline,
                    cleanup_owned_process,
                )
                .await;
                return Err(readiness_timeout_error(
                    "server_unreachable",
                    "WebCodex Service did not become ready",
                    "Check the local Service diagnostics and retry.",
                ));
            }
            sleep_or_cancel_until(POLL_INTERVAL, cancellation, deadline).await?;
        }
    }

    async fn wait_for_runner(
        &mut self,
        identity: &ProjectRuntimeIdentity,
        cancellation: &CancellationContext,
        deadline: Deadline,
        cleanup_owned_process: bool,
    ) -> DesktopResult<()> {
        loop {
            cancellation.check()?;
            if deadline.is_elapsed() {
                self.cleanup_readiness_process(
                    ProcessKind::LocalRunner,
                    deadline,
                    cleanup_owned_process,
                )
                .await;
                return Err(readiness_timeout_error(
                    "runner_offline",
                    "Runner did not become connected",
                    "Check Server reachability and Runner diagnostics, then retry.",
                ));
            }
            if let Some(process) = self.process_snapshot(ProcessKind::LocalRunner).await {
                if matches!(process.phase, ProcessPhase::Exited | ProcessPhase::Failed) {
                    self.cleanup_readiness_process(
                        ProcessKind::LocalRunner,
                        deadline,
                        cleanup_owned_process,
                    )
                    .await;
                    return Err(DesktopError::new(
                        "runner_offline",
                        "The Desktop-owned Runner exited while connecting",
                        "Open Activity for safe diagnostics and retry.",
                    ));
                }
            }
            if self
                .adapter
                .runner_ready_until(identity, cancellation, deadline)
                .await
                .unwrap_or(false)
            {
                return Ok(());
            }
            cancellation.check()?;
            if deadline.is_elapsed() {
                self.cleanup_readiness_process(
                    ProcessKind::LocalRunner,
                    deadline,
                    cleanup_owned_process,
                )
                .await;
                return Err(readiness_timeout_error(
                    "runner_offline",
                    "Runner did not become connected",
                    "Check Server reachability and Runner diagnostics, then retry.",
                ));
            }
            sleep_or_cancel_until(POLL_INTERVAL, cancellation, deadline).await?;
        }
    }

    async fn wait_for_project(
        &mut self,
        identity: &ProjectRuntimeIdentity,
        cancellation: &CancellationContext,
    ) -> DesktopResult<()> {
        let deadline = Deadline::after(PROJECT_READY_TIMEOUT);
        loop {
            cancellation.check()?;
            if deadline.is_elapsed() {
                return Err(project_not_loaded_error());
            }
            if self
                .adapter
                .project_ready_until(identity, cancellation, deadline)
                .await
                .unwrap_or(false)
            {
                return Ok(());
            }
            cancellation.check()?;
            if deadline.is_elapsed() {
                return Err(project_not_loaded_error());
            }
            sleep_or_cancel_until(POLL_INTERVAL, cancellation, deadline).await?;
        }
    }

    async fn cleanup_readiness_process(
        &self,
        kind: ProcessKind,
        deadline: Deadline,
        cleanup_owned_process: bool,
    ) {
        if cleanup_owned_process {
            self.stop_process_until(
                kind,
                Deadline::at(deadline.cleanup_deadline(READINESS_CLEANUP_SLACK)),
            )
            .await;
        }
    }

    async fn store_identity(
        &mut self,
        project: &ProjectSelection,
        identity: &ProjectRuntimeIdentity,
        server_env_file: Option<PathBuf>,
        runner_client_id: Option<String>,
    ) -> DesktopResult<()> {
        let mut project = project.clone();
        project.runtime_project_id = Some(identity.runtime_project_id.clone());
        self.config.project = Some(project.clone());
        self.config.runtime = Some(StoredRuntime {
            server_url: identity.server_url.clone(),
            server_env_file,
            runner_config: Some(identity.runner_config.clone()),
            user_token_file: Some(identity.user_token_file.clone()),
            runner_client_id,
            project_id: Some(identity.project_id.clone()),
            runtime_project_id: Some(identity.runtime_project_id.clone()),
        });
        self.snapshot.project = Some(project);
        self.save_config().await
    }

    async fn save_config(&self) -> DesktopResult<()> {
        tokio::fs::create_dir_all(&self.data_dir)
            .await
            .map_err(|_| {
                DesktopError::new(
                    "desktop_state_unavailable",
                    "Desktop cannot create its app-data directory",
                    "Check local filesystem permissions and retry.",
                )
            })?;
        let encoded = serde_json::to_vec_pretty(&self.config).map_err(|_| {
            DesktopError::new(
                "desktop_state_invalid",
                "Desktop could not encode its non-secret runtime state",
                "Retry the setup operation.",
            )
        })?;
        let config_path = self.config_path.clone();
        tokio::task::spawn_blocking(move || save_config_atomically(&config_path, &encoded))
            .await
            .map_err(|_| desktop_state_unavailable("Desktop state persistence worker stopped"))??;
        Ok(())
    }
}

async fn sleep_or_cancel_until(
    duration: Duration,
    cancellation: &CancellationContext,
    deadline: Deadline,
) -> DesktopResult<()> {
    cancellation.check()?;
    if deadline.is_elapsed() {
        return Ok(());
    }
    let wake_at = std::cmp::min(deadline.instant(), tokio::time::Instant::now() + duration);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(cancelled_error()),
        _ = tokio::time::sleep_until(wake_at) => Ok(()),
    }
}

fn readiness_timeout_error(
    code: &'static str,
    message: &'static str,
    action: &'static str,
) -> DesktopError {
    DesktopError::new(code, message, action)
        .with_details(serde_json::json!({ "category": "readiness_timeout" }))
}

// These are Desktop packaging defaults, not process-environment overrides.
// The Server loads its env file only into keys that are absent from the process
// environment, so the effective precedence remains built-in defaults < this
// config file < explicit environment variables.
fn ensure_desktop_server_defaults(path: &Path) -> DesktopResult<()> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        desktop_state_unavailable("Desktop could not inspect its local Server configuration")
            .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
    })?;
    if !metadata.is_file() || metadata.len() > DESKTOP_SERVER_ENV_MAX_BYTES {
        return Err(desktop_state_unavailable(
            "Desktop local Server configuration is not a bounded regular file",
        ));
    }
    let content = std::fs::read_to_string(path).map_err(|error| {
        desktop_state_unavailable("Desktop could not read its local Server configuration")
            .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
    })?;
    let has_key = |key: &str| {
        content.lines().any(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line).trim();
            line.split_once('=')
                .is_some_and(|(candidate, _)| candidate.trim() == key)
        })
    };
    let mut additions = Vec::new();
    if !has_key("WEBCODEX_MCP_MODEL_SURFACE") {
        additions.push(format!(
            "WEBCODEX_MCP_MODEL_SURFACE={DESKTOP_MCP_MODEL_SURFACE}"
        ));
    }
    if !has_key("WEBCODEX_MCP_COMPACT_SCHEMAS") {
        additions.push(format!(
            "WEBCODEX_MCP_COMPACT_SCHEMAS={DESKTOP_MCP_COMPACT_SCHEMAS}"
        ));
    }
    if additions.is_empty() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| {
            desktop_state_unavailable("Desktop could not update its local Server configuration")
                .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
        })?;
    if !content.is_empty() && !content.ends_with('\n') {
        file.write_all(b"\n").map_err(|error| {
            desktop_state_unavailable("Desktop could not update its local Server configuration")
                .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
        })?;
    }
    for addition in additions {
        writeln!(file, "{addition}").map_err(|error| {
            desktop_state_unavailable("Desktop could not update its local Server configuration")
                .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
        })?;
    }
    file.flush().map_err(|error| {
        desktop_state_unavailable("Desktop could not update its local Server configuration")
            .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
    })?;
    Ok(())
}

fn machine_event_overflow_error(event: &Value) -> DesktopError {
    DesktopError::new(
        "machine_event_overflow",
        "Desktop could not retain every critical machine-readiness event",
        "Retry the operation and inspect Activity if the child keeps emitting excessive machine events.",
    )
    .with_details(serde_json::json!({
        "category": "machine_event_overflow",
        "dropped_critical": event
            .get("dropped_critical")
            .and_then(Value::as_u64)
            .unwrap_or(1),
    }))
}

fn local_enrollment_directory(data_dir: &Path, config: &StoredDesktopConfig) -> PathBuf {
    // Two reusable slots bound local credential storage while keeping login's
    // --overwrite away from the currently committed recovery identity. Resolve
    // native path aliases (notably /var on macOS) before comparing paths.
    let root = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf())
        .join("local-connections");
    let first = root.join("a");
    let saved_runner = config
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.runner_config.as_ref());
    if saved_runner.is_some_and(|path| {
        path.canonicalize()
            .unwrap_or_else(|_| path.clone())
            .starts_with(&first)
    }) {
        root.join("b")
    } else {
        first
    }
}

fn project_snapshot(config: &StoredDesktopConfig) -> Option<ProjectSelection> {
    let mut project = config.project.clone()?;
    if identity_from_config(config).is_none() {
        project.runtime_project_id = None;
    }
    Some(project)
}

fn stored_runner_client_id(config: &StoredDesktopConfig) -> Option<String> {
    let runtime = config.runtime.as_ref()?;
    if let Some(client_id) = runtime
        .runner_client_id
        .as_deref()
        .map(str::trim)
        .filter(|client_id| !client_id.is_empty())
    {
        return Some(client_id.to_string());
    }
    // Pre-migration Desktop state did not persist client_id separately. Recover
    // it from the exact runtime Project identity instead of accepting whatever
    // client_id happens to be present in runner.toml during the first upgrade.
    let project_id = runtime.project_id.as_deref()?.trim();
    let runtime_project_id = runtime.runtime_project_id.as_deref()?.trim();
    if project_id.is_empty() || runtime_project_id.is_empty() {
        return None;
    }
    let suffix = format!(":{project_id}");
    runtime_project_id
        .strip_prefix("agent:")?
        .strip_suffix(&suffix)
        .map(str::trim)
        .filter(|client_id| !client_id.is_empty())
        .map(str::to_string)
}

fn identity_from_config(config: &StoredDesktopConfig) -> Option<ProjectRuntimeIdentity> {
    let runtime = config.runtime.as_ref()?;
    let project = config.project.as_ref()?;
    let runner_config = runtime.runner_config.clone()?;
    let user_token_file = runtime.user_token_file.clone()?;
    let project_id = runtime.project_id.clone()?.trim().to_string();
    let runtime_project_id = runtime.runtime_project_id.clone()?.trim().to_string();
    if project_id.is_empty()
        || runtime_project_id.is_empty()
        || !runner_config.is_file()
        || !user_token_file.is_file()
    {
        return None;
    }
    Some(ProjectRuntimeIdentity {
        project_id,
        runtime_project_id,
        project_path: project.path.clone(),
        runner_config,
        user_token_file,
        server_url: runtime.server_url.clone(),
    })
}

#[derive(Debug)]
enum StoredConfigFile {
    Missing,
    Valid {
        config: StoredDesktopConfig,
        bytes: Vec<u8>,
    },
    Corrupt,
}

fn load_config(path: &Path, activity: &ActivityLog) -> DesktopResult<StoredDesktopConfig> {
    let backup_path = desktop_state_backup_path(path);
    match read_stored_config(path)? {
        StoredConfigFile::Valid { config, .. } => Ok(config),
        StoredConfigFile::Missing => match read_stored_config(&backup_path)? {
            StoredConfigFile::Missing => Ok(StoredDesktopConfig::default()),
            StoredConfigFile::Valid { config, bytes } => {
                recover_config_from_backup(path, &bytes, activity)?;
                Ok(config)
            }
            StoredConfigFile::Corrupt => Err(desktop_state_corrupt()),
        },
        StoredConfigFile::Corrupt => match read_stored_config(&backup_path)? {
            StoredConfigFile::Valid { config, bytes } => {
                recover_config_from_backup(path, &bytes, activity)?;
                Ok(config)
            }
            StoredConfigFile::Missing | StoredConfigFile::Corrupt => Err(desktop_state_corrupt()),
        },
    }
}

fn recover_config_from_backup(
    primary_path: &Path,
    bytes: &[u8],
    activity: &ActivityLog,
) -> DesktopResult<()> {
    write_atomic_file(primary_path, bytes).map_err(|error| {
        desktop_state_unavailable("Desktop could not restore the previous known-good state")
            .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
    })?;
    activity.push(
        ActivityEventKind::StateRecovered,
        "desktop_state",
        ActivityLevel::Warning,
        "Recovered Desktop state from the previous known-good snapshot",
    );
    Ok(())
}

fn read_stored_config(path: &Path) -> DesktopResult<StoredConfigFile> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(StoredConfigFile::Missing)
        }
        Err(error) => {
            return Err(
                desktop_state_unavailable("Desktop could not inspect its saved state")
                    .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) })),
            )
        }
    };
    if !metadata.is_file() || metadata.len() > DESKTOP_STATE_MAX_BYTES {
        return Ok(StoredConfigFile::Corrupt);
    }
    let bytes = std::fs::read(path).map_err(|error| {
        desktop_state_unavailable("Desktop could not read its saved state")
            .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
    })?;
    match serde_json::from_slice::<StoredDesktopConfig>(&bytes) {
        Ok(config) => Ok(StoredConfigFile::Valid { config, bytes }),
        Err(_) => Ok(StoredConfigFile::Corrupt),
    }
}

fn save_config_atomically(path: &Path, encoded: &[u8]) -> DesktopResult<()> {
    if encoded.len() as u64 > DESKTOP_STATE_MAX_BYTES {
        return Err(DesktopError::new(
            "desktop_state_invalid",
            "Desktop state exceeded its bounded persistence size",
            "Retry after reducing the saved Desktop configuration.",
        ));
    }

    if let StoredConfigFile::Valid { bytes, .. } = read_stored_config(path)? {
        let backup = desktop_state_backup_path(path);
        write_atomic_file(&backup, &bytes).map_err(|error| {
            desktop_state_unavailable("Desktop could not preserve the previous known-good state")
                .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
        })?;
    }

    write_atomic_file(path, encoded).map_err(|error| {
        desktop_state_unavailable("Desktop could not persist its non-secret runtime state")
            .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
    })
}

fn desktop_state_backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("desktop-state.json");
    path.with_file_name(format!("{file_name}.bak"))
}

fn state_temp_path(path: &Path) -> PathBuf {
    let id = NEXT_STATE_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("desktop-state.json");
    path.with_file_name(format!(".{file_name}.{}.{}.tmp", std::process::id(), id))
}

fn write_atomic_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_file_with_hook(path, bytes, |_| Ok(()))
}

fn write_atomic_file_with_hook<F>(path: &Path, bytes: &[u8], before_replace: F) -> io::Result<()>
where
    F: FnOnce(&Path) -> io::Result<()>,
{
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temp_path = state_temp_path(path);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        before_replace(&temp_path)?;
        atomic_replace(&temp_path, path)?;
        sync_state_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_state_directory(path: &Path) -> io::Result<()> {
    let directory = File::open(path)?;
    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::Unsupported
                || error.raw_os_error() == Some(libc::EINVAL) =>
        {
            // Some Unix filesystems (notably macOS variants) do not support
            // directory fsync. The file itself has already been synced and the
            // same-directory rename is atomic, so treat this specific platform
            // limitation as best-effort durability rather than a false save
            // failure after replacement has already succeeded.
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn sync_state_directory(_path: &Path) -> io::Result<()> {
    // Windows uses MOVEFILE_WRITE_THROUGH for the replacement. Opening a
    // directory for FlushFileBuffers would require broader sharing semantics
    // than the app-data policy needs here.
    Ok(())
}

fn desktop_state_corrupt() -> DesktopError {
    DesktopError::new(
        "desktop_state_corrupt",
        "Desktop saved state is corrupt and no valid recovery snapshot is available",
        "Restore or remove the Desktop state files explicitly, then restart WebCodex Desktop.",
    )
    .with_details(serde_json::json!({ "category": "state_corrupt" }))
}

fn desktop_state_unavailable(message: &'static str) -> DesktopError {
    DesktopError::new(
        "desktop_state_unavailable",
        message,
        "Check local app-data permissions and retry.",
    )
}

fn reserve_loopback_address() -> DesktopResult<String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| {
        DesktopError::new(
            "local_port_unavailable",
            "Desktop could not reserve a loopback port for WebCodex",
            "Check local networking and retry.",
        )
    })?;
    let address = listener.local_addr().map_err(|_| {
        DesktopError::new(
            "local_port_unavailable",
            "Desktop could not inspect the reserved loopback port",
            "Retry setup.",
        )
    })?;
    Ok(address.to_string())
}

fn regular_tunnel_exposure(
    state: &mut Option<RegularTunnelState>,
    process_active: bool,
) -> Option<ExposureReadiness> {
    let state = state.as_mut()?;
    if !process_active {
        state.status = RegularTunnelStatus::Error;
        state.ready_for_chatgpt = false;
        return Some(ExposureReadiness::Error);
    }
    Some(match state.status {
        RegularTunnelStatus::Starting => ExposureReadiness::Starting,
        RegularTunnelStatus::Ready if state.ready_for_chatgpt => ExposureReadiness::LocalReady,
        RegularTunnelStatus::Ready => ExposureReadiness::Degraded,
        RegularTunnelStatus::Error => ExposureReadiness::Error,
    })
}

fn apply_regular_tunnel_next_action(
    snapshot: &mut DesktopStateSnapshot,
    exposure: &ExposureReadiness,
) {
    if !snapshot.readiness.runtime_ready {
        return;
    }
    match exposure {
        ExposureReadiness::Error => {
            snapshot.readiness.next_action_kind =
                Some(ReadinessNextActionKind::RestartSecureTunnel);
            snapshot.readiness.next_action = Some("Restart the secure tunnel.".to_string());
        }
        ExposureReadiness::LocalReady => {
            snapshot.readiness.summary_kind = ReadinessSummaryKind::TunnelReadyWaitingForChatGpt;
            snapshot.readiness.summary =
                "OpenAI Secure Tunnel is ready; waiting for ChatGPT to connect".to_string();
            snapshot.readiness.next_action_kind = Some(ReadinessNextActionKind::CheckConnection);
            snapshot.readiness.next_action = Some(
                "Connect the Tunnel in ChatGPT, then verify it with one real project read."
                    .to_string(),
            );
        }
        ExposureReadiness::Degraded => {
            snapshot.readiness.next_action_kind =
                Some(ReadinessNextActionKind::RestoreClipboardHandoff);
            snapshot.readiness.next_action = Some(
                "Restore clipboard access, then restart the secure tunnel handoff.".to_string(),
            );
        }
        _ => {}
    }
}

fn effective_topology(
    configured: Option<&RuntimeTopology>,
    regular_tunnel_selected: bool,
) -> Option<RuntimeTopology> {
    let mut topology = configured.cloned()?;
    if regular_tunnel_selected
        && topology.experience == Experience::Full
        && matches!(topology.server, ServerTopology::Local)
    {
        topology.exposure = Exposure::OpenAiTunnel;
    }
    Some(topology)
}

fn exposure_readiness(topology: Option<&RuntimeTopology>) -> ExposureReadiness {
    match topology.map(|topology| &topology.exposure) {
        Some(Exposure::None) => ExposureReadiness::LocalReady,
        // An HTTPS origin is a configured route, not evidence that the MCP
        // endpoint plus ChatGPT authentication/handoff is externally usable.
        Some(Exposure::ExistingHttps { .. }) => ExposureReadiness::Unknown,
        Some(Exposure::Cloudflare | Exposure::OpenAiTunnel) => ExposureReadiness::Unknown,
        None => ExposureReadiness::Unknown,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EffectiveTunnelProxy {
    url: Option<String>,
    source: &'static str,
    detected_url: Option<String>,
}

fn validate_tunnel_proxy_url(value: &str) -> DesktopResult<String> {
    crate::platform::normalize_proxy_server(value).ok_or_else(|| {
        DesktopError::new(
            "tunnel_proxy_invalid",
            "The Tunnel proxy must be an HTTP or HTTPS proxy URL without embedded credentials",
            "Use a value such as http://127.0.0.1:7890, or choose automatic proxy detection.",
        )
    })
}

fn environment_tunnel_proxy() -> Option<String> {
    ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .and_then(|value| crate::platform::normalize_proxy_server(&value))
        })
}

fn effective_tunnel_proxy(config: &TunnelProxyConfig) -> DesktopResult<EffectiveTunnelProxy> {
    let system = crate::platform::system_http_proxy_candidate();
    let detected_url = system.as_ref().map(|candidate| candidate.url.clone());
    match config.mode {
        TunnelProxyMode::Direct => Ok(EffectiveTunnelProxy {
            url: None,
            source: "direct",
            detected_url,
        }),
        TunnelProxyMode::Custom => Ok(EffectiveTunnelProxy {
            url: Some(validate_tunnel_proxy_url(
                config.custom_url.as_deref().unwrap_or(""),
            )?),
            source: "custom",
            detected_url,
        }),
        TunnelProxyMode::Auto => {
            if let Some(url) = environment_tunnel_proxy() {
                return Ok(EffectiveTunnelProxy {
                    url: Some(url),
                    source: "environment",
                    detected_url,
                });
            }
            if let Some(candidate) = system {
                if candidate.enabled || crate::platform::proxy_is_loopback(&candidate.url) {
                    return Ok(EffectiveTunnelProxy {
                        url: Some(candidate.url),
                        source: if candidate.enabled {
                            "windows_system"
                        } else {
                            "windows_loopback_candidate"
                        },
                        detected_url,
                    });
                }
            }
            Ok(EffectiveTunnelProxy {
                url: None,
                source: "direct",
                detected_url,
            })
        }
    }
}

fn runtime_autostart(config: &StoredDesktopConfig) -> bool {
    config.runtime_autostart.unwrap_or_else(|| {
        config.runtime.is_some()
            && config
                .topology
                .as_ref()
                .is_some_and(|topology| topology.experience == Experience::Full)
    })
}

fn default_management_project_dir(data_dir: &Path, resource_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let _ = data_dir;
        // The NSIS package is current-user scoped. Use the actual installation
        // directory as the default management Project so a fresh Desktop is
        // immediately manageable without asking the user to choose an unrelated
        // source checkout first. This intentionally grants the Project the same
        // install-directory authority the user has requested for Desktop
        // configuration and maintenance.
        return resource_dir.to_path_buf();
    }
    #[cfg(not(windows))]
    {
        // A macOS resource directory lives inside the signed app bundle and is
        // not a mutable workspace. Keep the same management-project semantics
        // in the per-user Desktop data directory there.
        let _ = resource_dir;
        data_dir.join("workspace")
    }
}

fn preferred_connection(config: &StoredDesktopConfig) -> RegularConnectionPreference {
    config.preferred_connection.unwrap_or_default()
}

fn apply_config_projection(snapshot: &mut DesktopStateSnapshot, config: &StoredDesktopConfig) {
    snapshot.runtime_autostart = runtime_autostart(config);
    snapshot.preferred_connection = preferred_connection(config);
    snapshot.tunnel_proxy = match effective_tunnel_proxy(&config.tunnel_proxy) {
        Ok(proxy) => TunnelProxySnapshot {
            mode: config.tunnel_proxy.mode,
            custom_url: config.tunnel_proxy.custom_url.clone(),
            effective_source: proxy.source.to_string(),
            effective_url: proxy.url,
            detected_url: proxy.detected_url,
        },
        Err(_) => TunnelProxySnapshot {
            mode: config.tunnel_proxy.mode,
            custom_url: config.tunnel_proxy.custom_url.clone(),
            effective_source: "invalid_custom".to_string(),
            effective_url: None,
            detected_url: crate::platform::system_http_proxy_candidate()
                .map(|candidate| candidate.url),
        },
    };
}

fn openai_tunnel_configuration() -> OpenAiTunnelConfigSnapshot {
    openai_tunnel_configuration_from_presence(
        environment_variable_present("CONTROL_PLANE_TUNNEL_ID"),
        environment_variable_present("CONTROL_PLANE_API_KEY"),
    )
}

fn openai_tunnel_configuration_from_presence(
    tunnel_id_present: bool,
    api_key_present: bool,
) -> OpenAiTunnelConfigSnapshot {
    OpenAiTunnelConfigSnapshot {
        tunnel_id_present,
        api_key_present,
    }
}

fn environment_variable_present(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn apply_openai_tunnel_configuration(snapshot: &mut DesktopStateSnapshot) {
    let configuration = openai_tunnel_configuration();
    snapshot.openai_tunnel_configured = configuration.is_configured();
    snapshot.openai_tunnel_config = configuration;
}

fn same_server(left: &str, right: &str) -> bool {
    left.trim_end_matches('/')
        .eq_ignore_ascii_case(right.trim_end_matches('/'))
}

#[cfg(test)]
fn same_project(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_management_project_uses_safe_platform_location() {
        let data = PathBuf::from(r"C:\Users\test\AppData\Local\WebCodex");
        let resources = PathBuf::from(r"D:\Apps\WebCodex Desktop");
        let project = default_management_project_dir(&data, &resources);
        #[cfg(windows)]
        assert_eq!(project, resources);
        #[cfg(not(windows))]
        assert_eq!(project, data.join("workspace"));
    }

    #[test]
    fn desktop_server_defaults_append_missing_values_and_preserve_explicit_config() {
        let dir = unique_state_dir("server-defaults-explicit");
        std::fs::create_dir_all(&dir).unwrap();
        let env_file = dir.join("webcodex.env");
        std::fs::write(
            &env_file,
            "WEBCODEX_TOKEN=secret\nWEBCODEX_MCP_COMPACT_SCHEMAS=false\n",
        )
        .unwrap();

        ensure_desktop_server_defaults(&env_file).unwrap();
        let once = std::fs::read_to_string(&env_file).unwrap();
        assert!(once.contains("WEBCODEX_TOKEN=secret\n"));
        assert!(once.contains("WEBCODEX_MCP_COMPACT_SCHEMAS=false\n"));
        assert!(once.contains("WEBCODEX_MCP_MODEL_SURFACE=adaptive-runtime-v1\n"));
        assert_eq!(once.matches("WEBCODEX_MCP_COMPACT_SCHEMAS=").count(), 1);

        ensure_desktop_server_defaults(&env_file).unwrap();
        assert_eq!(std::fs::read_to_string(&env_file).unwrap(), once);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn desktop_server_defaults_add_both_values_to_fresh_server_env() {
        let dir = unique_state_dir("server-defaults-fresh");
        std::fs::create_dir_all(&dir).unwrap();
        let env_file = dir.join("webcodex.env");
        std::fs::write(&env_file, "WEBCODEX_ADDR=127.0.0.1:12345").unwrap();

        ensure_desktop_server_defaults(&env_file).unwrap();
        let content = std::fs::read_to_string(&env_file).unwrap();
        assert!(content.starts_with("WEBCODEX_ADDR=127.0.0.1:12345\n"));
        assert!(content.contains("WEBCODEX_MCP_MODEL_SURFACE=adaptive-runtime-v1\n"));
        assert!(content.contains("WEBCODEX_MCP_COMPACT_SCHEMAS=true\n"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn unique_state_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "webcodex-desktop-state-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    fn test_stored_config(label: &str) -> StoredDesktopConfig {
        StoredDesktopConfig {
            topology: None,
            project: Some(ProjectSelection {
                path: format!("/{label}"),
                allowed_root: "/".to_string(),
                is_git_repository: false,
                runtime_project_id: None,
            }),
            runtime_autostart: None,
            preferred_connection: None,
            tunnel_proxy: TunnelProxyConfig::default(),
            runtime: None,
        }
    }

    #[test]
    fn chatgpt_activity_observation_is_fenced_to_the_captured_project_identity() {
        let data_dir = unique_state_dir("chatgpt-activity-fence");
        let resource_dir = data_dir.join("resources");
        std::fs::create_dir_all(&resource_dir).unwrap();
        let runner_config = data_dir.join("runner.toml");
        let user_token_file = data_dir.join("user-token");
        std::fs::write(&runner_config, "fixture").unwrap();
        std::fs::write(&user_token_file, "fixture").unwrap();

        let mut core = DesktopCore::new(data_dir.clone(), resource_dir).unwrap();
        core.snapshot.readiness.runtime_ready = true;
        core.config.project = Some(ProjectSelection {
            path: data_dir.join("project-b").to_string_lossy().to_string(),
            allowed_root: data_dir.to_string_lossy().to_string(),
            is_git_repository: false,
            runtime_project_id: Some("agent:desktop:project-b".to_string()),
        });
        core.config.runtime = Some(StoredRuntime {
            server_url: "http://127.0.0.1:8080".to_string(),
            server_env_file: None,
            runner_config: Some(runner_config),
            user_token_file: Some(user_token_file),
            runner_client_id: Some("desktop".to_string()),
            project_id: Some("project-b".to_string()),
            runtime_project_id: Some("agent:desktop:project-b".to_string()),
        });

        let current_identity = identity_from_config(&core.config).expect("current project identity");
        let mut stale_identity = current_identity.clone();
        stale_identity.project_id = "project-a".to_string();
        stale_identity.runtime_project_id = "agent:desktop:project-a".to_string();

        let stale = core
            .apply_chatgpt_activity_observation(&stale_identity, Some(1234))
            .unwrap();
        assert!(
            stale.chatgpt_activity.is_none(),
            "an observation captured for the old Project must not cross a Project switch"
        );

        let current = core
            .apply_chatgpt_activity_observation(&current_identity, Some(5678))
            .unwrap();
        assert_eq!(
            current
                .chatgpt_activity
                .as_ref()
                .and_then(|activity| activity.last_meaningful_activity_at_ms),
            Some(5678)
        );
        assert!(current.chatgpt_activity.unwrap().observed);
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn staging_a_project_scope_clears_prior_chatgpt_evidence() {
        let data_dir = unique_state_dir("chatgpt-project-scope-reset");
        let resource_dir = data_dir.join("resources");
        std::fs::create_dir_all(&resource_dir).unwrap();
        let mut core = DesktopCore::new(data_dir.clone(), resource_dir).unwrap();
        core.snapshot.chatgpt_activity = Some(ChatGptActivitySnapshot {
            observed: true,
            last_meaningful_activity_at_ms: Some(1234),
        });

        let project = ProjectSelection {
            path: data_dir.join("project-b").to_string_lossy().to_string(),
            allowed_root: data_dir.to_string_lossy().to_string(),
            is_git_repository: false,
            runtime_project_id: None,
        };
        core.stage_project_scope(project.clone());

        assert_eq!(core.snapshot.project, Some(project));
        assert!(core.snapshot.chatgpt_activity.is_none());
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn legacy_project_refresh_never_claims_external_or_inactive_runner_ownership() {
        assert!(!can_refresh_legacy_runner(None));
        assert!(!can_refresh_legacy_runner(Some(
            crate::process::ProcessSnapshot {
                kind: ProcessKind::LocalRunner,
                phase: ProcessPhase::Running,
                pid: Some(42),
                exit_code: None,
                owned_by_desktop: false,
            }
        )));
        assert!(!can_refresh_legacy_runner(Some(
            crate::process::ProcessSnapshot {
                kind: ProcessKind::LocalRunner,
                phase: ProcessPhase::Exited,
                pid: Some(43),
                exit_code: Some(0),
                owned_by_desktop: true,
            }
        )));
        assert!(can_refresh_legacy_runner(Some(
            crate::process::ProcessSnapshot {
                kind: ProcessKind::LocalRunner,
                phase: ProcessPhase::Running,
                pid: Some(44),
                exit_code: None,
                owned_by_desktop: true,
            }
        )));
    }

    #[test]
    fn legacy_runtime_project_identity_recovers_runner_client_id_without_project_coupling() {
        let mut config = test_stored_config("legacy");
        config.runtime = Some(StoredRuntime {
            server_url: "https://example.test".to_string(),
            server_env_file: None,
            runner_config: None,
            user_token_file: None,
            runner_client_id: None,
            project_id: Some("repo".to_string()),
            runtime_project_id: Some("agent:desktop:runner:repo".to_string()),
        });
        assert_eq!(
            stored_runner_client_id(&config).as_deref(),
            Some("desktop:runner")
        );
        config.runtime.as_mut().unwrap().runner_client_id = Some("explicit-runner".to_string());
        assert_eq!(
            stored_runner_client_id(&config).as_deref(),
            Some("explicit-runner")
        );
    }

    #[test]
    fn local_enrollment_preserves_saved_files_and_reuses_only_the_inactive_slot() {
        let dir = unique_state_dir("enrollment-slots");
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = test_stored_config("previous");
        let first = local_enrollment_directory(&dir, &config);
        let saved_runner = first.join("server").join("runner.toml");
        std::fs::create_dir_all(saved_runner.parent().unwrap()).unwrap();
        std::fs::write(&saved_runner, "previous project fixture").unwrap();
        config.runtime = Some(StoredRuntime {
            server_url: "http://127.0.0.1:7890".into(),
            server_env_file: None,
            runner_config: Some(saved_runner.clone()),
            user_token_file: None,
            runner_client_id: None,
            project_id: None,
            runtime_project_id: None,
        });

        let candidate = local_enrollment_directory(&dir, &config);
        assert_ne!(candidate, first);
        let candidate_runner = candidate.join("server").join("runner.toml");
        std::fs::create_dir_all(candidate_runner.parent().unwrap()).unwrap();
        std::fs::write(&candidate_runner, "replacement project fixture").unwrap();
        assert_eq!(
            std::fs::read_to_string(saved_runner).unwrap(),
            "previous project fixture"
        );
        // A failed activation retries the candidate, never the saved connection.
        assert_eq!(local_enrollment_directory(&dir, &config), candidate);
        config.runtime.as_mut().unwrap().runner_config = Some(candidate_runner);
        assert_eq!(local_enrollment_directory(&dir, &config), first);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_save_interruption_keeps_prior_valid_state() {
        let dir = unique_state_dir("interrupted-save");
        std::fs::create_dir_all(&dir).expect("create state fixture dir");
        let path = dir.join("desktop-state.json");
        let previous = test_stored_config("previous");
        let replacement = test_stored_config("replacement");
        let previous_bytes = serde_json::to_vec_pretty(&previous).unwrap();
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).unwrap();
        write_atomic_file(&path, &previous_bytes).expect("write previous state");

        let interrupted = write_atomic_file_with_hook(&path, &replacement_bytes, |_| {
            Err(io::Error::other("injected interruption before replace"))
        });
        assert!(interrupted.is_err());
        match read_stored_config(&path).expect("read state after interruption") {
            StoredConfigFile::Valid { config, .. } => assert_eq!(config, previous),
            other => panic!("previous state was not preserved: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn atomic_save_preserves_previous_known_good_backup() {
        let dir = unique_state_dir("known-good-backup");
        std::fs::create_dir_all(&dir).expect("create state fixture dir");
        let path = dir.join("desktop-state.json");
        let previous = test_stored_config("previous");
        let replacement = test_stored_config("replacement");
        save_config_atomically(&path, &serde_json::to_vec_pretty(&previous).unwrap())
            .expect("initial atomic save");
        save_config_atomically(&path, &serde_json::to_vec_pretty(&replacement).unwrap())
            .expect("replacement atomic save");

        match read_stored_config(&path).expect("read primary") {
            StoredConfigFile::Valid { config, .. } => assert_eq!(config, replacement),
            other => panic!("replacement state was not valid: {other:?}"),
        }
        match read_stored_config(&desktop_state_backup_path(&path)).expect("read backup") {
            StoredConfigFile::Valid { config, .. } => assert_eq!(config, previous),
            other => panic!("previous snapshot was not valid: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_primary_with_valid_backup_recovers_explicitly() {
        let dir = unique_state_dir("recover-backup");
        std::fs::create_dir_all(&dir).expect("create state fixture dir");
        let path = dir.join("desktop-state.json");
        let expected = test_stored_config("recovered");
        write_atomic_file(
            &desktop_state_backup_path(&path),
            &serde_json::to_vec_pretty(&expected).unwrap(),
        )
        .expect("write valid backup");
        std::fs::write(&path, b"{corrupt-primary").expect("write corrupt primary");
        let activity = ActivityLog::default();

        let recovered = load_config(&path, &activity).expect("recover from backup");
        assert_eq!(recovered, expected);
        assert!(matches!(
            read_stored_config(&path).expect("read restored primary"),
            StoredConfigFile::Valid { .. }
        ));
        assert!(activity.snapshot().iter().any(|entry| {
            entry.event_kind == ActivityEventKind::StateRecovered && entry.source == "desktop_state"
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_primary_and_backup_returns_explicit_error() {
        let dir = unique_state_dir("both-corrupt");
        std::fs::create_dir_all(&dir).expect("create state fixture dir");
        let path = dir.join("desktop-state.json");
        std::fs::write(&path, b"{corrupt-primary").expect("write corrupt primary");
        std::fs::write(desktop_state_backup_path(&path), b"{corrupt-backup")
            .expect("write corrupt backup");

        let error = load_config(&path, &ActivityLog::default())
            .expect_err("both corrupt copies must fail closed");
        assert_eq!(error.code, "desktop_state_corrupt");
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("category"))
                .and_then(Value::as_str),
            Some("state_corrupt")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stored_runtime_contains_paths_not_credentials() {
        let runtime = StoredRuntime {
            server_url: "https://example.com".to_string(),
            server_env_file: Some(PathBuf::from("webcodex.env")),
            runner_config: Some(PathBuf::from("runner.toml")),
            user_token_file: Some(PathBuf::from("user-token")),
            runner_client_id: Some("desktop-runner".to_string()),
            project_id: Some("project".to_string()),
            runtime_project_id: Some("agent:desktop:project".to_string()),
        };
        let json = serde_json::to_string(&runtime).unwrap();
        assert!(!json.contains("wc_pat_"));
        assert!(!json.contains("wc_agent_"));
        assert!(!json.contains("CONTROL_PLANE_API_KEY"));
    }

    #[test]
    fn legacy_full_runtime_defaults_to_autostart_but_explicit_stop_is_preserved() {
        let mut config = test_stored_config("resume");
        config.topology = Some(RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Local,
            runner: RunnerTopology::Local,
            exposure: Exposure::None,
            enrollment: Enrollment::ManagedPairing,
        });
        config.runtime = Some(StoredRuntime {
            server_url: "http://127.0.0.1:58208".to_string(),
            server_env_file: None,
            runner_config: None,
            user_token_file: None,
            runner_client_id: None,
            project_id: None,
            runtime_project_id: None,
        });
        assert!(runtime_autostart(&config));

        config.runtime_autostart = Some(false);
        assert!(!runtime_autostart(&config));
    }

    #[test]
    fn explicit_tunnel_proxy_is_bounded_and_direct_mode_clears_routing() {
        let custom = TunnelProxyConfig {
            mode: TunnelProxyMode::Custom,
            custom_url: Some("http://127.0.0.1:7890".to_string()),
        };
        let effective = effective_tunnel_proxy(&custom).expect("valid custom proxy");
        assert_eq!(effective.url.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(effective.source, "custom");

        let direct = effective_tunnel_proxy(&TunnelProxyConfig {
            mode: TunnelProxyMode::Direct,
            custom_url: Some("http://ignored.example.test:8080".to_string()),
        })
        .expect("direct mode");
        assert_eq!(direct.url, None);
        assert_eq!(direct.source, "direct");

        assert!(validate_tunnel_proxy_url("http://user:secret@127.0.0.1:7890").is_err());
    }

    #[test]
    fn failed_runtime_resume_restores_the_last_published_state() {
        let data_dir = unique_state_dir("resume-failure-reconcile");
        let mut core = DesktopCore::new(data_dir.clone(), data_dir.join("resources"))
            .expect("create Desktop core");
        let mut baseline_snapshot = DesktopStateSnapshot::default();
        baseline_snapshot.topology = Some(RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Local,
            runner: RunnerTopology::Local,
            exposure: Exposure::None,
            enrollment: Enrollment::ManagedPairing,
        });
        baseline_snapshot.readiness = aggregate_readiness(
            ServerReadiness::Stopped,
            RunnerReadiness::Stopped,
            ExposureReadiness::LocalReady,
            ProjectReadiness::Configured,
        );
        let baseline = ProcessBaseline {
            local_server: false,
            local_runner: false,
            quick_share: false,
            regular_tunnel: false,
            snapshot: baseline_snapshot.clone(),
        };
        core.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Starting,
            RunnerReadiness::Connecting,
            ExposureReadiness::LocalReady,
            ProjectReadiness::Configured,
        );

        core.reconcile_after_operation_failure(
            DesktopOperationKind::RuntimeResume,
            &baseline,
            ProcessCleanup {
                local_server: true,
                local_runner: true,
                ..ProcessCleanup::default()
            },
            false,
        );

        assert_eq!(core.snapshot.readiness, baseline_snapshot.readiness);
        assert_eq!(core.snapshot.topology, baseline_snapshot.topology);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn invalid_stored_identity_is_not_advertised_for_reuse() {
        let config = StoredDesktopConfig {
            topology: Some(RuntimeTopology {
                experience: Experience::Full,
                server: ServerTopology::Remote {
                    url: "https://example.test".to_string(),
                },
                runner: RunnerTopology::Local,
                exposure: Exposure::ExistingHttps {
                    url: "https://example.test".to_string(),
                },
                enrollment: Enrollment::ManagedPairing,
            }),
            project: Some(ProjectSelection {
                path: r"C:\repo".to_string(),
                allowed_root: r"C:\".to_string(),
                is_git_repository: true,
                runtime_project_id: Some("agent:desktop:repo".to_string()),
            }),
            runtime_autostart: None,
            preferred_connection: None,
            tunnel_proxy: TunnelProxyConfig::default(),
            runtime: Some(StoredRuntime {
                server_url: "https://example.test".to_string(),
                server_env_file: None,
                runner_config: Some(PathBuf::from("missing-runner.toml")),
                user_token_file: Some(PathBuf::from("missing-user-token")),
                runner_client_id: Some("desktop".to_string()),
                project_id: Some("repo".to_string()),
                runtime_project_id: Some("agent:desktop:repo".to_string()),
            }),
        };
        assert_eq!(
            project_snapshot(&config).and_then(|project| project.runtime_project_id),
            None
        );
    }

    #[tokio::test]
    async fn control_plane_stays_observable_and_cancel_is_exact_while_mutation_is_stuck() {
        let data_dir = std::env::temp_dir().join(format!(
            "webcodex-desktop-control-plane-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let state = Arc::new(
            AppState::new(data_dir.clone(), data_dir.join("test-resources"))
                .expect("create Desktop test state"),
        );
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let operation_state = Arc::clone(&state);
        let operation = tokio::spawn(async move {
            operation_state
                .hold_test_operation(started_tx, release_rx)
                .await
        });
        let first_id = tokio::time::timeout(Duration::from_secs(1), started_rx)
            .await
            .expect("stuck operation must start")
            .expect("operation id");

        let observed = tokio::time::timeout(Duration::from_millis(250), async {
            (state.get_state(), state.activity())
        })
        .await
        .expect("control-plane reads must not wait for the mutation core");
        assert_eq!(
            observed
                .0
                .current_operation
                .as_ref()
                .map(|operation| operation.id.as_str()),
            Some(first_id.as_str())
        );
        assert_eq!(
            observed
                .0
                .current_operation
                .as_ref()
                .map(|operation| operation.phase),
            Some(crate::models::DesktopOperationPhase::Running)
        );
        assert!(
            !observed.1.is_empty(),
            "Activity must stay independently readable"
        );

        let busy = tokio::time::timeout(Duration::from_millis(250), state.refresh_runtime_status())
            .await
            .expect("second mutation must fail fast")
            .expect_err("second mutation must not queue behind the stuck operation");
        assert_eq!(busy.code, "desktop_operation_busy");

        let cancelling = state
            .cancel_operation(&first_id)
            .expect("exact observed operation can be stopped");
        assert_eq!(
            cancelling
                .current_operation
                .as_ref()
                .map(|operation| operation.phase),
            Some(crate::models::DesktopOperationPhase::Cancelling)
        );
        let still_busy =
            tokio::time::timeout(Duration::from_millis(250), state.refresh_runtime_status())
                .await
                .expect("cancelling operation must retain the mutation slot")
                .expect_err("cleanup has not completed yet");
        assert_eq!(still_busy.code, "desktop_operation_busy");

        release_tx.send(()).expect("release first cleanup");
        let first_error = operation
            .await
            .expect("first operation task")
            .expect_err("first operation was cancelled");
        assert_eq!(first_error.code, "desktop_operation_cancelled");
        assert!(state.get_state().current_operation.is_none());

        let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
        let (second_release_tx, second_release_rx) = tokio::sync::oneshot::channel();
        let second_state = Arc::clone(&state);
        let second_operation = tokio::spawn(async move {
            second_state
                .hold_test_operation(second_started_tx, second_release_rx)
                .await
        });
        let second_id = tokio::time::timeout(Duration::from_secs(1), second_started_rx)
            .await
            .expect("second operation must start")
            .expect("second operation id");
        assert_ne!(first_id, second_id);

        let stale = state
            .cancel_operation(&first_id)
            .expect_err("late cancel for A must not target B");
        assert_eq!(stale.code, "desktop_operation_not_current");
        let second_snapshot = state.get_state();
        assert_eq!(
            second_snapshot
                .current_operation
                .as_ref()
                .map(|operation| operation.id.as_str()),
            Some(second_id.as_str())
        );
        assert_eq!(
            second_snapshot
                .current_operation
                .as_ref()
                .map(|operation| operation.phase),
            Some(crate::models::DesktopOperationPhase::Running)
        );

        state
            .cancel_operation(&second_id)
            .expect("exact second cancel");
        second_release_tx.send(()).expect("release second cleanup");
        let second_error = second_operation
            .await
            .expect("second operation task")
            .expect_err("second operation was cancelled");
        assert_eq!(second_error.code, "desktop_operation_cancelled");
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[cfg(target_os = "macos")]
    fn mac_process_exists(pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 {
            return true;
        }
        !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        )
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn shutdown_cancels_stuck_one_shot_and_reclaims_all_desktop_owned_trees() {
        let data_dir = std::env::temp_dir().join(format!(
            "webcodex-desktop-shutdown-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let long_marker = data_dir.join("long-lived-pids.txt");
        let one_shot_marker = data_dir.join("one-shot-pids.txt");
        std::fs::create_dir_all(&data_dir).expect("create shutdown fixture dir");
        let state = Arc::new(
            AppState::new(data_dir.clone(), data_dir.join("test-resources"))
                .expect("create Desktop shutdown test state"),
        );

        let mut long_command = std::process::Command::new("/bin/sh");
        long_command.args([
            "-c",
            "sleep 8 & descendant=$!; printf '%s %s\\n' \"$$\" \"$descendant\" > \"$1\"; wait \"$descendant\"",
            "webcodex-long-lived-shutdown",
            &long_marker.to_string_lossy(),
        ]);
        state
            .supervisor
            .lock()
            .await
            .spawn_owned(ProcessKind::LocalServer, long_command, false)
            .await
            .expect("start long-lived Desktop-owned fixture");

        let marker_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !long_marker.is_file() {
            assert!(
                tokio::time::Instant::now() < marker_deadline,
                "long-lived fixture did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let long_pids = std::fs::read_to_string(&long_marker)
            .expect("long-lived fixture pids")
            .split_whitespace()
            .map(|value| value.parse::<u32>().expect("long-lived fixture pid"))
            .collect::<Vec<_>>();

        let args = vec![
            "-c".to_string(),
            "sleep 8 & descendant=$!; printf '%s %s\\n' \"$$\" \"$descendant\" > \"$1\"; wait \"$descendant\"".to_string(),
            "webcodex-one-shot-shutdown".to_string(),
            one_shot_marker.to_string_lossy().to_string(),
        ];
        let operation_state = Arc::clone(&state);
        let operation = tokio::spawn(async move {
            operation_state
                .run_test_one_shot_operation(
                    PathBuf::from("/bin/sh"),
                    args,
                    vec![b'x'; 64 * 1024],
                    Duration::from_secs(8),
                )
                .await
        });
        let marker_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !one_shot_marker.is_file() {
            assert!(
                tokio::time::Instant::now() < marker_deadline,
                "one-shot fixture did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let one_shot_pids = std::fs::read_to_string(&one_shot_marker)
            .expect("one-shot fixture pids")
            .split_whitespace()
            .map(|value| value.parse::<u32>().expect("one-shot fixture pid"))
            .collect::<Vec<_>>();

        tokio::time::timeout(Duration::from_secs(7), state.shutdown())
            .await
            .expect("shutdown must remain bounded during a stuck one-shot operation");
        let operation_error = operation
            .await
            .expect("one-shot operation task")
            .expect_err("shutdown must cancel the one-shot operation");
        assert_eq!(operation_error.code, "desktop_operation_cancelled");
        for pid in long_pids.into_iter().chain(one_shot_pids) {
            assert!(
                !mac_process_exists(pid),
                "Desktop-owned PID {pid} survived application shutdown"
            );
        }
        assert!(state
            .supervisor
            .lock()
            .await
            .snapshot(ProcessKind::LocalServer)
            .is_none());
        tokio::time::timeout(Duration::from_millis(250), state.shutdown())
            .await
            .expect("repeated shutdown must be idempotent and fast");
        let _ = std::fs::remove_dir_all(data_dir);
    }

    /// Serializes tests that mutate process environment variables.
    ///
    /// Env mutation is process-global, so env-mutating tests in this test
    /// binary hold one shared lock for their whole body. Desktop is its own
    /// Cargo workspace, so this per-crate `TEST_ENV_LOCK` follows the
    /// convention documented in docs/TESTING.md.
    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Restores one process environment variable when the guard is dropped,
    /// including through panic unwinding. Holding `TEST_ENV_LOCK` for the
    /// guard's lifetime also serializes env mutation across sibling tests;
    /// poisoning is tolerated because the RAII restore already fixed the
    /// environment before the lock is released.
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
        _test_env_lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let test_env_lock = TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key,
                previous,
                _test_env_lock: test_env_lock,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires current-source dogfood binaries and a temporary project"]
    async fn native_local_full_dogfood_reuses_enrollment_and_stops_owned_runtime() {
        let project = std::env::var("WEBCODEX_DESKTOP_DOGFOOD_PROJECT")
            .expect("WEBCODEX_DESKTOP_DOGFOOD_PROJECT must point to the temporary fixture");
        // The Server only accepts lowercase local pairing usernames. Pin a
        // mixed-case OS username so the compatibility path is exercised on
        // every machine, not only where the login name already fails. The
        // guard restores the previous value on every exit path, including
        // panics, so this override never leaks to sibling tests.
        let _username_guard = EnvVarGuard::set("USERNAME", "Alice Dogfood");
        // macOS exposes its temporary root through /var -> /private/var.
        // Resolve the fixture root, not the credential-store security checks.
        let temporary_root = std::env::temp_dir()
            .canonicalize()
            .expect("resolve the native temporary fixture root");
        let data_dir = temporary_root.join(format!(
            "webcodex-desktop-local-dogfood-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&data_dir);
        let mut core = DesktopCore::new(data_dir.clone(), data_dir.join("test-resources"))
            .expect("create local dogfood state");
        let cancellation = CancellationContext::never();
        let setup = core
            .configure_local_setup(Some(&project), &cancellation)
            .await;
        let snapshot = match setup {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let stages: Vec<_> = core
                    .activity
                    .snapshot()
                    .into_iter()
                    .map(|entry| entry.event_kind)
                    .collect();
                let readiness = core.snapshot.readiness.clone();
                let initialized = data_dir.join("runtime/local/webcodex.env").is_file();
                core.supervisor.lock().await.stop_all().await;
                let _ = std::fs::remove_dir_all(&data_dir);
                panic!(
                    "local full setup failed: {error:?}; initialized={initialized}; readiness={readiness:?}; stages={stages:?}"
                );
            }
        };
        assert_eq!(snapshot.readiness.server, ServerReadiness::Ready);
        assert_eq!(snapshot.readiness.runner, RunnerReadiness::Ready);
        assert_eq!(snapshot.readiness.project, ProjectReadiness::Ready);
        assert_eq!(snapshot.readiness.exposure, ExposureReadiness::LocalReady);
        assert!(snapshot.readiness.runtime_ready);
        assert!(!snapshot.readiness.ready_for_chatgpt);

        let first_runtime = core
            .config
            .runtime
            .as_ref()
            .expect("local setup stores runtime identity");
        let first_user_token_file = first_runtime
            .user_token_file
            .clone()
            .expect("local setup stores managed user token path");
        let first_user_token =
            std::fs::read(&first_user_token_file).expect("read managed user token before restart");
        let first_runner_config = first_runtime.runner_config.clone().unwrap();
        let first_runner_bytes = std::fs::read(&first_runner_config).unwrap();

        let stopped = core
            .stop_local_runtime(&cancellation)
            .await
            .expect("stop local runtime");
        assert_eq!(stopped.readiness.server, ServerReadiness::Stopped);
        assert_eq!(stopped.readiness.runner, RunnerReadiness::Stopped);
        assert!(core
            .process_snapshot(ProcessKind::LocalServer)
            .await
            .is_none());
        assert!(core
            .process_snapshot(ProcessKind::LocalRunner)
            .await
            .is_none());

        let restarted = core
            .configure_local_setup(Some(&project), &cancellation)
            .await
            .expect("restart local full setup without re-enrollment");
        assert_eq!(restarted.readiness.server, ServerReadiness::Ready);
        assert_eq!(restarted.readiness.runner, RunnerReadiness::Ready);
        assert_eq!(restarted.readiness.project, ProjectReadiness::Ready);
        let second_user_token_file = core
            .config
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.user_token_file.clone())
            .expect("restarted local setup keeps managed user token path");
        assert_eq!(second_user_token_file, first_user_token_file);
        let second_user_token =
            std::fs::read(&second_user_token_file).expect("read managed user token after restart");
        assert!(
            first_user_token == second_user_token,
            "local restart must reuse enrollment instead of rotating the managed user token"
        );
        let project_a_identity =
            identity_from_config(&core.config).expect("project A runtime identity after restart");
        let first_runner_client_id =
            stored_runner_client_id(&core.config).expect("stored Runner client identity");
        let server_before_switch = core
            .process_snapshot(ProcessKind::LocalServer)
            .await
            .expect("Desktop owns local Server before project switch");
        let runner_before_switch = core
            .process_snapshot(ProcessKind::LocalRunner)
            .await
            .expect("Desktop owns local Runner before project switch");
        assert!(server_before_switch.owned_by_desktop);
        assert!(runner_before_switch.owned_by_desktop);
        // Switch projects while the Desktop-owned Server and Runner are still
        // running. A compatible connection hot-extends exact-root authority and
        // activates the new Project without replacing either owned process.
        let second_project = data_dir.join("second-project");
        std::fs::create_dir_all(&second_project).expect("create second project fixture");
        let expected_project = core
            .adapter
            .inspect_project(&second_project.to_string_lossy())
            .await
            .expect("inspect second project fixture");
        let switched = match core
            .configure_local_setup(Some(&second_project.to_string_lossy()), &cancellation)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let readiness = core.snapshot.readiness.clone();
                core.supervisor.lock().await.stop_all().await;
                let _ = std::fs::remove_dir_all(&data_dir);
                panic!("switch project setup failed: {error:?}; readiness={readiness:?}");
            }
        };
        assert_eq!(switched.readiness.server, ServerReadiness::Ready);
        assert_eq!(switched.readiness.runner, RunnerReadiness::Ready);
        assert_eq!(switched.readiness.project, ProjectReadiness::Ready);
        assert!(same_project(
            &core.config.project.as_ref().expect("selected project").path,
            &expected_project.path,
        ));
        let server_after_switch = core
            .process_snapshot(ProcessKind::LocalServer)
            .await
            .expect("Desktop still owns local Server after project switch");
        let runner_after_switch = core
            .process_snapshot(ProcessKind::LocalRunner)
            .await
            .expect("Desktop still owns the same Runner after project switch");
        assert_eq!(
            server_after_switch.pid, server_before_switch.pid,
            "project switch must keep the existing Desktop-owned Server"
        );
        assert_eq!(
            runner_after_switch.pid, runner_before_switch.pid,
            "compatible project switch must preserve the Desktop-owned Runner PID"
        );
        assert!(server_after_switch.owned_by_desktop);
        assert!(runner_after_switch.owned_by_desktop);
        let switched_identity =
            identity_from_config(&core.config).expect("project B runtime identity after switch");
        assert_eq!(
            stored_runner_client_id(&core.config).as_deref(),
            Some(first_runner_client_id.as_str()),
            "project switch must preserve the Runner client identity"
        );
        assert_eq!(
            switched_identity.runner_config, first_runner_config,
            "project switch must hot-update the same Runner config"
        );
        assert_eq!(
            switched_identity.user_token_file, first_user_token_file,
            "project switch must preserve the managed user-token path"
        );
        assert_ne!(
            switched_identity.runtime_project_id, project_a_identity.runtime_project_id,
            "different canonical projects must keep distinct runtime Project identities"
        );
        let config_after_switch = std::fs::read(&first_runner_config).unwrap();
        assert_ne!(
            config_after_switch, first_runner_bytes,
            "switching to a new exact root must persist the policy candidate in the existing Runner config"
        );
        assert_eq!(
            std::fs::read(&first_user_token_file).unwrap(),
            first_user_token,
            "switching projects must not rotate the managed user token"
        );
        assert!(
            core.adapter
                .project_ready(&project_a_identity, &cancellation)
                .await
                .expect("project A inventory remains observable"),
            "activating Project B must not delete Project A registration"
        );

        let project_b_runtime_id = switched_identity.runtime_project_id.clone();
        let repeated = core
            .configure_local_setup(Some(&second_project.to_string_lossy()), &cancellation)
            .await
            .expect("reselecting Project B is idempotent");
        assert_eq!(repeated.readiness.project, ProjectReadiness::Ready);
        let runner_after_repeat = core
            .process_snapshot(ProcessKind::LocalRunner)
            .await
            .expect("Desktop still owns Runner after idempotent activation");
        assert_eq!(
            runner_after_repeat.pid, runner_before_switch.pid,
            "idempotent project activation must not restart the Runner"
        );
        let repeated_identity = identity_from_config(&core.config)
            .expect("Project B identity after repeated activation");
        assert_eq!(repeated_identity.runtime_project_id, project_b_runtime_id);
        assert_eq!(
            stored_runner_client_id(&core.config),
            Some(first_runner_client_id)
        );
        assert_eq!(
            std::fs::read(&first_runner_config).unwrap(),
            config_after_switch,
            "idempotent activation must not duplicate or rewrite allowed_roots"
        );
        assert_eq!(
            std::fs::read(&first_user_token_file).unwrap(),
            first_user_token,
            "idempotent activation must keep the same managed user token"
        );
        core.stop_local_runtime(&cancellation)
            .await
            .expect("stop restarted local runtime");
        drop(core);
        std::fs::remove_dir_all(&data_dir).expect("remove local dogfood app data");
    }

    #[tokio::test]
    #[ignore = "requires current-source dogfood binaries and a temporary project"]
    async fn windows_quick_share_dogfood_reaches_ready_and_stops_foreground_owner() {
        if !cfg!(windows) {
            return;
        }
        let project = std::env::var("WEBCODEX_DESKTOP_DOGFOOD_PROJECT")
            .expect("WEBCODEX_DESKTOP_DOGFOOD_PROJECT must point to the temporary fixture");
        let data_dir = std::env::temp_dir().join(format!(
            "webcodex-desktop-share-dogfood-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&data_dir);
        let mut core = DesktopCore::new(data_dir.clone(), data_dir.join("test-resources"))
            .expect("create Quick Share dogfood state");
        let cancellation = CancellationContext::never();
        let started = core
            .start_quick_share(&project, "none", &cancellation)
            .await;
        let snapshot = match started {
            Ok(snapshot) => snapshot,
            Err(error) => {
                core.supervisor.lock().await.stop_all().await;
                let _ = std::fs::remove_dir_all(&data_dir);
                panic!("Quick Share setup failed: {error:?}");
            }
        };
        assert_eq!(snapshot.readiness.server, ServerReadiness::Ready);
        assert_eq!(snapshot.readiness.runner, RunnerReadiness::Ready);
        assert_eq!(snapshot.readiness.project, ProjectReadiness::Ready);
        assert_eq!(snapshot.readiness.exposure, ExposureReadiness::LocalReady);
        assert!(snapshot.readiness.runtime_ready);
        assert!(!snapshot.readiness.ready_for_chatgpt);
        assert!(core
            .process_snapshot(ProcessKind::QuickShare)
            .await
            .is_some());

        core.stop_quick_share(&cancellation)
            .await
            .expect("stop Quick Share");
        assert!(core
            .process_snapshot(ProcessKind::QuickShare)
            .await
            .is_none());
        drop(core);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    #[ignore = "requires current-source dogfood binaries and a temporary project"]
    async fn windows_remote_full_dogfood_reuses_enrollment_without_local_server() {
        if !cfg!(windows) {
            return;
        }
        let project = std::env::var("WEBCODEX_DESKTOP_DOGFOOD_PROJECT")
            .expect("WEBCODEX_DESKTOP_DOGFOOD_PROJECT must point to the temporary fixture");
        let suffix = std::process::id();
        let host_data =
            std::env::temp_dir().join(format!("webcodex-desktop-remote-host-dogfood-{suffix}"));
        let client_data =
            std::env::temp_dir().join(format!("webcodex-desktop-remote-client-dogfood-{suffix}"));
        let _ = std::fs::remove_dir_all(&host_data);
        let _ = std::fs::remove_dir_all(&client_data);

        let mut host = DesktopCore::new(host_data.clone(), host_data.join("test-resources"))
            .expect("create remote dogfood host state");
        let cancellation = CancellationContext::never();
        let host_runtime = host_data.join("runtime");
        let env_file = host_runtime.join("webcodex.env");
        let data_dir = host_runtime.join("data");
        tokio::fs::create_dir_all(&host_runtime)
            .await
            .expect("create remote dogfood host state");
        let listen = reserve_loopback_address().expect("reserve remote dogfood host port");
        let server_url = host
            .adapter
            .init_local_server(&listen, &data_dir, &env_file, &cancellation)
            .await
            .expect("initialize remote dogfood Server")
            .probe_url;
        let command = host
            .adapter
            .local_server_command(&env_file)
            .expect("build remote dogfood Server command");
        host.spawn_owned(ProcessKind::LocalServer, command, false, &cancellation)
            .await
            .expect("start remote dogfood Server");

        let mut client = DesktopCore::new(client_data.clone(), client_data.join("test-resources"))
            .expect("create remote dogfood client state");
        let result: DesktopResult<(DesktopStateSnapshot, DesktopStateSnapshot, bool, bool)> =
            async {
                host.wait_for_server(
                    &server_url,
                    Some(&env_file),
                    None,
                    &cancellation,
                    Deadline::after(SERVER_READY_TIMEOUT),
                    true,
                )
                .await?;
                let pairing_code = host
                    .adapter
                    .create_local_pairing(&server_url, &env_file, &cancellation)
                    .await?;
                let first = client
                    .configure_remote_setup(&server_url, &pairing_code, &project, &cancellation)
                    .await?;
                let first_started_server = client
                    .process_snapshot(ProcessKind::LocalServer)
                    .await
                    .is_some();
                client.stop_local_runtime(&cancellation).await?;

                let second = client
                    .configure_remote_setup(&server_url, "", &project, &cancellation)
                    .await?;
                let second_started_server = client
                    .process_snapshot(ProcessKind::LocalServer)
                    .await
                    .is_some();
                client.stop_local_runtime(&cancellation).await?;
                Ok((first, second, first_started_server, second_started_server))
            }
            .await;

        client.supervisor.lock().await.stop_all().await;
        host.supervisor.lock().await.stop_all().await;
        let _ = std::fs::remove_dir_all(&client_data);
        let _ = std::fs::remove_dir_all(&host_data);

        let (first, second, first_started_server, second_started_server) =
            result.expect("remote full dogfood should complete");
        for snapshot in [&first, &second] {
            assert_eq!(snapshot.readiness.server, ServerReadiness::Ready);
            assert_eq!(snapshot.readiness.runner, RunnerReadiness::Ready);
            assert_eq!(snapshot.readiness.project, ProjectReadiness::Ready);
            assert!(snapshot.readiness.runtime_ready);
            assert!(matches!(
                snapshot.topology.as_ref().map(|topology| &topology.server),
                Some(ServerTopology::Remote { .. })
            ));
        }
        assert!(!first_started_server);
        assert!(!second_started_server);
    }

    #[test]
    fn configured_https_exposure_stays_unverified_without_mcp_handoff_evidence() {
        let local = RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Local,
            runner: RunnerTopology::Local,
            exposure: Exposure::None,
            enrollment: Enrollment::ManagedPairing,
        };
        let remote = RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Remote {
                url: "https://example.com".to_string(),
            },
            runner: RunnerTopology::Local,
            exposure: Exposure::ExistingHttps {
                url: "https://example.com".to_string(),
            },
            enrollment: Enrollment::ManagedPairing,
        };
        assert_eq!(
            exposure_readiness(Some(&local)),
            ExposureReadiness::LocalReady
        );
        assert_eq!(
            exposure_readiness(Some(&remote)),
            ExposureReadiness::Unknown
        );
    }

    #[test]
    fn regular_tunnel_child_death_only_degrades_connection_readiness() {
        let mut state = Some(RegularTunnelState {
            provider: "openai".to_string(),
            status: RegularTunnelStatus::Ready,
            clipboard_state: "copied".to_string(),
            clipboard_contains: "tunnel_id".to_string(),
            ready_for_chatgpt: true,
        });
        assert_eq!(
            regular_tunnel_exposure(&mut state, true),
            Some(ExposureReadiness::LocalReady)
        );
        assert_eq!(
            regular_tunnel_exposure(&mut state, false),
            Some(ExposureReadiness::Error)
        );
        assert_eq!(state.as_ref().unwrap().status, RegularTunnelStatus::Error);
        assert!(!state.as_ref().unwrap().ready_for_chatgpt);

        let readiness = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            ExposureReadiness::Error,
            ProjectReadiness::Ready,
        );
        assert!(readiness.runtime_ready);
        assert!(!readiness.ready_for_chatgpt);
    }

    #[test]
    fn regular_tunnel_local_handoff_waits_for_external_chatgpt_evidence() {
        let mut snapshot = DesktopStateSnapshot::default();
        snapshot.readiness = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            ExposureReadiness::LocalReady,
            ProjectReadiness::Ready,
        );
        apply_regular_tunnel_next_action(&mut snapshot, &ExposureReadiness::LocalReady);
        assert!(snapshot.readiness.runtime_ready);
        assert!(!snapshot.readiness.ready_for_chatgpt);
        assert_eq!(
            snapshot.readiness.summary_kind,
            ReadinessSummaryKind::TunnelReadyWaitingForChatGpt
        );
        assert_eq!(
            snapshot.readiness.next_action_kind,
            Some(ReadinessNextActionKind::CheckConnection)
        );
    }

    #[test]
    fn tunnel_configuration_presence_matrix_is_value_free() {
        for (tunnel_id_present, api_key_present, configured) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ] {
            let observation =
                openai_tunnel_configuration_from_presence(tunnel_id_present, api_key_present);
            assert_eq!(observation.tunnel_id_present, tunnel_id_present);
            assert_eq!(observation.api_key_present, api_key_present);
            assert_eq!(observation.is_configured(), configured);
            let encoded =
                serde_json::to_value(&observation).expect("serialize presence observation");
            assert_eq!(encoded.as_object().map(|object| object.len()), Some(2));
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_failed_regular_tunnel_child_cannot_leave_fake_green_readiness() {
        let data_dir = std::env::temp_dir().join(format!(
            "webcodex-desktop-tunnel-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let mut core = DesktopCore::new(data_dir.clone(), data_dir.join("test-resources"))
            .expect("create tunnel failure state");
        let topology = RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Local,
            runner: RunnerTopology::Local,
            exposure: Exposure::None,
            enrollment: Enrollment::ManagedPairing,
        };
        core.config.topology = Some(topology.clone());
        core.snapshot.topology = Some(topology);
        core.snapshot.readiness = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            ExposureReadiness::LocalReady,
            ProjectReadiness::Ready,
        );
        core.snapshot.regular_tunnel = Some(RegularTunnelState {
            provider: "openai".to_string(),
            status: RegularTunnelStatus::Ready,
            clipboard_state: "copied".to_string(),
            clipboard_contains: "tunnel_id".to_string(),
            ready_for_chatgpt: true,
        });

        let mut command = std::process::Command::new("cmd.exe");
        command.args(["/D", "/C", "exit", "/B", "23"]);
        core.supervisor
            .lock()
            .await
            .spawn_owned(ProcessKind::RegularTunnel, command, false)
            .await
            .expect("start failing tunnel fixture");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let snapshot = loop {
            let snapshot = core.get_state().await.expect("observe failed tunnel");
            if snapshot.readiness.exposure == ExposureReadiness::Error {
                break snapshot;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "fake tunnel did not exit"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        assert!(snapshot.readiness.runtime_ready);
        assert!(!snapshot.readiness.ready_for_chatgpt);
        assert_eq!(snapshot.readiness.server, ServerReadiness::Ready);
        assert_eq!(snapshot.readiness.runner, RunnerReadiness::Ready);
        assert_eq!(snapshot.readiness.project, ProjectReadiness::Ready);
        assert_eq!(
            snapshot.readiness.next_action_kind,
            Some(ReadinessNextActionKind::RestartSecureTunnel)
        );
        core.supervisor.lock().await.stop_all().await;
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn regular_tunnel_selection_is_ephemeral_and_local_only() {
        let local = RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Local,
            runner: RunnerTopology::Local,
            exposure: Exposure::None,
            enrollment: Enrollment::ManagedPairing,
        };
        let selected = effective_topology(Some(&local), true).unwrap();
        assert_eq!(selected.exposure, Exposure::OpenAiTunnel);
        assert_eq!(local.exposure, Exposure::None);

        let remote = RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Remote {
                url: "https://example.test".to_string(),
            },
            runner: RunnerTopology::Local,
            exposure: Exposure::ExistingHttps {
                url: "https://example.test".to_string(),
            },
            enrollment: Enrollment::ManagedPairing,
        };
        assert_eq!(
            effective_topology(Some(&remote), true).unwrap().exposure,
            remote.exposure
        );
    }
}
