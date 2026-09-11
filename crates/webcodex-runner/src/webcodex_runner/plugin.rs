//! Runner-owned native stdio Tool Plugin runtime.
//!
//! Configured providers are eagerly initialized into one committed state. Each
//! admitted provider instance has one frozen catalog. Explicit reloads prepare
//! a complete candidate set and atomically replace the committed state only
//! after every configured provider is admitted.

use super::config::{
    load_config, PluginConfig, PluginProviderConfig, RunnerConfig, ShellConfig, ShellDialect,
};
use super::shell::{PreparedExecutionEnvironment, PreparedShellProfileCache};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};
use webcodex_core::plugin::{
    diagnose_invalid_tools, validate_plugin_input_arguments, validate_plugin_structured_output,
    validate_request, validate_tool_result, validate_tools, PluginCatalog, PluginCheckDiagnostic,
    PluginCheckPhase, PluginCheckReport, PluginCheckToolSummary, PluginDispatchState,
    PluginGatewayRequest, PluginGatewayResponse, PluginGatewayResponsePayload, PluginProviderView,
    PluginReloadFailure, PluginSchemaObservation, PluginSelectionAnnotations, PluginTool,
    PluginToolResult, ProjectPluginCatalog, ProjectPluginCatalogEntry, PLUGIN_MAX_MESSAGE_BYTES,
    PLUGIN_MAX_PROJECT_CATALOG_DESCRIPTION_BYTES, PLUGIN_PROJECT_CATALOG_REVISION_PREFIX,
    PLUGIN_PROTOCOL_VERSION,
};
use webcodex_process::ManagedChild;

const PLUGIN_READER_QUEUE: usize = 16;
const PLUGIN_WRITER_QUEUE: usize = 1;
const PLUGIN_STOP_POLL: Duration = Duration::from_millis(25);
const PLUGIN_TERMINATION_BUDGET: Duration = Duration::from_secs(1);
const PLUGIN_STDERR_DRAIN_BUDGET: Duration = Duration::from_millis(100);
const PLUGIN_STDERR_MAX_LINES: usize = 64;
const PLUGIN_STDERR_MAX_LINE_BYTES: usize = 1024;
const PLUGIN_STDERR_MAX_BYTES: usize = 32 * 1024;

pub(crate) struct PluginManager {
    committed: Mutex<CommittedState>,
    candidate_gate: Mutex<()>,
    last_check_stderr: Mutex<BTreeMap<String, PluginStderrSnapshot>>,
    config_path: PathBuf,
    prepared_profiles: PreparedShellProfileCache,
    next_generation: AtomicU64,
    stopping: Arc<AtomicBool>,
}

