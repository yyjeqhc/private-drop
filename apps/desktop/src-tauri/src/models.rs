use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Experience {
    Full,
    QuickShare,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerTopology {
    Local,
    Remote { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunnerTopology {
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Exposure {
    None,
    ExistingHttps { url: String },
    Cloudflare,
    OpenAiTunnel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Enrollment {
    ManagedPairing,
    SharedKey,
    ExistingProfile { profile: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeTopology {
    pub experience: Experience,
    pub server: ServerTopology,
    pub runner: RunnerTopology,
    pub exposure: Exposure,
    pub enrollment: Enrollment,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerReadiness {
    Stopped,
    Starting,
    Ready,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerReadiness {
    Stopped,
    Connecting,
    Ready,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExposureReadiness {
    Disabled,
    Starting,
    LocalReady,
    RemoteReady,
    Degraded,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectReadiness {
    None,
    Configured,
    ReloadRequired,
    Ready,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessSummaryKind {
    ReadyForChatGpt,
    ServiceNeedsAttention,
    RunnerDisconnected,
    ProjectNotReady,
    RuntimeReadyLocalOnly,
    TunnelReadyWaitingForChatGpt,
    ConnectionUnverified,
    QuickShareStopped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessNextActionKind {
    StartOrReconnectService,
    StartRunner,
    AddOrReloadProject,
    ChooseConnection,
    CheckConnection,
    RestartQuickShare,
    RestoreClipboardHandoff,
    RestartSecureTunnel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadinessSnapshot {
    pub server: ServerReadiness,
    pub runner: RunnerReadiness,
    pub exposure: ExposureReadiness,
    pub project: ProjectReadiness,
    pub runtime_ready: bool,
    pub ready_for_chatgpt: bool,
    pub summary_kind: ReadinessSummaryKind,
    pub next_action_kind: Option<ReadinessNextActionKind>,
    pub summary: String,
    pub next_action: Option<String>,
}

impl Default for ReadinessSnapshot {
    fn default() -> Self {
        aggregate_readiness(
            ServerReadiness::Unknown,
            RunnerReadiness::Unknown,
            ExposureReadiness::Unknown,
            ProjectReadiness::None,
        )
    }
}

pub fn aggregate_readiness(
    server: ServerReadiness,
    runner: RunnerReadiness,
    exposure: ExposureReadiness,
    project: ProjectReadiness,
) -> ReadinessSnapshot {
    let runtime_ready = server == ServerReadiness::Ready
        && runner == RunnerReadiness::Ready
        && project == ProjectReadiness::Ready;
    let ready_for_chatgpt = runtime_ready && exposure == ExposureReadiness::RemoteReady;
    let (summary_kind, next_action_kind, summary, next_action) = if ready_for_chatgpt {
        (
            ReadinessSummaryKind::ReadyForChatGpt,
            None,
            "Ready to use with ChatGPT".to_string(),
            None,
        )
    } else if !matches!(server, ServerReadiness::Ready) {
        (
            ReadinessSummaryKind::ServiceNeedsAttention,
            Some(ReadinessNextActionKind::StartOrReconnectService),
            "WebCodex Service needs attention".to_string(),
            Some("Start or reconnect the WebCodex Service.".to_string()),
        )
    } else if !matches!(runner, RunnerReadiness::Ready) {
        (
            ReadinessSummaryKind::RunnerDisconnected,
            Some(ReadinessNextActionKind::StartRunner),
            "Runner is not connected".to_string(),
            Some("Start the Runner and wait for it to connect.".to_string()),
        )
    } else if project != ProjectReadiness::Ready {
        (
            ReadinessSummaryKind::ProjectNotReady,
            Some(ReadinessNextActionKind::AddOrReloadProject),
            "Project is not ready".to_string(),
            Some("Prepare and activate the selected project.".to_string()),
        )
    } else if exposure == ExposureReadiness::Disabled || exposure == ExposureReadiness::LocalReady {
        (
            ReadinessSummaryKind::RuntimeReadyLocalOnly,
            Some(ReadinessNextActionKind::ChooseConnection),
            "Runtime ready on this computer".to_string(),
            Some("Choose a ChatGPT connection in Connection.".to_string()),
        )
    } else {
        (
            ReadinessSummaryKind::ConnectionUnverified,
            Some(ReadinessNextActionKind::CheckConnection),
            "ChatGPT connection is not verified".to_string(),
            Some("Check the ChatGPT connection status.".to_string()),
        )
    };
    ReadinessSnapshot {
        server,
        runner,
        exposure,
        project,
        runtime_ready,
        ready_for_chatgpt,
        summary_kind,
        next_action_kind,
        summary,
        next_action,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectSelection {
    pub path: String,
    pub allowed_root: String,
    pub is_git_repository: bool,
    pub runtime_project_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BinaryInfo {
    pub directory: String,
    pub version: String,
    pub git_commit: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuickShareState {
    pub provider: String,
    pub project: String,
    pub mcp_url: Option<String>,
    pub clipboard_state: String,
    pub clipboard_contains: String,
    pub ready_for_chatgpt: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegularTunnelStatus {
    Starting,
    Ready,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegularTunnelState {
    pub provider: String,
    pub status: RegularTunnelStatus,
    pub clipboard_state: String,
    pub clipboard_contains: String,
    pub ready_for_chatgpt: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesktopOperationKind {
    LocalSetup,
    RemoteSetup,
    QuickShareStart,
    QuickShareStop,
    RegularTunnelStart,
    RegularTunnelStop,
    LocalRuntimeStop,
    RuntimeRefresh,
    RuntimeResume,
    TunnelProxyUpdate,
    TunnelConfigUpdate,
}

impl DesktopOperationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalSetup => "local_setup",
            Self::RemoteSetup => "remote_setup",
            Self::QuickShareStart => "quick_share_start",
            Self::QuickShareStop => "quick_share_stop",
            Self::RegularTunnelStart => "regular_tunnel_start",
            Self::RegularTunnelStop => "regular_tunnel_stop",
            Self::LocalRuntimeStop => "local_runtime_stop",
            Self::RuntimeRefresh => "runtime_refresh",
            Self::RuntimeResume => "runtime_resume",
            Self::TunnelProxyUpdate => "tunnel_proxy_update",
            Self::TunnelConfigUpdate => "tunnel_config_update",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesktopOperationPhase {
    Running,
    Cancelling,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesktopOperationSnapshot {
    pub id: String,
    pub kind: DesktopOperationKind,
    pub phase: DesktopOperationPhase,
    pub started_at_ms: u64,
    pub cancellable: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RegularConnectionPreference {
    #[default]
    NoChatGpt,
    OpenAiTunnel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TunnelProxyMode {
    #[default]
    Auto,
    Direct,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TunnelProxyConfig {
    #[serde(default)]
    pub mode: TunnelProxyMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelProxySnapshot {
    pub mode: TunnelProxyMode,
    pub custom_url: Option<String>,
    pub effective_source: String,
    pub effective_url: Option<String>,
    pub detected_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TunnelConfigSource {
    #[default]
    Environment,
    File,
    Invalid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OpenAiTunnelConfigSnapshot {
    pub tunnel_id_present: bool,
    pub api_key_present: bool,
    #[serde(default)]
    pub source: TunnelConfigSource,
    #[serde(default)]
    pub saved_tunnel_id: Option<String>,
}

impl OpenAiTunnelConfigSnapshot {
    pub fn is_configured(&self) -> bool {
        self.tunnel_id_present && self.api_key_present
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PowerShellRuntimeSnapshot {
    pub pwsh_available: bool,
    pub windows_powershell_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ChatGptActivitySnapshot {
    pub observed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_meaningful_activity_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesktopStateSnapshot {
    pub topology: Option<RuntimeTopology>,
    pub readiness: ReadinessSnapshot,
    pub project: Option<ProjectSelection>,
    pub binaries: Option<BinaryInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub powershell_runtime: Option<PowerShellRuntimeSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chatgpt_activity: Option<ChatGptActivitySnapshot>,
    pub quick_share: Option<QuickShareState>,
    pub regular_tunnel: Option<RegularTunnelState>,
    pub current_operation: Option<DesktopOperationSnapshot>,
    pub activity_sequence: u64,
    pub openai_tunnel_configured: bool,
    pub openai_tunnel_config: OpenAiTunnelConfigSnapshot,
    pub regular_tunnel_available: bool,
    pub runtime_autostart: bool,
    pub preferred_connection: RegularConnectionPreference,
    pub tunnel_proxy: TunnelProxySnapshot,
}

impl Default for DesktopStateSnapshot {
    fn default() -> Self {
        Self {
            topology: None,
            readiness: ReadinessSnapshot::default(),
            project: None,
            binaries: None,
            powershell_runtime: None,
            chatgpt_activity: None,
            quick_share: None,
            regular_tunnel: None,
            current_operation: None,
            activity_sequence: 0,
            openai_tunnel_configured: false,
            openai_tunnel_config: OpenAiTunnelConfigSnapshot::default(),
            regular_tunnel_available: false,
            runtime_autostart: false,
            preferred_connection: RegularConnectionPreference::NoChatGpt,
            tunnel_proxy: TunnelProxySnapshot {
                mode: TunnelProxyMode::Auto,
                custom_url: None,
                effective_source: "direct".to_string(),
                effective_url: None,
                detected_url: None,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StoredDesktopConfig {
    pub topology: Option<RuntimeTopology>,
    pub project: Option<ProjectSelection>,
    pub runtime: Option<StoredRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_autostart: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_connection: Option<RegularConnectionPreference>,
    #[serde(default)]
    pub tunnel_proxy: TunnelProxyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredRuntime {
    pub server_url: String,
    pub server_env_file: Option<PathBuf>,
    pub runner_config: Option<PathBuf>,
    pub user_token_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_client_id: Option<String>,
    pub project_id: Option<String>,
    pub runtime_project_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_dimensions_stay_orthogonal() {
        let local = RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Local,
            runner: RunnerTopology::Local,
            exposure: Exposure::OpenAiTunnel,
            enrollment: Enrollment::ManagedPairing,
        };
        let remote = RuntimeTopology {
            experience: Experience::Full,
            server: ServerTopology::Remote {
                url: "https://server.example".to_string(),
            },
            runner: RunnerTopology::Local,
            exposure: Exposure::ExistingHttps {
                url: "https://server.example".to_string(),
            },
            enrollment: Enrollment::ManagedPairing,
        };
        assert_eq!(local.runner, remote.runner);
        assert_ne!(local.server, remote.server);
        assert_ne!(local.exposure, remote.exposure);
    }

    #[test]
    fn process_alive_is_not_aggregate_readiness() {
        let server_only = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Connecting,
            ExposureReadiness::RemoteReady,
            ProjectReadiness::Configured,
        );
        assert!(!server_only.runtime_ready);
        assert!(!server_only.ready_for_chatgpt);

        let missing_project = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            ExposureReadiness::RemoteReady,
            ProjectReadiness::None,
        );
        assert!(!missing_project.runtime_ready);
        assert!(!missing_project.ready_for_chatgpt);

        let local_only = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            ExposureReadiness::LocalReady,
            ProjectReadiness::Ready,
        );
        assert!(local_only.runtime_ready);
        assert!(!local_only.ready_for_chatgpt);

        let full = aggregate_readiness(
            ServerReadiness::Ready,
            RunnerReadiness::Ready,
            ExposureReadiness::RemoteReady,
            ProjectReadiness::Ready,
        );
        assert!(full.runtime_ready);
        assert!(full.ready_for_chatgpt);
    }
}
