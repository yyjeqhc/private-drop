//! Canonical project onboarding and readiness application service.
//!
//! Configuration lives outside the Git checkout. CLI, Connector HTTP, and the
//! Browser console project the same structured readiness facts; none parse
//! human-formatted output to decide whether coding is ready.

#[path = "project_entry_client_handoff.rs"]
mod client_handoff_service;
#[path = "project_entry_cloudflared.rs"]
mod cloudflared_service;
#[path = "project_entry_openai_tunnel.rs"]
mod openai_tunnel_service;
#[path = "project_entry_regular_tunnel.rs"]
mod regular_tunnel_service;
#[path = "project_entry_setup.rs"]
mod setup_service;
#[path = "project_entry_share.rs"]
mod share_service;
#[cfg(windows)]
#[path = "project_entry_windows.rs"]
mod windows_private_state;

pub(crate) use regular_tunnel_service::{run_regular_server_tunnel, RegularServerTunnelOptions};
use setup_service::{
    create_private_dir, local_readiness, prepare_runtime_private_state, read_private_value,
    read_project_agent_token, read_project_credential, read_toml_optional,
    validate_agent_authentication, validate_existing_registration, validate_existing_runner,
    validate_product_config, validate_profile, ProjectConfig,
};
pub(crate) use setup_service::{resolve_local_task_state, setup};
#[cfg(test)]
pub(crate) use share_service::TunnelProvider;
pub(crate) use share_service::{parse_share_options, share, ShareCommandOptions};

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

const DEFAULT_PROFILE: &str = "personal";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECTOR_PROJECT_REGISTRY_DIR_ENV: &str = "WEBCODEX_CONNECTOR_PROJECT_REGISTRY_DIR";
const LEGACY_CONNECTOR_PROJECTS_DIR_ENV: &str = "WEBCODEX_CONNECTOR_PROJECTS_DIR";

const NPM_WRAPPER_NETWORK_ENV_KEYS: [&str; 8] = [
    "npm_config_https_proxy",
    "npm_config_proxy",
    "npm_config_noproxy",
    "npm_config_no_proxy",
    "npm_config_cafile",
    "npm_config_ca",
    "npm_config_strict_ssl",
    "WEBCODEX_NPM_WRAPPER",
];

fn remove_npm_wrapper_network_environment(command: &mut Command) {
    for key in NPM_WRAPPER_NETWORK_ENV_KEYS {
        command.env_remove(key);
    }
}

fn remove_runner_parent_credentials(command: &mut Command) {
    for key in ["WEBCODEX_TOKEN", "WEBCODEX_PAT", "WEBCODEX_AGENT_TOKEN"] {
        command.env_remove(key);
    }
}

