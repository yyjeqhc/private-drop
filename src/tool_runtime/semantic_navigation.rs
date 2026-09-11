//! Compact project-specific semantic navigation capability for coding startup.
//!
//! The startup probe uses only the typed Runner `Status` transport operation. It never
//! enters public ToolCall dispatch, starts a language server, or exposes the
//! raw Runner transport/result envelope.

use super::lsp_tools::runner_local_project_id;
use super::project_resolution::ResolvedProject;
use super::ToolRuntime;
use crate::lsp_bridge::{
    parse_runner_lsp_result_envelope, LspAvailabilityStatus, LspStatusResult, RunnerLspPayload,
    RunnerLspRequest,
};
use crate::runner_http::{EnqueueLspError, RunnerFeature};
use serde::Serialize;
use std::time::Duration;
use tokio::time::Instant;

pub(crate) const DEFAULT_SEMANTIC_NAVIGATION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

const RUST_LANGUAGE: &str = "rust";
const RUST_ANALYZER_SERVER: &str = "rust-analyzer";
const GO_LANGUAGE: &str = "go";
const GOPLS_SERVER: &str = "gopls";
const SEMANTIC_NAVIGATION_TOOLS: [&str; 7] = [
    "lsp_status",
    "document_symbols",
    "goto_definition",
    "find_references",
    "document_diagnostics",
    "hover",
    "workspace_symbols",
];
const SEMANTIC_NAVIGATION_PREFERRED_FLOW: [&str; 6] = [
    "document_symbols",
    "goto_definition",
    "find_references",
    "hover",
    "read_files",
    "search_project_texts",
];
const RUST_SEMANTIC_NAVIGATION_LIMITATIONS: [&str; 5] = [
    "rust_only",
    "read_only",
    "workspace_only",
    "no_dependency_navigation",
    "full_text_sync_only",
];
const GO_SEMANTIC_NAVIGATION_LIMITATIONS: [&str; 5] = [
    "go_only",
    "read_only",
    "workspace_only",
    "no_dependency_navigation",
    "full_text_sync_only",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SemanticNavigationStartupStatus {
    Running,
    Available,
    Initializing,
    Crashed,
    Unavailable,
    NotApplicable,
    #[serde(rename = "agent_unavailable")]
    RunnerUnavailable,
    // Stable serialized compatibility status retained for pre-0.4 consumers.
    #[serde(rename = "agent_capability_unavailable")]
    RunnerCapabilityUnavailable,
    ProbeTimeout,
    ProbeFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SemanticNavigationReasonCode {
    RustNotDetected,
    #[serde(rename = "agent_not_connected")]
    RunnerNotConnected,
    LspCapabilityNotAdvertised,
    ServerCrashed,
    ServerUnavailable,
    StatusProbeTimedOut,
    StatusProbeFailed,
    #[serde(rename = "malformed_agent_result")]
    MalformedRunnerResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SemanticNavigationStartupSummary {
    supported: bool,
    /// `None` means the bounded startup status probe did not complete, so the
    /// Server has no fresh executable/slot availability fact. This is
    /// intentionally distinct from `Some(false)`, which is reserved for a
    /// positive unavailable observation.
    available: Option<bool>,
    recommended: bool,
    status: SemanticNavigationStartupStatus,
    language: Option<&'static str>,
    server: Option<&'static str>,
    position_encoding: Option<String>,
    tools: Vec<&'static str>,
    preferred_flow: Vec<&'static str>,
    limitations: Vec<&'static str>,
    reason_code: Option<SemanticNavigationReasonCode>,
}

impl SemanticNavigationStartupSummary {
    fn unsupported(
        status: SemanticNavigationStartupStatus,
        reason_code: SemanticNavigationReasonCode,
    ) -> Self {
        Self {
            supported: false,
            available: Some(false),
            recommended: false,
            status,
            language: None,
            server: None,
            position_encoding: None,
            tools: Vec::new(),
            preferred_flow: Vec::new(),
            limitations: Vec::new(),
            reason_code: Some(reason_code),
        }
    }

    fn supported_failure(
        status: SemanticNavigationStartupStatus,
        reason_code: SemanticNavigationReasonCode,
    ) -> Self {
        Self {
            supported: true,
            available: Some(false),
            recommended: false,
            status,
            language: None,
            // The status probe did not yield a usable language selection. Keep
            // the legacy Rust provider projection unchanged for compatibility.
            server: Some(RUST_ANALYZER_SERVER),
            position_encoding: None,
            tools: SEMANTIC_NAVIGATION_TOOLS.to_vec(),
            preferred_flow: Vec::new(),
            limitations: RUST_SEMANTIC_NAVIGATION_LIMITATIONS.to_vec(),
            reason_code: Some(reason_code),
        }
    }

    fn probe_timeout() -> Self {
        Self {
            supported: true,
            available: None,
            recommended: false,
            status: SemanticNavigationStartupStatus::ProbeTimeout,
            language: None,
            server: None,
            position_encoding: None,
            tools: SEMANTIC_NAVIGATION_TOOLS.to_vec(),
            preferred_flow: Vec::new(),
            limitations: vec![
                "read_only",
                "workspace_only",
                "no_dependency_navigation",
                "full_text_sync_only",
            ],
            reason_code: Some(SemanticNavigationReasonCode::StatusProbeTimedOut),
        }
    }

    fn rust_not_detected() -> Self {
        Self {
            supported: true,
            available: Some(false),
            recommended: false,
            status: SemanticNavigationStartupStatus::NotApplicable,
            language: None,
            server: Some(RUST_ANALYZER_SERVER),
            position_encoding: None,
            tools: Vec::new(),
            preferred_flow: Vec::new(),
            limitations: RUST_SEMANTIC_NAVIGATION_LIMITATIONS.to_vec(),
            reason_code: Some(SemanticNavigationReasonCode::RustNotDetected),
        }
    }

    fn from_enqueue_error(error: &EnqueueLspError) -> Self {
        match error {
            EnqueueLspError::UnknownRunner { .. } | EnqueueLspError::RunnerOffline { .. } => {
                Self::unsupported(
                    SemanticNavigationStartupStatus::RunnerUnavailable,
                    SemanticNavigationReasonCode::RunnerNotConnected,
                )
            }
            EnqueueLspError::UnsupportedCapability { .. } => Self::unsupported(
                SemanticNavigationStartupStatus::RunnerCapabilityUnavailable,
                SemanticNavigationReasonCode::LspCapabilityNotAdvertised,
            ),
            EnqueueLspError::InvalidRequest { .. } | EnqueueLspError::QueueFull { .. } => {
                Self::supported_failure(
                    SemanticNavigationStartupStatus::ProbeFailed,
                    SemanticNavigationReasonCode::StatusProbeFailed,
                )
            }
        }
    }

    fn from_lsp_status(
        result: LspStatusResult,
        expected_project_id: &str,
    ) -> Result<Self, SemanticNavigationReasonCode> {
        if result.project != expected_project_id {
            return Err(SemanticNavigationReasonCode::MalformedRunnerResult);
        }
        // Preserve the pre-existing Rust-first behavior in mixed workspaces.
        // Go is additive: a Go-only workspace selects gopls, while existing
        // Python/TypeScript-only startup behavior remains unchanged.
        let (language, server_name, limitations) = if result
            .detected_languages
            .iter()
            .any(|language| language == RUST_LANGUAGE)
        {
            (
                RUST_LANGUAGE,
                RUST_ANALYZER_SERVER,
                RUST_SEMANTIC_NAVIGATION_LIMITATIONS.as_slice(),
            )
        } else if result
            .detected_languages
            .iter()
            .any(|language| language == GO_LANGUAGE)
        {
            (
                GO_LANGUAGE,
                GOPLS_SERVER,
                GO_SEMANTIC_NAVIGATION_LIMITATIONS.as_slice(),
            )
        } else {
            return Ok(Self::rust_not_detected());
        };
        let Some(server) = result
            .servers
            .iter()
            .find(|entry| entry.language == language && entry.server == server_name)
        else {
            return Err(SemanticNavigationReasonCode::MalformedRunnerResult);
        };
        if server
            .position_encoding
            .as_deref()
            .is_some_and(|encoding| !matches!(encoding, "utf-8" | "utf-16" | "utf-32"))
        {
            return Err(SemanticNavigationReasonCode::MalformedRunnerResult);
        }

        let (available, recommended, status, reason_code, position_encoding) = match server.status {
            LspAvailabilityStatus::Running => (
                true,
                true,
                SemanticNavigationStartupStatus::Running,
                None,
                server.position_encoding.clone(),
            ),
            LspAvailabilityStatus::Available => (
                true,
                true,
                SemanticNavigationStartupStatus::Available,
                None,
                None,
            ),
            LspAvailabilityStatus::Initializing => (
                true,
                false,
                SemanticNavigationStartupStatus::Initializing,
                None,
                None,
            ),
            // A crashed slot restarts on the next request, so navigation is
            // still worth offering — but only while the Runner reports the
            // executable itself as available. Hardcoding `true` here would
            // advertise navigation after the binary was removed.
            LspAvailabilityStatus::Crashed => (
                server.available,
                false,
                SemanticNavigationStartupStatus::Crashed,
                Some(SemanticNavigationReasonCode::ServerCrashed),
                None,
            ),
            LspAvailabilityStatus::Unavailable => (
                false,
                false,
                SemanticNavigationStartupStatus::Unavailable,
                Some(SemanticNavigationReasonCode::ServerUnavailable),
                None,
            ),
        };

        Ok(Self {
            supported: true,
            available: Some(available),
            recommended,
            status,
            language: Some(language),
            server: Some(server_name),
            position_encoding,
            tools: SEMANTIC_NAVIGATION_TOOLS.to_vec(),
            preferred_flow: if recommended {
                SEMANTIC_NAVIGATION_PREFERRED_FLOW.to_vec()
            } else {
                Vec::new()
            },
            limitations: limitations.to_vec(),
            reason_code,
        })
    }
}

impl ToolRuntime {
    pub(crate) async fn probe_semantic_navigation_for_startup(
        &self,
        resolved: &ResolvedProject,
    ) -> SemanticNavigationStartupSummary {
        let client_id = resolved.config.client_id.clone();
        let Some(client) = self
            .runner_registry
            .get_runner_semantic_view(&client_id)
            .await
        else {
            return SemanticNavigationStartupSummary::unsupported(
                SemanticNavigationStartupStatus::RunnerUnavailable,
                SemanticNavigationReasonCode::RunnerNotConnected,
            );
        };
        if !client.view.connected {
            return SemanticNavigationStartupSummary::unsupported(
                SemanticNavigationStartupStatus::RunnerUnavailable,
                SemanticNavigationReasonCode::RunnerNotConnected,
            );
        }
        if !client.supports(RunnerFeature::LspReadOnlyNavigation) {
            return SemanticNavigationStartupSummary::unsupported(
                SemanticNavigationStartupStatus::RunnerCapabilityUnavailable,
                SemanticNavigationReasonCode::LspCapabilityNotAdvertised,
            );
        }
        let Some(agent_project_id) = runner_local_project_id(&resolved.resolved_id) else {
            return SemanticNavigationStartupSummary::supported_failure(
                SemanticNavigationStartupStatus::ProbeFailed,
                SemanticNavigationReasonCode::StatusProbeFailed,
            );
        };

        let deadline = Instant::now() + self.semantic_navigation_probe_timeout;
        let payload = RunnerLspPayload {
            project_id: agent_project_id.to_string(),
            request: RunnerLspRequest::Status,
        };
        let timeout_secs = self.semantic_navigation_probe_timeout.as_secs().max(1);
        let enqueued = tokio::time::timeout_at(
            deadline,
            self.runner_registry.enqueue_lsp(
                client_id,
                payload,
                "coding_startup_probe".to_string(),
                timeout_secs,
            ),
        )
        .await;
        let (request_id, receiver) = match enqueued {
            Err(_) => return SemanticNavigationStartupSummary::probe_timeout(),
            Ok(Err(error)) => return SemanticNavigationStartupSummary::from_enqueue_error(&error),
            Ok(Ok(request)) => request,
        };

        let response = match tokio::time::timeout_at(deadline, receiver).await {
            Err(_) => {
                self.runner_registry.cancel_request(&request_id).await;
                return SemanticNavigationStartupSummary::probe_timeout();
            }
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&request_id).await;
                return SemanticNavigationStartupSummary::supported_failure(
                    SemanticNavigationStartupStatus::ProbeFailed,
                    SemanticNavigationReasonCode::StatusProbeFailed,
                );
            }
            Ok(Ok(response)) => response,
        };
        if !response.success || response.error.is_some() || response.exit_code != Some(0) {
            return SemanticNavigationStartupSummary::supported_failure(
                SemanticNavigationStartupStatus::ProbeFailed,
                SemanticNavigationReasonCode::StatusProbeFailed,
            );
        }
        let Some(stdout) = response.stdout.as_deref() else {
            return SemanticNavigationStartupSummary::supported_failure(
                SemanticNavigationStartupStatus::ProbeFailed,
                SemanticNavigationReasonCode::MalformedRunnerResult,
            );
        };
        let envelope = match parse_runner_lsp_result_envelope(stdout) {
            Ok(envelope) => envelope,
            Err(_) => {
                return SemanticNavigationStartupSummary::supported_failure(
                    SemanticNavigationStartupStatus::ProbeFailed,
                    SemanticNavigationReasonCode::MalformedRunnerResult,
                )
            }
        };
        if !envelope.success {
            return SemanticNavigationStartupSummary::supported_failure(
                SemanticNavigationStartupStatus::ProbeFailed,
                SemanticNavigationReasonCode::StatusProbeFailed,
            );
        }
        let Some(result) = envelope.result else {
            return SemanticNavigationStartupSummary::supported_failure(
                SemanticNavigationStartupStatus::ProbeFailed,
                SemanticNavigationReasonCode::MalformedRunnerResult,
            );
        };
        let result = match serde_json::from_value::<LspStatusResult>(result) {
            Ok(result) => result,
            Err(_) => {
                return SemanticNavigationStartupSummary::supported_failure(
                    SemanticNavigationStartupStatus::ProbeFailed,
                    SemanticNavigationReasonCode::MalformedRunnerResult,
                )
            }
        };
        match SemanticNavigationStartupSummary::from_lsp_status(result, agent_project_id) {
            Ok(summary) => summary,
            Err(reason_code) => SemanticNavigationStartupSummary::supported_failure(
                SemanticNavigationStartupStatus::ProbeFailed,
                reason_code,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp_bridge::LspServerStatusEntry;

    fn crashed_status(executable_available: bool) -> LspStatusResult {
        LspStatusResult {
            project: "demo".to_string(),
            detected_languages: vec!["rust".to_string()],
            servers: vec![LspServerStatusEntry {
                language: "rust".to_string(),
                server: "rust-analyzer".to_string(),
                available: executable_available,
                running: false,
                status: LspAvailabilityStatus::Crashed,
                source: None,
                position_encoding: None,
            }],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn crashed_available_follows_runner_reported_executable_availability() {
        // Executable still present: the slot restarts on demand.
        let summary =
            SemanticNavigationStartupSummary::from_lsp_status(crashed_status(true), "demo")
                .unwrap();
        let value = serde_json::to_value(summary).unwrap();
        assert_eq!(value["status"], "crashed");
        assert_eq!(value["available"], true);
        assert_eq!(value["recommended"], false);
        assert_eq!(value["reason_code"], "server_crashed");

        // Executable removed after the crash: navigation must not be offered.
        let summary =
            SemanticNavigationStartupSummary::from_lsp_status(crashed_status(false), "demo")
                .unwrap();
        let value = serde_json::to_value(summary).unwrap();
        assert_eq!(value["status"], "crashed");
        assert_eq!(value["available"], false);
        assert_eq!(value["reason_code"], "server_crashed");
    }

    #[test]
    fn go_status_selects_gopls_without_changing_rust_first_precedence() {
        let go = LspStatusResult {
            project: "demo".to_string(),
            detected_languages: vec!["go".to_string()],
            servers: vec![LspServerStatusEntry {
                language: "go".to_string(),
                server: "gopls".to_string(),
                available: true,
                running: false,
                status: LspAvailabilityStatus::Available,
                source: None,
                position_encoding: None,
            }],
            warnings: Vec::new(),
        };
        let summary = SemanticNavigationStartupSummary::from_lsp_status(go, "demo").unwrap();
        let value = serde_json::to_value(summary).unwrap();
        assert_eq!(value["language"], "go");
        assert_eq!(value["server"], "gopls");
        assert_eq!(value["available"], true);
        assert!(value["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "go_only"));

        let mixed = LspStatusResult {
            project: "demo".to_string(),
            detected_languages: vec!["rust".to_string(), "go".to_string()],
            servers: vec![
                LspServerStatusEntry {
                    language: "go".to_string(),
                    server: "gopls".to_string(),
                    available: true,
                    running: false,
                    status: LspAvailabilityStatus::Available,
                    source: None,
                    position_encoding: None,
                },
                LspServerStatusEntry {
                    language: "rust".to_string(),
                    server: "rust-analyzer".to_string(),
                    available: true,
                    running: false,
                    status: LspAvailabilityStatus::Available,
                    source: None,
                    position_encoding: None,
                },
            ],
            warnings: Vec::new(),
        };
        let summary = SemanticNavigationStartupSummary::from_lsp_status(mixed, "demo").unwrap();
        let value = serde_json::to_value(summary).unwrap();
        assert_eq!(value["language"], "rust");
        assert_eq!(value["server"], "rust-analyzer");
    }

    #[test]
    fn runner_semantic_navigation_names_preserve_legacy_wire_codes() {
        assert_eq!(
            serde_json::to_value(SemanticNavigationStartupStatus::RunnerUnavailable).unwrap(),
            "agent_unavailable"
        );
        assert_eq!(
            serde_json::to_value(SemanticNavigationStartupStatus::RunnerCapabilityUnavailable)
                .unwrap(),
            "agent_capability_unavailable"
        );
        assert_eq!(
            serde_json::to_value(SemanticNavigationReasonCode::RunnerNotConnected).unwrap(),
            "agent_not_connected"
        );
        assert_eq!(
            serde_json::to_value(SemanticNavigationReasonCode::MalformedRunnerResult).unwrap(),
            "malformed_agent_result"
        );
    }

    #[test]
    fn enqueue_error_classification_uses_variants_not_display_text() {
        let cases = [
            (
                EnqueueLspError::UnknownRunner {
                    client_id: "runner does not support navigation".to_string(),
                },
                SemanticNavigationStartupStatus::RunnerUnavailable,
                SemanticNavigationReasonCode::RunnerNotConnected,
            ),
            (
                EnqueueLspError::UnsupportedCapability {
                    client_id: "unknown shell client wording".to_string(),
                    capability: crate::runner_protocol::RUNNER_CAPABILITY_LSP_READ_ONLY_NAVIGATION,
                },
                SemanticNavigationStartupStatus::RunnerCapabilityUnavailable,
                SemanticNavigationReasonCode::LspCapabilityNotAdvertised,
            ),
            (
                EnqueueLspError::QueueFull {
                    client_id: "does not support".to_string(),
                    limit: 256,
                },
                SemanticNavigationStartupStatus::ProbeFailed,
                SemanticNavigationReasonCode::StatusProbeFailed,
            ),
        ];

        for (error, expected_status, expected_reason) in cases {
            let displayed = error.to_string();
            let summary = SemanticNavigationStartupSummary::from_enqueue_error(&error);
            assert_eq!(summary.status, expected_status, "{displayed}");
            assert_eq!(summary.reason_code, Some(expected_reason), "{displayed}");
        }
    }
}