struct CommittedState {
    providers: BTreeMap<String, Arc<ProviderEntry>>,
    config: PluginConfig,
    environment: PluginEnvironmentSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginEnvironmentSnapshot {
    environment_mode: super::config::ShellEnvironmentMode,
    default_profile: Option<String>,
    profiles: BTreeMap<String, PluginProfileEnvironment>,
    program: String,
    args: Vec<String>,
    dialect: Option<ShellDialect>,
    path_prepend: Vec<PathBuf>,
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginProfileEnvironment {
    program: Option<String>,
    args: Option<Vec<String>>,
    dialect: Option<ShellDialect>,
    env: BTreeMap<String, String>,
    init_script: Option<String>,
}

impl PluginEnvironmentSnapshot {
    fn from_config(shell: &ShellConfig, plugins: &PluginConfig) -> Self {
        let mut profile_names = BTreeSet::new();
        let uses_default_profile = plugins
            .providers
            .iter()
            .any(|provider| provider.profile.is_none());
        for provider in &plugins.providers {
            if let Some(profile) = provider.profile.as_ref() {
                profile_names.insert(profile.clone());
            } else if let Some(profile) = shell.default_profile.as_ref() {
                profile_names.insert(profile.clone());
            }
        }
        let has_providers = !plugins.providers.is_empty();
        Self {
            environment_mode: if has_providers {
                shell.environment_mode
            } else {
                Default::default()
            },
            default_profile: uses_default_profile
                .then(|| shell.default_profile.clone())
                .flatten(),
            profiles: profile_names
                .into_iter()
                .filter_map(|name| {
                    shell.profiles.get(&name).map(|profile| {
                        (
                            name,
                            PluginProfileEnvironment {
                                program: profile.program.clone(),
                                args: profile.args.clone(),
                                dialect: profile.dialect,
                                env: profile.env.clone(),
                                init_script: profile.init_script.clone(),
                            },
                        )
                    })
                })
                .collect(),
            program: has_providers
                .then(|| shell.program.clone())
                .unwrap_or_default(),
            args: has_providers
                .then(|| shell.args.clone())
                .unwrap_or_default(),
            dialect: has_providers.then_some(shell.dialect).flatten(),
            path_prepend: has_providers
                .then(|| shell.path_prepend.clone())
                .unwrap_or_default(),
            env: if has_providers {
                shell
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            } else {
                BTreeMap::new()
            },
        }
    }
}

struct ProviderEntry {
    config: PluginProviderConfig,
    instance_id: String,
    timeout: Duration,
    failed: AtomicBool,
    error_code: Mutex<Option<String>>,
    process: Arc<ProviderProcess>,
    catalog: OnceLock<PluginCatalog>,
    session: Mutex<Option<ProviderConnection>>,
}

struct ProviderProcess {
    child: Mutex<Option<ManagedChild>>,
    stderr: Arc<Mutex<PluginStderrDiagnostics>>,
    stderr_drained: Arc<AtomicBool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginStderrSnapshot {
    pub(crate) lines: Vec<PluginStderrLine>,
    pub(crate) aggregate_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginStderrLine {
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

#[derive(Default)]
struct PluginStderrDiagnostics {
    lines: VecDeque<PluginStderrLine>,
    aggregate_bytes: usize,
}

struct ProviderConnection {
    process: Arc<ProviderProcess>,
    stopping: Arc<AtomicBool>,
    writer: mpsc::SyncSender<WriteRequest>,
    incoming: mpsc::Receiver<ReaderEvent>,
    next_id: u64,
}

struct WriteRequest {
    frame: Vec<u8>,
    ack: mpsc::Sender<WriteAck>,
}

enum WriteAck {
    Written,
    Failed,
}

enum ReaderEvent {
    Message(Result<Value, ReaderFault>),
    Eof,
}

#[derive(Clone, Copy)]
enum ReaderFault {
    Malformed,
    TooLarge,
    Io,
}

struct ProviderFailure {
    dispatch_state: PluginDispatchState,
    code: &'static str,
    fatal: bool,
}

struct PluginToolsListFailure {
    failure: ProviderFailure,
    diagnostic: Option<PluginCheckDiagnostic>,
}

struct ProviderPreparationFailure {
    phase: PluginCheckPhase,
    code: &'static str,
    detail: &'static str,
    diagnostic: Option<PluginCheckDiagnostic>,
}

enum PluginReloadAttempt {
    Committed {
        providers: Vec<PluginProviderView>,
    },
    Rejected {
        providers: Vec<PluginProviderView>,
        failures: Vec<PluginReloadFailure>,
    },
    NotStarted {
        code: &'static str,
        message: &'static str,
    },
}

fn initialize_failure_detail(code: &str) -> &'static str {
    match code {
        "plugin_protocol_version_mismatch" => {
            "Plugin initialize response did not confirm webcodex-plugin-v1"
        }
        "plugin_initialize_invalid" => "Plugin initialize result violates protocol bounds",
        "plugin_timeout" => "Plugin initialize did not complete within the provider timeout",
        "plugin_eof" => "Plugin protocol output ended before initialize completed; the process may have exited or closed stdout. Verify the configured command and arguments. For a generated TypeScript Plugin, run npm run build and ensure dist/plugin.js exists on the Runner host before retrying.",
        _ => "Plugin initialize did not complete successfully",
    }
}

fn plugin_config_error_code(error: &str) -> &'static str {
    if error.starts_with("failed to read config") {
        "plugin_config_read_failed"
    } else if error.starts_with("failed to parse config") {
        "plugin_config_parse_failed"
    } else {
        "plugin_config_invalid"
    }
}

fn plugin_config_check_detail(code: &str) -> &'static str {
    match code {
        "plugin_config_read_failed" => "runner.toml could not be read",
        "plugin_config_parse_failed" => "runner.toml could not be parsed",
        _ => "runner.toml failed Runner configuration validation",
    }
}

fn failed_check_report(
    provider_id: &str,
    phase: PluginCheckPhase,
    code: &str,
    detail: &str,
    diagnostic: Option<PluginCheckDiagnostic>,
) -> PluginCheckReport {
    PluginCheckReport {
        provider_id: provider_id.to_string(),
        ready: false,
        phase,
        code: Some(code.to_string()),
        detail: Some(detail.to_string()),
        tool_count: 0,
        tools: Vec::new(),
        diagnostic,
    }
}

impl ProviderFailure {
    fn not_started(code: &'static str) -> Self {
        Self {
            dispatch_state: PluginDispatchState::NotStarted,
            code,
            fatal: true,
        }
    }

    fn after_send(code: &'static str, effectful: bool) -> Self {
        Self {
            dispatch_state: if effectful {
                PluginDispatchState::OutcomeUnknown
            } else {
                PluginDispatchState::NotStarted
            },
            code,
            fatal: true,
        }
    }

    fn before_send_timeout() -> Self {
        Self {
            dispatch_state: PluginDispatchState::NotStarted,
            code: "plugin_timeout",
            fatal: false,
        }
    }

    fn completed(code: &'static str, fatal: bool) -> Self {
        Self {
            dispatch_state: PluginDispatchState::Completed,
            code,
            fatal,
        }
    }
}

impl PluginManager {
    pub(crate) fn new(startup: &RunnerConfig, config_path: PathBuf) -> Self {
        let prepared_profiles = PreparedShellProfileCache::default();
        let stopping = Arc::new(AtomicBool::new(false));
        let request_timeout = Duration::from_secs(startup.plugins.request_timeout_secs);
        let mut providers = BTreeMap::new();

        for provider in &startup.plugins.providers {
            let (entry, _listed_tools, _failure) = prepare_provider(
                provider,
                &startup.shell,
                1,
                request_timeout,
                &prepared_profiles,
                &stopping,
            );
            providers.insert(provider.id.clone(), entry);
        }

        Self {
            committed: Mutex::new(CommittedState {
                providers,
                config: startup.plugins.clone(),
                environment: PluginEnvironmentSnapshot::from_config(
                    &startup.shell,
                    &startup.plugins,
                ),
            }),
            candidate_gate: Mutex::new(()),
            last_check_stderr: Mutex::new(BTreeMap::new()),
            config_path,
            prepared_profiles,
            next_generation: AtomicU64::new(2),
            stopping,
        }
    }

    /// Runner-local diagnostic projection only. This is intentionally not part
    /// of Plugin gateway responses or any Server-facing protocol contract.
    #[allow(dead_code)]
    pub(crate) fn local_stderr_diagnostics(
        &self,
        provider_id: &str,
        provider_instance_id: &str,
    ) -> Option<PluginStderrSnapshot> {
        self.resolve_provider(provider_id, provider_instance_id)
            .map(|provider| provider.process.stderr_snapshot())
    }

    /// Last disposable `check` candidate stderr for local operator tooling.
    /// The projection is bounded/sanitized and is never serialized into a
    /// PluginGatewayResponse.
    #[allow(dead_code)]
    pub(crate) fn local_check_stderr_diagnostics(
        &self,
        provider_id: &str,
    ) -> Option<PluginStderrSnapshot> {
        self.last_check_stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(provider_id)
            .cloned()
    }

    pub(crate) fn handle_project_catalog(
        &self,
        project_id: &str,
        project_registry_dir: &Path,
    ) -> PluginGatewayResponse {
        let request = PluginGatewayRequest::ProjectCatalog {
            project_id: project_id.to_string(),
        };
        if let Err(error) = validate_request(&request) {
            tracing::warn!(error = %error, "rejected invalid Plugin project catalog request");
            return gateway_error(
                PluginDispatchState::NotStarted,
                "invalid_plugin_request",
                "Plugin project catalog request was invalid and was not dispatched",
            );
        }
        if self.stopping.load(Ordering::SeqCst) {
            return gateway_error(
                PluginDispatchState::NotStarted,
                "plugin_manager_stopping",
                "Plugin manager is stopping; request was not dispatched",
            );
        }
        let Some(project) =
            super::projects::find_project_shell_context_by_id(project_registry_dir, project_id)
        else {
            return gateway_error(
                PluginDispatchState::NotStarted,
                "project_target_unavailable",
                "The authoritative Runner Project target is unavailable",
            );
        };
        let Ok(project_root) = Path::new(&project.path).canonicalize() else {
            return gateway_error(
                PluginDispatchState::NotStarted,
                "project_target_unavailable",
                "The authoritative Runner Project root is unavailable",
            );
        };
        PluginGatewayResponse::success(PluginGatewayResponsePayload::ProjectCatalog {
            catalog: self.project_catalog_for_root(&project_root),
        })
    }

    fn project_catalog_for_root(&self, project_root: &Path) -> ProjectPluginCatalog {
        let committed = self
            .committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut entries = Vec::new();
        let mut hasher = Sha256::new();
        hasher.update(b"webcodex.project-plugin-catalog.v1\0");
        for provider in committed.providers.values() {
            if provider.failed.load(Ordering::SeqCst) {
                continue;
            }
            let Some(configured_cwd) = provider.config.cwd.as_deref() else {
                continue;
            };
            let Ok(provider_root) = Path::new(configured_cwd).canonicalize() else {
                continue;
            };
            if !webcodex_runner_config::paths::paths_equal(&provider_root, project_root) {
                continue;
            }
            let Some(catalog) = provider.catalog.get() else {
                continue;
            };

            // Provider-instance and frozen schema/catalog identity participate in
            // the opaque revision without entering the model-facing projection.
            for value in [
                provider.config.id.as_str(),
                provider.instance_id.as_str(),
                provider.config.name.as_str(),
                catalog.digest(),
            ] {
                hasher.update((value.len() as u64).to_le_bytes());
                hasher.update(value.as_bytes());
            }
            for tool in catalog.tools() {
                entries.push(ProjectPluginCatalogEntry {
                    plugin: provider.config.id.clone(),
                    name: provider.config.name.clone(),
                    tool: tool.name.clone(),
                    title: tool.title.clone(),
                    description: tool
                        .description
                        .as_deref()
                        .map(bounded_project_catalog_description),
                    annotations: PluginSelectionAnnotations::from_value(tool.annotations.as_ref()),
                });
            }
        }
        let catalog_revision = format!(
            "{PLUGIN_PROJECT_CATALOG_REVISION_PREFIX}{:x}",
            hasher.finalize()
        );
        ProjectPluginCatalog {
            catalog_revision,
            total_count: entries.len(),
            entries,
        }
    }

    pub(crate) fn handle(&self, request: PluginGatewayRequest) -> PluginGatewayResponse {
        if let Err(error) = validate_request(&request) {
            tracing::warn!(error = %error, "rejected invalid Plugin gateway request");
            return gateway_error(
                PluginDispatchState::NotStarted,
                "invalid_plugin_request",
                "Plugin gateway request was invalid and was not dispatched",
            );
        }
        if self.stopping.load(Ordering::SeqCst) {
            return gateway_error(
                PluginDispatchState::NotStarted,
                "plugin_manager_stopping",
                "Plugin manager is stopping; request was not dispatched",
            );
        }
        match request {
            PluginGatewayRequest::Check { provider_id } => self.check_candidate(&provider_id),
            PluginGatewayRequest::Reload => self.reload_from_path(),
            PluginGatewayRequest::ProjectCatalog { .. } => gateway_error(
                PluginDispatchState::NotStarted,
                "project_target_unavailable",
                "Plugin project catalog discovery requires an authoritative Runner Project target",
            ),
            PluginGatewayRequest::ProvidersList => {
                PluginGatewayResponse::success(PluginGatewayResponsePayload::Providers {
                    providers: self.provider_views(),
                })
            }
            PluginGatewayRequest::ToolsList {
                provider_id,
                provider_instance_id,
            } => {
                let Some(provider) = self.resolve_provider(&provider_id, &provider_instance_id)
                else {
                    return stale_provider();
                };
                match provider.frozen_tools() {
                    Ok(tools) => {
                        PluginGatewayResponse::success(PluginGatewayResponsePayload::Tools {
                            tools,
                        })
                    }
                    Err(error) => provider_failure_response(error),
                }
            }
            PluginGatewayRequest::ToolsCall {
                provider_id,
                provider_instance_id,
                name,
                arguments,
                expected_schema,
            } => {
                let Some(provider) = self.resolve_provider(&provider_id, &provider_instance_id)
                else {
                    return stale_provider();
                };
                match provider.call_tool(&name, arguments, &expected_schema) {
                    Ok(result) => {
                        PluginGatewayResponse::success(PluginGatewayResponsePayload::ToolResult {
                            result,
                        })
                    }
                    Err(error) => provider_failure_response(error),
                }
            }
        }
    }

    fn resolve_provider(
        &self,
        provider_id: &str,
        provider_instance_id: &str,
    ) -> Option<Arc<ProviderEntry>> {
        let provider = self
            .committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .providers
            .get(provider_id)
            .cloned()?;
        (provider.instance_id == provider_instance_id).then_some(provider)
    }

    fn provider_views(&self) -> Vec<PluginProviderView> {
        self.committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .providers
            .values()
            .map(|provider| provider.view())
            .collect()
    }

    fn check_candidate(&self, provider_id: &str) -> PluginGatewayResponse {
        let _candidate_guard = match self.candidate_gate.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return gateway_error(
                PluginDispatchState::NotStarted,
                "plugin_check_busy",
                "Another Plugin candidate operation is already running; this check was not started",
            ),
            Err(TryLockError::Poisoned(_)) => {
                return gateway_error(
                    PluginDispatchState::NotStarted,
                    "plugin_check_state_failed",
                    "Plugin candidate-operation state is unavailable; this check was not started",
                )
            }
        };
        self.last_check_stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(provider_id);
        let candidate = match load_config(&self.config_path) {
            Ok(candidate) => candidate,
            Err(error) => {
                let code = plugin_config_error_code(&error);
                return PluginGatewayResponse::success(PluginGatewayResponsePayload::Checked {
                    report: failed_check_report(
                        provider_id,
                        PluginCheckPhase::Config,
                        code,
                        plugin_config_check_detail(code),
                        None,
                    ),
                });
            }
        };
        let Some(provider) = candidate
            .plugins
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
        else {
            return PluginGatewayResponse::success(PluginGatewayResponsePayload::Checked {
                report: failed_check_report(
                    provider_id,
                    PluginCheckPhase::Config,
                    "plugin_not_configured",
                    "requested Plugin provider is not configured in current runner.toml",
                    None,
                ),
            });
        };
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let (entry, tools, failure) = prepare_provider(
            provider,
            &candidate.shell,
            generation,
            Duration::from_secs(candidate.plugins.request_timeout_secs),
            &self.prepared_profiles,
            &self.stopping,
        );
        if self.stopping.load(Ordering::SeqCst)
            || failure
                .as_ref()
                .is_some_and(|failure| failure.code == "plugin_manager_stopping")
        {
            entry.shutdown();
            self.last_check_stderr
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(provider_id.to_string(), entry.process.stderr_snapshot());
            return gateway_error(
                PluginDispatchState::NotStarted,
                "plugin_manager_stopping",
                "Plugin manager began stopping during candidate check; no state was changed",
            );
        }
        let report = if let Some(failure) = failure {
            failed_check_report(
                provider_id,
                failure.phase,
                failure.code,
                failure.detail,
                failure.diagnostic,
            )
        } else {
            let tools = tools.unwrap_or_default();
            PluginCheckReport {
                provider_id: provider_id.to_string(),
                ready: true,
                phase: PluginCheckPhase::Ready,
                code: None,
                detail: None,
                tool_count: tools.len(),
                tools: tools
                    .iter()
                    .map(|tool| PluginCheckToolSummary {
                        name: tool.name.clone(),
                        title: tool.title.clone(),
                    })
                    .collect(),
                diagnostic: None,
            }
        };
        entry.shutdown();
        self.last_check_stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(provider_id.to_string(), entry.process.stderr_snapshot());
        PluginGatewayResponse::success(PluginGatewayResponsePayload::Checked { report })
    }

    fn reload_from_path(&self) -> PluginGatewayResponse {
        let _candidate_guard = match self.candidate_gate.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => {
                return gateway_error(
                    PluginDispatchState::NotStarted,
                    "plugin_reload_busy",
                    "Another Plugin candidate operation is already running; this reload was not started",
                )
            }
            Err(TryLockError::Poisoned(_)) => {
                return gateway_error(
                    PluginDispatchState::NotStarted,
                    "plugin_reload_state_failed",
                    "Plugin reload state is unavailable; committed state was unchanged",
                )
            }
        };
        let candidate = match load_config(&self.config_path) {
            Ok(candidate) => candidate,
            Err(error) => {
                let code = plugin_config_error_code(&error);
                return gateway_error(
                    PluginDispatchState::NotStarted,
                    code,
                    "Runner-owned Plugin configuration could not be loaded; committed state was unchanged",
                );
            }
        };
        // An explicit Plugin reload is also the code-reload primitive: the
        // executable or script may have changed without changing runner.toml.
        // Always prepare a fresh provider set for this path.
        self.reload_attempt_response(self.reload_candidate_locked(&candidate, true))
    }