fn configure_connector_project_registry_environment(command: &mut Command, path: &Path) {
    // `Command` inherits the parent environment. Clear the pre-0.4 alias before
    // setting the canonical variable so a stale shell/service environment cannot
    // make the child Server observe both names and fail its dual-alias fence.
    command
        .env_remove(LEGACY_CONNECTOR_PROJECTS_DIR_ENV)
        .env(CONNECTOR_PROJECT_REGISTRY_DIR_ENV, path);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectCommandOptions {
    pub root: PathBuf,
    pub profile: String,
    pub state_dir: Option<PathBuf>,
    pub json: bool,
    pub console_assets_dir: Option<PathBuf>,
}

impl ProjectCommandOptions {
    pub(crate) fn current() -> Result<Self, String> {
        Ok(Self {
            root: std::env::current_dir().map_err(|error| format!("cannot read cwd: {error}"))?,
            profile: DEFAULT_PROFILE.to_string(),
            state_dir: None,
            json: false,
            console_assets_dir: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProductError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub user_action_required: bool,
    pub next_action: Option<String>,
}

impl ProductError {
    fn new(code: &str, message: impl Into<String>, next_action: Option<&str>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            retryable: matches!(code, "server_unreachable" | "agent_offline"),
            user_action_required: true,
            next_action: next_action.map(str::to_string),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SetupReport {
    pub project: String,
    pub connection_url: String,
    pub status: String,
    pub changed: Vec<String>,
    pub next_action: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReadinessStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReadinessFact {
    pub name: String,
    pub status: ReadinessStatus,
    pub code: String,
    pub summary: String,
    pub next_action: Option<String>,
}

impl ReadinessFact {
    fn pass(name: &str, code: &str, summary: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: ReadinessStatus::Pass,
            code: code.to_string(),
            summary: summary.into(),
            next_action: None,
        }
    }

    fn warn(name: &str, code: &str, summary: impl Into<String>, action: &str) -> Self {
        Self {
            name: name.to_string(),
            status: ReadinessStatus::Warn,
            code: code.to_string(),
            summary: summary.into(),
            next_action: Some(action.to_string()),
        }
    }

    fn fail(name: &str, code: &str, summary: impl Into<String>, action: &str) -> Self {
        Self {
            name: name.to_string(),
            status: ReadinessStatus::Fail,
            code: code.to_string(),
            summary: summary.into(),
            next_action: Some(action.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProjectReadiness {
    pub project: Option<String>,
    pub connection: String,
    pub agent: String,
    pub capabilities: String,
    pub ready: bool,
    pub next_action: Option<String>,
    pub findings: Vec<ReadinessFact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteProbe {
    Unreachable,
    CredentialRejected,
    Ready,
    RunnerOffline,
    ProjectMissing,
    RequiredCapabilityMissing,
    StructuredValidationMissing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalTaskState {
    pub root: PathBuf,
    pub state: PathBuf,
    pub data: PathBuf,
    pub runs: PathBuf,
    pub project_registry: PathBuf,
    pub cargo_target: PathBuf,
    pub logical_project_id: String,
}

pub(crate) fn parse_options(
    args: &[String],
    command: &str,
) -> Result<ProjectCommandOptions, String> {
    let mut options = ProjectCommandOptions::current()?;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = |index: &mut usize| -> Result<String, String> {
            *index += 1;
            args.get(*index)
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag {
            "--root" => options.root = PathBuf::from(value(&mut index)?),
            "--profile" => options.profile = value(&mut index)?,
            "--state-dir" => options.state_dir = Some(PathBuf::from(value(&mut index)?)),
            "--json" if command != "run" => options.json = true,
            "--console-assets-dir" if command == "run" => {
                let directory = PathBuf::from(value(&mut index)?);
                if !directory.is_absolute() {
                    return Err(
                        "--console-assets-dir requires an absolute directory path".to_string()
                    );
                }
                options.console_assets_dir = Some(directory);
            }
            "--help" | "-h" => return Err("help requested".to_string()),
            _ => return Err(format!("unknown {command} option '{flag}'")),
        }
        index += 1;
    }
    validate_profile(&options.profile).map_err(|error| error.message)?;
    Ok(options)
}

pub(crate) fn usage() -> &'static str {
    "Usage: webcodex share [--root PATH] [--profile NAME] [--state-dir PATH] [--json]\n\
                     [--tunnel cloudflare|openai|none] [--auth bearer|query-token|oauth]\n\
                     [--oauth-redirect-uri URL] [--public-url URL] [--no-copy-url]\n\
                     [--stop-on-stdin-eof]\n\
       webcodex status [--root PATH] [--profile NAME] [--state-dir PATH] [--json]\n\
       webcodex doctor [--root PATH] [--profile NAME] [--state-dir PATH] [--json]\n\
       webcodex setup [--root PATH] [--profile NAME] [--state-dir PATH] [--json]\n\
       webcodex run [--root PATH] [--profile NAME] [--state-dir PATH]\n\
                              [--console-assets-dir ABSOLUTE_PATH]\n\n\
`share` is the Quick Trial path: it temporarily shares this one project for ChatGPT/remote MCP,\n\
starts a local Server + Runner for the foreground lifetime, and ends when the command exits.\n\
For full daily use, configure the regular WebCodex Server + Runner flow instead. The default\n\
Cloudflare Quick Tunnel reuses or auto-manages a verified `cloudflared`. The opt-in\n\
OpenAI Secure MCP Tunnel provider uses a pinned verified `tunnel-client` and keeps\n\
the temporary WebCodex Bearer credential local. Public URL sharing best-effort\n\
copies only the MCP URL by default; `--auth query-token` explicitly opts into a\n\
single sensitive URL carrying the temporary share credential. Use `--no-copy-url`\n\
to disable clipboard access. `--stop-on-stdin-eof` is a `--json` machine-integration\n\
lifecycle hook: the foreground share exits cleanly when its supervising parent closes stdin.\n\
`setup`, `doctor`, and `run` remain the local/manual workflow; setup writes private state without\n\
starting services. `run` is the explicit foreground local runtime step. Its optional\n\
`--console-assets-dir` enables loopback-only development assets for that run.\n\
`--auth query-token` is a temporary share-only convenience for MCP clients that\n\
cannot configure a Bearer header; `--auth oauth` adds project-bound OAuth.\n\
On Windows, explicit `webcodex share` is supported. Managed Cloudflare acquisition is available on Windows x64; Windows ARM64 requires a trusted explicit/PATH cloudflared because the pinned upstream release has no official ARM64 artifact. Managed OpenAI tunnel-client supports Windows x64/arm64.\n"
}

pub(crate) fn readiness_with_probe(
    options: &ProjectCommandOptions,
    remote: RemoteProbe,
) -> ProjectReadiness {
    let local = local_readiness(options);
    let config = local.config;
    let paths = local.paths;
    let setup_present = local.setup_present;
    let can_probe_remote = local.can_probe_remote;
    let mut findings = local.findings;
    let project = config.as_ref().map(|config| config.project_name.clone());
    let mut connection = if can_probe_remote {
        "configured".to_string()
    } else if setup_present {
        "invalid".to_string()
    } else {
        "not configured".to_string()
    };
    let mut runner_status = "unknown".to_string();
    let mut capabilities = "not_ready".to_string();
    let local_complete = can_probe_remote
        && paths.as_ref().is_some_and(|paths| {
            paths
                .resolved_runner_config()
                .is_ok_and(|config| config.is_file())
                && paths.connector_key.is_file()
                && paths.agent_token.is_file()
                && paths.bootstrap_key.is_file()
        })
        && !findings
            .iter()
            .any(|finding| finding.status == ReadinessStatus::Fail);
    if can_probe_remote {
        let runtime = runtime_readiness(project.clone(), remote);
        connection = runtime.connection;
        runner_status = runtime.agent;
        capabilities = runtime.capabilities;
        findings.extend(runtime.findings);
    }
    if local_complete {
        findings.push(ReadinessFact::pass(
            "Setup",
            "setup_complete",
            "Project setup is complete.",
        ));
    }
    findings.push(gitignore_hygiene_fact(&options.root));
    let ready = local_complete
        && connection == "connected"
        && runner_status == "online"
        && capabilities == "ready";
    if ready {
        findings.push(ReadinessFact::pass(
            "Coding access",
            "ready",
            "Coding access is ready.",
        ));
    }
    let next_action = if ready {
        None
    } else {
        findings
            .iter()
            .find(|finding| finding.status == ReadinessStatus::Fail)
            .and_then(|finding| finding.next_action.clone())
            .or_else(|| Some("webcodex run".to_string()))
    };
    ProjectReadiness {
        project,
        connection,
        agent: runner_status,
        capabilities,
        ready,
        next_action,
        findings,
    }
}

pub(crate) fn runtime_readiness(project: Option<String>, probe: RemoteProbe) -> ProjectReadiness {
    let mut findings = vec![ReadinessFact::pass(
        "Connection",
        "server_reachable",
        "WebCodex is reachable.",
    )];
    let (connection, runner_status, capabilities) = match probe {
        RemoteProbe::Unreachable => {
            findings[0] = ReadinessFact::fail(
                "Connection",
                "server_unreachable",
                "WebCodex is not reachable.",
                "Run webcodex run, then retry.",
            );
            ("unreachable", "unknown", "not_ready")
        }
        RemoteProbe::CredentialRejected => {
            findings.push(ReadinessFact::fail(
                "Authentication",
                "project_credential_rejected",
                "WebCodex rejected the configured project credential.",
                "Restore the matching private credential or explicitly rotate the project setup.",
            ));
            ("connected", "unknown", "not_ready")
        }
        RemoteProbe::RunnerOffline => {
            findings.push(ReadinessFact::fail(
                "Runner",
                "agent_offline",
                "The local Runner is offline.",
                "Run webcodex run.",
            ));
            ("connected", "offline", "not_ready")
        }
        RemoteProbe::ProjectMissing => {
            findings.push(ReadinessFact::fail(
                "Project",
                "project_registration_invalid",
                "The Runner registration does not contain this project.",
                "Stop the Runner, run webcodex setup, then start it again.",
            ));
            ("connected", "online", "not_ready")
        }
        RemoteProbe::RequiredCapabilityMissing => {
            findings.push(ReadinessFact::fail(
                "Capabilities",
                "required_capability_unavailable",
                "The local Runner is missing a required coding capability.",
                "Upgrade the WebCodex Runner and restart it.",
            ));
            ("connected", "online", "not_ready")
        }
        RemoteProbe::StructuredValidationMissing => {
            findings.push(ReadinessFact::fail(
                "Capabilities",
                "structured_validation_unavailable",
                "Structured validation is unavailable.",
                "Upgrade the WebCodex Runner and restart it.",
            ));
            ("connected", "online", "not_ready")
        }
        RemoteProbe::Ready => {
            findings.push(ReadinessFact::pass(
                "Runner",
                "agent_online",
                "The local Runner is online.",
            ));
            findings.push(ReadinessFact::pass(
                "Project",
                "project_registered",
                "The current project is registered.",
            ));
            findings.push(ReadinessFact::pass(
                "Capabilities",
                "required_capabilities_available",
                "Required coding capabilities are available.",
            ));
            findings.push(ReadinessFact::pass(
                "Structured validation",
                "structured_validation_available",
                "Structured validation is available.",
            ));
            ("connected", "online", "ready")
        }
    };
    ProjectReadiness {
        project,
        connection: connection.to_string(),
        agent: runner_status.to_string(),
        capabilities: capabilities.to_string(),
        ready: probe == RemoteProbe::Ready,
        next_action: (probe != RemoteProbe::Ready).then(|| "webcodex doctor".to_string()),
        findings,
    }
}

/// Untracked build artifacts poison the workspace provenance fingerprint and
/// used to wedge check validation irrecoverably; catch the setup before the
/// first task does.
const IGNORE_HYGIENE_DIRS: &[&str] = &[
    "target/",
    "node_modules/",
    "__pycache__/",
    ".venv/",
    "venv/",
    "dist/",
    "build/",
    "coverage/",
    ".pytest_cache/",
];

fn gitignore_hygiene_fact(root: &Path) -> ReadinessFact {
    let untracked_artifacts: Vec<String> = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.strip_prefix("?? ").map(str::to_string))
                .filter(|path| {
                    IGNORE_HYGIENE_DIRS
                        .iter()
                        .any(|dir| path == dir || path.starts_with(dir))
                })
                .take(3)
                .collect()
        })
        .unwrap_or_default();
    let has_gitignore = root.join(".gitignore").exists();
    if !untracked_artifacts.is_empty() {
        ReadinessFact::warn(
            "Ignore hygiene",
            "untracked_build_artifacts",
            format!(
                "Untracked build artifacts present ({}); checks that touch them will fail \
                 workspace provenance validation.",
                untracked_artifacts.join(", ")
            ),
            "Add a .gitignore covering build artifacts (target/, __pycache__/, ...), then rerun webcodex doctor.",
        )
    } else if !has_gitignore {
        ReadinessFact::warn(
            "Ignore hygiene",
            "gitignore_missing",
            "The project has no .gitignore; checks that generate build artifacts will fail \
             workspace provenance validation.",
            "Add a .gitignore covering build artifacts before running checks.",
        )
    } else {
        ReadinessFact::pass(
            "Ignore hygiene",
            "gitignore_present",
            "No untracked build artifacts detected.",
        )
    }
}

pub(crate) async fn collect_readiness(options: &ProjectCommandOptions) -> ProjectReadiness {
    let local = local_readiness(options);
    if !local.can_probe_remote {
        return readiness_with_probe(options, RemoteProbe::Unreachable);
    }
    let (Some(config), Some(paths)) = (local.config, local.paths) else {
        return readiness_with_probe(options, RemoteProbe::Unreachable);
    };
    let key = match read_project_credential(&paths.connector_key) {
        Ok(key) => key,
        Err(_) => return readiness_with_probe(options, RemoteProbe::Unreachable),
    };
    collect_readiness_from_remote(options, &config, &key).await
}

async fn collect_readiness_from_remote(
    options: &ProjectCommandOptions,
    config: &ProjectConfig,
    key: &str,
) -> ProjectReadiness {
    let url = format!("{}/api/connector/readiness", config.server_url());
    let response = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(3))
        .build()
        .ok();
    let Some(client) = response else {
        return readiness_with_probe(options, RemoteProbe::Unreachable);
    };
    let remote = match client
        .post(url)
        .bearer_auth(key)
        .json(&serde_json::json!({}))
        .send()
        .await
    {
        Ok(response)
            if matches!(
                response.status(),
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
            ) =>
        {
            return readiness_with_probe(options, RemoteProbe::CredentialRejected);
        }
        Ok(response) if response.status().is_success() => {
            response.json::<ProjectReadiness>().await.ok()
        }
        _ => None,
    };
    match remote {
        Some(remote) => {
            let probe = remote_probe_from_readiness(&remote);
            readiness_with_probe(options, probe)
        }
        None => readiness_with_probe(options, RemoteProbe::Unreachable),
    }
}

pub(crate) fn render_setup_text(report: &SetupReport) -> String {
    let state = if report.status == "already_configured" {
        "already configured"
    } else {
        "configured"
    };
    format!(
        "Project: {}\nConnection: {state} at {}\nAgent: configured, not running\nCapabilities: ready after Agent starts\n\nNext:\n  {}\n",
        report.project, report.connection_url, report.next_action
    )
}

pub(crate) fn render_doctor_text(readiness: &ProjectReadiness) -> String {
    let mut output = format!(
        "WebCodex doctor — {}\n",
        readiness.project.as_deref().unwrap_or("current project")
    );
    for finding in &readiness.findings {
        let status = match finding.status {
            ReadinessStatus::Pass => "pass",
            ReadinessStatus::Warn => "warn",
            ReadinessStatus::Fail => "fail",
        };
        output.push_str(&format!(
            "[{status}] {}: {}\n",
            finding.name, finding.summary
        ));
    }
    output.push_str(if readiness.ready {
        "\nVerdict: Ready\n"
    } else {
        "\nVerdict: Needs action\n"
    });
    if let Some(action) = &readiness.next_action {
        output.push_str(&format!("Next:\n  {action}\n"));
    }
    output
}

pub(crate) fn render_status_text(readiness: &ProjectReadiness) -> String {
    let mut output = format!(
        "Project: {}\nConnection: {}\nAgent: {}\nCapabilities: {}\nCoding access: {}\n",
        readiness.project.as_deref().unwrap_or("not configured"),
        readiness.connection,
        readiness.agent,
        readiness.capabilities,
        if readiness.ready {
            "ready"
        } else {
            "not ready"
        }
    );
    if let Some(action) = &readiness.next_action {
        output.push_str(&format!("\nNext:\n  {action}\n"));
    }
    output
}

pub(crate) fn render_error(error: &ProductError, json: bool) -> String {
    if json {
        serde_json::to_string_pretty(&serde_json::json!({
            "ok": false,
            "error": error
        }))
        .unwrap_or_else(|_| format!("{{\"ok\":false,\"error\":{{\"code\":\"{}\"}}}}", error.code))
    } else {
        let mut output = format!("{}: {}", error.code, error.message);
        if let Some(action) = &error.next_action {
            output.push_str(&format!("\nNext:\n  {action}"));
        }
        output
    }
}

#[derive(Debug, Clone)]
pub(super) struct ProjectShareOAuthRuntimeOptions {
    pub(super) project_grant_id: String,
    pub(super) session_id: String,
}

#[derive(Debug, Clone)]
pub(super) struct LocalRuntimeOptions {
    pub(super) public_url: Option<String>,
    pub(super) connector_credential_file: Option<PathBuf>,
    pub(super) mcp_query_token_auth: bool,
    pub(super) project_share_oauth: Option<ProjectShareOAuthRuntimeOptions>,
    pub(super) child_environment_remove: Vec<&'static str>,
    pub(super) port_conflict_action: &'static str,
    pub(super) readiness_deadline: Option<Instant>,
}

impl Default for LocalRuntimeOptions {
    fn default() -> Self {
        Self {
            public_url: None,
            connector_credential_file: None,
            mcp_query_token_auth: false,
            project_share_oauth: None,
            child_environment_remove: Vec::new(),
            port_conflict_action: "Stop the conflicting process, then run webcodex run.",
            readiness_deadline: None,
        }
    }
}

pub(super) struct LocalRuntimeHandle {
    pub(super) project_name: String,
    pub(super) local_url: String,
    pub(super) public_url: String,
    pub(super) console_assets_dir: Option<PathBuf>,
    server: Child,
    runner: Child,
}

impl LocalRuntimeHandle {
    pub(super) async fn wait_for_exit(&mut self) -> Result<(), ProductError> {
        tokio::select! {
            status = self.server.wait() => Err(ProductError::new(
                "server_unreachable",
                format!("WebCodex stopped unexpectedly ({:?})", status.ok()),
                Some("Run webcodex doctor."),
            )),
            status = self.runner.wait() => Err(ProductError::new(
                "agent_offline",
                format!("the local Runner stopped unexpectedly ({:?})", status.ok()),
                Some("Run webcodex doctor."),
            )),
        }
    }

    pub(super) async fn stop(&mut self) {
        let _ = self.runner.start_kill();
        let _ = self.server.start_kill();
        let _ = self.runner.wait().await;
        let _ = self.server.wait().await;
    }
}

fn configured_project(
    options: &ProjectCommandOptions,
) -> Result<(ProjectConfig, setup_service::ProjectPaths), ProductError> {
    let (expected, paths) = ProjectConfig::resolve(options)?;
    let config = read_toml_optional::<ProjectConfig>(&paths.config)?.ok_or_else(|| {
        ProductError::new(
            "project_not_configured",
            "the current project has not been set up",
            Some("Run webcodex setup."),
        )
    })?;
    validate_product_config(&expected, &config)?;
    validate_existing_runner(&config, &paths)?;
    validate_existing_registration(&config, &paths)?;
    Ok((config, paths))
}

pub(super) fn ensure_local_runtime_port_available(
    port: u16,
    port_conflict_action: &'static str,
) -> Result<(), ProductError> {
    if TcpListener::bind(("127.0.0.1", port)).is_err() {
        return Err(ProductError::new(
            "server_unreachable",
            "the configured loopback port is already in use",
            Some(port_conflict_action),
        ));
    }
    Ok(())
}

pub(super) async fn start_local_runtime(
    options: &ProjectCommandOptions,
    runtime_options: LocalRuntimeOptions,
) -> Result<LocalRuntimeHandle, ProductError> {
    let console_assets_dir = resolve_console_assets_directory(options)?;
    let (config, paths) = configured_project(options)?;
    prepare_runtime_private_state(&paths)?;
    ensure_local_runtime_port_available(config.port, runtime_options.port_conflict_action)?;
    let readiness_deadline = runtime_options
        .readiness_deadline
        .unwrap_or_else(|| Instant::now() + START_TIMEOUT);
    let project_share_oauth = runtime_options.project_share_oauth.clone();
    let mcp_query_token_auth = runtime_options.mcp_query_token_auth;
    let runner_binary = locate_runner_binary().ok_or_else(|| {
        ProductError::new(
            "required_capability_unavailable",
            "the WebCodex Runner executable is unavailable",
            Some("Install all WebCodex binaries, then run webcodex doctor."),
        )
    })?;
    let bootstrap = read_private_value(&paths.bootstrap_key)?;
    let credential_file = runtime_options
        .connector_credential_file
        .unwrap_or_else(|| paths.connector_key.clone());
    let connector_key = read_project_credential(&credential_file)?;
    let _agent_token = read_project_agent_token(&paths.agent_token)?;
    validate_agent_authentication(&config, &paths)?;
    let server_binary = locate_companion_binary("webcodex-server").ok_or_else(|| {
        ProductError::new(
            "required_capability_unavailable",
            "the WebCodex Server executable is unavailable",
            Some("Install all WebCodex binaries, then run webcodex doctor."),
        )
    })?;
    let local_url = config.server_url();
    let public_url = runtime_options
        .public_url
        .unwrap_or_else(|| local_url.clone());
    let server_log = open_log(&paths.logs.join("server.log"))?;
    let server_error = server_log.try_clone().map_err(io_error)?;
    let mut server_command = Command::new(server_binary);
    remove_npm_wrapper_network_environment(&mut server_command);
    for name in &runtime_options.child_environment_remove {
        server_command.env_remove(name);
    }
    configure_connector_project_registry_environment(&mut server_command, &paths.project_registry);
    server_command
        .current_dir(&paths.state)
        .env_remove("WEBCODEX_ENV_FILE")
        .env("WEBCODEX_ADDR", format!("127.0.0.1:{}", config.port))
        .env("WEBCODEX_DATA", &paths.data)
        .env("WEBCODEX_TOKEN", bootstrap)
        .env("WEBCODEX_SHARED_KEY_ENABLED", "false")
        .env("WEBCODEX_ALLOW_ANONYMOUS", "false")
        .env("WEBCODEX_PUBLIC_URL", &public_url)
        .env("WEBCODEX_OAUTH2_SHARED_KEY_BRIDGE", "false")
        .env("WEBCODEX_OAUTH2_REQUIRE_PKCE", "true")
        .env("WEBCODEX_OAUTH2_ACCESS_TOKEN_TTL_SECS", "3600")
        .env("WEBCODEX_OAUTH2_REFRESH_TOKEN_TTL_SECS", "2592000")
        .env("WEBCODEX_OAUTH2_AUTH_CODE_TTL_SECS", "300")
        .env("WEBCODEX_OAUTH2_TRUSTED_MCP_FILE_CLIENT_IDS", "")
        .env("WEBCODEX_QUIC_ENABLED", "false")
        .env("WEBCODEX_CONNECTOR_SURFACE", "task-v1")
        .env(
            "WEBCODEX_CONNECTOR_PROJECT_GRANT_ID",
            config.project_grant_id(&paths),
        )
        .env("WEBCODEX_PROJECT_CREDENTIAL_FILE", &credential_file)
        .env(
            "WEBCODEX_PROJECT_SHARE_MCP_QUERY_TOKEN_ENABLED",
            if mcp_query_token_auth {
                "true"
            } else {
                "false"
            },
        )
        .env("WEBCODEX_PROJECT_AGENT_TOKEN_FILE", &paths.agent_token)
        .env("WEBCODEX_CONNECTOR_PROJECT_ID", &config.logical_project_id)
        .env("WEBCODEX_CONNECTOR_PROJECT_NAME", &config.project_name)
        .env("WEBCODEX_CONNECTOR_WORKSPACE_ID", &config.workspace_id)
        .env(
            "WEBCODEX_CONNECTOR_EXECUTOR_PROJECT",
            config.runtime_project_id(),
        )
        .env("WEBCODEX_CONNECTOR_EXECUTOR_ROOT", &config.root)
        .env("WEBCODEX_CONNECTOR_RUNS_ROOT", &paths.runs)
        .env("WEBCODEX_CONNECTOR_RESULTS_ROOT", &paths.results)
        .env("WEBCODEX_CONNECTOR_PROFILE", &config.profile)
        .stdout(Stdio::from(server_log))
        .stderr(Stdio::from(server_error))
        .kill_on_drop(true);
    if let Some(oauth) = project_share_oauth {
        server_command
            .env("WEBCODEX_OAUTH2_ENABLED", "true")
            .env("WEBCODEX_OAUTH2_ISSUER", &public_url)
            .env(
                "WEBCODEX_OAUTH2_PROJECT_SHARE_GRANT_ID",
                oauth.project_grant_id,
            )
            .env("WEBCODEX_OAUTH2_PROJECT_SHARE_SESSION_ID", oauth.session_id);
    } else {
        server_command
            .env("WEBCODEX_OAUTH2_ENABLED", "false")
            .env_remove("WEBCODEX_OAUTH2_ISSUER")
            .env_remove("WEBCODEX_OAUTH2_PROJECT_SHARE_GRANT_ID")
            .env_remove("WEBCODEX_OAUTH2_PROJECT_SHARE_SESSION_ID");
    }
    configure_console_assets_environment(&mut server_command, console_assets_dir.as_deref());
    let mut server = server_command.spawn().map_err(|_| {
        ProductError::new(
            "server_unreachable",
            "WebCodex could not start",
            Some("Run webcodex doctor."),
        )
    })?;
    if let Err(error) =
        wait_for_server(&mut server, &local_url, &connector_key, readiness_deadline).await
    {
        stop_child(&mut server).await;
        return Err(error);
    }

    let runner_log = open_log(&paths.logs.join("agent.log"))?;
    let runner_error = runner_log.try_clone().map_err(io_error)?;
    let runner_config = paths.resolved_runner_config()?;
    let mut runner_command = Command::new(runner_binary);
    remove_npm_wrapper_network_environment(&mut runner_command);
    for name in &runtime_options.child_environment_remove {
        runner_command.env_remove(name);
    }
    remove_runner_parent_credentials(&mut runner_command);
    runner_command
        .arg("--config")
        .arg(&runner_config)
        .current_dir(&paths.state)
        .stdout(Stdio::from(runner_log))
        .stderr(Stdio::from(runner_error))
        .kill_on_drop(true);
    let mut runner = match runner_command.spawn() {
        Ok(runner) => runner,
        Err(_) => {
            stop_child(&mut server).await;
            return Err(ProductError::new(
                "agent_offline",
                "the local Runner could not start",
                Some("Run webcodex doctor."),
            ));
        }
    };
    if let Err(error) = wait_for_ready(
        &mut server,
        &mut runner,
        options,
        &config,
        &connector_key,
        readiness_deadline,
    )
    .await
    {
        stop_child(&mut runner).await;
        stop_child(&mut server).await;
        return Err(error);
    }
    Ok(LocalRuntimeHandle {
        project_name: config.project_name,
        local_url,
        public_url,
        console_assets_dir,
        server,
        runner,
    })
}

pub(crate) async fn start_runner(options: &ProjectCommandOptions) -> Result<(), ProductError> {
    let mut runtime = start_local_runtime(options, LocalRuntimeOptions::default()).await?;
    let mut started = format!(
        "Project: {}\nConnection: connected at {}\nConsole: {}/console\nConsole assets: {}",
        runtime.project_name,
        runtime.local_url,
        runtime.local_url,
        if runtime.console_assets_dir.is_some() {
            "local development"
        } else {
            "embedded"
        }
    );
    if let Some(directory) = &runtime.console_assets_dir {
        started.push_str(&format!("\nAssets directory: {}", directory.display()));
    }
    started.push_str("\nRunner: online\nCoding access: ready\n\nPress Ctrl-C to stop.");
    println!("{started}");
    let outcome = tokio::select! {
        _ = wait_for_local_runtime_stop_signal() => Ok(()),
        result = runtime.wait_for_exit() => result,
    };
    runtime.stop().await;
    outcome
}

#[cfg(not(windows))]
async fn wait_for_local_runtime_stop_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(windows)]
async fn wait_for_local_runtime_stop_signal() {
    let mut ctrl_break = match tokio::signal::windows::ctrl_break() {
        Ok(signal) => signal,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = ctrl_break.recv() => {},
    }
}

fn resolve_console_assets_directory(
    options: &ProjectCommandOptions,
) -> Result<Option<PathBuf>, ProductError> {
    let Some(directory) = options.console_assets_dir.as_deref() else {
        return Ok(None);
    };
    let source =
        crate::console_web::ConsoleAssetSource::from_directory(directory).map_err(|error| {
            ProductError::new(
                "console_assets_invalid",
                error.to_string(),
                Some("Fix the development build directory, then retry."),
            )
        })?;
    Ok(source.directory().map(Path::to_path_buf))
}

fn configure_console_assets_environment(command: &mut Command, directory: Option<&Path>) {
    command.env_remove(crate::console_web::CONSOLE_ASSETS_DIR_ENV);
    if let Some(directory) = directory {
        command.env(crate::console_web::CONSOLE_ASSETS_DIR_ENV, directory);
    }
}

fn remote_probe_from_readiness(readiness: &ProjectReadiness) -> RemoteProbe {
    for (code, probe) in [
        (
            "structured_validation_unavailable",
            RemoteProbe::StructuredValidationMissing,
        ),
        (
            "required_capability_unavailable",
            RemoteProbe::RequiredCapabilityMissing,
        ),
        ("project_registration_invalid", RemoteProbe::ProjectMissing),
        ("agent_offline", RemoteProbe::RunnerOffline),
    ] {
        if readiness
            .findings
            .iter()
            .any(|finding| finding.code == code && finding.status == ReadinessStatus::Fail)
        {
            return probe;
        }
    }
    if readiness.ready {
        RemoteProbe::Ready
    } else {
        RemoteProbe::Unreachable
    }
}

fn locate_runner_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("WEBCODEX_AGENT_BIN").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }
    locate_companion_binary("webcodex-runner")
}

fn locate_companion_binary(name: &str) -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let parent = current.parent()?;
    for candidate in [
        parent.join(executable_name(name)),
        parent
            .parent()
            .map(|path| path.join(executable_name(name)))
            .unwrap_or_default(),
    ] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(executable_name(name)))
        .find(|candidate| candidate.is_file())
}

pub(super) fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn runner_runtime_available() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        locate_runner_binary().is_some()
    }
}

