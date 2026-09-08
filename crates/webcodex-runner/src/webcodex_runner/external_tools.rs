//! Allowlisted external tool backends for agent file operations.
//!
//! The first provider is a deliberately small stdio MCP client for
//! `claude mcp serve`. Native execution remains the default.

use super::config::{ClaudeCodeMcpConfig, ToolProviderStrategy, ToolProvidersConfig};
use super::output::CommandResult;
use super::shell::cwd_allowed;
use super::shutdown::{lock_unpoison, SHUTDOWN_POLL_INTERVAL};
use super::RunnerPolicy;
#[cfg(test)]
use crate::runner_protocol::RunnerRequest;
use crate::runner_protocol::{
    ClaudeCodeProviderStatus, ProviderCallSummary, ToolProvidersStatus,
    EXTERNAL_SEARCH_REQUEST_PREFIX,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
#[cfg(windows)]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
#[cfg(test)]
use webcodex_core::runner_operation::RunnerOperation;
use webcodex_core::runner_operation::RunnerShellOperation;

use webcodex_process::{GracefulTermination, ManagedChild};

const MAX_DISCOVERED_TOOLS: usize = 64;
const MAX_DISCOVERED_TOOL_SCHEMA_BYTES: usize = 64 * 1024;

struct DiscoveredTool {
    fields: BTreeSet<String>,
}

fn discovered_tool_entry(tool: &Value) -> Option<(String, DiscoveredTool)> {
    let name = tool.get("name")?.as_str()?.to_string();
    sanitize_tool_name(&name)?;
    let input_schema = tool
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    let schema_within_bound = serde_json::to_vec(&input_schema)
        .is_ok_and(|encoded| encoded.len() <= MAX_DISCOVERED_TOOL_SCHEMA_BYTES);
    let fields = if schema_within_bound {
        input_schema
            .pointer("/properties")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|properties| properties.keys().cloned())
            .collect()
    } else {
        BTreeSet::new()
    };
    Some((name, DiscoveredTool { fields }))
}

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_MCP_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_MCP_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_PENDING_REQUESTS: usize = 32;
const MCP_TERMINATION_GRACE: Duration = Duration::from_millis(250);
const MCP_FALLBACK_SHUTDOWN_BUDGET: Duration = Duration::from_secs(1);

const MCP_RUNNING: usize = 0;
const MCP_TERM_SENT: usize = 1;
const MCP_KILL_SENT: usize = 2;
const MCP_REAPED: usize = 3;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExternalShutdownOutcome {
    pub(crate) connections: usize,
    pub(crate) timed_out: usize,
    pub(crate) failures: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ProviderCapability {
    SearchProjectText,
}

impl ProviderCapability {
    fn name(self) -> &'static str {
        "search_project_text"
    }
}

#[derive(Debug, Clone)]
struct ProviderError {
    code: &'static str,
}

impl ProviderError {
    fn new(code: &'static str) -> Self {
        Self { code }
    }
}

struct ToolExecutionContext<'a> {
    project_root: &'a Path,
    target: PathBuf,
    max_output_bytes: usize,
    timeout_secs: u64,
}

pub(crate) enum ExternalRoute {
    Native,
    NativeFallback(NativeFallback),
    Handled(CommandResult),
}

pub(crate) struct NativeFallback {
    capability: ProviderCapability,
    started: Instant,
}

pub(crate) struct ExternalToolRouter {
    strategy: ToolProviderStrategy,
    claude: ClaudeCodeMcpProvider,
    metadata_revision: AtomicU64,
    observed_provider_revision: AtomicU64,
    sent_status_revision: AtomicU64,
    claimed_status_revision: AtomicU64,
}

impl ExternalToolRouter {
    pub(crate) fn new(config: &ToolProvidersConfig) -> Self {
        Self {
            strategy: config.strategy,
            claude: ClaudeCodeMcpProvider::new(config.claude_code.clone()),
            metadata_revision: AtomicU64::new(1),
            observed_provider_revision: AtomicU64::new(1),
            sent_status_revision: AtomicU64::new(0),
            claimed_status_revision: AtomicU64::new(0),
        }
    }

    pub(crate) fn shutdown_until(&self, deadline: Instant) -> ExternalShutdownOutcome {
        self.claude.shutdown_until(deadline)
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> ToolProvidersStatus {
        self.status_with_revision().0
    }

    #[cfg(test)]
    pub(crate) fn configured_search_tool_name(&self) -> Option<&str> {
        self.claude
            .config
            .mapping
            .get(ProviderCapability::SearchProjectText.name())
            .map(String::as_str)
    }

    fn status_with_revision(&self) -> (ToolProvidersStatus, u64) {
        let (claude_code, provider_revision) = self.claude.status_with_revision();
        if self
            .observed_provider_revision
            .fetch_max(provider_revision, Ordering::SeqCst)
            < provider_revision
        {
            self.metadata_revision.fetch_add(1, Ordering::SeqCst);
        }
        (
            ToolProvidersStatus {
                strategy: self.strategy_name().to_string(),
                claude_code,
                config_reload: Default::default(),
            },
            self.metadata_revision.load(Ordering::SeqCst),
        )
    }

    pub(crate) fn configuration_status_changed(&self) {
        self.metadata_revision.fetch_add(1, Ordering::SeqCst);
    }

    fn strategy_name(&self) -> &'static str {
        match self.strategy {
            ToolProviderStrategy::Native => "native",
            ToolProviderStrategy::ClaudeCode => "claude_code",
            ToolProviderStrategy::ClaudeCodeThenNative => "claude_code_then_native",
        }
    }