    /// Apply an already parsed/validated runner.toml candidate through the same
    /// authoritative Plugin admission/commit primitive used by `plugin_tool`
    /// reload. Callers retain their own runner:manage or plugin:manage boundary.
    pub(crate) fn apply_config_candidate_and_then(
        &self,
        candidate: &RunnerConfig,
        after_plugin_commit: impl FnOnce(),
    ) -> Result<(), &'static str> {
        let _candidate_guard = match self.candidate_gate.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return Err("plugin_reload_busy"),
            Err(TryLockError::Poisoned(_)) => return Err("plugin_reload_state_failed"),
        };
        match self.reload_candidate_locked(candidate, false) {
            PluginReloadAttempt::Committed { .. } => {
                // Keep the Plugin candidate gate held until the caller commits
                // the rest of the same Runner-config activation. Otherwise a
                // specialized Plugin reload could interleave after Plugin
                // commit but before the generic Hot config snapshot advances.
                after_plugin_commit();
                Ok(())
            }
            PluginReloadAttempt::Rejected { .. } => Err("plugin_reload_failed"),
            PluginReloadAttempt::NotStarted { code, .. } => Err(code),
        }
    }

    fn reload_candidate_locked(
        &self,
        candidate: &RunnerConfig,
        force_provider_restart: bool,
    ) -> PluginReloadAttempt {
        let candidate_environment =
            PluginEnvironmentSnapshot::from_config(&candidate.shell, &candidate.plugins);
        {
            let committed = self
                .committed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !force_provider_restart
                && committed.config == candidate.plugins
                && committed.environment == candidate_environment
            {
                return PluginReloadAttempt::Committed {
                    providers: committed
                        .providers
                        .values()
                        .map(|provider| provider.view())
                        .collect(),
                };
            }
        }

        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let mut prepared = BTreeMap::new();
        let mut failures = Vec::new();
        for provider in &candidate.plugins.providers {
            let (entry, _tools, failure) = prepare_provider(
                provider,
                &candidate.shell,
                generation,
                Duration::from_secs(candidate.plugins.request_timeout_secs),
                &self.prepared_profiles,
                &self.stopping,
            );
            if let Some(failure) = failure {
                failures.push(PluginReloadFailure {
                    provider_id: provider.id.clone(),
                    code: failure.code.to_string(),
                });
            }
            prepared.insert(provider.id.clone(), entry);
        }

        if self.stopping.load(Ordering::SeqCst) {
            for provider in prepared.values() {
                provider.shutdown();
            }
            return PluginReloadAttempt::NotStarted {
                code: "plugin_manager_stopping",
                message: "Plugin manager began stopping before reload commit; committed state was unchanged",
            };
        }

        if !failures.is_empty() {
            for provider in prepared.values() {
                provider.shutdown();
            }
            return PluginReloadAttempt::Rejected {
                providers: self.provider_views(),
                failures,
            };
        }

        // Config/environment identity and provider instances commit together.
        // Retired provider teardown happens after the lock is released.
        let retired = {
            let mut committed = self
                .committed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.stopping.load(Ordering::SeqCst) {
                drop(committed);
                for provider in prepared.values() {
                    provider.shutdown();
                }
                return PluginReloadAttempt::NotStarted {
                    code: "plugin_manager_stopping",
                    message: "Plugin manager began stopping before reload commit; committed state was unchanged",
                };
            }
            committed.config = candidate.plugins.clone();
            committed.environment = candidate_environment;
            std::mem::replace(&mut committed.providers, prepared)
        };
        drop(retired);

        PluginReloadAttempt::Committed {
            providers: self.provider_views(),
        }
    }

    fn reload_attempt_response(&self, attempt: PluginReloadAttempt) -> PluginGatewayResponse {
        match attempt {
            PluginReloadAttempt::Committed { providers } => {
                PluginGatewayResponse::success(PluginGatewayResponsePayload::Reloaded {
                    providers,
                    failures: Vec::new(),
                })
            }
            PluginReloadAttempt::Rejected {
                providers,
                failures,
            } => PluginGatewayResponse::success(PluginGatewayResponsePayload::Reloaded {
                providers,
                failures,
            }),
            PluginReloadAttempt::NotStarted { code, message } => {
                gateway_error(PluginDispatchState::NotStarted, code, message)
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        let providers = {
            let committed = self
                .committed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            committed.providers.values().cloned().collect::<Vec<_>>()
        };
        for provider in providers {
            provider.shutdown();
        }
    }
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn bounded_project_catalog_description(value: &str) -> String {
    if value.len() <= PLUGIN_MAX_PROJECT_CATALOG_DESCRIPTION_BYTES {
        return value.to_string();
    }
    let mut end = PLUGIN_MAX_PROJECT_CATALOG_DESCRIPTION_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

impl ProviderEntry {
    fn frozen_catalog(&self) -> Result<&PluginCatalog, ProviderFailure> {
        if self.failed.load(Ordering::SeqCst) {
            return Err(ProviderFailure::not_started("plugin_provider_unavailable"));
        }
        self.catalog
            .get()
            .ok_or_else(|| ProviderFailure::not_started("plugin_provider_unavailable"))
    }

    fn frozen_tools(&self) -> Result<Vec<PluginTool>, ProviderFailure> {
        Ok(self.frozen_catalog()?.tools().to_vec())
    }

    fn view(&self) -> PluginProviderView {
        let failed = self.failed.load(Ordering::SeqCst);
        PluginProviderView {
            provider_id: self.config.id.clone(),
            provider_instance_id: self.instance_id.clone(),
            name: self.config.name.clone(),
            status: if failed { "failed" } else { "ready" }.to_string(),
            error_code: self.error_code.lock().unwrap().clone(),
        }
    }

    fn retire(&self, code: &str) {
        self.failed.store(true, Ordering::SeqCst);
        *self.error_code.lock().unwrap() = Some(code.to_string());
    }

    fn with_connection<T>(
        &self,
        f: impl FnOnce(&mut ProviderConnection, Duration) -> Result<T, ProviderFailure>,
    ) -> Result<T, ProviderFailure> {
        if self.failed.load(Ordering::SeqCst) {
            return Err(ProviderFailure::not_started("plugin_provider_unavailable"));
        }
        let mut session = match self.session.try_lock() {
            Ok(session) => session,
            Err(TryLockError::WouldBlock) => {
                return Err(ProviderFailure {
                    dispatch_state: PluginDispatchState::NotStarted,
                    code: "plugin_provider_busy",
                    fatal: false,
                })
            }
            Err(TryLockError::Poisoned(_)) => {
                self.retire("plugin_provider_state_failed");
                return Err(ProviderFailure::not_started("plugin_provider_state_failed"));
            }
        };
        let Some(connection) = session.as_mut() else {
            self.retire("plugin_provider_unavailable");
            return Err(ProviderFailure::not_started("plugin_provider_unavailable"));
        };
        let timeout = self.timeout;
        match f(connection, timeout) {
            Ok(value) => Ok(value),
            Err(error) => {
                if error.fatal {
                    self.retire(error.code);
                    self.process.terminate();
                    *session = None;
                }
                Err(error)
            }
        }
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        expected_schema: &PluginSchemaObservation,
    ) -> Result<PluginToolResult, ProviderFailure> {
        let catalog = self.frozen_catalog()?;
        let Some(tool) = catalog.tool(name) else {
            return Err(ProviderFailure {
                dispatch_state: PluginDispatchState::NotStarted,
                code: "plugin_tool_unavailable",
                fatal: false,
            });
        };
        if &tool.schema_observation() != expected_schema {
            return Err(ProviderFailure {
                dispatch_state: PluginDispatchState::NotStarted,
                code: "plugin_schema_changed",
                fatal: false,
            });
        }
        validate_plugin_input_arguments(&tool.input_schema, &arguments).map_err(|_| {
            ProviderFailure {
                dispatch_state: PluginDispatchState::NotStarted,
                code: "plugin_arguments_schema_invalid",
                fatal: false,
            }
        })?;
        let output_schema = tool.output_schema.clone();
        self.with_connection(move |connection, timeout| {
            let result = connection.tools_call(name, arguments, timeout)?;
            if let Some(output_schema) = output_schema.as_ref() {
                let structured = result.structured_content.as_ref().ok_or_else(|| {
                    ProviderFailure::completed("plugin_output_schema_violation", true)
                })?;
                validate_plugin_structured_output(output_schema, structured).map_err(|_| {
                    ProviderFailure::completed("plugin_output_schema_violation", true)
                })?;
            }
            Ok(result)
        })
    }

    fn shutdown(&self) {
        self.process.terminate();
        if let Ok(mut session) = self.session.try_lock() {
            *session = None;
        }
    }
}

impl Drop for ProviderEntry {
    fn drop(&mut self) {
        self.process.terminate();
    }
}

impl PluginStderrDiagnostics {
    fn push_line(&mut self, text: String, truncated: bool) {
        let bytes = text.len();
        self.aggregate_bytes = self.aggregate_bytes.saturating_add(bytes);
        self.lines.push_back(PluginStderrLine { text, truncated });
        while self.lines.len() > PLUGIN_STDERR_MAX_LINES
            || self.aggregate_bytes > PLUGIN_STDERR_MAX_BYTES
        {
            let Some(removed) = self.lines.pop_front() else {
                break;
            };
            self.aggregate_bytes = self.aggregate_bytes.saturating_sub(removed.text.len());
        }
    }

    fn snapshot(&self) -> PluginStderrSnapshot {
        PluginStderrSnapshot {
            lines: self.lines.iter().cloned().collect(),
            aggregate_bytes: self.aggregate_bytes,
        }
    }
}

impl ProviderProcess {
    fn new() -> Self {
        Self {
            child: Mutex::new(None),
            stderr: Arc::new(Mutex::new(PluginStderrDiagnostics::default())),
            stderr_drained: Arc::new(AtomicBool::new(true)),
        }
    }

    fn stderr_snapshot(&self) -> PluginStderrSnapshot {
        self.stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot()
    }

    fn install(&self, child: ManagedChild) {
        let mut slot = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(slot.is_none());
        *slot = Some(child);
    }

    fn terminate(&self) {
        let child = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(mut child) = child {
            terminate_provider_process(&mut child);
        }
        self.wait_for_stderr_drain();
    }

    fn wait_for_stderr_drain(&self) {
        let deadline = Instant::now() + PLUGIN_STDERR_DRAIN_BUDGET;
        while !self.stderr_drained.load(Ordering::Acquire) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1).min(remaining));
        }
    }
}