fn open_log(path: &Path) -> Result<File, ProductError> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|_| {
            ProductError::new(
                "workspace_unavailable",
                "WebCodex could not open its local log",
                Some("Check local filesystem permissions, then retry."),
            )
        })
}

async fn stop_child(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

fn io_error(_: std::io::Error) -> ProductError {
    ProductError::new(
        "workspace_unavailable",
        "WebCodex could not prepare local process output",
        Some("Check local filesystem permissions, then retry."),
    )
}

async fn wait_for_server(
    server: &mut Child,
    base_url: &str,
    key: &str,
    deadline: Instant,
) -> Result<(), ProductError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|_| {
            ProductError::new(
                "server_unreachable",
                "WebCodex readiness client could not start",
                Some("Run webcodex doctor."),
            )
        })?;
    while Instant::now() < deadline {
        if server.try_wait().ok().flatten().is_some() {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let response = tokio::time::timeout(
            remaining,
            client
                .post(format!("{base_url}/api/connector/readiness"))
                .bearer_auth(key)
                .json(&serde_json::json!({}))
                .send(),
        )
        .await
        .ok()
        .and_then(Result::ok);
        if response.is_some_and(|response| response.status().is_success()) {
            return Ok(());
        }
        tokio::time::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        )
        .await;
    }
    Err(ProductError::new(
        "server_unreachable",
        "WebCodex did not become reachable",
        Some("Run webcodex doctor."),
    ))
}