    /// Claim one changed status revision for an existing transport message.
    /// Snapshotting completes before the caller performs any network I/O.
    pub(crate) fn claim_status_update(&self) -> Option<(ToolProvidersStatus, u64)> {
        if self.claimed_status_revision.load(Ordering::SeqCst) != 0 {
            return None;
        }
        let (status, revision) = self.status_with_revision();
        if revision <= self.sent_status_revision.load(Ordering::SeqCst) {
            return None;
        }
        self.claimed_status_revision
            .compare_exchange(0, revision, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| (status, revision))
    }

    pub(crate) fn mark_status_reported(&self, revision: u64) {
        self.sent_status_revision
            .fetch_max(revision, Ordering::SeqCst);
        let _ = self.claimed_status_revision.compare_exchange(
            revision,
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    pub(crate) fn release_status_update(&self, revision: u64) {
        let _ = self.claimed_status_revision.compare_exchange(
            revision,
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    pub(crate) fn registration_status(&self) -> (ToolProvidersStatus, u64) {
        self.status_with_revision()
    }

    #[cfg(test)]
    pub(crate) fn route(&self, policy: &RunnerPolicy, request: &RunnerRequest) -> ExternalRoute {
        match request.decode_operation() {
            Ok(RunnerOperation::RunShell(operation)) => {
                self.route_with_shutdown(policy, &operation, None)
            }
            _ => ExternalRoute::Native,
        }
    }

    pub(crate) fn route_with_shutdown(
        &self,
        policy: &RunnerPolicy,
        operation: &RunnerShellOperation,
        shutdown: Option<&AtomicBool>,
    ) -> ExternalRoute {
        if self.strategy == ToolProviderStrategy::Native {
            return ExternalRoute::Native;
        }
        let capability = if operation.command.lines().next() == Some(EXTERNAL_SEARCH_REQUEST_PREFIX)
        {
            ProviderCapability::SearchProjectText
        } else {
            return ExternalRoute::Native;
        };
        let started = Instant::now();
        let raw = operation.stdin.as_deref();
        let payload = match raw
            .ok_or_else(request_error)
            .and_then(|raw| serde_json::from_str(raw).map_err(|_| request_error()))
        {
            Ok(payload) => payload,
            Err(error) => return self.failure_or_native(capability, error, started),
        };
        let checked = validate_context(policy, operation.cwd.as_deref(), capability, &payload);
        let (root, target) = match checked {
            Ok(checked) => checked,
            Err(error) => {
                self.claude.record_error(&error);
                self.claude.record_call(
                    call_summary(
                        capability,
                        "claude_code",
                        false,
                        false,
                        started,
                        Some(error.code),
                    ),
                    false,
                );
                return ExternalRoute::Handled(provider_error_result(capability, error, started));
            }
        };
        let context = ToolExecutionContext {
            project_root: &root,
            target,
            max_output_bytes: MAX_MCP_OUTPUT_BYTES.min(policy.max_output_bytes),
            timeout_secs: operation.timeout_secs.max(1).min(policy.max_timeout_secs),
        };
        match self
            .claude
            .call_with_shutdown(capability, payload, context, shutdown)
        {
            Ok(output) => {
                self.claude.record_call(
                    call_summary(capability, "claude_code", false, true, started, None),
                    true,
                );
                let stdout = output
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| output.to_string());
                let exit_code = normalized_search_exit_code(&stdout);
                ExternalRoute::Handled(command_result_with_exit(stdout, exit_code, started))
            }
            Err(error) => self.failure_or_native(capability, error, started),
        }
    }

    fn failure_or_native(
        &self,
        capability: ProviderCapability,
        error: ProviderError,
        started: Instant,
    ) -> ExternalRoute {
        self.claude.record_error(&error);
        if self.strategy == ToolProviderStrategy::ClaudeCodeThenNative
            && !matches!(
                error.code,
                "mcp_connection_closed" | "claude_code_unavailable"
            )
        {
            ExternalRoute::NativeFallback(NativeFallback {
                capability,
                started,
            })
        } else {
            self.claude.record_call(
                call_summary(
                    capability,
                    "claude_code",
                    false,
                    false,
                    started,
                    Some(error.code),
                ),
                false,
            );
            ExternalRoute::Handled(provider_error_result(capability, error, started))
        }
    }

    pub(crate) fn complete_native_fallback(
        &self,
        fallback: NativeFallback,
        result: &CommandResult,
    ) {
        let succeeded = native_result_succeeded(fallback.capability, result);
        self.claude.record_call(
            call_summary(
                fallback.capability,
                "native",
                true,
                succeeded,
                fallback.started,
                (!succeeded).then_some("native_tool_failed"),
            ),
            false,
        );
    }
}

impl Drop for ExternalToolRouter {
    fn drop(&mut self) {
        let _ = self.shutdown_until(Instant::now() + MCP_FALLBACK_SHUTDOWN_BUDGET);
    }
}

fn call_summary(
    capability: ProviderCapability,
    selected_provider: &str,
    fallback_used: bool,
    succeeded: bool,
    started: Instant,
    error_code: Option<&str>,
) -> ProviderCallSummary {
    ProviderCallSummary {
        capability: capability.name().to_string(),
        selected_provider: selected_provider.to_string(),
        fallback_used,
        result: if succeeded { "success" } else { "failure" }.to_string(),
        write_state: None,
        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        error_code: error_code.map(str::to_string),
    }
}

fn native_result_succeeded(_capability: ProviderCapability, result: &CommandResult) -> bool {
    if result.error.is_some() {
        return false;
    }
    matches!(result.exit_code, Some(0 | 1))
}

fn provider_error_result(
    capability: ProviderCapability,
    error: ProviderError,
    started: Instant,
) -> CommandResult {
    let output = json!({
        "format": "webcodex.external_provider_error.v1",
        "provider": "claude_code",
        "capability": capability.name(),
        "code": error.code,
        "message": error.code,
        "write_state": "not_submitted",
        "changed": false,
        "error": error.code,
    });
    command_result(output.to_string(), started)
}

fn command_result(stdout: String, started: Instant) -> CommandResult {
    command_result_with_exit(stdout, 0, started)
}

fn command_result_with_exit(stdout: String, exit_code: i32, started: Instant) -> CommandResult {
    CommandResult {
        exit_code: Some(exit_code),
        stdout: Some(stdout),
        stderr: Some(String::new()),
        duration_ms: Some(started.elapsed().as_millis() as u64),
        error: None,
    }
}

fn normalized_search_exit_code(stdout: &str) -> i32 {
    if stdout.lines().skip(1).any(|line| !line.is_empty()) {
        0
    } else {
        1
    }
}

fn validate_context(
    policy: &RunnerPolicy,
    cwd: Option<&str>,
    _capability: ProviderCapability,
    payload: &Value,
) -> Result<(PathBuf, PathBuf), ProviderError> {
    let root = cwd.ok_or_else(path_error)?;
    let root = Path::new(root).canonicalize().map_err(|_| path_error())?;
    cwd_allowed(policy, &root).map_err(|_| path_error())?;
    let relative = payload.get("path").and_then(Value::as_str).unwrap_or(".");
    let raw = Path::new(relative);
    if raw.is_absolute()
        || raw
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(path_error());
    }
    let target = root.join(raw).canonicalize().map_err(|_| path_error())?;
    if !target.starts_with(&root) {
        return Err(path_error());
    }
    Ok((root, target))
}

fn path_error() -> ProviderError {
    ProviderError::new("provider_path_rejected")
}

fn unmapped_capabilities() -> BTreeMap<String, String> {
    BTreeMap::from([("search_project_text".to_string(), "unmapped".to_string())])
}

struct ProviderState {
    status: Mutex<ClaudeCodeProviderStatus>,
    revision: AtomicU64,
}

impl ProviderState {
    fn new(enabled: bool) -> Self {
        Self {
            status: Mutex::new(ClaudeCodeProviderStatus {
                enabled,
                version: None,
                available: false,
                process_state: "not_started".to_string(),
                discovered_tool_names: Vec::new(),
                capabilities: unmapped_capabilities(),
                last_error_code: None,
                last_call: None,
            }),
            // Revision one represents the initialized configuration snapshot.
            revision: AtomicU64::new(1),
        }
    }

    fn update(&self, update: impl FnOnce(&mut ClaudeCodeProviderStatus)) {
        let mut status = lock_unpoison(&self.status);
        let previous = status.clone();
        update(&mut status);
        if *status != previous {
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn snapshot_with_revision(&self) -> (ClaudeCodeProviderStatus, u64) {
        let status = lock_unpoison(&self.status);
        (status.clone(), self.revision.load(Ordering::SeqCst))
    }

    fn stopped(&self, error_code: Option<&str>) {
        self.update(|status| {
            status.available = false;
            status.process_state = "stopped".to_string();
            if let Some(error_code) = error_code {
                status.last_error_code = Some(error_code.to_string());
            }
        });
    }
}

struct ClaudeCodeMcpProvider {
    config: ClaudeCodeMcpConfig,
    projects: Mutex<HashMap<PathBuf, Arc<ProjectMcpClient>>>,
    state: Arc<ProviderState>,
    shutting_down: AtomicBool,
}

impl ClaudeCodeMcpProvider {
    fn new(config: ClaudeCodeMcpConfig) -> Self {
        let state = Arc::new(ProviderState::new(config.enabled));
        Self {
            config,
            projects: Mutex::new(HashMap::new()),
            state,
            shutting_down: AtomicBool::new(false),
        }
    }

    fn shutdown_until(&self, deadline: Instant) -> ExternalShutdownOutcome {
        self.shutting_down.store(true, Ordering::SeqCst);
        let clients = self
            .projects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, client)| client)
            .collect::<Vec<_>>();
        let connections = clients
            .iter()
            .map(|client| Arc::clone(&client.connection))
            .collect::<Vec<_>>();
        for connection in &connections {
            connection.signal_shutdown();
        }
        let mut outcome = ExternalShutdownOutcome {
            connections: connections.len(),
            ..ExternalShutdownOutcome::default()
        };
        for connection in connections {
            let result = connection.finish_shutdown(deadline);
            outcome.timed_out += usize::from(!result.reaped || !result.reader_joined);
            outcome.failures += result.failures;
        }
        self.state.stopped(None);
        outcome
    }

    #[cfg(test)]
    fn shutdown(&self) {
        let _ = self.shutdown_until(Instant::now() + MCP_FALLBACK_SHUTDOWN_BUDGET);
    }

    fn record_error(&self, error: &ProviderError) {
        self.state.update(|status| {
            status.last_error_code = Some(error.code.to_string());
        });
    }

    fn record_call(&self, summary: ProviderCallSummary, clear_error: bool) {
        self.state.update(|status| {
            status.last_call = Some(summary);
            if clear_error && status.process_state == "running" {
                status.last_error_code = None;
            }
        });
    }

    #[cfg(test)]
    fn status(&self) -> ClaudeCodeProviderStatus {
        self.status_with_revision().0
    }

    fn status_with_revision(&self) -> (ClaudeCodeProviderStatus, u64) {
        if self
            .projects
            .try_lock()
            .ok()
            .is_some_and(|projects| projects.values().any(|client| client.connection.is_alive()))
        {
            self.state.update(|status| {
                if status.process_state == "stopped" {
                    status.available = true;
                    status.process_state = "running".to_string();
                }
            });
        }
        self.state.snapshot_with_revision()
    }

    #[cfg(test)]
    fn project_client(
        &self,
        root: &Path,
        deadline: Instant,
    ) -> Result<Arc<ProjectMcpClient>, ProviderError> {
        self.project_client_with_shutdown(root, deadline, None)
    }

    fn project_client_with_shutdown(
        &self,
        root: &Path,
        deadline: Instant,
        shutdown: Option<&AtomicBool>,
    ) -> Result<Arc<ProjectMcpClient>, ProviderError> {
        if !self.config.enabled
            || self.shutting_down.load(Ordering::SeqCst)
            || shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            let error = ProviderError::new("claude_code_unavailable");
            self.record_error(&error);
            return Err(error);
        }
        let stale = {
            let mut projects = lock_unpoison(&self.projects);
            if let Some(client) = projects.get(root) {
                if client.connection.is_alive() {
                    return Ok(Arc::clone(client));
                }
            }
            projects.remove(root)
        };
        if let Some(stale) = stale {
            let _ = stale.connection.finish_shutdown(deadline);
        }
        self.state.update(|status| {
            status.available = false;
            status.process_state = "starting".to_string();
            status.version = None;
            status.discovered_tool_names.clear();
            status.capabilities = unmapped_capabilities();
            status.last_error_code = None;
        });
        let client = match ProjectMcpClient::start(
            root,
            &self.config,
            deadline,
            Arc::clone(&self.state),
            shutdown,
        ) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                self.state.stopped(Some(error.code));
                self.record_error(&error);
                return Err(error);
            }
        };
        enum PublishClient {
            Inserted,
            Existing(Arc<ProjectMcpClient>),
            ShuttingDown,
        }
        let published = {
            let mut projects = lock_unpoison(&self.projects);
            if self.shutting_down.load(Ordering::SeqCst)
                || shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst))
            {
                PublishClient::ShuttingDown
            } else if let Some(existing) = projects
                .get(root)
                .filter(|existing| existing.connection.is_alive())
                .cloned()
            {
                PublishClient::Existing(existing)
            } else {
                projects.insert(root.to_path_buf(), Arc::clone(&client));
                PublishClient::Inserted
            }
        };
        match published {
            PublishClient::Inserted => Ok(client),
            PublishClient::Existing(existing) => {
                client.connection.signal_shutdown();
                let _ = client.connection.finish_shutdown(deadline);
                Ok(existing)
            }
            PublishClient::ShuttingDown => {
                client.connection.signal_shutdown();
                let _ = client.connection.finish_shutdown(deadline);
                Err(ProviderError::new("mcp_connection_closed"))
            }
        }
    }

    #[cfg(test)]
    fn call(
        &self,
        capability: ProviderCapability,
        request: Value,
        context: ToolExecutionContext<'_>,
    ) -> Result<Value, ProviderError> {
        self.call_with_shutdown(capability, request, context, None)
    }

    fn call_with_shutdown(
        &self,
        capability: ProviderCapability,
        request: Value,
        context: ToolExecutionContext<'_>,
        shutdown: Option<&AtomicBool>,
    ) -> Result<Value, ProviderError> {
        let budget = self.config.timeout_secs.min(context.timeout_secs);
        let deadline = Instant::now() + Duration::from_secs(budget);
        let client = self.project_client_with_shutdown(context.project_root, deadline, shutdown)?;
        let result = client.call_with_shutdown(
            capability,
            request,
            &context,
            &self.config,
            deadline,
            shutdown,
        );
        if !client.connection.is_alive() {
            lock_unpoison(&self.projects).remove(context.project_root);
        }
        if let Err(error) = &result {
            self.record_error(error);
        }
        result
    }
}