impl Drop for ProviderProcess {
    fn drop(&mut self) {
        let child = self
            .child
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(mut child) = child {
            terminate_provider_process(&mut child);
        }
        self.wait_for_stderr_drain();
    }
}

fn prepare_provider(
    config: &PluginProviderConfig,
    shell: &ShellConfig,
    generation: u64,
    request_timeout: Duration,
    prepared_profiles: &PreparedShellProfileCache,
    stopping: &Arc<AtomicBool>,
) -> (
    Arc<ProviderEntry>,
    Option<Vec<PluginTool>>,
    Option<ProviderPreparationFailure>,
) {
    let process = Arc::new(ProviderProcess::new());
    let entry = Arc::new(ProviderEntry {
        config: config.clone(),
        instance_id: uuid::Uuid::new_v4().simple().to_string(),
        timeout: config
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(request_timeout),
        failed: AtomicBool::new(false),
        error_code: Mutex::new(None),
        process: Arc::clone(&process),
        catalog: OnceLock::new(),
        session: Mutex::new(None),
    });
    let cwd = config
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let environment = match PreparedExecutionEnvironment::prepare(
        generation,
        shell,
        config.profile.as_deref(),
        &cwd,
        prepared_profiles,
        Some(stopping.as_ref()),
    ) {
        Ok(environment) => environment,
        Err(_) => {
            entry.retire("plugin_environment_prepare_failed");
            return (
                entry,
                None,
                Some(ProviderPreparationFailure {
                    phase: PluginCheckPhase::Environment,
                    code: "plugin_environment_prepare_failed",
                    detail: "configured shell/profile environment could not be prepared",
                    diagnostic: None,
                }),
            );
        }
    };
    let mut command = match environment.native_command(&config.command, &config.args, &cwd) {
        Ok(command) => command,
        Err(error) => {
            let code = if error.starts_with("unsupported_executable_type:") {
                "plugin_executable_unsupported"
            } else {
                "plugin_executable_unavailable"
            };
            entry.retire(code);
            let detail = if code == "plugin_executable_unsupported" {
                "configured Plugin executable type is unsupported for the native Plugin ABI"
            } else {
                "configured Plugin executable could not be resolved in the prepared environment"
            };
            return (
                entry,
                None,
                Some(ProviderPreparationFailure {
                    phase: PluginCheckPhase::Executable,
                    code,
                    detail,
                    diagnostic: None,
                }),
            );
        }
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match ManagedChild::spawn(&mut command) {
        Ok(child) => child,
        Err(_) => {
            entry.retire("plugin_spawn_failed");
            return (
                entry,
                None,
                Some(ProviderPreparationFailure {
                    phase: PluginCheckPhase::Spawn,
                    code: "plugin_spawn_failed",
                    detail: "configured Plugin executable could not be started",
                    diagnostic: None,
                }),
            );
        }
    };
    let stdin = match child.child_mut().stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = child.terminate_tree();
            entry.retire("plugin_stdio_unavailable");
            return (
                entry,
                None,
                Some(ProviderPreparationFailure {
                    phase: PluginCheckPhase::Stdio,
                    code: "plugin_stdio_unavailable",
                    detail: "Plugin protocol stdio pipes could not be created",
                    diagnostic: None,
                }),
            );
        }
    };
    let stdout = match child.child_mut().stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.terminate_tree();
            entry.retire("plugin_stdio_unavailable");
            return (
                entry,
                None,
                Some(ProviderPreparationFailure {
                    phase: PluginCheckPhase::Stdio,
                    code: "plugin_stdio_unavailable",
                    detail: "Plugin protocol stdio pipes could not be created",
                    diagnostic: None,
                }),
            );
        }
    };
    let stderr = match child.child_mut().stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.terminate_tree();
            entry.retire("plugin_stdio_unavailable");
            return (
                entry,
                None,
                Some(ProviderPreparationFailure {
                    phase: PluginCheckPhase::Stdio,
                    code: "plugin_stdio_unavailable",
                    detail: "Plugin protocol stdio pipes could not be created",
                    diagnostic: None,
                }),
            );
        }
    };
    let stderr_thread_name = format!("wc-plugin-stderr-{}", config.id);
    let stderr_diagnostics = Arc::clone(&process.stderr);
    let stderr_drained = Arc::clone(&process.stderr_drained);
    stderr_drained.store(false, Ordering::Release);
    if std::thread::Builder::new()
        .name(stderr_thread_name)
        .spawn(move || {
            provider_stderr_reader(stderr, stderr_diagnostics);
            stderr_drained.store(true, Ordering::Release);
        })
        .is_err()
    {
        process.stderr_drained.store(true, Ordering::Release);
        let _ = child.terminate_tree();
        entry.retire("plugin_reader_unavailable");
        return (
            entry,
            None,
            Some(ProviderPreparationFailure {
                phase: PluginCheckPhase::Stdio,
                code: "plugin_reader_unavailable",
                detail: "Plugin stdout/stderr protocol workers could not be started",
                diagnostic: None,
            }),
        );
    }
    let (sender, incoming) = mpsc::sync_channel(PLUGIN_READER_QUEUE);
    let stdout_thread_name = format!("wc-plugin-{}", config.id);
    if std::thread::Builder::new()
        .name(stdout_thread_name)
        .spawn(move || provider_stdout_reader(stdout, sender))
        .is_err()
    {
        let _ = child.terminate_tree();
        entry.retire("plugin_reader_unavailable");
        return (
            entry,
            None,
            Some(ProviderPreparationFailure {
                phase: PluginCheckPhase::Stdio,
                code: "plugin_reader_unavailable",
                detail: "Plugin stdout/stderr protocol workers could not be started",
                diagnostic: None,
            }),
        );
    }
    let (writer, write_requests) = mpsc::sync_channel(PLUGIN_WRITER_QUEUE);
    let stdin_thread_name = format!("wc-plugin-stdin-{}", config.id);
    if std::thread::Builder::new()
        .name(stdin_thread_name)
        .spawn(move || provider_stdin_writer(stdin, write_requests))
        .is_err()
    {
        let _ = child.terminate_tree();
        entry.retire("plugin_writer_unavailable");
        return (
            entry,
            None,
            Some(ProviderPreparationFailure {
                phase: PluginCheckPhase::Stdio,
                code: "plugin_writer_unavailable",
                detail: "Plugin stdin writer worker could not be started",
                diagnostic: None,
            }),
        );
    }
    process.install(child);
    if stopping.load(Ordering::SeqCst) {
        process.terminate();
        entry.retire("plugin_manager_stopping");
        return (
            entry,
            None,
            Some(ProviderPreparationFailure {
                phase: PluginCheckPhase::Initialize,
                code: "plugin_manager_stopping",
                detail: "Plugin manager began stopping during candidate preparation",
                diagnostic: None,
            }),
        );
    }
    let mut connection = ProviderConnection {
        process: Arc::clone(&process),
        stopping: Arc::clone(stopping),
        writer,
        incoming,
        next_id: 1,
    };
    let timeout = config
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(request_timeout);
    if let Err(failure) = connection.initialize(timeout) {
        process.terminate();
        entry.retire(failure.code);
        return (
            entry,
            None,
            Some(ProviderPreparationFailure {
                phase: PluginCheckPhase::Initialize,
                code: failure.code,
                detail: initialize_failure_detail(failure.code),
                diagnostic: None,
            }),
        );
    }
    let tools = match connection.tools_list_with_diagnostic(timeout) {
        Ok(tools) => tools,
        Err(list_failure) => {
            let PluginToolsListFailure {
                failure,
                diagnostic,
            } = list_failure;
            process.terminate();
            entry.retire(failure.code);
            let phase = if failure.code == "plugin_tools_list_invalid" {
                PluginCheckPhase::Validation
            } else {
                PluginCheckPhase::ToolsList
            };
            let detail = if phase == PluginCheckPhase::Validation {
                "Plugin tools/list result violates Tool schema or Plugin bounds"
            } else {
                "Plugin tools/list did not complete successfully"
            };
            return (
                entry,
                None,
                Some(ProviderPreparationFailure {
                    phase,
                    code: failure.code,
                    detail,
                    diagnostic,
                }),
            );
        }
    };
    let catalog = match PluginCatalog::admit(tools) {
        Ok(catalog) => catalog,
        Err(_) => {
            process.terminate();
            entry.retire("plugin_tools_list_invalid");
            return (
                entry,
                None,
                Some(ProviderPreparationFailure {
                    phase: PluginCheckPhase::Validation,
                    code: "plugin_tools_list_invalid",
                    detail: "Plugin tools/list result could not be admitted as a canonical catalog",
                    diagnostic: None,
                }),
            );
        }
    };
    let tools = catalog.tools().to_vec();
    if entry.catalog.set(catalog).is_err() {
        process.terminate();
        entry.retire("plugin_provider_state_failed");
        return (
            entry,
            None,
            Some(ProviderPreparationFailure {
                phase: PluginCheckPhase::Validation,
                code: "plugin_provider_state_failed",
                detail: "Plugin provider catalog state could not be frozen",
                diagnostic: None,
            }),
        );
    }
    *entry.session.lock().unwrap() = Some(connection);
    (entry, Some(tools), None)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginToolsListResult {
    tools: Vec<PluginTool>,
}