async fn wait_for_ready(
    server: &mut Child,
    runner: &mut Child,
    options: &ProjectCommandOptions,
    config: &ProjectConfig,
    connector_key: &str,
    deadline: Instant,
) -> Result<(), ProductError> {
    while Instant::now() < deadline {
        if server.try_wait().ok().flatten().is_some() {
            return Err(ProductError::new(
                "server_unreachable",
                "WebCodex stopped during startup",
                Some("Run webcodex doctor."),
            ));
        }
        if runner.try_wait().ok().flatten().is_some() {
            return Err(ProductError::new(
                "agent_offline",
                "the local Runner stopped during startup",
                Some("Run webcodex doctor."),
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if tokio::time::timeout(
            remaining,
            collect_readiness_from_remote(options, config, connector_key),
        )
        .await
        .is_ok_and(|readiness| readiness.ready)
        {
            return Ok(());
        }
        tokio::time::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(150)),
        )
        .await;
    }
    Err(ProductError::new(
        "agent_offline",
        "the local Runner did not become ready",
        Some("Run webcodex doctor."),
    ))
}

#[cfg(all(test, windows))]
#[path = "project_entry_windows_migration_tests.rs"]
mod windows_migration_tests;

#[cfg(test)]
#[path = "project_entry_tests.rs"]
mod tests;