struct ProjectMcpClient {
    connection: Arc<McpConnection>,
    tools: BTreeMap<String, DiscoveredTool>,
}

impl ProjectMcpClient {
    fn start(
        root: &Path,
        config: &ClaudeCodeMcpConfig,
        deadline: Instant,
        state: Arc<ProviderState>,
        shutdown: Option<&AtomicBool>,
    ) -> Result<Self, ProviderError> {
        let connection = McpConnection::spawn(root, config, Arc::clone(&state))?;
        let timeout = || {
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(10))
        };
        let initialized = connection.request_with_shutdown(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "webcodex-runner", "version": env!("CARGO_PKG_VERSION")},
            }),
            timeout(),
            shutdown,
        )?;
        let version = initialized
            .pointer("/serverInfo/version")
            .and_then(Value::as_str)
            .and_then(sanitize_version);
        state.update(|status| {
            status.version = version.clone();
            status.process_state = "discovering".to_string();
            status.last_error_code = None;
        });
        connection.write_json(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        )?;
        let listed =
            connection.request_with_shutdown("tools/list", json!({}), timeout(), shutdown)?;
        let mut tools = BTreeMap::new();
        for tool in listed
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(protocol_error)?
        {
            let Some((name, discovered)) = discovered_tool_entry(tool) else {
                continue;
            };
            if tools.contains_key(&name) {
                continue;
            }
            if tools.len() >= MAX_DISCOVERED_TOOLS {
                // Valid 65th+ tool discovered; keep only the stored bound.
                break;
            }
            tools.insert(name, discovered);
        }
        let client = Self { connection, tools };
        let mut discovered_tool_names = client
            .tools
            .keys()
            .filter_map(|name| sanitize_tool_name(name))
            .collect::<Vec<_>>();
        discovered_tool_names.sort();
        discovered_tool_names.dedup();
        discovered_tool_names.truncate(MAX_DISCOVERED_TOOLS);
        state.update(|status| {
            status.discovered_tool_names = discovered_tool_names;
            status.process_state = "mapping".to_string();
        });
        let capabilities = BTreeMap::from([(
            "search_project_text".to_string(),
            client
                .mapping_status(ProviderCapability::SearchProjectText, config)
                .to_string(),
        )]);
        state.update(|status| {
            status.capabilities = capabilities;
            status.available = true;
            status.process_state = "running".to_string();
            status.last_error_code = None;
        });
        Ok(client)
    }

    fn tool_for<'a>(
        &self,
        capability: ProviderCapability,
        config: &'a ClaudeCodeMcpConfig,
    ) -> Result<&'a str, ProviderError> {
        let configured = config
            .mapping
            .get(capability.name())
            .map(String::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(capability_error)?;
        if self.mapping_status(capability, config) == "available" {
            Ok(configured)
        } else {
            Err(capability_error())
        }
    }

    fn mapping_status(
        &self,
        capability: ProviderCapability,
        config: &ClaudeCodeMcpConfig,
    ) -> &'static str {
        let tool = config
            .mapping
            .get(capability.name())
            .filter(|name| !name.trim().is_empty())
            .and_then(|name| self.tools.get(name));
        match tool {
            None => "unmapped",
            Some(tool)
                if required_fields(capability)
                    .iter()
                    .all(|field| tool.fields.contains(*field)) =>
            {
                "available"
            }
            Some(_) => "schema_mismatch",
        }
    }

    #[cfg(test)]
    fn call(
        &self,
        capability: ProviderCapability,
        request: Value,
        context: &ToolExecutionContext<'_>,
        config: &ClaudeCodeMcpConfig,
        deadline: Instant,
    ) -> Result<Value, ProviderError> {
        self.call_with_shutdown(capability, request, context, config, deadline, None)
    }

    fn call_with_shutdown(
        &self,
        capability: ProviderCapability,
        request: Value,
        context: &ToolExecutionContext<'_>,
        config: &ClaudeCodeMcpConfig,
        deadline: Instant,
        shutdown: Option<&AtomicBool>,
    ) -> Result<Value, ProviderError> {
        let tool = self.tool_for(capability, config)?;
        let arguments = build_arguments(capability, &request, context)?;
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(ProviderError::new("mcp_request_timeout"));
        }
        let result = self.connection.request_with_shutdown(
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            timeout,
            shutdown,
        )?;
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(ProviderError::new("claude_tool_failed"));
        }
        normalize_search_result(&result, context)
    }
}

