#[cfg(windows)]
use super::config::{dialect_for_program, platform_default_dialect, ShellDialect};
use super::config::{
    validate_shell_config, RunnerPolicy, ShellConfig, ShellProfileConfig, SshConfig,
};
use super::projects::{find_project_shell_context_by_id, RunnerProjectShellContext};
#[cfg(any(unix, windows))]
use super::remote_shell::{remote_shell_bootstrap, RemoteShellTransport};
#[cfg(unix)]
use super::shell::shell_quote;
#[cfg(windows)]
use super::shell::shell_quote_powershell;
use super::shell::{base_shell_env, cwd_allowed};
use super::ssh::SshConnectionPool;
#[cfg(test)]
use crate::runner_protocol::RunnerRequest;
use crate::runner_protocol::{
    PersistentShellRequest, PersistentShellResult, RAW_SHELL_COMMAND_MAX_BYTES,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use webcodex_core::runner_operation::RunnerPersistentShellOperation;
#[cfg(any(unix, windows))]
use webcodex_persistent_shell::canonical_dialect;
use webcodex_persistent_shell::{
    PersistentShellManager as ProcessManager, ShellError, ShellExecResult, ShellIdentity,
    ShellLaunch, ShellLimits, ShellState, ShellSummary,
};

const EXECUTOR_AGENT: &str = "agent";
/// Remote named-SSH persistent shells use this executor on supported hosts.
#[cfg(any(unix, windows))]
const EXECUTOR_SSH: &str = "ssh";
const TERMINAL_RECORDS: usize = 128;

#[derive(Debug, Clone)]
pub(crate) struct PersistentShellManager {
    processes: ProcessManager,
    /// Runner-local SSH authority/preparation state. Windows persistent SSH uses
    /// the named-resource resolver without Unix ControlMaster multiplexing.
    ssh_pool: SshConnectionPool,
}

impl PersistentShellManager {
    pub(crate) fn new(shell: &ShellConfig, ssh_pool: super::ssh::SshConnectionPool) -> Self {
        Self {
            processes: ProcessManager::new(limits(shell)),
            ssh_pool,
        }
    }

    pub(crate) fn handle_operation(
        &self,
        policy: &RunnerPolicy,
        shell: &ShellConfig,
        ssh: &SshConfig,
        ssh_generation: u64,
        project_registry_dir: &Path,
        client_id: &str,
        request: &RunnerPersistentShellOperation,
    ) -> PersistentShellResult {
        self.processes.update_limits(limits(shell));
        let ssh_resource = request
            .job_context
            .as_ref()
            .and_then(|context| context.ssh_resource.as_deref());
        let operation = &request.request;
        if operation.action == "close" {
            return self.close(operation);
        }

        let project =
            match validate_boundary(policy, shell, project_registry_dir, client_id, operation) {
                Ok(project) => project,
                Err((code, message)) => {
                    if operation.action != "open" {
                        let _ = self.processes.close(
                            &operation.shell_id,
                            &operation.workflow_session_id,
                            &operation.runtime_project_id,
                            code,
                        );
                    }
                    return error_result(
                        &operation.shell_id,
                        &operation.workflow_session_id,
                        &operation.runtime_project_id,
                        code,
                        message,
                    );
                }
            };

        match operation.action.as_str() {
            "open" => {
                if let Some(resource) = ssh_resource {
                    self.open_ssh(
                        policy,
                        ssh,
                        ssh_generation,
                        client_id,
                        operation,
                        resource,
                        &project,
                    )
                } else {
                    self.open(policy, shell, client_id, operation, &project)
                }
            }
            "exec" => {
                if let Some(resource) = ssh_resource {
                    self.exec_ssh(policy, ssh, ssh_generation, operation, resource, &project)
                } else {
                    self.exec(policy, shell, operation, &project)
                }
            }
            "status" => {
                if let Some(resource) = ssh_resource {
                    self.status_ssh(ssh, ssh_generation, operation, resource)
                } else {
                    self.status(operation)
                }
            }
            _ => error_result(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
                "persistent_shell_invalid_action",
                format!("unsupported persistent shell action '{}'", operation.action),
            ),
        }
    }

    #[cfg(any(unix, windows))]
    fn open_ssh(
        &self,
        policy: &RunnerPolicy,
        ssh: &SshConfig,
        ssh_generation: u64,
        client_id: &str,
        operation: &PersistentShellRequest,
        resource_name: &str,
        _project: &RunnerProjectShellContext,
    ) -> PersistentShellResult {
        if !policy.allow_raw_shell {
            return error_result(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
                "raw_shell_disabled",
                "persistent shells are disabled by the current Runner raw shell policy",
            );
        }
        let explicit = operation.shell.as_deref();
        if explicit.is_some_and(|dialect| !matches!(dialect, "sh" | "bash")) {
            return error_result(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
                "persistent_shell_dialect_unsupported",
                "persistent shell must be 'sh' or 'bash'",
            );
        }
        let shell_program = explicit.unwrap_or("sh");
        let dialect = canonical_dialect(shell_program).unwrap_or("sh").to_string();
        let requested_cwd = operation
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
            .map(str::to_string);
        let (transport, default_cwd) = match RemoteShellTransport::spawn(
            &self.ssh_pool,
            ssh_generation,
            ssh,
            resource_name,
            &operation.workflow_session_id,
            shell_program,
            policy.max_output_bytes,
        ) {
            Ok((transport, default_cwd)) => (transport, default_cwd),
            Err(error) => {
                return error_result(
                    &operation.shell_id,
                    &operation.workflow_session_id,
                    &operation.runtime_project_id,
                    error.code,
                    error.message,
                )
            }
        };
        // Remote cwd priority: explicit open cwd > Session/runner-provided cwd
        // > SSH resource default_cwd > remote login default. The effective cwd
        // is applied to the remote shell's bootstrap below; `initial_cwd` only
        // seeds the summary until the first control frame reports the trusted
        // absolute cwd. When nothing is requested, no `cd` is issued and the
        // shell keeps the remote login directory (never a fabricated `/`).
        let effective_cwd = requested_cwd.or(default_cwd);
        let initial_cwd = effective_cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_default();
        let identity = ShellIdentity {
            shell_id: operation.shell_id.clone(),
            workflow_session_id: operation.workflow_session_id.clone(),
            runtime_project_id: operation.runtime_project_id.clone(),
            executor: EXECUTOR_SSH.to_string(),
            client_id: Some(client_id.to_string()),
        };
        // The bootstrap is the initialization command: it reserves FD 7/8 on the
        // remote shell (so the shared command wrapper's markers always reach the
        // Runner regardless of later user redirects) and applies the effective
        // remote cwd, runs once, drains, and its control frame reports the
        // trusted absolute cwd used to seed the shell. A failed `cd` makes the
        // initialization control frame report a non-zero status, so open fails
        // and the remote shell is torn down instead of falling back to the
        // login directory. It never reaches later commands. No local profile
        // env or init script is sent to the remote host.
        let initialization = remote_shell_bootstrap(effective_cwd.as_deref());
        let transport_box: Box<dyn webcodex_persistent_shell::ShellTransport> = Box::new(transport);
        match self.processes.open_with_transport(
            identity,
            dialect,
            None,
            initial_cwd,
            Some(initialization),
            transport_box,
        ) {
            Ok(summary) => summary_result(summary, "opened", false),
            Err(error) => shell_error_result(operation, error),
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn open_ssh(
        &self,
        _policy: &RunnerPolicy,
        _ssh: &SshConfig,
        _ssh_generation: u64,
        _client_id: &str,
        operation: &PersistentShellRequest,
        _resource_name: &str,
        _project: &RunnerProjectShellContext,
    ) -> PersistentShellResult {
        error_result(
            &operation.shell_id,
            &operation.workflow_session_id,
            &operation.runtime_project_id,
            "persistent_shell_unsupported",
            "named SSH persistent shell is not supported on this Runner host",
        )
    }

    fn exec_ssh(
        &self,
        policy: &RunnerPolicy,
        ssh: &SshConfig,
        ssh_generation: u64,
        operation: &PersistentShellRequest,
        resource_name: &str,
        _project: &RunnerProjectShellContext,
    ) -> PersistentShellResult {
        if !policy.allow_raw_shell {
            return error_result(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
                "raw_shell_disabled",
                "persistent shells are disabled by the current Runner raw shell policy",
            );
        }
        if let Err((code, message)) = validate_ssh_binding_current(
            &self.processes,
            ssh,
            ssh_generation,
            operation,
            resource_name,
        ) {
            let _ = self.processes.close(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
                code,
            );
            return error_result(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
                code,
                message,
            );
        }
        let command = match operation.command.as_deref() {
            Some(command) => command,
            None => {
                return summary_error_result_from_status(
                    &self.processes,
                    operation,
                    "persistent_shell_invalid_request",
                    "command is required for persistent shell exec",
                )
            }
        };
        if command.len() > RAW_SHELL_COMMAND_MAX_BYTES {
            return summary_error_result_from_status(
                &self.processes,
                operation,
                "persistent_shell_invalid_command",
                format!("command exceeds the {RAW_SHELL_COMMAND_MAX_BYTES}-byte Runner limit"),
            );
        }
        let timeout_secs = operation.timeout_secs.unwrap_or(30);
        if timeout_secs == 0 || timeout_secs > policy.max_timeout_secs {
            return summary_error_result_from_status(
                &self.processes,
                operation,
                "persistent_shell_invalid_timeout",
                format!(
                    "timeout_secs must be between 1 and {}",
                    policy.max_timeout_secs
                ),
            );
        }
        if let Err(error) = self.processes.set_output_limit(
            &operation.shell_id,
            &operation.workflow_session_id,
            &operation.runtime_project_id,
            policy.max_output_bytes,
        ) {
            return shell_error_result(operation, error);
        }
        match self.processes.exec(
            &operation.shell_id,
            &operation.workflow_session_id,
            &operation.runtime_project_id,
            command,
            Duration::from_secs(timeout_secs),
        ) {
            Ok(result) => exec_result(operation, result),
            Err(error) => match self.processes.status(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
            ) {
                Ok(summary) => summary_error_result(summary, error.code, error.message),
                Err(_) => shell_error_result(operation, error),
            },
        }
    }

    fn status_ssh(
        &self,
        ssh: &SshConfig,
        ssh_generation: u64,
        operation: &PersistentShellRequest,
        resource_name: &str,
    ) -> PersistentShellResult {
        if let Err((code, message)) = validate_ssh_binding_current(
            &self.processes,
            ssh,
            ssh_generation,
            operation,
            resource_name,
        ) {
            let _ = self.processes.close(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
                code,
            );
            return error_result(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
                code,
                message,
            );
        }
        self.status(operation)
    }

    fn open(
        &self,
        policy: &RunnerPolicy,
        shell: &ShellConfig,
        client_id: &str,
        operation: &PersistentShellRequest,
        project: &RunnerProjectShellContext,
    ) -> PersistentShellResult {
        let launch = match build_launch(policy, shell, client_id, operation, project) {
            Ok(launch) => launch,
            Err((code, message)) => {
                return error_result(
                    &operation.shell_id,
                    &operation.workflow_session_id,
                    &operation.runtime_project_id,
                    code,
                    message,
                )
            }
        };
        match self.processes.open(launch) {
            Ok(summary) => {
                if let Err((code, message)) =
                    validate_open_shell_boundary(policy, shell, project, &summary)
                {
                    let terminal = self.processes.close(
                        &operation.shell_id,
                        &operation.workflow_session_id,
                        &operation.runtime_project_id,
                        code,
                    );
                    let mut result = terminal
                        .map(|closed| {
                            summary_result(closed.summary, "rejected", closed.already_closed)
                        })
                        .unwrap_or_else(|_| {
                            error_result(
                                &operation.shell_id,
                                &operation.workflow_session_id,
                                &operation.runtime_project_id,
                                code,
                                &message,
                            )
                        });
                    result.error_code = Some(code.to_string());
                    result.error = Some(message);
                    result
                } else {
                    summary_result(summary, "opened", false)
                }
            }
            Err(error) => shell_error_result(operation, error),
        }
    }

    fn exec(
        &self,
        policy: &RunnerPolicy,
        shell: &ShellConfig,
        operation: &PersistentShellRequest,
        project: &RunnerProjectShellContext,
    ) -> PersistentShellResult {
        let summary = match self.processes.status(
            &operation.shell_id,
            &operation.workflow_session_id,
            &operation.runtime_project_id,
        ) {
            Ok(summary) => summary,
            Err(error) => return shell_error_result(operation, error),
        };
        if let Err((code, message)) = validate_open_shell_boundary(policy, shell, project, &summary)
        {
            let _ = self.processes.close(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
                code,
            );
            return error_result(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
                code,
                message,
            );
        }
        let command = match operation.command.as_deref() {
            Some(command) => command,
            None => {
                return summary_error_result(
                    summary,
                    "persistent_shell_invalid_request",
                    "command is required for persistent shell exec",
                )
            }
        };
        if command.len() > RAW_SHELL_COMMAND_MAX_BYTES {
            return summary_error_result(
                summary,
                "persistent_shell_invalid_command",
                format!("command exceeds the {RAW_SHELL_COMMAND_MAX_BYTES}-byte Runner limit"),
            );
        }
        let timeout_secs = operation.timeout_secs.unwrap_or(30);
        if timeout_secs == 0 || timeout_secs > policy.max_timeout_secs {
            return summary_error_result(
                summary,
                "persistent_shell_invalid_timeout",
                format!(
                    "timeout_secs must be between 1 and {}",
                    policy.max_timeout_secs
                ),
            );
        }
        if let Err(error) = self.processes.set_output_limit(
            &operation.shell_id,
            &operation.workflow_session_id,
            &operation.runtime_project_id,
            policy.max_output_bytes,
        ) {
            return shell_error_result(operation, error);
        }
        match self.processes.exec(
            &operation.shell_id,
            &operation.workflow_session_id,
            &operation.runtime_project_id,
            command,
            Duration::from_secs(timeout_secs),
        ) {
            Ok(result) => self.finalize_exec(policy, shell, operation, project, result),
            Err(error) => match self.processes.status(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
            ) {
                Ok(summary) => summary_error_result(summary, error.code, error.message),
                Err(_) => shell_error_result(operation, error),
            },
        }
    }

    fn finalize_exec(
        &self,
        policy: &RunnerPolicy,
        shell: &ShellConfig,
        operation: &PersistentShellRequest,
        project: &RunnerProjectShellContext,
        mut result: ShellExecResult,
    ) -> PersistentShellResult {
        if result.shell_state != ShellState::Running {
            return exec_result(operation, result);
        }
        let boundary = self
            .processes
            .status(
                &operation.shell_id,
                &operation.workflow_session_id,
                &operation.runtime_project_id,
            )
            .map_err(|error| (error.code, error.message))
            .and_then(|summary| validate_open_shell_boundary(policy, shell, project, &summary));
        if let Err((code, message)) = boundary {
            let terminal_state = self
                .processes
                .close(
                    &operation.shell_id,
                    &operation.workflow_session_id,
                    &operation.runtime_project_id,
                    code,
                )
                .map(|closed| closed.summary.state)
                .unwrap_or(ShellState::Poisoned);
            result.shell_state = terminal_state;
            result.error_code = Some("shell_reset_required".to_string());
            result.error = Some(format!(
                "{message}; persistent shell was closed before another command could run"
            ));
        }
        exec_result(operation, result)
    }

    fn status(&self, operation: &PersistentShellRequest) -> PersistentShellResult {
        match self.processes.status(
            &operation.shell_id,
            &operation.workflow_session_id,
            &operation.runtime_project_id,
        ) {
            Ok(summary) => {
                let state = if summary.busy { "executing" } else { "idle" };
                summary_result(summary, state, false)
            }
            Err(error) => shell_error_result(operation, error),
        }
    }

    fn close(&self, operation: &PersistentShellRequest) -> PersistentShellResult {
        let reason = operation.purpose.as_deref().unwrap_or("explicit_close");
        match self.processes.close(
            &operation.shell_id,
            &operation.workflow_session_id,
            &operation.runtime_project_id,
            reason,
        ) {
            Ok(result) => summary_result(result.summary, "closed", result.already_closed),
            Err(error) => shell_error_result(operation, error),
        }
    }

    pub(crate) fn close_project(&self, runtime_project_id: &str, reason: &str) -> usize {
        self.processes.close_project(runtime_project_id, reason)
    }

    #[cfg(test)]
    pub(crate) fn handle(
        &self,
        policy: &RunnerPolicy,
        shell: &ShellConfig,
        ssh: &SshConfig,
        ssh_generation: u64,
        project_registry_dir: &Path,
        request: &RunnerRequest,
    ) -> PersistentShellResult {
        let Some(operation) = request.persistent_shell.clone() else {
            return error_result(
                "",
                "",
                "",
                "persistent_shell_invalid_request",
                "persistent shell payload is required",
            );
        };
        self.handle_operation(
            policy,
            shell,
            ssh,
            ssh_generation,
            project_registry_dir,
            &request.client_id,
            &RunnerPersistentShellOperation {
                request: operation,
                job_context: request.job_context.clone(),
            },
        )
    }

    pub(crate) fn close_exact(
        &self,
        shell_id: &str,
        workflow_session_id: &str,
        runtime_project_id: &str,
        reason: &str,
    ) -> Result<(), ShellError> {
        self.processes
            .close(shell_id, workflow_session_id, runtime_project_id, reason)
            .map(|_| ())
    }

    pub(crate) fn close_all(&self, reason: &str) -> usize {
        self.processes.close_all(reason)
    }

    #[cfg(test)]
    pub(crate) fn active_count(&self) -> usize {
        self.processes.active_count()
    }
}

fn limits(shell: &ShellConfig) -> ShellLimits {
    ShellLimits {
        max_shells: shell.max_persistent_shells,
        idle_timeout: Duration::from_secs(shell.persistent_shell_idle_timeout_secs),
        max_terminal_records: TERMINAL_RECORDS,
    }
}

fn validate_boundary(
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    client_id: &str,
    operation: &PersistentShellRequest,
) -> Result<RunnerProjectShellContext, (&'static str, String)> {
    if !policy.allow_raw_shell {
        return Err((
            "raw_shell_disabled",
            "persistent shells are disabled by the current Runner raw shell policy".to_string(),
        ));
    }
    validate_shell_config(shell).map_err(|message| ("persistent_shell_config_invalid", message))?;
    let prefix = format!("agent:{client_id}:");
    let project_id = operation
        .runtime_project_id
        .strip_prefix(&prefix)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            (
                "persistent_shell_project_mismatch",
                "runtime project does not belong to this Runner".to_string(),
            )
        })?;
    find_project_shell_context_by_id(project_registry_dir, project_id).ok_or_else(|| {
        (
            "persistent_shell_project_unavailable",
            "project is disabled, unregistered, or not executable by this Runner".to_string(),
        )
    })
}

/// Confirm the SSH binding this shell opened against is still current. The
/// binding captures the named resource and the Runner configuration generation
/// at open time (stored as transport metadata on the shared shell entry), and
/// every later `exec`/`status` on an *active* shell compares it against the
/// active config. A removed resource, an unknown active generation, or a
/// generation that advanced past the opened one invalidates an already-open
/// remote shell: it must be closed and reopened rather than reused against a
/// stale transport. A terminal shell (closed, exited, poisoned, lost) has
/// already released its binding and simply reports its terminal state through
/// the shared state machine.
fn validate_ssh_binding_current(
    processes: &ProcessManager,
    ssh: &SshConfig,
    ssh_generation: u64,
    operation: &PersistentShellRequest,
    resource_name: &str,
) -> Result<(), (&'static str, String)> {
    let summary = match processes.status(
        &operation.shell_id,
        &operation.workflow_session_id,
        &operation.runtime_project_id,
    ) {
        Ok(summary) => summary,
        Err(error) => return Err((error.code, error.message)),
    };
    if !matches!(summary.state, ShellState::Opening | ShellState::Running) {
        // The shell cannot run a command anymore; leave its terminal state and
        // the Server record intact instead of forcing a reset.
        return Ok(());
    }
    let metadata = match processes.metadata(
        &operation.shell_id,
        &operation.workflow_session_id,
        &operation.runtime_project_id,
    ) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err((error.code, error.message));
        }
    };
    let opened_generation = match metadata.as_ref().and_then(|metadata| metadata.generation) {
        Some(generation) => generation,
        None => {
            // No transport binding was recorded: the shell was not opened as an
            // SSH persistent shell (or has already gone terminal). Never run a
            // user command against a shell whose binding cannot be verified.
            return Err((
                "shell_reset_required",
                "persistent shell has no recorded SSH binding; close and reopen it".to_string(),
            ));
        }
    };
    if metadata
        .as_ref()
        .and_then(|metadata| metadata.resource.as_deref())
        != Some(resource_name)
    {
        return Err((
            "shell_reset_required",
            format!(
                "persistent shell is bound to a different SSH resource than requested; close and reopen it (expected '{}')",
                resource_name
            ),
        ));
    }
    if !ssh.resources.contains_key(resource_name) {
        return Err((
            "shell_reset_required",
            format!(
                "SSH resource '{}' is no longer configured on this Runner; close and reopen the persistent shell",
                resource_name
            ),
        ));
    }
    if ssh_generation == 0 {
        return Err((
            "shell_reset_required",
            "SSH configuration generation is unknown; close and reopen the persistent shell"
                .to_string(),
        ));
    }
    if opened_generation != ssh_generation {
        return Err((
            "shell_reset_required",
            format!(
                "SSH resource '{}' changed to configuration generation {ssh_generation} after this persistent shell was opened at generation {opened_generation}; close and reopen the persistent shell",
                resource_name
            ),
        ));
    }
    Ok(())
}

