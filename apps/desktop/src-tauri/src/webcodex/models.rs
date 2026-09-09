use serde::Deserialize;

#[derive(Deserialize)]
pub struct PairingCreateOutput {
    pub pairing_code: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisteredProjectOutput {
    pub id: String,
    pub path: String,
    pub runtime_project: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectActivationOutput {
    pub client_id: String,
    pub project: RegisteredProjectOutput,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyProjectRegisterOutput {
    pub project: LegacyRegisteredProjectOutput,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyRegisteredProjectOutput {
    pub id: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginOutput {
    pub server_url: String,
    pub runner_config: String,
    pub user_token_file: String,
    #[serde(default)]
    pub registered_projects: Vec<RegisteredProjectOutput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerStatusOutput {
    pub http_reachable: bool,
    pub probe_url: String,
    #[serde(default)]
    pub revision_check: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunnerStatusOutput {
    pub config: RunnerConfigOutput,
    #[serde(default)]
    pub runtime: Option<RunnerRuntimeOutput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunnerConfigOutput {
    pub path: String,
    pub client_id: String,
    pub server_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunnerRuntimeOutput {
    #[serde(default)]
    pub checked: bool,
    #[serde(default)]
    pub reachable: Option<bool>,
    #[serde(default)]
    pub client_online: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpsProjectsOutput {
    pub summary: OpsProjectsSummary,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpsProjectsSummary {
    #[serde(default)]
    pub projects: Vec<OpsProject>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpsProject {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub connected: Option<bool>,
    #[serde(default)]
    pub agent_status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpsWindowsOutput {
    pub summary: OpsWindowsSummary,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpsWindowsSummary {
    #[serde(default)]
    pub windows: Vec<OpsWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpsWindow {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub last_meaningful_activity_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuickShareReadyEvent {
    pub event: String,
    pub schema_version: u64,
    pub experience: String,
    pub project: String,
    pub exposure: QuickShareExposure,
    pub connection: QuickShareConnection,
    pub ready_for_chatgpt: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuickShareExposure {
    pub kind: String,
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuickShareConnection {
    #[serde(default)]
    pub mcp_url: Option<String>,
    pub clipboard_state: String,
    pub clipboard_contains: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegularTunnelReadyEvent {
    pub event: String,
    pub schema_version: u64,
    pub provider: String,
    pub ready_for_chatgpt: bool,
    pub connection: RegularTunnelConnection,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegularTunnelConnection {
    pub kind: String,
    pub clipboard_state: String,
    pub clipboard_contains: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_contracts_tolerate_unknown_fields_and_missing_optional_metadata() {
        let login: LoginOutput = serde_json::from_str(
            r#"{
                "server_url":"https://example.test",
                "runner_config":"C:\\state\\runner.toml",
                "user_token_file":"C:\\state\\user-token",
                "future_metadata":{"generation":3}
            }"#,
        )
        .unwrap();
        assert!(login.registered_projects.is_empty());

        let runner: RunnerStatusOutput = serde_json::from_str(
            r#"{
                "config":{
                    "path":"C:\\state\\runner.toml",
                    "client_id":"desktop-runner",
                    "server_url":"https://example.test",
                    "future":"ignored"
                },
                "future_top_level":true
            }"#,
        )
        .unwrap();
        assert!(runner.runtime.is_none());

        let share: QuickShareReadyEvent = serde_json::from_str(
            r#"{
                "event":"ready",
                "schema_version":1,
                "experience":"quick_share",
                "project":"fixture",
                "exposure":{"kind":"cloudflare","state":"remote_ready","future":1},
                "connection":{
                    "clipboard_state":"copied",
                    "clipboard_contains":"bearer_credential",
                    "future":2
                },
                "ready_for_chatgpt":true,
                "future_top_level":3
            }"#,
        )
        .unwrap();
        assert!(share.connection.mcp_url.is_none());

        let tunnel: RegularTunnelReadyEvent = serde_json::from_str(
            r#"{
                "event":"ready",
                "schema_version":1,
                "provider":"openai",
                "ready_for_chatgpt":true,
                "connection":{
                    "kind":"openai_tunnel",
                    "clipboard_state":"copied",
                    "clipboard_contains":"tunnel_id",
                    "future":1
                },
                "future_top_level":2
            }"#,
        )
        .unwrap();
        assert_eq!(tunnel.connection.kind, "openai_tunnel");
    }

    #[test]
    fn json_contracts_fail_closed_when_required_identity_is_missing() {
        let invalid = serde_json::from_str::<RunnerStatusOutput>(
            r#"{
                "config":{
                    "path":"C:\\state\\runner.toml",
                    "server_url":"https://example.test"
                }
            }"#,
        );
        assert!(invalid.is_err());
    }
}