fn required_fields(_capability: ProviderCapability) -> &'static [&'static str] {
    const GREP: &[&str] = &[
        "pattern",
        "path",
        "output_mode",
        "head_limit",
        "-n",
        "-B",
        "-A",
    ];
    GREP
}

fn build_arguments(
    _capability: ProviderCapability,
    request: &Value,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, ProviderError> {
    let target = context.target.to_string_lossy();
    if ["include_globs", "exclude_globs"].iter().any(|field| {
        request
            .get(field)
            .and_then(Value::as_array)
            .is_some_and(|values| !values.is_empty())
    }) {
        return Err(capability_error());
    }
    let output_mode = if request["result_mode"] == "matches" {
        "content"
    } else {
        request["result_mode"].as_str().ok_or_else(request_error)?
    };
    let mut args = json!({
        "pattern": request["pattern"],
        "path": target,
        "output_mode": output_mode,
        "head_limit": request["limit"],
    });
    if request["result_mode"] == "matches" {
        args["-n"] = json!(true);
        args["-B"] = request["context_before"].clone();
        args["-A"] = request["context_after"].clone();
    }
    Ok(args)
}

fn normalize_search_result(
    result: &Value,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, ProviderError> {
    let raw = tool_text(result, context.max_output_bytes)?;
    let root = context.project_root.to_string_lossy();
    let root_prefix = format!("{}/", root.trim_end_matches('/'));
    let mut lines = Vec::new();
    lines.push(
        json!({"webcodex_search":{"backend":"claude_code","feature_unavailable":false}})
            .to_string(),
    );
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        let normalized = line.strip_prefix(&root_prefix).unwrap_or(line);
        let path = normalized
            .split_once(':')
            .map_or(normalized, |(path, _)| path);
        if Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return Err(ProviderError::new("provider_output_untrusted"));
        }
        lines.push(normalized.to_string());
    }
    Ok(Value::String(lines.join("\n")))
}