fn resolve_cwd(
    project: &RunnerProjectShellContext,
    requested: Option<&str>,
) -> Result<PathBuf, (&'static str, String)> {
    let root = PathBuf::from(&project.path)
        .canonicalize()
        .map_err(|error| {
            (
                "persistent_shell_project_unavailable",
                format!("failed to access project root: {error}"),
            )
        })?;
    let requested = requested
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| root.clone());
    let requested = if requested.is_absolute() {
        requested
    } else {
        root.join(requested)
    };
    let cwd = requested.canonicalize().map_err(|error| {
        (
            "persistent_shell_cwd_invalid",
            format!("failed to access persistent shell cwd: {error}"),
        )
    })?;
    if cwd != root && !cwd.starts_with(&root) {
        return Err((
            "persistent_shell_cwd_outside_project",
            "persistent shell cwd is outside the registered project root".to_string(),
        ));
    }
    Ok(cwd)
}

fn selected_profile<'a>(
    shell: &'a ShellConfig,
    project: &RunnerProjectShellContext,
) -> Result<(Option<String>, Option<&'a ShellProfileConfig>), (&'static str, String)> {
    let name = project
        .shell_profile
        .as_deref()
        .or(shell.default_profile.as_deref());
    match name {
        Some(name) => shell
            .profiles
            .get(name)
            .map(|profile| (Some(name.to_string()), Some(profile)))
            .ok_or_else(|| {
                (
                    "persistent_shell_profile_unavailable",
                    format!("shell profile '{name}' is not configured"),
                )
            }),
        None => Ok((None, None)),
    }
}