impl ProviderConnection {
    fn initialize(&mut self, timeout: Duration) -> Result<(), ProviderFailure> {
        let result = self.request(
            "initialize",
            json!({"protocolVersion": PLUGIN_PROTOCOL_VERSION}),
            timeout,
            false,
        )?;
        webcodex_core::plugin::validate_json_value(
            &result,
            PLUGIN_MAX_MESSAGE_BYTES,
            "plugin initialize result",
        )
        .map_err(|_| ProviderFailure::not_started("plugin_initialize_invalid"))?;
        if result
            .as_object()
            .and_then(|object| object.get("protocolVersion"))
            .and_then(Value::as_str)
            != Some(PLUGIN_PROTOCOL_VERSION)
        {
            return Err(ProviderFailure::not_started(
                "plugin_protocol_version_mismatch",
            ));
        }
        Ok(())
    }

    fn tools_list_with_diagnostic(
        &mut self,
        timeout: Duration,
    ) -> Result<Vec<PluginTool>, PluginToolsListFailure> {
        self.tools_list_internal(timeout, true)
    }

    fn tools_list_internal(
        &mut self,
        timeout: Duration,
        include_diagnostic: bool,
    ) -> Result<Vec<PluginTool>, PluginToolsListFailure> {
        let result = self
            .request("tools/list", json!({}), timeout, false)
            .map_err(|failure| PluginToolsListFailure {
                failure,
                diagnostic: None,
            })?;
        let result: PluginToolsListResult =
            serde_json::from_value(result).map_err(|_| PluginToolsListFailure {
                failure: ProviderFailure::not_started("plugin_tools_list_invalid"),
                diagnostic: include_diagnostic.then(|| PluginCheckDiagnostic {
                    code: "tools_list_result_malformed".to_string(),
                    tool: None,
                    field: None,
                }),
            })?;
        if validate_tools(&result.tools).is_err() {
            return Err(PluginToolsListFailure {
                failure: ProviderFailure::not_started("plugin_tools_list_invalid"),
                diagnostic: include_diagnostic.then(|| diagnose_invalid_tools(&result.tools)),
            });
        }
        Ok(result.tools)
    }