fn tool_text(result: &Value, maximum: usize) -> Result<String, ProviderError> {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.len() > maximum {
        return Err(ProviderError::new("provider_response_too_large"));
    }
    Ok(text)
}

fn capability_error() -> ProviderError {
    ProviderError::new("provider_capability_unavailable")
}

fn request_error() -> ProviderError {
    ProviderError::new("provider_invalid_request")
}

fn protocol_error() -> ProviderError {
    ProviderError::new("mcp_protocol_error")
}

type PendingSender = mpsc::Sender<Result<Value, ProviderError>>;

struct McpConnection {
    child: Mutex<ManagedChild>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, PendingSender>>>,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    shutdown_state: Arc<AtomicUsize>,
    shutdown_started_at: Mutex<Option<Instant>>,
    shutdown_deadline: Mutex<Option<Instant>>,
    shutdown_failures: AtomicUsize,
    reader_thread: Mutex<Option<JoinHandle<()>>>,
    state: Arc<ProviderState>,
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        self.signal_shutdown();
        let deadline = lock_unpoison(&self.shutdown_deadline)
            .unwrap_or_else(|| Instant::now() + MCP_FALLBACK_SHUTDOWN_BUDGET);
        let _ = self.finish_shutdown(deadline);
    }
}

#[derive(Debug, Clone, Copy)]
struct McpShutdownResult {
    reaped: bool,
    reader_joined: bool,
    failures: usize,
}