fn build_launch(
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    client_id: &str,
    operation: &PersistentShellRequest,
    project: &RunnerProjectShellContext,
) -> Result<ShellLaunch, (&'static str, String)> {
    let cwd = resolve_cwd(project, operation.cwd.as_deref())?;
    cwd_allowed(policy, &cwd).map_err(|message| ("persistent_shell_cwd_denied", message))?;
    build_launch_at_cwd(
        shell,
        client_id,
        operation,
        project,
        cwd,
        policy.max_output_bytes,
    )
}

fn build_launch_at_cwd(
    shell: &ShellConfig,
    client_id: &str,
    operation: &PersistentShellRequest,
    project: &RunnerProjectShellContext,
    cwd: PathBuf,
    max_output_bytes: usize,
) -> Result<ShellLaunch, (&'static str, String)> {
    let (profile_name, profile) = selected_profile(shell, project)?;
    let empty_profile = ShellProfileConfig::default();
    let env = base_shell_env(shell, profile.unwrap_or(&empty_profile))
        .map_err(|message| ("persistent_shell_environment_invalid", message))?;

    #[cfg(unix)]
    let (program, dialect, args, initialization) = {
        let explicit = operation.shell.as_deref();
        if explicit.is_some_and(|dialect| !matches!(dialect, "sh" | "bash")) {
            return Err((
                "persistent_shell_dialect_unsupported",
                "persistent shell must be 'sh' or 'bash'".to_string(),
            ));
        }
        let program = explicit
            .map(str::to_string)
            .or_else(|| profile.and_then(|profile| profile.program.clone()))
            .unwrap_or_else(|| shell.program.clone());
        let dialect = canonical_dialect(&program).ok_or_else(|| {
            (
                "persistent_shell_dialect_unsupported",
                "configured persistent shell program must resolve to sh or bash".to_string(),
            )
        })?;
        let initialization = match profile {
            Some(profile) => profile.init_script.clone(),
            None => shell
                .init_script
                .as_ref()
                .map(|path| format!(". {}", shell_quote(&path.to_string_lossy()))),
        };
        let args = if dialect == "bash" {
            vec!["--noprofile".to_string(), "--norc".to_string()]
        } else {
            Vec::new()
        };
        (program, dialect.to_string(), args, initialization)
    };

    #[cfg(windows)]
    let (program, dialect, args, initialization) = {
        if let Some(explicit) = operation.shell.as_deref() {
            return Err((
                "persistent_shell_dialect_unsupported",
                format!(
                    "Windows local persistent shell uses the configured PowerShell profile; explicit shell override '{explicit}' is unsupported"
                ),
            ));
        }
        let program = profile
            .and_then(|profile| profile.program.clone())
            .unwrap_or_else(|| shell.program.clone());
        let resolved_dialect = profile
            .and_then(|profile| profile.dialect)
            .or(shell.dialect)
            .or_else(|| dialect_for_program(&program))
            .unwrap_or_else(platform_default_dialect);
        if resolved_dialect != ShellDialect::PowerShell {
            return Err((
                "persistent_shell_dialect_unsupported",
                "Windows local persistent shell requires a configured PowerShell program/profile"
                    .to_string(),
            ));
        }
        let configured_args = profile
            .and_then(|profile| profile.args.clone())
            .unwrap_or_else(|| shell.args.clone());
        let args = windows_persistent_shell_prefix_args(&configured_args)?;
        let initialization = match profile {
            Some(profile) => profile.init_script.clone(),
            None => shell
                .init_script
                .as_ref()
                .map(|path| format!(". {}", shell_quote_powershell(&path.to_string_lossy()))),
        };
        (program, "powershell".to_string(), args, initialization)
    };

    #[cfg(not(any(unix, windows)))]
    return Err((
        "persistent_shell_unsupported",
        "local persistent shell is unsupported on this platform".to_string(),
    ));

    Ok(ShellLaunch {
        identity: ShellIdentity {
            shell_id: operation.shell_id.clone(),
            workflow_session_id: operation.workflow_session_id.clone(),
            runtime_project_id: operation.runtime_project_id.clone(),
            executor: EXECUTOR_AGENT.to_string(),
            client_id: Some(client_id.to_string()),
        },
        dialect,
        profile: profile_name,
        program,
        args,
        initial_cwd: cwd,
        env,
        initialization,
        max_output_bytes,
    })
}