    fn tools_call(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<PluginToolResult, ProviderFailure> {
        let result = self.request(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
            timeout,
            true,
        )?;
        let result: PluginToolResult = serde_json::from_value(result)
            .map_err(|_| ProviderFailure::completed("plugin_result_invalid", true))?;
        validate_tool_result(&result)
            .map_err(|_| ProviderFailure::completed("plugin_result_invalid", true))?;
        Ok(result)
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
        effectful: bool,
    ) -> Result<Value, ProviderFailure> {
        // The deadline is created before any request-side validation or
        // serialization. Queue admission, the complete write+flush ack, and the
        // response wait all consume this same absolute budget.
        let deadline = Instant::now() + timeout;
        match self.incoming.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) | Ok(ReaderEvent::Eof) => {
                return Err(ProviderFailure::not_started("plugin_eof"));
            }
            Ok(ReaderEvent::Message(Ok(_))) => {
                return Err(ProviderFailure::not_started("plugin_unexpected_message"));
            }
            Ok(ReaderEvent::Message(Err(fault))) => {
                return Err(ProviderFailure::not_started(reader_fault_code(fault)));
            }
        }

        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut encoded = serde_json::to_vec(&message)
            .map_err(|_| ProviderFailure::not_started("plugin_request_invalid"))?;
        if encoded.len() > PLUGIN_MAX_MESSAGE_BYTES {
            return Err(ProviderFailure::not_started("plugin_request_too_large"));
        }
        encoded.push(b'\n');
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            return Err(ProviderFailure::before_send_timeout());
        }

        let (ack_sender, ack_receiver) = mpsc::channel();
        let mut write_request = WriteRequest {
            frame: encoded,
            ack: ack_sender,
        };
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                return Err(ProviderFailure::not_started("plugin_manager_stopping"));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ProviderFailure::before_send_timeout());
            }
            match self.writer.try_send(write_request) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(request)) => {
                    write_request = request;
                    std::thread::sleep(Duration::from_millis(1).min(remaining));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(ProviderFailure::not_started("plugin_stdin_failed"));
                }
            }
        }

        // Once the bounded queue accepted the frame, an effectful request may
        // already have started writing. A timeout or transport failure before
        // the full write+flush ack is therefore OutcomeUnknown for effects.
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                self.process.terminate();
                return Err(ProviderFailure::after_send(
                    "plugin_manager_stopping",
                    effectful,
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ProviderFailure::after_send("plugin_timeout", effectful));
            }
            match ack_receiver.recv_timeout(remaining.min(PLUGIN_STOP_POLL)) {
                Ok(WriteAck::Written) => break,
                Ok(WriteAck::Failed) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProviderFailure::after_send(
                        "plugin_stdin_failed",
                        effectful,
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            }
        }

        loop {
            if self.stopping.load(Ordering::SeqCst) {
                self.process.terminate();
                return Err(ProviderFailure::after_send(
                    "plugin_manager_stopping",
                    effectful,
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ProviderFailure::after_send("plugin_timeout", effectful));
            }
            let response = match self.incoming.recv_timeout(remaining.min(PLUGIN_STOP_POLL)) {
                Ok(ReaderEvent::Message(Ok(response))) => response,
                Ok(ReaderEvent::Message(Err(fault))) => {
                    return Err(ProviderFailure::after_send(
                        reader_fault_code(fault),
                        effectful,
                    ));
                }
                Ok(ReaderEvent::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProviderFailure::after_send("plugin_eof", effectful));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            };
            return validate_rpc_response(response, id, effectful);
        }
    }
}