/// Resolve the MCP program to a concrete path on Windows so an
/// extensionless POSIX shim shadowing `claude.exe`/`claude.cmd` is never
/// picked (CreateProcess would fail with error 193). Unix keeps the
/// configured value verbatim.
fn mcp_program(command: &str) -> OsString {
    #[cfg(windows)]
    {
        if let Some(program) = super::util::resolve_program_in_path(
            command,
            std::env::var_os("PATH")
                .as_deref()
                .unwrap_or(OsStr::new("")),
        ) {
            return program.path().as_os_str().to_os_string();
        }
    }
    #[cfg(not(windows))]
    let _ = command;
    command.into()
}

impl McpConnection {
    fn spawn(
        root: &Path,
        config: &ClaudeCodeMcpConfig,
        state: Arc<ProviderState>,
    ) -> Result<Arc<Self>, ProviderError> {
        let program = mcp_program(&config.command);
        let mut command = Command::new(program);
        command
            .args(&config.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        apply_safe_environment(&mut command);
        let mut managed = ManagedChild::spawn(&mut command)
            .map_err(|_| ProviderError::new("claude_code_spawn_failed"))?;
        let stdin = match managed.child_mut().stdin.take() {
            Some(stdin) => Arc::new(Mutex::new(stdin)),
            None => {
                cleanup_failed_mcp_child(
                    &mut managed,
                    Instant::now() + MCP_FALLBACK_SHUTDOWN_BUDGET,
                );
                return Err(protocol_error());
            }
        };
        let stdout = match managed.child_mut().stdout.take() {
            Some(stdout) => stdout,
            None => {
                cleanup_failed_mcp_child(
                    &mut managed,
                    Instant::now() + MCP_FALLBACK_SHUTDOWN_BUDGET,
                );
                return Err(protocol_error());
            }
        };
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let shutdown_state = Arc::new(AtomicUsize::new(MCP_RUNNING));
        state.update(|status| {
            status.process_state = "initializing".to_string();
        });
        let connection = Arc::new(Self {
            child: Mutex::new(managed),
            stdin: Arc::clone(&stdin),
            pending: Arc::clone(&pending),
            next_id: AtomicU64::new(1),
            alive: Arc::clone(&alive),
            shutdown_state: Arc::clone(&shutdown_state),
            shutdown_started_at: Mutex::new(None),
            shutdown_deadline: Mutex::new(None),
            shutdown_failures: AtomicUsize::new(0),
            reader_thread: Mutex::new(None),
            state: Arc::clone(&state),
        });
        let reader = spawn_stdout_reader(stdout, stdin, pending, alive, shutdown_state, state);
        *lock_unpoison(&connection.reader_thread) = Some(reader);
        Ok(connection)
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    #[cfg(all(test, unix))]
    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ProviderError> {
        self.request_with_shutdown(method, params, timeout, None)
    }

    fn request_with_shutdown(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        shutdown: Option<&AtomicBool>,
    ) -> Result<Value, ProviderError> {
        if shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            return Err(ProviderError::new("mcp_connection_closed"));
        }
        if !self.is_alive() {
            return Err(protocol_error());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = lock_unpoison(&self.pending);
            if !self.is_alive() {
                return Err(protocol_error());
            }
            if pending.len() >= MAX_PENDING_REQUESTS {
                return Err(ProviderError::new("mcp_pending_limit"));
            }
            pending.insert(id, tx);
        }
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        // Encode + size-check before any stdin write so malformed or oversized
        // requests fail before anything is submitted to the provider.
        let encoded = match encode_mcp_message(&message) {
            Ok(bytes) => bytes,
            Err(error) => {
                lock_unpoison(&self.pending).remove(&id);
                return Err(error);
            }
        };
        if let Err(error) = write_mcp_message(&self.stdin, &encoded) {
            lock_unpoison(&self.pending).remove(&id);
            return Err(error);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                lock_unpoison(&self.pending).remove(&id);
                self.signal_shutdown();
                let _ = self.finish_shutdown(Instant::now() + MCP_FALLBACK_SHUTDOWN_BUDGET);
                return Err(ProviderError::new("mcp_connection_closed"));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                lock_unpoison(&self.pending).remove(&id);
                self.signal_shutdown();
                let _ = self.finish_shutdown(Instant::now() + MCP_FALLBACK_SHUTDOWN_BUDGET);
                return Err(ProviderError::new("mcp_request_timeout"));
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(25))) {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProviderError::new("mcp_connection_closed"));
                }
            }
        }
    }

    fn write_json(&self, value: &Value) -> Result<(), ProviderError> {
        write_json(&self.stdin, value)
    }

    fn signal_shutdown(&self) {
        if self
            .shutdown_state
            .compare_exchange(
                MCP_RUNNING,
                MCP_TERM_SENT,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return;
        }
        self.alive.store(false, Ordering::SeqCst);
        self.state.stopped(None);
        *lock_unpoison(&self.shutdown_started_at) = Some(Instant::now());
        fail_pending(&self.pending, ProviderError::new("mcp_connection_closed"));
        // Graceful tree termination. On Unix this preserves the old SIGTERM
        // grace phase; on Windows it reports Unsupported and this first
        // shutdown transition immediately escalates with terminate_tree().
        let request = lock_unpoison(&self.child).request_terminate_tree();
        match request {
            Ok(GracefulTermination::Requested) | Ok(GracefulTermination::AlreadyExited) => {}
            Ok(GracefulTermination::Unsupported) => {
                if lock_unpoison(&self.child).terminate_tree().is_err() {
                    self.shutdown_failures.fetch_add(1, Ordering::SeqCst);
                }
            }
            Err(_) => {
                self.shutdown_failures.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn finish_shutdown(&self, deadline: Instant) -> McpShutdownResult {
        let deadline = {
            let mut recorded = lock_unpoison(&self.shutdown_deadline);
            let deadline = recorded.map_or(deadline, |existing| existing.min(deadline));
            *recorded = Some(deadline);
            deadline
        };
        self.signal_shutdown();
        let started_at = lock_unpoison(&self.shutdown_started_at).unwrap_or_else(Instant::now);
        let grace_deadline = deadline.min(started_at + MCP_TERMINATION_GRACE);
        while Instant::now() < grace_deadline && !self.process_reaped_and_group_gone() {
            let remaining = grace_deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(SHUTDOWN_POLL_INTERVAL.min(remaining));
        }

        if !self.process_reaped_and_group_gone()
            && self
                .shutdown_state
                .compare_exchange(
                    MCP_TERM_SENT,
                    MCP_KILL_SENT,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            && lock_unpoison(&self.child).terminate_tree().is_err()
        {
            self.shutdown_failures.fetch_add(1, Ordering::SeqCst);
        }

        while Instant::now() < deadline && !self.process_reaped_and_group_gone() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(SHUTDOWN_POLL_INTERVAL.min(remaining));
        }
        let reaped = self.process_reaped_and_group_gone();
        if reaped {
            self.shutdown_state.store(MCP_REAPED, Ordering::SeqCst);
        }
        let reader_joined = join_mcp_reader_until(&self.reader_thread, deadline);
        McpShutdownResult {
            reaped,
            reader_joined,
            failures: self.shutdown_failures.load(Ordering::SeqCst),
        }
    }

    fn process_reaped_and_group_gone(&self) -> bool {
        // The direct child must be reaped AND the complete managed process
        // tree must be empty. A direct-child exit alone is never "reaped".
        match self.child.try_lock() {
            Ok(mut child) => {
                let child_reaped = matches!(child.try_wait(), Ok(Some(_)) | Err(_));
                child_reaped && child.wait_tree_exit(Duration::ZERO).unwrap_or(false)
            }
            Err(std::sync::TryLockError::WouldBlock) => false,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                let mut child = poisoned.into_inner();
                let child_reaped = matches!(child.try_wait(), Ok(Some(_)) | Err(_));
                child_reaped && child.wait_tree_exit(Duration::ZERO).unwrap_or(false)
            }
        }
    }
}

/// Clean up an MCP child that failed early (missing pipe or thread spawn).
///
/// The server can never be used, so the whole managed tree is forcefully
/// terminated, then the direct child and the whole tree are reaped within the
/// shared deadline. Never re-arms a fresh wait, never leaks the Job Object /
/// process group, and never leaves a descendant holding stdout open.
fn cleanup_failed_mcp_child(managed: &mut ManagedChild, deadline: Instant) {
    let _ = managed.terminate_tree();
    while Instant::now() < deadline {
        match managed.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(SHUTDOWN_POLL_INTERVAL.min(remaining));
            }
        }
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let _ = managed.wait_tree_exit(remaining);
}