#[cfg(windows)]
fn windows_persistent_shell_prefix_args(
    configured_args: &[String],
) -> Result<Vec<String>, (&'static str, String)> {
    let Some((command_flag, prefix)) = configured_args.split_last() else {
        return Err((
            "persistent_shell_config_invalid",
            "Windows PowerShell shell args must end with -Command".to_string(),
        ));
    };
    if !command_flag.eq_ignore_ascii_case("-Command") {
        return Err((
            "persistent_shell_config_invalid",
            "Windows PowerShell shell args must end with -Command so persistent-shell transport can replace the one-shot payload mode"
                .to_string(),
        ));
    }
    if prefix.iter().any(|arg| {
        matches!(
            arg.to_ascii_lowercase().as_str(),
            "-command" | "-encodedcommand" | "-file"
        )
    }) {
        return Err((
            "persistent_shell_config_invalid",
            "Windows PowerShell shell args contain a conflicting command/file payload switch"
                .to_string(),
        ));
    }
    Ok(prefix.to_vec())
}

fn validate_open_shell_boundary(
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project: &RunnerProjectShellContext,
    summary: &ShellSummary,
) -> Result<(), (&'static str, String)> {
    let cwd = resolve_cwd(project, Some(&summary.cwd.to_string_lossy()))?;
    cwd_allowed(policy, &cwd).map_err(|message| ("persistent_shell_cwd_denied", message))?;
    let (profile_name, _) = selected_profile(shell, project)?;
    if summary.profile != profile_name {
        return Err((
            "shell_reset_required",
            "the project shell profile changed; close and reopen the persistent shell".to_string(),
        ));
    }
    Ok(())
}