fn terminate_provider_process(child: &mut ManagedChild) {
    let deadline = Instant::now() + PLUGIN_TERMINATION_BUDGET;
    let _ = child.terminate_tree();
    let _ = child.wait_tree_exit(deadline.saturating_duration_since(Instant::now()));
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10).min(remaining));
            }
        }
    }
}

fn provider_stdin_writer(mut stdin: ChildStdin, requests: mpsc::Receiver<WriteRequest>) {
    while let Ok(request) = requests.recv() {
        let result = stdin.write_all(&request.frame).and_then(|_| stdin.flush());
        let failed = result.is_err();
        let _ = request.ack.send(if failed {
            WriteAck::Failed
        } else {
            WriteAck::Written
        });
        if failed {
            return;
        }
    }
}

fn provider_stderr_reader(mut stderr: impl Read, diagnostics: Arc<Mutex<PluginStderrDiagnostics>>) {
    let mut buffer = [0u8; 4096];
    let mut line = Vec::with_capacity(PLUGIN_STDERR_MAX_LINE_BYTES);
    let mut truncated = false;
    loop {
        let read = match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        for byte in &buffer[..read] {
            if *byte == b'\n' {
                let text = String::from_utf8(line.clone()).unwrap_or_default();
                diagnostics
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push_line(text, truncated);
                line.clear();
                truncated = false;
                continue;
            }
            if *byte == b'\r' {
                continue;
            }
            if line.len() < PLUGIN_STDERR_MAX_LINE_BYTES {
                line.push(match *byte {
                    b'\t' => b' ',
                    b' '..=b'~' => *byte,
                    _ => b'?',
                });
            } else {
                truncated = true;
            }
        }
    }
    if !line.is_empty() || truncated {
        let text = String::from_utf8(line).unwrap_or_default();
        diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_line(text, truncated);
    }
}