fn join_mcp_reader_until(reader: &Mutex<Option<JoinHandle<()>>>, deadline: Instant) -> bool {
    loop {
        let finished = lock_unpoison(reader)
            .as_ref()
            .map(JoinHandle::is_finished)
            .unwrap_or(true);
        if finished {
            let handle = lock_unpoison(reader).take();
            if let Some(handle) = handle {
                let _ = handle.join();
            }
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(SHUTDOWN_POLL_INTERVAL.min(remaining));
    }
}

/// Serialize and size-check an MCP JSON line. Does not touch stdin.
fn encode_mcp_message(value: &Value) -> Result<Vec<u8>, ProviderError> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| protocol_error())?;
    if bytes.len() > MAX_MCP_MESSAGE_BYTES {
        return Err(ProviderError::new("mcp_message_too_large"));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Write a previously encoded MCP line to stdin (may partially write).
fn write_mcp_message(stdin: &Mutex<ChildStdin>, bytes: &[u8]) -> Result<(), ProviderError> {
    let mut writer = lock_unpoison(stdin);
    writer.write_all(bytes).map_err(|_| protocol_error())?;
    writer.flush().map_err(|_| protocol_error())
}

fn write_json(stdin: &Mutex<ChildStdin>, value: &Value) -> Result<(), ProviderError> {
    let bytes = encode_mcp_message(value)?;
    write_mcp_message(stdin, &bytes)
}

fn spawn_stdout_reader(
    stdout: impl Read + Send + 'static,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, PendingSender>>>,
    alive: Arc<AtomicBool>,
    shutdown_state: Arc<AtomicUsize>,
    state: Arc<ProviderState>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut terminal_error = ProviderError::new("mcp_connection_closed");
        loop {
            let bytes = match read_bounded_line(&mut reader) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => break,
                Err(error) => {
                    terminal_error = error;
                    break;
                }
            };
            let value: Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => {
                    terminal_error = ProviderError::new("mcp_invalid_json");
                    break;
                }
            };
            let method = value.get("method");
            let id = value.get("id");
            if method.is_some() && id.is_none() {
                continue;
            }
            if method.is_some() {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id.cloned().unwrap_or(Value::Null),
                    "error": {"code": -32601, "message": "Method not found"},
                });
                if let Err(error) = write_json(&stdin, &response) {
                    terminal_error = error;
                    break;
                }
                continue;
            }
            let Some(id) = id.and_then(Value::as_u64) else {
                continue;
            };
            let Some(sender) = lock_unpoison(&pending).remove(&id) else {
                continue;
            };
            let response = if value.get("error").is_some() {
                Err(ProviderError::new("mcp_rpc_error"))
            } else {
                value.get("result").cloned().ok_or_else(protocol_error)
            };
            let _ = sender.send(response);
        }
        alive.store(false, Ordering::SeqCst);
        if shutdown_state.load(Ordering::SeqCst) == MCP_RUNNING {
            state.stopped(Some(terminal_error.code));
        }
        fail_pending(&pending, terminal_error);
    })
}