fn summary_result(
    summary: ShellSummary,
    execution_state: &str,
    already_closed: bool,
) -> PersistentShellResult {
    PersistentShellResult {
        shell_id: summary.identity.shell_id,
        workflow_session_id: summary.identity.workflow_session_id,
        runtime_project_id: summary.identity.runtime_project_id,
        shell_state: summary.state.as_str().to_string(),
        execution_state: execution_state.to_string(),
        command_started: false,
        command_completed: false,
        exit_code: summary.exit_code,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        duration_ms: 0,
        cwd: Some(summary.cwd.to_string_lossy().to_string()),
        initial_cwd: Some(summary.initial_cwd.to_string_lossy().to_string()),
        shell: Some(summary.dialect),
        profile: summary.profile,
        created_at: Some(summary.created_at),
        last_activity_at: Some(summary.last_activity_at),
        busy: summary.busy,
        already_closed,
        close_reason: summary.close_reason,
        error_code: None,
        error: None,
    }
}

fn exec_result(
    operation: &PersistentShellRequest,
    result: ShellExecResult,
) -> PersistentShellResult {
    PersistentShellResult {
        shell_id: result.shell_id,
        workflow_session_id: operation.workflow_session_id.clone(),
        runtime_project_id: operation.runtime_project_id.clone(),
        shell_state: result.shell_state.as_str().to_string(),
        execution_state: result.execution_state,
        command_started: result.command_started,
        command_completed: result.command_completed,
        exit_code: result.exit_code,
        stdout: result.stdout,
        stderr: result.stderr,
        stdout_truncated: result.stdout_truncated,
        stderr_truncated: result.stderr_truncated,
        duration_ms: result.duration_ms,
        cwd: Some(result.cwd.to_string_lossy().to_string()),
        initial_cwd: None,
        shell: None,
        profile: None,
        created_at: None,
        last_activity_at: None,
        busy: false,
        already_closed: false,
        close_reason: None,
        error_code: result.error_code,
        error: result.error,
    }
}

fn shell_error_result(
    operation: &PersistentShellRequest,
    error: ShellError,
) -> PersistentShellResult {
    error_result(
        &operation.shell_id,
        &operation.workflow_session_id,
        &operation.runtime_project_id,
        error.code,
        error.message,
    )
}

fn summary_error_result(
    summary: ShellSummary,
    code: &str,
    message: impl Into<String>,
) -> PersistentShellResult {
    let mut result = summary_result(summary, "rejected", false);
    result.error_code = Some(code.to_string());
    result.error = Some(message.into());
    result
}

/// Like `summary_error_result` but fetches the current status from the manager
/// first, so an SSH exec rejection still reports the authoritative running
/// state (and closes a stale shell) the way the local path does.
fn summary_error_result_from_status(
    processes: &ProcessManager,
    operation: &PersistentShellRequest,
    code: &str,
    message: impl Into<String>,
) -> PersistentShellResult {
    match processes.status(
        &operation.shell_id,
        &operation.workflow_session_id,
        &operation.runtime_project_id,
    ) {
        Ok(summary) => summary_error_result(summary, code, message),
        Err(error) => shell_error_result(operation, error),
    }
}