fn provider_stdout_reader(
    stdout: std::process::ChildStdout,
    sender: mpsc::SyncSender<ReaderEvent>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let message = match read_bounded_line(&mut reader) {
            Ok(Some(message)) => ReaderEvent::Message(
                serde_json::from_slice(&message).map_err(|_| ReaderFault::Malformed),
            ),
            Ok(None) => ReaderEvent::Eof,
            Err(fault) => ReaderEvent::Message(Err(fault)),
        };
        let terminal = !matches!(&message, ReaderEvent::Message(Ok(_)));
        if sender.send(message).is_err() || terminal {
            return;
        }
    }
}

fn read_bounded_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, ReaderFault> {
    let mut line = Vec::new();
    let read = (&mut *reader)
        .take((PLUGIN_MAX_MESSAGE_BYTES + 2) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|_| ReaderFault::Io)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(if line.len() > PLUGIN_MAX_MESSAGE_BYTES {
            ReaderFault::TooLarge
        } else {
            ReaderFault::Malformed
        });
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.is_empty() || line.len() > PLUGIN_MAX_MESSAGE_BYTES {
        return Err(if line.len() > PLUGIN_MAX_MESSAGE_BYTES {
            ReaderFault::TooLarge
        } else {
            ReaderFault::Malformed
        });
    }
    Ok(Some(line))
}

fn validate_rpc_response(
    response: Value,
    expected_id: u64,
    effectful: bool,
) -> Result<Value, ProviderFailure> {
    let object = response
        .as_object()
        .ok_or_else(|| ProviderFailure::after_send("plugin_protocol_error", effectful))?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || object.contains_key("method")
    {
        return Err(ProviderFailure::after_send(
            "plugin_protocol_error",
            effectful,
        ));
    }
    let response_id = object
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| ProviderFailure::after_send("plugin_response_id_invalid", effectful))?;
    if response_id != expected_id {
        return Err(ProviderFailure::after_send(
            if response_id < expected_id {
                "plugin_duplicate_response_id"
            } else {
                "plugin_unknown_response_id"
            },
            effectful,
        ));
    }
    match (object.get("result"), object.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(Value::Object(error)))
            if error.get("code").and_then(Value::as_i64).is_some()
                && error
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| !message.is_empty() && message.len() <= 512) =>
        {
            Err(ProviderFailure::completed("plugin_rpc_error", false))
        }
        _ => Err(ProviderFailure::after_send(
            "plugin_protocol_error",
            effectful,
        )),
    }
}

fn reader_fault_code(fault: ReaderFault) -> &'static str {
    match fault {
        ReaderFault::Malformed => "plugin_malformed_json",
        ReaderFault::TooLarge => "plugin_message_too_large",
        ReaderFault::Io => "plugin_stdout_failed",
    }
}

fn provider_failure_response(error: ProviderFailure) -> PluginGatewayResponse {
    let message = match error.dispatch_state {
        PluginDispatchState::NotStarted => {
            "Plugin request was not started; no downstream effect was dispatched"
        }
        PluginDispatchState::OutcomeUnknown => {
            "Plugin request may have been dispatched; outcome is unknown and must not be retried automatically"
        }
        PluginDispatchState::Completed => {
            "Plugin completed the request-response exchange but returned an error or invalid bounded result"
        }
    };
    gateway_error(error.dispatch_state, error.code, message)
}

fn gateway_error(
    state: PluginDispatchState,
    code: &'static str,
    message: &'static str,
) -> PluginGatewayResponse {
    PluginGatewayResponse::error(state, code, message)
}

fn stale_provider() -> PluginGatewayResponse {
    gateway_error(
        PluginDispatchState::NotStarted,
        "stale_plugin_provider",
        "Exact Plugin provider instance is unavailable; request was not routed elsewhere",
    )
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "plugin_check_tests.rs"]
mod check_tests;