fn read_bounded_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, ProviderError> {
    let mut line = Vec::new();
    reader
        .take((MAX_MCP_MESSAGE_BYTES + 2) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|_| protocol_error())?;
    if line.is_empty() {
        return Ok(None);
    }
    let terminated = line.last() == Some(&b'\n');
    if line.len() > MAX_MCP_MESSAGE_BYTES + usize::from(terminated) {
        return Err(ProviderError::new("mcp_message_too_large"));
    }
    if terminated {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    Ok(Some(line))
}

fn fail_pending(pending: &Mutex<HashMap<u64, PendingSender>>, error: ProviderError) {
    let senders = lock_unpoison(pending)
        .drain()
        .map(|(_, sender)| sender)
        .collect::<Vec<_>>();
    for sender in senders {
        let _ = sender.send(Err(error.clone()));
    }
}

fn apply_safe_environment(command: &mut Command) {
    command.env_clear();
    for key in "PATH HOME LANG LC_ALL TMPDIR XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME CLAUDE_CONFIG_DIR"
        .split_whitespace()
    {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

fn sanitize_tool_name(value: &str) -> Option<String> {
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-.:".contains(character))
    {
        return None;
    }
    Some(value.chars().take(120).collect())
}

#[cfg(test)]
fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(120)
        .collect()
}

fn sanitize_version(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().any(|character| {
            !(character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '.' | '_' | '-' | '+' | '(' | ')'))
        })
    {
        return None;
    }
    Some(value.chars().take(80).collect())
}

#[cfg(test)]
#[path = "external_tools_tests.rs"]
mod tests;