fn error_result(
    shell_id: &str,
    workflow_session_id: &str,
    runtime_project_id: &str,
    code: &str,
    message: impl Into<String>,
) -> PersistentShellResult {
    PersistentShellResult {
        shell_id: shell_id.to_string(),
        workflow_session_id: workflow_session_id.to_string(),
        runtime_project_id: runtime_project_id.to_string(),
        shell_state: if code == "shell_reset_required" {
            ShellState::Poisoned.as_str().to_string()
        } else {
            "unknown".to_string()
        },
        execution_state: "rejected".to_string(),
        command_started: false,
        command_completed: false,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        duration_ms: 0,
        cwd: None,
        initial_cwd: None,
        shell: None,
        profile: None,
        created_at: None,
        last_activity_at: None,
        busy: false,
        already_closed: false,
        close_reason: None,
        error_code: Some(code.to_string()),
        error: Some(message.into()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::runner_protocol::RunnerRequest;
    use std::collections::BTreeMap;

    fn request(action: &str, shell_id: &str, command: Option<&str>) -> RunnerRequest {
        RunnerRequest {
            request_id: format!("req-{action}"),
            client_id: "agent-1".to_string(),
            kind: "persistent_shell".to_string(),
            job_id: None,
            cwd: None,
            path: None,
            content: None,
            max_bytes: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            create_dirs: false,
            command: command.unwrap_or_default().to_string(),
            process: None,
            script: None,
            stdin: None,
            timeout_secs: 5,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            job_context: None,
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: None,
            persistent_shell: Some(PersistentShellRequest {
                action: action.to_string(),
                shell_id: shell_id.to_string(),
                workflow_session_id: "wc_sess_test".to_string(),
                runtime_project_id: "agent:agent-1:demo".to_string(),
                cwd: None,
                shell: Some("bash".to_string()),
                command: command.map(str::to_string),
                timeout_secs: Some(5),
                purpose: None,
            }),
        }
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, RunnerPolicy) {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let projects = temp.path().join("project-registry");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(project.join("sub")).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(
            projects.join("demo.toml"),
            format!("id = \"demo\"\npath = \"{}\"\n", project.display()),
        )
        .unwrap();
        let policy = RunnerPolicy {
            allow_raw_shell: true,
            allow_cwd_anywhere: false,
            allowed_roots: vec![project.clone()],
            max_timeout_secs: 30,
            max_output_bytes: 16 * 1024,
        };
        (temp, project, projects, policy)
    }

    #[test]
    fn runner_preserves_state_and_rechecks_raw_shell_policy() {
        let (_temp, _project, projects, policy) = fixture();
        let shell = ShellConfig::default();
        let manager = PersistentShellManager::new(&shell, SshConnectionPool::default());
        let mut denied_open_policy = policy.clone();
        denied_open_policy.allow_raw_shell = false;
        let denied_open = manager.handle(
            &denied_open_policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request("open", "wc_shell_denied_open", None),
        );
        assert_eq!(
            denied_open.error_code.as_deref(),
            Some("raw_shell_disabled")
        );
        assert_eq!(manager.active_count(), 0);

        let opened = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request("open", "wc_shell_runner", None),
        );
        assert_eq!(opened.shell_state, "running");

        let exported = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_runner",
                Some("export WC_RUNNER_STATE=ready; cd sub; wc_runner_fn() { printf fn; }"),
            ),
        );
        assert_eq!(exported.exit_code, Some(0));
        let observed = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_runner",
                Some("printf '%s:%s:' \"$WC_RUNNER_STATE\" \"$PWD\"; wc_runner_fn"),
            ),
        );
        assert!(observed.stdout.starts_with("ready:"));
        assert!(observed.stdout.ends_with(":fn"));

        let mut denied_policy = policy;
        denied_policy.allow_raw_shell = false;
        let denied = manager.handle(
            &denied_policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request("exec", "wc_shell_runner", Some("printf denied")),
        );
        assert_eq!(denied.error_code.as_deref(), Some("raw_shell_disabled"));
        assert_eq!(manager.active_count(), 0);
    }

    #[test]
    fn rejected_exec_keeps_authoritative_running_state() {
        let (_temp, _project, projects, policy) = fixture();
        let shell = ShellConfig::default();
        let manager = PersistentShellManager::new(&shell, SshConnectionPool::default());
        assert_eq!(
            manager
                .handle(
                    &policy,
                    &shell,
                    &SshConfig::default(),
                    1,
                    &projects,
                    &request("open", "wc_shell_rejected_exec", None),
                )
                .shell_state,
            "running"
        );

        let mut invalid = request("exec", "wc_shell_rejected_exec", Some("printf ignored"));
        invalid.persistent_shell.as_mut().unwrap().timeout_secs = Some(policy.max_timeout_secs + 1);
        let rejected = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &invalid,
        );
        assert_eq!(
            rejected.error_code.as_deref(),
            Some("persistent_shell_invalid_timeout")
        );
        assert_eq!(rejected.shell_state, "running");
        assert_eq!(manager.active_count(), 1);

        let oversized = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_rejected_exec",
                Some(&"x".repeat(RAW_SHELL_COMMAND_MAX_BYTES + 1)),
            ),
        );
        assert_eq!(
            oversized.error_code.as_deref(),
            Some("persistent_shell_invalid_command")
        );
        assert_eq!(oversized.shell_state, "running");

        let observed = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_rejected_exec",
                Some("printf still-running"),
            ),
        );
        assert_eq!(observed.stdout, "still-running");
    }

    #[test]
    fn exec_reapplies_current_output_limit() {
        let (_temp, _project, projects, mut policy) = fixture();
        let shell = ShellConfig::default();
        let manager = PersistentShellManager::new(&shell, SshConnectionPool::default());
        assert_eq!(
            manager
                .handle(
                    &policy,
                    &shell,
                    &SshConfig::default(),
                    1,
                    &projects,
                    &request("open", "wc_shell_output_policy", None),
                )
                .shell_state,
            "running"
        );

        policy.max_output_bytes = 1024;
        let result = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_output_policy",
                Some("i=0; while [ \"$i\" -lt 5000 ]; do printf x; i=$((i+1)); done"),
            ),
        );
        assert!(result.stdout_truncated);
        assert!(result.stdout.len() <= policy.max_output_bytes);
    }

    #[test]
    fn runner_profile_initialization_runs_once_at_open() {
        let (_temp, _project, projects, policy) = fixture();
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "persistent".to_string(),
            ShellProfileConfig {
                program: Some("bash".to_string()),
                init_script: Some("WC_PROFILE_COUNT=1; export WC_PROFILE_COUNT".to_string()),
                ..ShellProfileConfig::default()
            },
        );
        let mut shell = ShellConfig {
            default_profile: Some("persistent".to_string()),
            profiles,
            ..ShellConfig::default()
        };
        shell.max_persistent_shells = 2;
        let manager = PersistentShellManager::new(&shell, SshConnectionPool::default());
        assert_eq!(
            manager
                .handle(
                    &policy,
                    &shell,
                    &SshConfig::default(),
                    1,
                    &projects,
                    &request("open", "wc_shell_profile", None),
                )
                .shell_state,
            "running"
        );
        let first = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_profile",
                Some("printf %s \"$WC_PROFILE_COUNT\"; WC_PROFILE_COUNT=2"),
            ),
        );
        assert_eq!(first.stdout, "1");
        let second = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_profile",
                Some("printf %s \"$WC_PROFILE_COUNT\""),
            ),
        );
        assert_eq!(second.stdout, "2");
    }

    #[test]
    fn runner_rejects_profile_initialization_outside_project() {
        let (_temp, _project, projects, policy) = fixture();
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "escaping".to_string(),
            ShellProfileConfig {
                init_script: Some("cd ..".to_string()),
                ..ShellProfileConfig::default()
            },
        );
        let shell = ShellConfig {
            default_profile: Some("escaping".to_string()),
            profiles,
            ..ShellConfig::default()
        };
        let manager = PersistentShellManager::new(&shell, SshConnectionPool::default());

        let result = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request("open", "wc_shell_profile_escape", None),
        );
        assert_eq!(
            result.error_code.as_deref(),
            Some("persistent_shell_cwd_outside_project")
        );
        assert_ne!(result.shell_state, "running");
        assert_eq!(manager.active_count(), 0);
    }

    #[test]
    fn runner_closes_shell_that_moves_outside_project() {
        let (_temp, _project, projects, policy) = fixture();
        let shell = ShellConfig::default();
        let manager = PersistentShellManager::new(&shell, SshConnectionPool::default());
        assert_eq!(
            manager
                .handle(
                    &policy,
                    &shell,
                    &SshConfig::default(),
                    1,
                    &projects,
                    &request("open", "wc_shell_boundary", None),
                )
                .shell_state,
            "running"
        );

        let escaped = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request("exec", "wc_shell_boundary", Some("cd ..")),
        );
        assert_eq!(escaped.error_code.as_deref(), Some("shell_reset_required"));
        assert_ne!(escaped.shell_state, "running");
        assert_eq!(manager.active_count(), 0);
    }

    #[test]
    fn runner_rejects_runtime_project_identity_mismatch() {
        let (_temp, _project, projects, policy) = fixture();
        let shell = ShellConfig::default();
        let manager = PersistentShellManager::new(&shell, SshConnectionPool::default());
        let mut wrong = request("open", "wc_shell_wrong", None);
        wrong.persistent_shell.as_mut().unwrap().runtime_project_id =
            "agent:other:demo".to_string();
        let result = manager.handle(&policy, &shell, &SshConfig::default(), 1, &projects, &wrong);
        assert_eq!(
            result.error_code.as_deref(),
            Some("persistent_shell_project_mismatch")
        );
        assert_eq!(manager.active_count(), 0);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::super::config::SshResourceConfig;
    use super::super::ssh::SshConnectionPool;
    use super::*;
    use crate::runner_protocol::RunnerRequest;
    use std::collections::BTreeMap;

    fn request(action: &str, shell_id: &str, command: Option<&str>) -> RunnerRequest {
        RunnerRequest {
            request_id: format!("req-{action}"),
            client_id: "msi".to_string(),
            kind: "persistent_shell".to_string(),
            job_id: None,
            cwd: None,
            path: None,
            content: None,
            max_bytes: None,
            expected_sha256: None,
            expected_prefix: None,
            start_line: None,
            end_line: None,
            create_dirs: false,
            command: command.unwrap_or_default().to_string(),
            process: None,
            script: None,
            stdin: None,
            timeout_secs: 5,
            requested_by: "tester".to_string(),
            created_at: 0,
            validation: None,
            lsp: None,
            job_context: None,
            mcp_gateway: None,
            plugin_gateway: None,
            coding_agent: None,
            persistent_shell: Some(PersistentShellRequest {
                action: action.to_string(),
                shell_id: shell_id.to_string(),
                workflow_session_id: "wc_sess_windows".to_string(),
                runtime_project_id: "agent:msi:demo".to_string(),
                cwd: None,
                shell: None,
                command: command.map(str::to_string),
                timeout_secs: Some(5),
                purpose: None,
            }),
        }
    }

    fn ssh_request(
        action: &str,
        shell_id: &str,
        resource: &str,
        command: Option<&str>,
    ) -> RunnerRequest {
        serde_json::from_value(serde_json::json!({
            "request_id": format!("req-ssh-{action}-{shell_id}"),
            "client_id": "msi",
            "kind": "persistent_shell",
            "command": command.unwrap_or(""),
            "timeout_secs": 5,
            "requested_by": "tester",
            "created_at": 0,
            "job_context": {
                "runtime_project_id": "agent:msi:demo",
                "workflow_session_id": "wc_sess_windows",
                "ssh_resource": resource,
                "project_cwd": ".",
                "purpose": "other",
                "shell": "remote",
                "command_preview": "",
                "validation_steps": []
            },
            "persistent_shell": {
                "action": action,
                "shell_id": shell_id,
                "workflow_session_id": "wc_sess_windows",
                "runtime_project_id": "agent:msi:demo",
                "cwd": null,
                "shell": "bash",
                "command": command,
                "timeout_secs": 5,
                "purpose": null
            }
        }))
        .expect("build Windows named-SSH persistent-shell request")
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, RunnerPolicy) {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let projects = temp.path().join("project-registry");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        let escaped = project.to_string_lossy().replace('\\', "\\\\");
        std::fs::write(
            projects.join("demo.toml"),
            format!("id = \"demo\"\npath = \"{escaped}\"\n"),
        )
        .unwrap();
        let policy = RunnerPolicy {
            allow_raw_shell: true,
            allow_cwd_anywhere: false,
            allowed_roots: vec![project.clone()],
            max_timeout_secs: 30,
            max_output_bytes: 16 * 1024,
        };
        (temp, project, projects, policy)
    }

    #[test]
    fn runner_windows_local_persistent_shell_preserves_state_and_existing_protocol() {
        let (_temp, _project, projects, mut policy) = fixture();
        let shell = ShellConfig::default();
        let manager = PersistentShellManager::new(&shell, SshConnectionPool::default());

        let opened = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request("open", "wc_shell_windows_runner", None),
        );
        assert_eq!(opened.shell_state, "running");
        assert_eq!(opened.shell.as_deref(), Some("powershell"));

        let set = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_windows_runner",
                Some("$env:WC_RUNNER_STATE='ready'; Set-Location -LiteralPath 'sub'; $WC_LOCAL='beta'; function WC_FN { [Console]::Out.Write('fn') }"),
            ),
        );
        assert_eq!(set.exit_code, Some(0));
        let observed = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_windows_runner",
                Some("[Console]::Out.Write($env:WC_RUNNER_STATE + '|' + (Get-Location).Path + '|' + $WC_LOCAL + '|'); WC_FN"),
            ),
        );
        assert!(observed.stdout.starts_with("ready|"), "{}", observed.stdout);
        assert!(
            observed.stdout.contains("\\sub|beta|fn"),
            "{}",
            observed.stdout
        );

        let unicode = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_windows_runner",
                Some("[Console]::Out.Write(\"hello`r`n中文`r`n🙂\")"),
            ),
        );
        assert_eq!(unicode.stdout, "hello\r\n中文\r\n🙂");

        let failed = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_windows_runner",
                Some("Write-Error 'expected-runner-failure'"),
            ),
        );
        assert_eq!(failed.exit_code, Some(1));
        assert_eq!(failed.shell_state, "running");
        assert!(failed.stderr.contains("expected-runner-failure"));

        let next = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_windows_runner",
                Some("[Console]::Out.Write('still-running')"),
            ),
        );
        assert_eq!(next.stdout, "still-running");
        assert!(
            next.stderr.is_empty(),
            "previous stderr leaked: {}",
            next.stderr
        );

        policy.max_output_bytes = 1024;
        let bounded = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request(
                "exec",
                "wc_shell_windows_runner",
                Some("[Console]::Out.Write('x' * 5000)"),
            ),
        );
        assert!(bounded.stdout_truncated);
        assert!(bounded.stdout.len() <= policy.max_output_bytes);

        let closed = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &request("close", "wc_shell_windows_runner", None),
        );
        assert_eq!(closed.shell_state, "closed");
        assert_eq!(manager.active_count(), 0);
    }

    #[test]
    fn windows_launch_uses_configured_powershell_profile_and_rejects_payload_modes() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_path_buf();
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "modern".to_string(),
            ShellProfileConfig {
                program: Some("pwsh.exe".to_string()),
                args: Some(vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                ]),
                init_script: Some("$env:WC_PROFILE='ready'".to_string()),
                ..ShellProfileConfig::default()
            },
        );
        let shell = ShellConfig {
            profiles,
            ..ShellConfig::default()
        };
        let project = RunnerProjectShellContext {
            id: "demo".to_string(),
            path: cwd.to_string_lossy().to_string(),
            shell_profile: Some("modern".to_string()),
        };
        let open = request("open", "wc_shell_profile_mapping", None);
        let operation = open.persistent_shell.as_ref().unwrap();
        let launch =
            build_launch_at_cwd(&shell, &open, operation, &project, cwd.clone(), 4096).unwrap();
        assert_eq!(launch.program, "pwsh.exe");
        assert_eq!(launch.dialect, "powershell");
        assert_eq!(launch.args, vec!["-NoProfile", "-NonInteractive"]);
        assert_eq!(
            launch.initialization.as_deref(),
            Some("$env:WC_PROFILE='ready'")
        );
        assert_eq!(
            dialect_for_program("pwsh.exe"),
            Some(ShellDialect::PowerShell)
        );

        let mut explicit = request("open", "wc_shell_explicit_sh", None);
        explicit.persistent_shell.as_mut().unwrap().shell = Some("bash".to_string());
        let error = build_launch_at_cwd(
            &ShellConfig::default(),
            &explicit,
            explicit.persistent_shell.as_ref().unwrap(),
            &RunnerProjectShellContext {
                id: "demo".to_string(),
                path: cwd.to_string_lossy().to_string(),
                shell_profile: None,
            },
            cwd.clone(),
            4096,
        )
        .unwrap_err();
        assert_eq!(error.0, "persistent_shell_dialect_unsupported");

        let mut invalid_shell = ShellConfig::default();
        invalid_shell.args = vec!["-NoProfile".to_string()];
        let default_project = RunnerProjectShellContext {
            id: "demo".to_string(),
            path: cwd.to_string_lossy().to_string(),
            shell_profile: None,
        };
        let invalid = request("open", "wc_shell_bad_args", None);
        let error = build_launch_at_cwd(
            &invalid_shell,
            &invalid,
            invalid.persistent_shell.as_ref().unwrap(),
            &default_project,
            cwd,
            4096,
        )
        .unwrap_err();
        assert_eq!(error.0, "persistent_shell_config_invalid");
    }

    #[test]
    fn windows_named_ssh_resource_routes_remote_without_changing_local_powershell() {
        let (_temp, _project, projects, policy) = fixture();
        let shell = ShellConfig::default();
        let manager = PersistentShellManager::new(&shell, SshConnectionPool::default());

        let mut local_bash = request("open", "wc_shell_local_bash", None);
        local_bash.persistent_shell.as_mut().unwrap().shell = Some("bash".to_string());
        let local = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &local_bash,
        );
        assert_eq!(
            local.error_code.as_deref(),
            Some("persistent_shell_dialect_unsupported"),
            "Windows local shell must remain PowerShell: {local:?}"
        );

        let remote = manager.handle(
            &policy,
            &shell,
            &SshConfig::default(),
            1,
            &projects,
            &ssh_request("open", "wc_shell_remote_missing", "missing", None),
        );
        assert_eq!(
            remote.error_code.as_deref(),
            Some("ssh_persistent_shell_spawn_failed"),
            "named resource must route to SSH before local PowerShell validation: {remote:?}"
        );
        assert!(
            remote
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_resource_not_found")),
            "missing named SSH resource must fail from SSH authority: {remote:?}"
        );
        assert_eq!(manager.active_count(), 0);
    }

    #[test]
    fn windows_named_ssh_persistent_shell_real_transport_opt_in() {
        let Ok(host) = std::env::var("WEBCODEX_TEST_WINDOWS_SSH_HOST") else {
            eprintln!("skipping Windows SSH persistent-shell integration test; WEBCODEX_TEST_WINDOWS_SSH_HOST is unset");
            return;
        };
        let (_temp, _project, projects, mut policy) = fixture();
        let shell = ShellConfig::default();
        let mut resources = BTreeMap::new();
        resources.insert(
            "dogfood".to_string(),
            SshResourceConfig {
                host,
                default_cwd: Some("/tmp".to_string()),
            },
        );
        let config = SshConfig { resources };
        let manager = PersistentShellManager::new(&shell, SshConnectionPool::default());

        let opened = manager.handle(
            &policy,
            &shell,
            &config,
            7,
            &projects,
            &ssh_request("open", "wc_shell_win_ssh_state", "dogfood", None),
        );
        assert_eq!(opened.shell_state, "running", "{opened:?}");
        assert_eq!(opened.shell.as_deref(), Some("bash"), "{opened:?}");
        assert_eq!(opened.cwd.as_deref(), Some("/tmp"), "{opened:?}");

        let setup = manager.handle(
            &policy,
            &shell,
            &config,
            7,
            &projects,
            &ssh_request(
                "exec",
                "wc_shell_win_ssh_state",
                "dogfood",
                Some("export WC_WIN_SSH=ready; cd /tmp; wc_win_fn() { printf fn; }"),
            ),
        );
        assert_eq!(setup.exit_code, Some(0), "{setup:?}");
        let observed = manager.handle(
            &policy,
            &shell,
            &config,
            7,
            &projects,
            &ssh_request(
                "exec",
                "wc_shell_win_ssh_state",
                "dogfood",
                Some("printf '%s|%s|' \"$WC_WIN_SSH\" \"$PWD\"; wc_win_fn; printf '|中文🙂'; printf '错误🙂' >&2"),
            ),
        );
        assert_eq!(observed.exit_code, Some(0), "{observed:?}");
        assert!(
            observed.stdout.contains("ready|/tmp|fn|中文🙂"),
            "{observed:?}"
        );
        assert!(observed.stderr.contains("错误🙂"), "{observed:?}");
        assert!(!observed.stdout.contains("WCPS"), "{observed:?}");
        assert!(!observed.stderr.contains("WCPS"), "{observed:?}");

        let failed = manager.handle(
            &policy,
            &shell,
            &config,
            7,
            &projects,
            &ssh_request("exec", "wc_shell_win_ssh_state", "dogfood", Some("false")),
        );
        assert_eq!(failed.exit_code, Some(1), "{failed:?}");
        assert_eq!(failed.shell_state, "running", "{failed:?}");
        let next = manager.handle(
            &policy,
            &shell,
            &config,
            7,
            &projects,
            &ssh_request(
                "exec",
                "wc_shell_win_ssh_state",
                "dogfood",
                Some("printf clean"),
            ),
        );
        assert_eq!(next.stdout, "clean", "{next:?}");
        assert!(next.stderr.is_empty(), "previous stderr leaked: {next:?}");

        policy.max_output_bytes = 1024;
        let bounded = manager.handle(
            &policy,
            &shell,
            &config,
            7,
            &projects,
            &ssh_request(
                "exec",
                "wc_shell_win_ssh_state",
                "dogfood",
                Some("i=0; while [ \"$i\" -lt 5000 ]; do printf x; i=$((i+1)); done"),
            ),
        );
        assert!(bounded.stdout_truncated, "{bounded:?}");
        assert!(
            bounded.stdout.len() <= policy.max_output_bytes,
            "{bounded:?}"
        );

        let mut removed = config.clone();
        removed.resources.remove("dogfood");
        let removed_status = manager.handle(
            &policy,
            &shell,
            &removed,
            7,
            &projects,
            &ssh_request("status", "wc_shell_win_ssh_state", "dogfood", None),
        );
        assert_eq!(
            removed_status.error_code.as_deref(),
            Some("shell_reset_required"),
            "{removed_status:?}"
        );
        assert_eq!(manager.active_count(), 0, "{removed_status:?}");

        let generation_open = manager.handle(
            &policy,
            &shell,
            &config,
            7,
            &projects,
            &ssh_request("open", "wc_shell_win_ssh_generation", "dogfood", None),
        );
        assert_eq!(
            generation_open.shell_state, "running",
            "{generation_open:?}"
        );
        let stale = manager.handle(
            &policy,
            &shell,
            &config,
            8,
            &projects,
            &ssh_request("status", "wc_shell_win_ssh_generation", "dogfood", None),
        );
        assert_eq!(
            stale.error_code.as_deref(),
            Some("shell_reset_required"),
            "{stale:?}"
        );
        assert_eq!(manager.active_count(), 0, "{stale:?}");

        let reopened = manager.handle(
            &policy,
            &shell,
            &config,
            8,
            &projects,
            &ssh_request("open", "wc_shell_win_ssh_timeout", "dogfood", None),
        );
        assert_eq!(reopened.shell_state, "running", "{reopened:?}");
        let mut timeout_request = ssh_request(
            "exec",
            "wc_shell_win_ssh_timeout",
            "dogfood",
            Some("sleep 3; printf late"),
        );
        timeout_request
            .persistent_shell
            .as_mut()
            .unwrap()
            .timeout_secs = Some(1);
        let timed_out = manager.handle(&policy, &shell, &config, 8, &projects, &timeout_request);
        assert_eq!(timed_out.execution_state, "timed_out", "{timed_out:?}");
        assert_ne!(timed_out.shell_state, "running", "{timed_out:?}");
        assert_eq!(
            timed_out.error_code.as_deref(),
            Some("shell_reset_required"),
            "{timed_out:?}"
        );
        let after_timeout = manager.handle(
            &policy,
            &shell,
            &config,
            8,
            &projects,
            &ssh_request(
                "exec",
                "wc_shell_win_ssh_timeout",
                "dogfood",
                Some("printf forbidden"),
            ),
        );
        assert!(!after_timeout.command_started, "{after_timeout:?}");

        let reopened = manager.handle(
            &policy,
            &shell,
            &config,
            8,
            &projects,
            &ssh_request("open", "wc_shell_win_ssh_exit", "dogfood", None),
        );
        assert_eq!(reopened.shell_state, "running", "{reopened:?}");
        let exited = manager.handle(
            &policy,
            &shell,
            &config,
            8,
            &projects,
            &ssh_request("exec", "wc_shell_win_ssh_exit", "dogfood", Some("exit 7")),
        );
        assert_eq!(exited.shell_state, "exited", "{exited:?}");
        assert_eq!(exited.exit_code, Some(7), "{exited:?}");
        let after_exit = manager.handle(
            &policy,
            &shell,
            &config,
            8,
            &projects,
            &ssh_request(
                "exec",
                "wc_shell_win_ssh_exit",
                "dogfood",
                Some("printf forbidden"),
            ),
        );
        assert!(!after_exit.command_started, "{after_exit:?}");

        let reopened = manager.handle(
            &policy,
            &shell,
            &config,
            8,
            &projects,
            &ssh_request("open", "wc_shell_win_ssh_close", "dogfood", None),
        );
        assert_eq!(reopened.shell_state, "running", "{reopened:?}");
        let closed = manager.handle(
            &policy,
            &shell,
            &config,
            8,
            &projects,
            &ssh_request("close", "wc_shell_win_ssh_close", "dogfood", None),
        );
        assert_eq!(closed.shell_state, "closed", "{closed:?}");
        assert_eq!(manager.active_count(), 0, "{closed:?}");
    }
}
