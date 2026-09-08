//! Runner-local SSH resource execution.
//!
//! The pool deliberately delegates authentication and Host alias resolution to
//! the Runner host's OpenSSH client. WebCodex only keeps named resource
//! metadata plus short-lived in-memory control-socket state; it never stores
//! keys, passwords, SSH configuration, or a transport in a Workflow Session.

use super::config::SshConfig;
use super::output::{CommandResult, ShellCommandResult};
use super::shutdown::lock_unpoison;
use super::RunnerPolicy;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
#[cfg(windows)]
use webcodex_process::ManagedChild;

const SSH_CONNECT_TIMEOUT_SECS: u64 = 10;
const SSH_CONTROL_PERSIST_SECS: u64 = 300;
const SSH_PIPE_DRAIN_TIMEOUT_SECS: u64 = 2;
const SSH_REMOTE_CWD_MAX_BYTES: usize = 4096;
/// The public wire command plus the worst-case POSIX single-quote expansion of
/// a maximum remote cwd and fixed cwd wrapper fit below this internal ceiling.
/// It is transport headroom, not a smaller Windows product limit.
#[cfg(any(test, windows))]
const WINDOWS_DIRECT_PROGRAM_MAX_BYTES: usize =
    crate::runner_protocol::RAW_SHELL_WIRE_MAX_BYTES + SSH_REMOTE_CWD_MAX_BYTES * 5 + 512;
/// Windows Direct transports a shell-quoted `eval <program>` frame over stdin.
/// POSIX single-quote encoding expands each input byte by at most 5x; the fixed
/// `eval ` prefix and surrounding quotes add seven bytes. This is internal
/// transport headroom only and does not lower the public raw-shell ceilings.
#[cfg(any(test, windows))]
const WINDOWS_DIRECT_FRAME_MAX_BYTES: usize = WINDOWS_DIRECT_PROGRAM_MAX_BYTES * 5 + 7;
#[cfg(any(test, windows))]
const WINDOWS_DIRECT_BOOTSTRAP_PREFIX: &str = ": WEBCODEX_SSH_PROGRAM_BYTES=";

/// In-memory identity for one OpenSSH multiplex transport. The Runner config
/// generation protects a request after an operator changes a resource host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SshConnectionKey {
    session_id: String,
    resource_name: String,
    generation: u64,
}

#[derive(Debug, Clone)]
struct SshConnection {
    key: SshConnectionKey,
    control_path: PathBuf,
    host: String,
    default_cwd: Option<String>,
    /// Test-only callers can point the OpenSSH client at an isolated config;
    /// production leaves this `None` and uses the Runner user's normal SSH
    /// configuration exactly as intended.
    config_path: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct SshPoolState {
    control_root: Option<PathBuf>,
    entries: HashMap<SshConnectionKey, SshConnection>,
    next_control_id: u64,
    test_config_path: Option<PathBuf>,
    #[cfg(all(test, windows))]
    test_executable: Option<PathBuf>,
}

/// Runner-local OpenSSH resource preparation state.
///
/// Unix one-shot/background and persistent SSH paths may reuse a process-local
/// multiplex transport. Windows resolves the same named resources without mux
/// state: one-shot/background calls own one direct ssh.exe process, while a
/// persistent shell owns one direct long-lived ssh.exe channel.
#[derive(Debug, Clone, Default)]
pub(crate) struct SshConnectionPool {
    state: Arc<Mutex<SshPoolState>>,
}

/// Local transport used by one prepared SSH command. Unix keeps its existing
/// Runner-local mux identity; Windows owns one direct ssh.exe process and has no
/// reusable transport to invalidate.
#[derive(Debug, Clone)]
pub(crate) enum PreparedSshTransport {
    Mux(SshConnectionKey),
    // Constructed only by the Windows direct-OpenSSH path.
    #[cfg_attr(not(windows), allow(dead_code))]
    Direct,
}

/// How one prepared SSH invocation receives the remote program.
///
/// Unix mux commands keep their historical remote-program argv. Windows Direct
/// keeps only a fixed bootstrap in CreateProcess argv and sends a bounded,
/// shell-quoted transport frame over SSH stdin before any caller stdin bytes.
/// The frame unwraps to the exact remote program in the same remote shell.
#[derive(Debug, Clone)]
pub(crate) enum PreparedSshProgramDelivery {
    Argv,
    // Constructed only by the Windows direct-OpenSSH path.
    #[cfg_attr(not(windows), allow(dead_code))]
    StdinFramed {
        program: Vec<u8>,
    },
}

impl PreparedSshProgramDelivery {
    pub(crate) fn requires_stdin(&self) -> bool {
        matches!(self, Self::StdinFramed { .. })
    }

    pub(crate) fn spawn_writer(
        self,
        child_stdin: Option<ChildStdin>,
        caller_stdin: Option<Vec<u8>>,
    ) -> Result<Option<SshStdinWriter>, String> {
        let needs_stdin = self.requires_stdin() || caller_stdin.is_some();
        if !needs_stdin {
            return Ok(None);
        }
        let mut child_stdin = child_stdin.ok_or_else(|| {
            "ssh_command_stdin_unavailable: SSH was already started but its stdin pipe is unavailable; remote command outcome is unknown; do not blindly retry".to_string()
        })?;
        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let result = (|| {
                if let Self::StdinFramed { program } = self {
                    child_stdin.write_all(&program).map_err(|error| {
                        format!(
                            "ssh_program_write_failed: remote program delivery failed after SSH start; remote command outcome is unknown; do not blindly retry: {error}"
                        )
                    })?;
                }
                if let Some(caller_stdin) = caller_stdin {
                    child_stdin.write_all(&caller_stdin).map_err(|error| {
                        format!(
                            "ssh_command_stdin_failed: caller stdin delivery failed after SSH start; remote command outcome is unknown; do not blindly retry: {error}"
                        )
                    })?;
                }
                Ok(())
            })();
            drop(child_stdin);
            let _ = completion_tx.send(result);
        });
        Ok(Some(SshStdinWriter {
            completion_rx,
            handle: Some(handle),
            result: None,
        }))
    }
}

/// Tracked writer for the single SSH stdin byte stream. It keeps the program
/// and caller-input regions logically separate while allowing stop/timeout
/// polling to proceed independently of potentially blocking pipe writes.
pub(crate) struct SshStdinWriter {
    completion_rx: mpsc::Receiver<Result<(), String>>,
    handle: Option<std::thread::JoinHandle<()>>,
    result: Option<Result<(), String>>,
}

impl SshStdinWriter {
    fn observe_result(&mut self) -> Option<Result<(), String>> {
        if self.result.is_none() {
            match self.completion_rx.try_recv() {
                Ok(result) => self.result = Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result = Some(Err(
                        "ssh_stdin_writer_failed: stdin writer exited without reporting completion; remote command outcome is unknown; do not blindly retry"
                            .to_string(),
                    ));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.result.clone()
    }

    pub(crate) fn poll_failure(&mut self) -> Option<String> {
        match self.observe_result() {
            Some(Err(error)) => Some(error),
            _ => None,
        }
    }

    fn wait_completion_until(&mut self, deadline: Instant) -> Result<Result<(), String>, String> {
        loop {
            if let Some(result) = self.observe_result() {
                if self
                    .handle
                    .as_ref()
                    .is_some_and(|handle| handle.is_finished())
                {
                    if let Some(handle) = self.handle.take() {
                        let _ = handle.join();
                    }
                    return Ok(result);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(
                    "ssh_stdin_writer_drain_failed: stdin writer did not stop within the bounded drain after SSH tree cleanup; remote command outcome is unknown; do not blindly retry"
                        .to_string(),
                );
            }
            std::thread::sleep(Duration::from_millis(10).min(remaining));
        }
    }

    /// Natural completion must also prove that every program/caller-input byte
    /// was accepted by the local SSH client.
    pub(crate) fn finish_bounded(&mut self) -> Result<(), String> {
        self.wait_completion_until(
            Instant::now() + Duration::from_secs(SSH_PIPE_DRAIN_TIMEOUT_SECS),
        )?
    }

    /// Once stop/timeout/wait failure has already terminated the SSH tree, a
    /// BrokenPipe-like writer result is expected. Only failure to quiesce the
    /// writer itself is a new cleanup uncertainty.
    pub(crate) fn finish_after_tree_cleanup(&mut self) -> Result<(), String> {
        let _ = self.wait_completion_until(
            Instant::now() + Duration::from_secs(SSH_PIPE_DRAIN_TIMEOUT_SECS),
        )?;
        Ok(())
    }
}

/// A ready-to-spawn SSH command paired with its transport semantics and program
/// delivery contract.
pub(crate) struct PreparedSshCommand {
    pub(crate) command: Command,
    pub(crate) transport: PreparedSshTransport,
    pub(crate) program_delivery: PreparedSshProgramDelivery,
}

/// A ready-to-spawn long-lived SSH shell command with the resource's default
/// remote cwd. Unix reuses the authenticated mux transport; Windows opens one
/// direct long-lived ssh.exe channel owned by the persistent-shell transport.
#[cfg(any(unix, windows))]
pub(crate) struct PreparedPersistentShellCommand {
    pub(crate) command: Command,
    pub(crate) default_cwd: Option<String>,
}

impl SshConnectionPool {
    /// Whether this Runner can advertise SSH-shell support. A missing OpenSSH
    /// executable is a capability absence, not a later silent local fallback.
    pub(crate) fn is_available() -> bool {
        if !cfg!(any(unix, windows)) {
            return false;
        }
        ssh_shell_probe_available(
            Command::new(ssh_executable())
                .arg("-V")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
    }

    /// Whether this host can open a named SSH persistent shell. This is separate
    /// from one-shot/background `ssh_shell`; Windows uses one direct long-lived
    /// ssh.exe process rather than a Unix ControlMaster socket.
    pub(crate) fn persistent_shell_available() -> bool {
        if !cfg!(any(unix, windows)) {
            return false;
        }
        Command::new(ssh_executable())
            .arg("-V")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Resolve a named resource and prepare the platform transport for one raw
    /// remote shell command. Unix may reuse its mux; Windows prepares direct
    /// ssh.exe without creating pool state.
    pub(crate) fn prepare_command(
        &self,
        generation: u64,
        config: &SshConfig,
        resource_name: &str,
        session_id: &str,
        cwd: Option<&str>,
        command: &str,
    ) -> Result<PreparedSshCommand, String> {
        self.prepare_command_inner(
            generation,
            config,
            resource_name,
            session_id,
            cwd,
            command,
            true,
        )
    }

    /// Build an SSH command for the existing JobManager ManagedChild lifecycle.
    /// Unix still uses its mux transport; Windows returns one direct ssh.exe.
    pub(crate) fn prepare_job_command(
        &self,
        generation: u64,
        config: &SshConfig,
        resource_name: &str,
        session_id: &str,
        cwd: Option<&str>,
        command: &str,
    ) -> Result<PreparedSshCommand, String> {
        self.prepare_command_inner(
            generation,
            config,
            resource_name,
            session_id,
            cwd,
            command,
            false,
        )
    }

    fn prepare_command_inner(
        &self,
        generation: u64,
        config: &SshConfig,
        resource_name: &str,
        session_id: &str,
        cwd: Option<&str>,
        command: &str,
        configure_process_group: bool,
    ) -> Result<PreparedSshCommand, String> {
        if !is_safe_resource_name(resource_name) {
            return Err(
                "ssh_resource_invalid: resource name is invalid; command was not started"
                    .to_string(),
            );
        }
        if !is_safe_session_id(session_id) {
            return Err(
                "ssh_session_required: an SSH resource requires a valid Workflow Session id; command was not started".to_string(),
            );
        }
        let resource = match config.resources.get(resource_name) {
            Some(resource) => resource,
            None => {
                // A hot reload can remove a resource while an old transport
                // is still pooled. It must not stay alive merely because the
                // next request fails name resolution.
                self.release_resource(resource_name);
                return Err(format!(
                    "ssh_resource_not_found: resource '{}' is not configured on this Runner; command was not started",
                    resource_name
                ));
            }
        };
        let requested_cwd = normalize_remote_cwd(cwd)?;

        #[cfg(unix)]
        {
            let connection = self.connection_for(
                generation,
                resource_name,
                session_id,
                &resource.host,
                resource.default_cwd.as_deref(),
            )?;
            let effective_cwd = requested_cwd.or(connection.default_cwd.clone());
            let remote_script = remote_script(effective_cwd.as_deref(), command);
            let mut ssh = ssh_command(&connection);
            ssh.arg("-o")
                .arg("BatchMode=yes")
                .arg("-o")
                .arg("LogLevel=ERROR")
                .arg("-S")
                .arg(&connection.control_path)
                .arg(&connection.host)
                .arg(remote_script);
            if configure_process_group {
                configure_private_process_group(&mut ssh);
            }
            return Ok(PreparedSshCommand {
                command: ssh,
                transport: PreparedSshTransport::Mux(connection.key),
                program_delivery: PreparedSshProgramDelivery::Argv,
            });
        }

        #[cfg(windows)]
        {
            let _ = generation;
            let _ = configure_process_group;
            let effective_cwd = requested_cwd.or_else(|| resource.default_cwd.clone());
            let remote_script = remote_script(effective_cwd.as_deref(), command);
            if remote_script.len() > WINDOWS_DIRECT_PROGRAM_MAX_BYTES {
                return Err(format!(
                    "ssh_program_too_long: prepared remote program exceeds internal Windows transport headroom ({WINDOWS_DIRECT_PROGRAM_MAX_BYTES} bytes); command was not started"
                ));
            }
            let program_frame = windows_direct_program_frame(&remote_script);
            debug_assert!(program_frame.len() <= WINDOWS_DIRECT_FRAME_MAX_BYTES);
            let bootstrap = windows_direct_program_bootstrap(program_frame.len());
            let mut ssh = self.direct_ssh_command();
            ssh.arg("-o")
                .arg("BatchMode=yes")
                .arg("-o")
                .arg("LogLevel=ERROR")
                .arg(&resource.host)
                .arg(bootstrap);
            return Ok(PreparedSshCommand {
                command: ssh,
                transport: PreparedSshTransport::Direct,
                program_delivery: PreparedSshProgramDelivery::StdinFramed {
                    program: program_frame.into_bytes(),
                },
            });
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = generation;
            let _ = configure_process_group;
            Err("ssh_shell_unavailable: this Runner host does not support SSH resources; command was not started".to_string())
        }
    }

    /// A transport error after spawning has uncertain remote delivery. Forget
    /// the entry so the *next* command establishes a fresh transport; never
    /// retry the just-submitted command automatically. Direct Windows commands
    /// have no reusable transport state to invalidate.
    pub(crate) fn invalidate_after_transport_failure(&self, transport: &PreparedSshTransport) {
        let PreparedSshTransport::Mux(key) = transport else {
            return;
        };
        let mut state = lock_unpoison(&self.state);
        if let Some(connection) = state.entries.remove(key) {
            close_control_socket(&connection);
        }
    }

    /// Resolve a named resource and construct a ready-to-spawn `ssh` command
    /// that runs one long-lived remote shell. The remote command is just sh/bash
    /// (no `-c` script); the Runner drives it over stdin.
    ///
    /// Unix preserves the existing authenticated ControlMaster reuse. OpenSSH for
    /// Windows does not provide a usable Unix-domain mux socket in the supported
    /// Runner environment, so Windows opens one direct BatchMode ssh.exe channel.
    /// Authentication, Host aliases, and keys still come only from the Runner's
    /// local OpenSSH configuration; no credential or executable is caller input.
    #[cfg(any(unix, windows))]
    pub(crate) fn prepare_persistent_shell_command(
        &self,
        generation: u64,
        config: &SshConfig,
        resource_name: &str,
        session_id: &str,
        shell_program: &str,
    ) -> Result<PreparedPersistentShellCommand, String> {
        if !is_safe_resource_name(resource_name) {
            return Err(
                "ssh_resource_invalid: resource name is invalid; shell was not started".to_string(),
            );
        }
        if !is_safe_session_id(session_id) {
            return Err(
                "ssh_session_required: an SSH resource requires a valid Workflow Session id; shell was not started".to_string(),
            );
        }
        let resource = match config.resources.get(resource_name) {
            Some(resource) => resource,
            None => {
                self.release_resource(resource_name);
                return Err(format!(
                    "ssh_resource_not_found: resource '{}' is not configured on this Runner; shell was not started",
                    resource_name
                ));
            }
        };

        #[cfg(unix)]
        {
            let connection = self.connection_for(
                generation,
                resource_name,
                session_id,
                &resource.host,
                resource.default_cwd.as_deref(),
            )?;
            let mut ssh = ssh_command(&connection);
            ssh.arg("-o")
                .arg("BatchMode=yes")
                .arg("-o")
                .arg("LogLevel=ERROR")
                .arg("-o")
                .arg("ControlMaster=no")
                .arg("-S")
                .arg(&connection.control_path)
                .arg(&connection.host)
                .arg(shell_program);
            configure_private_process_group(&mut ssh);
            return Ok(PreparedPersistentShellCommand {
                command: ssh,
                default_cwd: connection.default_cwd.clone(),
            });
        }

        #[cfg(windows)]
        {
            let _ = generation;
            let mut ssh = Command::new(ssh_executable());
            ssh.arg("-o")
                .arg("BatchMode=yes")
                .arg("-o")
                .arg("LogLevel=ERROR")
                .arg(&resource.host)
                .arg(shell_program);
            Ok(PreparedPersistentShellCommand {
                command: ssh,
                default_cwd: resource.default_cwd.clone(),
            })
        }
    }

    /// Release every Session's generation for a resource that is no longer
    /// valid in the active Runner configuration.
    fn release_resource(&self, resource_name: &str) {
        let removed = {
            let mut state = lock_unpoison(&self.state);
            let keys = state
                .entries
                .keys()
                .filter(|key| key.resource_name == resource_name)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| state.entries.remove(&key))
                .collect::<Vec<_>>()
        };
        for connection in removed {
            retire_control_socket(&connection);
        }
    }

    #[cfg(test)]
    pub(crate) fn connection_count(&self) -> usize {
        lock_unpoison(&self.state).entries.len()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_test_config(config_path: PathBuf) -> Self {
        let pool = Self::default();
        lock_unpoison(&pool.state).test_config_path = Some(config_path);
        pool
    }

    #[cfg(all(test, unix))]
    pub(crate) fn control_path_for(
        &self,
        generation: u64,
        resource_name: &str,
        session_id: &str,
    ) -> Option<PathBuf> {
        lock_unpoison(&self.state)
            .entries
            .get(&SshConnectionKey {
                generation,
                resource_name: resource_name.to_string(),
                session_id: session_id.to_string(),
            })
            .map(|connection| connection.control_path.clone())
    }

    #[cfg(all(test, windows))]
    pub(crate) fn with_test_executable(executable: PathBuf) -> Self {
        let pool = Self::default();
        lock_unpoison(&pool.state).test_executable = Some(executable);
        pool
    }

    #[cfg(windows)]
    fn direct_ssh_command(&self) -> Command {
        #[cfg(test)]
        {
            let test_executable = lock_unpoison(&self.state).test_executable.clone();
            if let Some(executable) = test_executable {
                return Command::new(executable);
            }
        }
        Command::new(ssh_executable())
    }

    fn connection_for(
        &self,
        generation: u64,
        resource_name: &str,
        session_id: &str,
        host: &str,
        default_cwd: Option<&str>,
    ) -> Result<SshConnection, String> {
        let key = SshConnectionKey {
            session_id: session_id.to_string(),
            resource_name: resource_name.to_string(),
            generation,
        };
        // Hold the small pool mutex across an initial OpenSSH handshake. This
        // prevents two concurrent requests with the same Session/resource
        // from creating separate masters. Existing checks are local control
        // socket probes and ordinarily return immediately.
        let mut state = lock_unpoison(&self.state);
        // A changed hot configuration must not leave an older host mapping
        // attached to this Session/resource pair. Close it eagerly rather
        // than waiting for process exit; a subsequent request gets a master
        // for the new generation only.
        let stale = state
            .entries
            .keys()
            .filter(|candidate| {
                candidate.session_id == session_id
                    && candidate.resource_name == resource_name
                    && candidate.generation != generation
            })
            .cloned()
            .collect::<Vec<_>>();
        for stale_key in stale {
            if let Some(entry) = state.entries.remove(&stale_key) {
                retire_control_socket(&entry);
            }
        }
        if let Some(existing) = state.entries.get(&key).cloned() {
            if control_socket_healthy(&existing) {
                return Ok(existing);
            }
            if let Some(connection) = state.entries.remove(&key) {
                close_control_socket(&connection);
            }
        }
        let root = ensure_control_root(&mut state)?;
        state.next_control_id = state.next_control_id.saturating_add(1);
        let control_path = root.join(format!("c{:016x}", state.next_control_id));
        let connection = SshConnection {
            key: key.clone(),
            control_path,
            host: host.to_string(),
            default_cwd: default_cwd.map(str::to_string),
            config_path: state.test_config_path.clone(),
        };
        establish_control_socket(&connection)?;
        state.entries.insert(key, connection.clone());
        Ok(connection)
    }
}

impl Drop for SshConnectionPool {
    fn drop(&mut self) {
        if Arc::strong_count(&self.state) != 1 {
            return;
        }
        let (root, entries) = {
            let mut state = lock_unpoison(&self.state);
            let root = state.control_root.take();
            let entries = std::mem::take(&mut state.entries)
                .into_values()
                .collect::<Vec<_>>();
            (root, entries)
        };
        for entry in entries {
            close_control_socket(&entry);
        }
        if let Some(root) = root {
            // The directory is freshly created by this process under the
            // system temp directory and contains only control sockets.
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

/// Execute a short remote shell command through a Session-bound SSH resource.
#[cfg(test)]
pub(crate) fn run_ssh_shell(
    pool: &SshConnectionPool,
    generation: u64,
    config: &SshConfig,
    policy: &RunnerPolicy,
    resource_name: &str,
    session_id: &str,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> CommandResult {
    run_ssh_shell_with_execution_state(
        pool,
        generation,
        config,
        policy,
        resource_name,
        session_id,
        cwd,
        command,
        stdin,
        timeout_secs,
        stop_requested,
    )
    .result
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_ssh_shell_with_execution_state(
    pool: &SshConnectionPool,
    generation: u64,
    config: &SshConfig,
    policy: &RunnerPolicy,
    resource_name: &str,
    session_id: &str,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> ShellCommandResult {
    let start = Instant::now();
    if !policy.allow_raw_shell {
        return ShellCommandResult::not_started(command_error(
            start,
            "raw shell is disabled by local Runner policy".to_string(),
        ));
    }
    let prepared =
        match pool.prepare_command(generation, config, resource_name, session_id, cwd, command) {
            Ok(prepared) => prepared,
            Err(error) => return ShellCommandResult::not_started(command_error(start, error)),
        };
    let transport = prepared.transport.clone();
    let mut result = run_piped_ssh_command(
        prepared.command,
        prepared.program_delivery,
        policy.max_output_bytes,
        timeout_secs.min(policy.max_timeout_secs).max(1),
        stdin,
        stop_requested,
        start,
    );
    apply_transport_failure_policy(pool, &transport, &mut result, policy.max_output_bytes);
    result
}

fn apply_transport_failure_policy(
    pool: &SshConnectionPool,
    transport: &PreparedSshTransport,
    result: &mut ShellCommandResult,
    max_output_bytes: usize,
) {
    if !matches!(
        result.execution_state,
        crate::runner_protocol::ShellCommandExecutionState::Completed
    ) || !is_transport_failure(
        transport,
        result.result.exit_code,
        result.result.stderr.as_deref(),
    ) {
        return;
    }
    pool.invalidate_after_transport_failure(transport);
    if let Some(stderr) = result.result.stderr.as_mut() {
        append_bounded_line(
            stderr,
            "webcodex: SSH transport ended after dispatch; the command may have started and was not retried",
            max_output_bytes,
        );
    }
    if result.result.error.is_none() {
        result.result.error =
            Some("ssh_transport_failed: command may have started and was not retried".to_string());
    }
    result.execution_state = crate::runner_protocol::ShellCommandExecutionState::OutcomeUnknown;
}

/// Used by async job handling to apply the same conservative invalidation
/// decision after its SSH child exits.
pub(crate) fn is_transport_failure(
    transport: &PreparedSshTransport,
    exit_code: Option<i32>,
    stderr: Option<&str>,
) -> bool {
    if exit_code != Some(255) {
        return false;
    }
    if matches!(transport, PreparedSshTransport::Direct) {
        // Direct OpenSSH cannot distinguish a local transport/auth/connect 255
        // from a remote command that deliberately exits 255. Once ssh.exe was
        // spawned, preserve at-most-once semantics and report uncertainty.
        return true;
    }
    let stderr = stderr.unwrap_or_default().to_ascii_lowercase();
    [
        "control socket connect",
        "mux_client",
        "master is dead",
        "connection reset",
        "broken pipe",
        "connection closed",
    ]
    .iter()
    .any(|marker| stderr.contains(marker))
}

fn ensure_control_root(state: &mut SshPoolState) -> Result<PathBuf, String> {
    if let Some(root) = &state.control_root {
        return Ok(root.clone());
    }
    for _ in 0..4 {
        let candidate = std::env::temp_dir().join(format!(
            "wc-ssh-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        #[cfg(unix)]
        let create_result = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700).create(&candidate)
        };
        #[cfg(not(unix))]
        let create_result = std::fs::create_dir(&candidate);
        match create_result {
            Ok(()) => {
                state.control_root = Some(candidate.clone());
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                return Err(
                    "ssh_connection_pool_unavailable: could not create Runner-local control socket directory; command was not started".to_string(),
                )
            }
        }
    }
    Err(
        "ssh_connection_pool_unavailable: could not allocate Runner-local control socket directory; command was not started".to_string(),
    )
}

fn establish_control_socket(connection: &SshConnection) -> Result<(), String> {
    let mut ssh = ssh_command(connection);
    ssh.arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg("-o")
        .arg(format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"))
        .arg("-o")
        .arg("ControlMaster=yes")
        .arg("-o")
        // Keep the master available long enough for interactive Session reuse,
        // but bound idle transports so abandoned Sessions cannot accumulate
        // one OpenSSH process and control socket forever.
        .arg(format!("ControlPersist={SSH_CONTROL_PERSIST_SECS}"))
        .arg("-o")
        .arg(format!("ControlPath={}", connection.control_path.display()))
        .arg("-N")
        .arg("-f")
        .arg(&connection.host)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match ssh.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(_) | Err(_) => Err(format!(
            "ssh_connection_failed: resource '{}'; command was not started",
            connection.key.resource_name
        )),
    }
}

fn control_socket_healthy(connection: &SshConnection) -> bool {
    ssh_command(connection)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg("-S")
        .arg(&connection.control_path)
        .arg("-O")
        .arg("check")
        .arg(&connection.host)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Stop accepting new multiplexed channels without terminating channels that
/// are already running. Config changes and resource removal retire a transport
/// this way; the bounded ControlPersist lifetime lets it exit after the last
/// active channel finishes.
fn retire_control_socket(connection: &SshConnection) {
    let _ = ssh_command(connection)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg("-S")
        .arg(&connection.control_path)
        .arg("-O")
        .arg("stop")
        .arg(&connection.host)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn close_control_socket(connection: &SshConnection) {
    let status = ssh_command(connection)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg("-S")
        .arg(&connection.control_path)
        .arg("-O")
        .arg("exit")
        .arg(&connection.host)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !status.is_ok_and(|status| status.success()) {
        release_control_master_after_failed_exit(connection);
    }
}

/// `-O exit` is the normal path. Some OpenSSH builds can reject the control
/// request after a transport-side failure even though the master is still
/// alive. The socket lives in our private 0700 directory, so a successful
/// `-O check` identifies only the master that this pool created. Release it
/// conservatively instead of leaking an orphaned local client.
#[cfg(unix)]
fn release_control_master_after_failed_exit(connection: &SshConnection) {
    if let Some(pid) = control_master_pid(connection) {
        // SAFETY: `pid` came from the authenticated control socket located in
        // this pool's private directory. SIGTERM affects only that SSH master.
        unsafe {
            let _ = libc::kill(pid, libc::SIGTERM);
        }
    }
}

#[cfg(not(unix))]
fn release_control_master_after_failed_exit(_connection: &SshConnection) {
    // Windows never has an `-O check`-derived master pid to release.
}

#[cfg(unix)]
fn control_master_pid(connection: &SshConnection) -> Option<i32> {
    let output = ssh_command(connection)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg("-S")
        .arg(&connection.control_path)
        .arg("-O")
        .arg("check")
        .arg(&connection.host)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    message.split("pid=").nth(1).and_then(|suffix| {
        suffix
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>()
            .parse::<i32>()
            .ok()
    })
}

fn ssh_command(connection: &SshConnection) -> Command {
    ssh_command_with_config(connection.config_path.as_deref())
}

fn ssh_command_with_config(config_path: Option<&Path>) -> Command {
    let mut command = Command::new(ssh_executable());
    if let Some(config_path) = config_path {
        command.arg("-F").arg(config_path);
    }
    command
}

fn ssh_executable() -> &'static str {
    #[cfg(windows)]
    {
        "ssh.exe"
    }
    #[cfg(not(windows))]
    {
        "ssh"
    }
}

fn ssh_shell_probe_available(status: std::io::Result<ExitStatus>) -> bool {
    #[cfg(unix)]
    {
        status.is_ok()
    }
    #[cfg(windows)]
    {
        status.is_ok_and(|status| status.success())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = status;
        false
    }
}

fn normalize_remote_cwd(cwd: Option<&str>) -> Result<Option<String>, String> {
    let Some(cwd) = cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) else {
        return Ok(None);
    };
    if cwd.len() > SSH_REMOTE_CWD_MAX_BYTES || cwd.chars().any(char::is_control) {
        return Err(
            "ssh_remote_cwd_invalid: cwd must be a bounded remote path without control characters; command was not started".to_string(),
        );
    }
    Ok(Some(cwd.to_string()))
}

fn remote_script(cwd: Option<&str>, command: &str) -> String {
    match cwd {
        Some(cwd) => format!(
            "if ! cd {}; then printf >&2 '%s\\n' 'webcodex: remote cwd is unavailable'; exit 125; fi\n{}",
            shell_quote(cwd),
            command
        ),
        None => command.to_string(),
    }
}

fn shell_quote(value: &str) -> String {
    let mut escaped = String::from("'");
    for (index, part) in value.split('\'').enumerate() {
        if index > 0 {
            escaped.push_str("'\"'\"'");
        }
        escaped.push_str(part);
    }
    escaped.push('\'');
    escaped
}

/// Encode the exact user program as transport data whose own lexical EOF is
/// independent from the user's EOF. The outer loader evaluates this complete
/// wrapper; the inner `eval` builtin then receives exactly `program` as one
/// argument. The frame always ends in a closing single quote, so command
/// substitution cannot strip user trailing newlines from the represented data.
#[cfg(any(test, windows))]
fn windows_direct_program_frame(program: &str) -> String {
    format!("eval {}", shell_quote(program))
}

/// Small fixed remote loader for Windows Direct SSH. The only dynamic argv
/// material is a bounded decimal transport-frame byte count. OpenSSH's one
/// remote shell runs the outer `eval`; all framing state lives only in a
/// command-substitution subshell. The stdin frame is already a self-contained
/// `eval <shell-quoted exact program>` wrapper and always ends in a non-newline
/// quote, so no parser-visible trailing sentinel is needed. `dd bs=1 count=N`
/// consumes only the frame region, and the complete frame is byte-counted before
/// any user program can execute. The wrapper's inner builtin `eval` receives the
/// exact original program text, with its own lexical EOF.
#[cfg(any(test, windows))]
fn windows_direct_program_bootstrap(frame_len: usize) -> String {
    format!(
        r#"{WINDOWS_DIRECT_BOOTSTRAP_PREFIX}{frame_len}; eval "$(set +e; WEBCODEX_SSH_N={frame_len}; [ "$WEBCODEX_SSH_N" -gt 0 ] 2>/dev/null && [ "$WEBCODEX_SSH_N" -le {WINDOWS_DIRECT_FRAME_MAX_BYTES} ] 2>/dev/null || {{ printf >&2 '%s\n' 'webcodex: SSH program length out of range'; printf '%s' 'exit 126'; exit 0; }}; WEBCODEX_SSH_FRAME=$(dd bs=1 count="$WEBCODEX_SSH_N" 2>/dev/null); WEBCODEX_SSH_DD_STATUS=$?; [ "$WEBCODEX_SSH_DD_STATUS" -eq 0 ] || {{ printf >&2 '%s\n' 'webcodex: SSH program upload failed'; printf '%s' 'exit 126'; exit 0; }}; WEBCODEX_SSH_ACTUAL=$(printf '%s' "$WEBCODEX_SSH_FRAME" | wc -c); [ "$WEBCODEX_SSH_ACTUAL" -eq "$WEBCODEX_SSH_N" ] 2>/dev/null || {{ printf >&2 '%s\n' 'webcodex: incomplete SSH program upload'; printf '%s' 'exit 126'; exit 0; }}; printf '%s' "$WEBCODEX_SSH_FRAME")""#
    )
}

fn is_safe_resource_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && !value.contains("..")
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

fn is_safe_session_id(value: &str) -> bool {
    value.starts_with("wc_sess_")
        && value.len() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

// On non-Unix the body is a no-op, so the `command` parameter is unused there.
#[cfg_attr(not(unix), allow(unused_variables))]
fn configure_private_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and runs only in the spawned
        // child before exec, giving job stop/timeout a private group to reap.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
}

#[cfg(unix)]
type PipedSshChild = Child;
#[cfg(windows)]
type PipedSshChild = ManagedChild;

fn spawn_piped_ssh_child(command: &mut Command) -> std::io::Result<PipedSshChild> {
    #[cfg(unix)]
    {
        command.spawn()
    }
    #[cfg(windows)]
    {
        ManagedChild::spawn(command)
    }
}

fn piped_ssh_process_mut(child: &mut PipedSshChild) -> &mut Child {
    #[cfg(unix)]
    {
        child
    }
    #[cfg(windows)]
    {
        child.child_mut()
    }
}

fn try_wait_piped_ssh_child(child: &mut PipedSshChild) -> std::io::Result<Option<ExitStatus>> {
    #[cfg(unix)]
    {
        child.try_wait()
    }
    #[cfg(windows)]
    {
        child.try_wait()
    }
}

fn run_piped_ssh_command(
    mut command: Command,
    program_delivery: PreparedSshProgramDelivery,
    max_output_bytes: usize,
    timeout_secs: u64,
    stdin: Option<&str>,
    stop_requested: Option<&AtomicBool>,
    start: Instant,
) -> ShellCommandResult {
    let needs_stdin = program_delivery.requires_stdin() || stdin.is_some();
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if needs_stdin {
        command.stdin(Stdio::piped());
    }
    let mut child = match spawn_piped_ssh_child(&mut command) {
        Ok(child) => child,
        Err(_) => {
            return ShellCommandResult::not_started(command_error(
                start,
                "ssh_command_spawn_failed: command was not started".to_string(),
            ))
        }
    };

    // Drain output before starting any potentially blocking stdin write. This
    // ordering prevents the classic full-stdin/full-output pipe deadlock.
    let (child_stdin, stdout, stderr) = {
        let process = piped_ssh_process_mut(&mut child);
        (
            process.stdin.take(),
            process.stdout.take(),
            process.stderr.take(),
        )
    };
    let (stdout_tx, stdout_rx) = mpsc::sync_channel(1);
    let (stderr_tx, stderr_rx) = mpsc::sync_channel(1);
    if let Some(stdout) = stdout {
        std::thread::spawn(move || {
            let _ = stdout_tx.send(read_bounded_pipe_tail(stdout, max_output_bytes));
        });
    } else {
        drop(stdout_tx);
    }
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let _ = stderr_tx.send(read_bounded_pipe_tail(stderr, max_output_bytes));
        });
    } else {
        drop(stderr_tx);
    }

    let mut writer_start_error = None;
    let mut stdin_writer = match program_delivery
        .spawn_writer(child_stdin, stdin.map(|input| input.as_bytes().to_vec()))
    {
        Ok(writer) => writer,
        Err(error) => {
            writer_start_error = Some(error);
            let _ = terminate_ssh_child(&mut child);
            None
        }
    };
    let mut writer_error = None;
    let mut wait_error = None;
    let mut stopped = false;
    let mut timed_out = false;
    let status = loop {
        if let Some(error) = writer_start_error
            .take()
            .or_else(|| stdin_writer.as_mut().and_then(SshStdinWriter::poll_failure))
        {
            writer_error = Some(error);
            break terminate_ssh_child(&mut child).ok();
        }
        match try_wait_piped_ssh_child(&mut child) {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if stop_requested.is_some_and(|stop| stop.load(Ordering::SeqCst)) {
                    stopped = true;
                    break terminate_ssh_child(&mut child).ok();
                }
                if start.elapsed() >= Duration::from_secs(timeout_secs) {
                    timed_out = true;
                    break terminate_ssh_child(&mut child).ok();
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                wait_error = Some(format!(
                    "ssh_command_wait_failed: command may have started and was not retried: {error}"
                ));
                break terminate_ssh_child(&mut child).ok();
            }
        }
    };

    // Natural exit and every interruption converge on bounded local cleanup.
    // Killing the SSH tree closes the pipe read end, which unblocks a writer
    // stalled in write_all; we observe it rather than joining blindly.
    let tree_cleanup_uncertain = ensure_piped_ssh_tree_exit(&mut child).is_err();
    let writer_finish_error = stdin_writer.as_mut().and_then(|writer| {
        let interrupted = stopped || timed_out || writer_error.is_some() || wait_error.is_some();
        let result = if interrupted {
            writer.finish_after_tree_cleanup()
        } else {
            writer.finish_bounded()
        };
        result.err()
    });

    let deadline = Instant::now() + Duration::from_secs(SSH_PIPE_DRAIN_TIMEOUT_SECS);
    let stdout = stdout_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let stderr = stderr_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr).into_owned();

    if let Some(error) = writer_finish_error {
        return ShellCommandResult::outcome_unknown(CommandResult {
            exit_code: None,
            stdout: Some(stdout),
            stderr: Some(stderr),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(error),
        });
    }
    if stopped {
        append_bounded_line(
            &mut stderr,
            "webcodex: local SSH client was stopped after dispatch; remote command outcome is unknown; do not blindly retry",
            max_output_bytes,
        );
        return ShellCommandResult::outcome_unknown(CommandResult {
            exit_code: None,
            stdout: Some(stdout),
            stderr: Some(stderr),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(
                "ssh_command_stopped_after_dispatch: local SSH tree was terminated, remote command outcome is unknown; do not blindly retry"
                    .to_string(),
            ),
        });
    }
    if timed_out {
        append_bounded_line(
            &mut stderr,
            &format!("command timed out after {timeout_secs} seconds"),
            max_output_bytes,
        );
        let result = CommandResult {
            exit_code: Some(-1),
            stdout: Some(stdout),
            stderr: Some(stderr),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some("command timed out".to_string()),
        };
        return if status.is_some() && !tree_cleanup_uncertain {
            ShellCommandResult::timed_out(result)
        } else {
            ShellCommandResult::outcome_unknown(result)
        };
    }
    if let Some(error) = writer_error.or(wait_error) {
        return ShellCommandResult::outcome_unknown(CommandResult {
            exit_code: None,
            stdout: Some(stdout),
            stderr: Some(stderr),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(error),
        });
    }
    if tree_cleanup_uncertain {
        return ShellCommandResult::outcome_unknown(CommandResult {
            exit_code: status.and_then(|status| status.code()).or(Some(-1)),
            stdout: Some(stdout),
            stderr: Some(stderr),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(
                "ssh_command_cleanup_failed: local SSH process tree exit could not be proven; command may have started and was not retried".to_string(),
            ),
        });
    }
    ShellCommandResult::completed(CommandResult {
        exit_code: status.and_then(|status| status.code()).or(Some(-1)),
        stdout: Some(stdout),
        stderr: Some(stderr),
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
    })
}

fn terminate_ssh_child(child: &mut PipedSshChild) -> Result<ExitStatus, String> {
    #[cfg(unix)]
    {
        let pid = child.id();
        if pid != 0 {
            // SAFETY: the SSH child creates its own process group before exec,
            // so this only signals the command started by this invocation.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        child.terminate_tree().map_err(|error| error.to_string())?;
        if !child
            .wait_tree_exit(Duration::from_secs(1))
            .map_err(|error| error.to_string())?
        {
            return Err("SSH process tree did not exit after termination".to_string());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match try_wait_piped_ssh_child(child) {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => return Err("SSH child did not exit after termination".to_string()),
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn ensure_piped_ssh_tree_exit(child: &mut PipedSshChild) -> Result<(), String> {
    #[cfg(unix)]
    {
        let _ = child;
        Ok(())
    }
    #[cfg(windows)]
    {
        if child
            .wait_tree_exit(Duration::from_millis(100))
            .map_err(|error| error.to_string())?
        {
            return Ok(());
        }
        child.terminate_tree().map_err(|error| error.to_string())?;
        if child
            .wait_tree_exit(Duration::from_secs(1))
            .map_err(|error| error.to_string())?
        {
            Ok(())
        } else {
            Err("SSH process tree remained live after direct child exit".to_string())
        }
    }
}

fn read_bounded_pipe_tail(mut pipe: impl Read, max_bytes: usize) -> Result<Vec<u8>, String> {
    let retained_limit = max_bytes;
    let mut output = Vec::with_capacity(retained_limit.min(64 * 1024));
    let mut chunk = [0_u8; 8192];
    loop {
        let read = pipe.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(output);
        }
        output.extend_from_slice(&chunk[..read]);
        if output.len() > retained_limit {
            let discard = output.len() - retained_limit;
            output.drain(..discard);
        }
    }
}

fn append_bounded_line(value: &mut String, suffix: &str, max_bytes: usize) {
    if !value.is_empty() && !value.ends_with('\n') {
        value.push('\n');
    }
    value.push_str(suffix);
    if value.len() <= max_bytes {
        return;
    }
    if max_bytes == 0 {
        value.clear();
        return;
    }
    let mut start = value.len().saturating_sub(max_bytes);
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    value.drain(..start);
}

fn command_error(start: Instant, error: String) -> CommandResult {
    CommandResult {
        exit_code: None,
        stdout: None,
        stderr: None,
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: Some(error),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        is_transport_failure, remote_script, run_ssh_shell, shell_quote, ssh_shell_probe_available,
        windows_direct_program_bootstrap, windows_direct_program_frame, PreparedSshTransport,
        SshConnectionKey, SshConnectionPool,
    };
    use crate::webcodex_runner::config::{RunnerPolicy, SshConfig, SshResourceConfig};
    use std::collections::BTreeMap;
    use std::io::Write;
    #[cfg(target_os = "linux")]
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct TestSshServer {
        _temp: tempfile::TempDir,
        child: Child,
        client_config: PathBuf,
        alias: String,
        remote_cwd: PathBuf,
    }

    impl Drop for TestSshServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl TestSshServer {
        #[cfg(target_os = "linux")]
        fn start() -> Option<Self> {
            let sshd = executable_on_path("sshd")?;
            if Command::new(&sshd)
                .arg("-V")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_err()
            {
                return None;
            }
            let temp = tempfile::tempdir().expect("create SSH test directory");
            let remote_cwd = temp.path().join("remote");
            std::fs::create_dir(&remote_cwd).expect("create remote cwd");
            let host_key = temp.path().join("host_ed25519");
            let user_key = temp.path().join("user_ed25519");
            generate_key(&host_key);
            generate_key(&user_key);
            let authorized_keys = temp.path().join("authorized_keys");
            std::fs::copy(user_key.with_extension("pub"), &authorized_keys)
                .expect("copy test public key");
            let listener = TcpListener::bind("127.0.0.1:0").expect("reserve SSH test port");
            let port = listener.local_addr().expect("read SSH test port").port();
            drop(listener);
            let user = String::from_utf8(
                Command::new("id")
                    .args(["-un"])
                    .output()
                    .expect("resolve SSH test user")
                    .stdout,
            )
            .expect("SSH test user is UTF-8")
            .trim()
            .to_string();
            let server_config = temp.path().join("sshd_config");
            std::fs::write(
                &server_config,
                format!(
                    "ListenAddress 127.0.0.1\nPort {port}\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nChallengeResponseAuthentication no\nPubkeyAuthentication yes\nPermitRootLogin yes\nStrictModes no\nUsePAM no\nPrintMotd no\nLogLevel ERROR\n",
                    host_key.display(),
                    temp.path().join("sshd.pid").display(),
                    authorized_keys.display(),
                ),
            )
            .expect("write SSH daemon config");
            let alias = "webcodex-test-ssh".to_string();
            let client_config = temp.path().join("ssh_config");
            std::fs::write(
                &client_config,
                format!(
                    "Host {alias}\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  IdentityFile {}\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  GlobalKnownHostsFile /dev/null\n  LogLevel ERROR\n",
                    user_key.display(),
                ),
            )
            .expect("write SSH client config");
            let checked = Command::new(&sshd)
                .arg("-t")
                .arg("-f")
                .arg(&server_config)
                .output()
                .expect("validate SSH test daemon config");
            assert!(
                checked.status.success(),
                "invalid SSH test daemon config: {}",
                String::from_utf8_lossy(&checked.stderr)
            );
            let mut child = Command::new(&sshd)
                .arg("-D")
                .arg("-e")
                .arg("-f")
                .arg(&server_config)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start SSH test daemon");
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Some(Self {
                        _temp: temp,
                        child,
                        client_config,
                        alias,
                        remote_cwd,
                    });
                }
                if let Some(status) = child.try_wait().expect("poll SSH test daemon") {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.stderr.take() {
                        use std::io::Read;
                        let _ = pipe.read_to_string(&mut stderr);
                    }
                    panic!("SSH test daemon exited early ({status}): {stderr}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("SSH test daemon did not listen within five seconds");
        }

        #[cfg(not(target_os = "linux"))]
        fn start() -> Option<Self> {
            // These tests launch a local OpenSSH daemon and exercise its
            // Linux fixture configuration. macOS still compiles the SSH client
            // production surface, but its system sshd account/auth policy is
            // not a hermetic equivalent of this fixture.
            None
        }
    }

    #[cfg(target_os = "linux")]
    fn generate_key(path: &Path) {
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(path)
            .status()
            .expect("run ssh-keygen for test SSH daemon");
        assert!(status.success(), "ssh-keygen failed");
    }

    #[cfg(target_os = "linux")]
    fn executable_on_path(name: &str) -> Option<PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
        })
    }

    fn ssh_config(server: &TestSshServer) -> SshConfig {
        let mut resources = BTreeMap::new();
        for name in ["tmp", "alt"] {
            resources.insert(
                name.to_string(),
                SshResourceConfig {
                    host: server.alias.clone(),
                    default_cwd: Some(server.remote_cwd.to_string_lossy().into_owned()),
                },
            );
        }
        SshConfig { resources }
    }

    fn run(
        pool: &SshConnectionPool,
        config: &SshConfig,
        resource: &str,
        session_id: &str,
        command: &str,
    ) -> super::CommandResult {
        run_at_cwd(pool, config, resource, session_id, None, command)
    }

    fn run_at_cwd(
        pool: &SshConnectionPool,
        config: &SshConfig,
        resource: &str,
        session_id: &str,
        cwd: Option<&str>,
        command: &str,
    ) -> super::CommandResult {
        run_ssh_shell(
            pool,
            7,
            config,
            &RunnerPolicy::default(),
            resource,
            session_id,
            cwd,
            command,
            None,
            10,
            None,
        )
    }

    #[test]
    fn unix_ssh_shell_capability_preserves_launch_only_probe_semantics() {
        let nonzero = Command::new("sh")
            .arg("-c")
            .arg("exit 7")
            .status()
            .expect("start Unix capability probe fixture");
        assert!(!nonzero.success());
        assert!(ssh_shell_probe_available(Ok(nonzero)));
    }

    #[test]
    fn remote_cwd_is_shell_quoted_without_interpolating_it() {
        let script = remote_script(Some("/tmp/a' b"), "printf ok");
        assert!(script.contains("cd '/tmp/a'\"'\"' b'"), "{script}");
        assert!(script.ends_with("printf ok"), "{script}");
        assert_eq!(shell_quote("plain"), "'plain'");

        for value in ["abc", "a'b", "'a", "''"] {
            let output = Command::new("sh")
                .arg("-c")
                .arg(format!("printf '%s' {}", shell_quote(value)))
                .output()
                .expect("run shell_quote round-trip probe");
            assert!(output.status.success(), "{value:?}: {output:?}");
            assert_eq!(output.stdout, value.as_bytes(), "{value:?}");
        }
        let dense = "'".repeat(4096);
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", shell_quote(&dense)))
            .output()
            .expect("run dense shell_quote round-trip probe");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, dense.as_bytes());
    }

    fn shell_output(
        shell: &str,
        setup: &str,
        program: &str,
        env_paths: &[(&str, &Path)],
    ) -> std::process::Output {
        let command_text = if setup.is_empty() {
            program.to_string()
        } else {
            format!("{setup}; {program}")
        };
        let mut command = Command::new(shell);
        command
            .arg("-c")
            .arg(command_text)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, path) in env_paths {
            command.env(name, path);
        }
        command.output().expect("run direct shell control")
    }

    fn bootstrap_output(
        shell: &str,
        setup: &str,
        program: &str,
        caller_stdin: &[u8],
        env_paths: &[(&str, &Path)],
    ) -> std::process::Output {
        let frame = windows_direct_program_frame(program);
        let bootstrap = windows_direct_program_bootstrap(frame.len());
        let command_text = if setup.is_empty() {
            bootstrap
        } else {
            format!("{setup}; {bootstrap}")
        };
        let mut command = Command::new(shell);
        command
            .arg("-c")
            .arg(command_text)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, path) in env_paths {
            command.env(name, path);
        }
        let mut child = command.spawn().expect("spawn bootstrap probe");
        let mut input = child.stdin.take().expect("bootstrap stdin");
        input.write_all(frame.as_bytes()).unwrap();
        input.write_all(caller_stdin).unwrap();
        drop(input);
        child.wait_with_output().unwrap()
    }

    fn assert_bootstrap_matches_direct(shell: &str, program: &str) {
        let direct = shell_output(shell, "", program, &[]);
        let candidate = bootstrap_output(shell, "", program, b"", &[]);
        assert_eq!(
            candidate.status.code(),
            direct.status.code(),
            "{shell} exit mismatch for {program:?}: direct={direct:?} candidate={candidate:?}"
        );
        assert_eq!(
            candidate.stdout, direct.stdout,
            "{shell} stdout mismatch for {program:?}: direct={direct:?} candidate={candidate:?}"
        );
        assert_eq!(
            candidate.stderr, direct.stderr,
            "{shell} stderr mismatch for {program:?}: direct={direct:?} candidate={candidate:?}"
        );
    }

    fn shell_parser_error_without_transport_failure(stderr: &[u8]) -> bool {
        let text = String::from_utf8_lossy(stderr);
        !stderr.is_empty() && !text.contains("webcodex:")
    }

    #[test]
    fn windows_direct_bootstrap_matches_direct_shell_at_exact_eof() {
        let backslash = '\\';
        let cases = vec![
            "printf x".to_string(),
            "printf x\n".to_string(),
            "printf x\n\n\n".to_string(),
            "printf x\\\n".to_string(),
            format!("printf x{backslash}"),
            format!("printf x{backslash}{backslash}"),
            format!("printf x{backslash}{backslash}{backslash}"),
            "printf '%s' 'quoted\\'".to_string(),
            "printf x # ending comment".to_string(),
            "printf '%s' '#webcodex-ssh-frame'".to_string(),
            "printf x;".to_string(),
            "printf x   \t".to_string(),
            "printf '%s' '中文🙂'".to_string(),
        ];
        for shell in ["sh", "bash"] {
            for program in &cases {
                assert_bootstrap_matches_direct(shell, program);
            }
        }
        let frame = windows_direct_program_frame(&format!("printf x{}", '\\'));
        let bootstrap = windows_direct_program_bootstrap(frame.len());
        assert!(
            frame.ends_with('\''),
            "transport frame must have lexical closure: {frame:?}"
        );
        assert!(
            !bootstrap.contains("#webcodex-ssh-frame"),
            "transport sentinel must not enter parser input: {bootstrap}"
        );
    }

    #[test]
    fn windows_direct_bootstrap_preserves_lone_trailing_backslash_eof() {
        let program = format!("printf x{}", '\\');
        for shell in ["sh", "bash"] {
            assert_bootstrap_matches_direct(shell, &program);
        }
        if Command::new("zsh").arg("--version").output().is_ok() {
            assert_bootstrap_matches_direct("zsh", &program);
        }
    }

    #[test]
    fn windows_direct_bootstrap_preserves_shell_syntax_error_class() {
        for shell in ["sh", "bash"] {
            for program in [
                "printf \"unterminated",
                "printf x &&",
                "if true; then printf x",
                "(printf x",
            ] {
                let direct = shell_output(shell, "", program, &[]);
                let candidate = bootstrap_output(shell, "", program, b"", &[]);
                // Parser-time `-c` syntax errors and builtin `eval` syntax errors
                // do not have a portable numeric status equivalence. In
                // particular, macOS Bash 3.2 invoked as `sh` reports the former
                // as 2 and the latter as 1. The transport contract here is that
                // both remain non-successful syntax errors; valid exact-EOF
                // programs retain strict status equality in the tests above.
                assert_eq!(
                    candidate.stdout, direct.stdout,
                    "{shell} syntax stdout mismatch for {program:?}: direct={direct:?} candidate={candidate:?}"
                );
                assert!(
                    !direct.status.success()
                        && !candidate.status.success()
                        && shell_parser_error_without_transport_failure(&direct.stderr)
                        && shell_parser_error_without_transport_failure(&candidate.stderr),
                    "{shell} syntax error class drift for {program:?}: direct={direct:?} candidate={candidate:?}"
                );
            }
        }
    }

    #[test]
    fn windows_direct_bootstrap_preserves_top_level_eval_scope() {
        for shell in ["sh", "bash"] {
            for program in [
                "return",
                "return 7",
                "break",
                "continue",
                "false; printf '|%s' \"$?\"",
            ] {
                assert_bootstrap_matches_direct(shell, program);
            }
        }
        let setup = "set -- before 'two words'";
        let program = "printf '%s|%s|%s' \"$#\" \"$1\" \"$2\"";
        for shell in ["sh", "bash"] {
            let direct = shell_output(shell, setup, program, &[]);
            let candidate = bootstrap_output(shell, setup, program, b"", &[]);
            assert_eq!(
                candidate.status.code(),
                direct.status.code(),
                "{shell}: {direct:?} {candidate:?}"
            );
            assert_eq!(
                candidate.stdout, direct.stdout,
                "{shell}: {direct:?} {candidate:?}"
            );
            assert_eq!(
                candidate.stderr, direct.stderr,
                "{shell}: {direct:?} {candidate:?}"
            );
        }
    }

    #[test]
    fn windows_direct_bootstrap_preserves_caller_stdin_and_rejects_partial_program() {
        let program = "IFS= read -r line; printf '<%s>' \"$line\"";
        let output = bootstrap_output("sh", "", program, b"caller-data\n", &[]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"<caller-data>");

        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("partial-program-ran");
        let partial_program = format!(
            "printf ran > {}",
            shell_quote(marker.to_string_lossy().as_ref())
        );
        let partial_frame = windows_direct_program_frame(&partial_program);
        let bootstrap = windows_direct_program_bootstrap(partial_frame.len() + 9);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("set -e; {bootstrap}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn partial bootstrap probe");
        let mut input = child.stdin.take().expect("partial bootstrap stdin");
        input.write_all(partial_frame.as_bytes()).unwrap();
        drop(input);
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("incomplete SSH program upload"),
            "{output:?}"
        );
        assert!(!marker.exists(), "partial remote program was executed");
    }

    #[test]
    fn windows_direct_bootstrap_preserves_umask_and_loader_state() {
        for (mask, expected_file_mode, expected_dir_mode) in
            [("022", 0o644, 0o755), ("027", 0o640, 0o750)]
        {
            let temp = tempfile::tempdir().unwrap();
            let file = temp.path().join(format!("umask-{mask}.file"));
            let dir = temp.path().join(format!("umask-{mask}.dir"));
            let setup = format!(
                "umask {mask}; set -- before 'two words'; export WEBCODEX_SSH_PROGRAM_BYTES=original WEBCODEX_SSH_N=OLD_N WEBCODEX_SSH_FRAME=OLD_TRANSPORT_FRAME WEBCODEX_SSH_FRAMED=OLD_FRAME WEBCODEX_SSH_DD_STATUS=OLD_STATUS WEBCODEX_SSH_PROGRAM=OLD_PROGRAM WEBCODEX_SSH_ACTUAL=OLD_ACTUAL n=N i=I d=D f=F actual=A; cleanup() {{ printf cleanup-original; }}"
            );
            let program = format!(
                "printf 'mask=%s|meta=%s|private=%s,%s,%s,%s,%s,%s|generic=%s,%s,%s,%s,%s|args=%s,%s,%s|' \"$(umask)\" \"$WEBCODEX_SSH_PROGRAM_BYTES\" \"$WEBCODEX_SSH_N\" \"$WEBCODEX_SSH_FRAME\" \"$WEBCODEX_SSH_FRAMED\" \"$WEBCODEX_SSH_DD_STATUS\" \"$WEBCODEX_SSH_PROGRAM\" \"$WEBCODEX_SSH_ACTUAL\" \"$n\" \"$i\" \"$d\" \"$f\" \"$actual\" \"$#\" \"$1\" \"$2\"; cleanup; touch {}; mkdir {}",
                shell_quote(file.to_string_lossy().as_ref()),
                shell_quote(dir.to_string_lossy().as_ref())
            );
            let output = bootstrap_output("sh", &setup, &program, b"", &[]);
            assert!(output.status.success(), "{mask}: {output:?}");
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("mask=0{mask}|meta=original|private=OLD_N,OLD_TRANSPORT_FRAME,OLD_FRAME,OLD_STATUS,OLD_PROGRAM,OLD_ACTUAL|generic=N,I,D,F,A|args=2,before,two words|cleanup-original")
            );
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                expected_file_mode
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                expected_dir_mode
            );
        }
    }

    #[test]
    fn windows_direct_bootstrap_preserves_traps_exit_and_exec() {
        let exited = bootstrap_output("sh", "trap 'printf exit-trap' 0", "exit 7", b"", &[]);
        assert_eq!(exited.status.code(), Some(7), "{exited:?}");
        assert_eq!(exited.stdout, b"exit-trap");

        let terminated = bootstrap_output(
            "sh",
            "trap 'printf term-trap; exit 42' TERM",
            "kill -TERM $$; printf never",
            b"",
            &[],
        );
        assert_eq!(terminated.status.code(), Some(42), "{terminated:?}");
        assert_eq!(terminated.stdout, b"term-trap");

        let execed = bootstrap_output("sh", "", "exec printf exec-ok", b"", &[]);
        assert!(execed.status.success(), "{execed:?}");
        assert_eq!(execed.stdout, b"exec-ok");
    }

    #[test]
    fn windows_direct_bootstrap_preserves_trailing_newlines_quotes_and_unicode() {
        for (program, expected) in [
            ("printf no-trailing", "no-trailing"),
            ("printf one\\\n", "one"),
            ("printf many\\\n\n\n", "many"),
        ] {
            let output = bootstrap_output("sh", "", program, b"", &[]);
            assert!(output.status.success(), "{program:?}: {output:?}");
            assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        }

        let text = "中文🙂 'single' \"double\" $ & | ;";
        let program = format!("printf '%s' {}\n\n", shell_quote(text));
        let output = bootstrap_output("sh", "", &program, b"", &[]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), text);
    }

    #[test]
    fn windows_direct_bootstrap_does_not_restart_bash() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("bash-env.marker");
        let hook = temp.path().join("bash-env.sh");
        std::fs::write(
            &hook,
            format!(
                "printf '%s\\n' hook >> {}\n",
                shell_quote(marker.to_string_lossy().as_ref())
            ),
        )
        .unwrap();

        let control = Command::new("bash")
            .arg("-c")
            .arg("printf control")
            .env("BASH_ENV", &hook)
            .output()
            .expect("run ordinary single-shell BASH_ENV control");
        assert!(control.status.success(), "{control:?}");
        assert_eq!(control.stdout, b"control");
        assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
        std::fs::remove_file(&marker).unwrap();

        let output = bootstrap_output("bash", "", "printf user", b"", &[("BASH_ENV", &hook)]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"user");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().lines().count(),
            1,
            "BASH_ENV must run only for the one shell OpenSSH would start"
        );
        let bootstrap = windows_direct_program_bootstrap(windows_direct_program_frame("x").len());
        for forbidden in ["\"$0\" -c", "sh -c", "bash -c", "zsh -c"] {
            assert!(!bootstrap.contains(forbidden), "{forbidden}: {bootstrap}");
        }
    }

    #[test]
    fn windows_direct_bootstrap_does_not_restart_zsh_when_available() {
        if Command::new("zsh").arg("--version").output().is_err() {
            eprintln!("skipping zsh startup-hook regression because zsh is unavailable");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("zshenv.marker");
        let hook = temp.path().join(".zshenv");
        std::fs::write(
            &hook,
            format!(
                "printf '%s\\n' hook >> {}\n",
                shell_quote(marker.to_string_lossy().as_ref())
            ),
        )
        .unwrap();

        let control = Command::new("zsh")
            .arg("-c")
            .arg("printf control")
            .env("ZDOTDIR", temp.path())
            .output()
            .expect("run ordinary single-shell zshenv control");
        assert!(control.status.success(), "{control:?}");
        assert_eq!(control.stdout, b"control");
        assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
        std::fs::remove_file(&marker).unwrap();

        let output = bootstrap_output("zsh", "", "printf user", b"", &[("ZDOTDIR", temp.path())]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"user");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().lines().count(),
            1,
            ".zshenv must run only for the one shell OpenSSH would start"
        );
    }

    #[test]
    fn config_generation_change_does_not_interrupt_active_remote_channel() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let marker = server.remote_cwd.join("generation-channel-started");
        let remote_command = format!(
            "printf started > {}; sleep 1; printf old-generation",
            shell_quote(marker.to_string_lossy().as_ref())
        );
        let prepared = pool
            .prepare_command(
                7,
                &config,
                "tmp",
                "wc_sess_generation",
                None,
                &remote_command,
            )
            .expect("prepare generation 7 channel");
        let old_channel = std::thread::spawn(move || {
            let mut command = prepared.command;
            command.output().expect("run generation 7 channel")
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(marker.exists(), "old generation channel did not start");

        let next = run_ssh_shell(
            &pool,
            8,
            &config,
            &RunnerPolicy::default(),
            "tmp",
            "wc_sess_generation",
            None,
            "printf new-generation",
            None,
            10,
            None,
        );
        assert_eq!(next.exit_code, Some(0), "{next:?}");
        assert_eq!(next.stdout.as_deref(), Some("new-generation"));

        let old = old_channel.join().expect("join generation 7 channel");
        assert!(
            old.status.success(),
            "generation change interrupted active channel: {}",
            String::from_utf8_lossy(&old.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&old.stdout), "old-generation");
        assert_eq!(
            pool.connection_count(),
            1,
            "only the active config generation remains reusable"
        );
    }

    #[test]
    fn reuses_session_transport_but_not_remote_shell_state_and_reconnects() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let missing = run(&pool, &config, "missing", "wc_sess_missing", "printf no");
        assert!(
            missing
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_resource_not_found")),
            "{missing:?}"
        );

        let cwd_pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let unavailable_cwd_path = server
            .remote_cwd
            .join("not-there")
            .to_string_lossy()
            .into_owned();
        let unavailable_cwd = run_at_cwd(
            &cwd_pool,
            &config,
            "tmp",
            "wc_sess_missing_cwd",
            Some(&unavailable_cwd_path),
            "printf never-runs",
        );
        assert_eq!(unavailable_cwd.exit_code, Some(125), "{unavailable_cwd:?}");
        assert!(
            unavailable_cwd
                .stderr
                .as_deref()
                .is_some_and(|stderr| stderr.contains("webcodex: remote cwd is unavailable")),
            "{unavailable_cwd:?}"
        );

        let first = run(
            &pool,
            &config,
            "tmp",
            "wc_sess_alpha",
            "export WEBCODEX_SSH_TEST_STATE=kept; pwd; printf first",
        );
        assert_eq!(first.exit_code, Some(0), "{first:?}");
        assert!(first.error.is_none(), "{first:?}");
        assert_eq!(first.stderr.as_deref(), Some(""), "{first:?}");
        assert!(
            first
                .stdout
                .as_deref()
                .is_some_and(|stdout| stdout.contains(server.remote_cwd.to_string_lossy().as_ref())),
            "{first:?}"
        );
        let first_control = pool
            .control_path_for(7, "tmp", "wc_sess_alpha")
            .expect("initial control socket");
        assert_eq!(
            std::fs::metadata(first_control.parent().expect("control root"))
                .expect("control root metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "SSH control sockets must stay inside a private Runner directory"
        );
        assert_eq!(pool.connection_count(), 1);

        let isolated = run(
            &pool,
            &config,
            "tmp",
            "wc_sess_alpha",
            "test -z \"${WEBCODEX_SSH_TEST_STATE+x}\" && printf isolated",
        );
        assert_eq!(isolated.exit_code, Some(0), "{isolated:?}");
        assert_eq!(isolated.stdout.as_deref(), Some("isolated"));
        assert_eq!(isolated.stderr.as_deref(), Some(""), "{isolated:?}");
        assert_eq!(
            pool.connection_count(),
            1,
            "same Session/resource reuses transport"
        );

        let other_session = run(
            &pool,
            &config,
            "tmp",
            "wc_sess_beta",
            "printf other-session",
        );
        assert_eq!(other_session.exit_code, Some(0), "{other_session:?}");
        let other_resource = run(
            &pool,
            &config,
            "alt",
            "wc_sess_alpha",
            "printf other-resource",
        );
        assert_eq!(other_resource.exit_code, Some(0), "{other_resource:?}");
        assert_eq!(
            pool.connection_count(),
            3,
            "pool keys include Session and resource"
        );

        let check_result = Command::new("ssh")
            .arg("-F")
            .arg(&server.client_config)
            .arg("-S")
            .arg(&first_control)
            .arg("-O")
            .arg("check")
            .arg(&server.alias)
            .output()
            .expect("inspect test SSH master");
        assert!(
            check_result.status.success(),
            "could not inspect test SSH master: {}",
            String::from_utf8_lossy(&check_result.stderr)
        );
        let master_pid = format!(
            "{}{}",
            String::from_utf8_lossy(&check_result.stdout),
            String::from_utf8_lossy(&check_result.stderr)
        )
        .split("pid=")
        .nth(1)
        .and_then(|suffix| {
            suffix
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect::<String>()
                .parse::<i32>()
                .ok()
        })
        .expect("SSH master check reports its pid");
        // SAFETY: the pid is reported by the control socket owned by this
        // test's pool, so it identifies only the temporary test master.
        assert_eq!(unsafe { libc::kill(master_pid, libc::SIGTERM) }, 0);
        std::thread::sleep(Duration::from_millis(50));
        let reconnected = run(&pool, &config, "tmp", "wc_sess_alpha", "printf reconnected");
        assert_eq!(reconnected.exit_code, Some(0), "{reconnected:?}");
        assert_eq!(reconnected.stdout.as_deref(), Some("reconnected"));
        assert_ne!(
            pool.control_path_for(7, "tmp", "wc_sess_alpha"),
            Some(first_control),
            "a dead master gets a fresh control socket on the next command"
        );

        let mut removed_resource_config = config.clone();
        removed_resource_config.resources.remove("tmp");
        let removed = run(
            &pool,
            &removed_resource_config,
            "tmp",
            "wc_sess_alpha",
            "printf never-started",
        );
        assert!(
            removed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_resource_not_found")),
            "{removed:?}"
        );
        assert!(
            pool.control_path_for(7, "tmp", "wc_sess_alpha").is_none(),
            "removing a resource releases its old Session transport"
        );
        assert_eq!(
            pool.connection_count(),
            1,
            "removing a resource releases every pooled Session transport for that name"
        );
    }

    #[test]
    fn remote_async_jobs_stream_output_and_stop_through_the_existing_lifecycle() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let mut manager = crate::JobManager::new(1);
        manager.ssh_pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let sink = crate::webcodex_runner::RunnerSink::WebSocket {
            tx,
            client_id: "ssh-agent".to_string(),
            runner_instance_id: "ssh-instance".to_string(),
        };
        manager.enqueue(
            sink.clone(),
            crate::PendingJobStart::from_wire(
                11,
                RunnerPolicy::default(),
                crate::webcodex_runner::ShellConfig::default(),
                config.clone(),
                PathBuf::new(),
                ssh_job_request("ssh-job-complete", "tmp", "printf job-ok"),
            ),
        );
        let completed = wait_for_job_update(&mut rx, "ssh-job-complete", |update| update.finished);
        assert_eq!(completed.status, "completed", "{completed:?}");
        assert_eq!(completed.exit_code, Some(0), "{completed:?}");
        assert_eq!(
            completed.command_execution_state,
            Some(crate::runner_protocol::ShellCommandExecutionState::Completed),
            "{completed:?}"
        );
        assert!(
            completed
                .log_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.stdout.tail.contains("job-ok")),
            "{completed:?}"
        );

        manager.enqueue(
            sink.clone(),
            crate::PendingJobStart::from_wire(
                11,
                RunnerPolicy::default(),
                crate::webcodex_runner::ShellConfig::default(),
                config.clone(),
                PathBuf::new(),
                ssh_job_request("ssh-job-stop", "tmp", "sleep 30"),
            ),
        );
        let running =
            wait_for_job_update(&mut rx, "ssh-job-stop", |update| update.status == "running");
        assert!(!running.finished, "{running:?}");
        manager.stop("ssh-job-stop").expect("stop remote job");
        let stopped = wait_for_job_update(&mut rx, "ssh-job-stop", |update| update.finished);
        assert_eq!(stopped.status, "failed", "{stopped:?}");
        assert_eq!(stopped.exit_code, None, "{stopped:?}");
        assert_eq!(
            stopped.command_execution_state,
            Some(crate::runner_protocol::ShellCommandExecutionState::OutcomeUnknown),
            "{stopped:?}"
        );
        assert!(
            stopped
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_command_stopped_after_dispatch")),
            "{stopped:?}"
        );

        manager.enqueue(
            sink,
            crate::PendingJobStart::from_wire(
                11,
                RunnerPolicy::default(),
                crate::webcodex_runner::ShellConfig::default(),
                config,
                PathBuf::new(),
                ssh_job_request("ssh-job-missing", "missing", "printf never-started"),
            ),
        );
        let missing = wait_for_job_update(&mut rx, "ssh-job-missing", |update| update.finished);
        assert_eq!(missing.status, "failed", "{missing:?}");
        assert_eq!(
            missing.command_execution_state,
            Some(crate::runner_protocol::ShellCommandExecutionState::NotStarted),
            "{missing:?}"
        );
        assert!(
            missing
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_resource_not_found")),
            "{missing:?}"
        );
        assert!(
            missing
                .error
                .as_deref()
                .is_some_and(|error| error.contains("command was not started")),
            "{missing:?}"
        );
    }

    #[test]
    fn unix_mux_exit_255_classification_remains_transport_evidence_based() {
        let transport = PreparedSshTransport::Mux(SshConnectionKey {
            session_id: "wc_sess_mux_classifier".to_string(),
            resource_name: "tmp".to_string(),
            generation: 7,
        });
        assert!(!is_transport_failure(
            &transport,
            Some(7),
            Some("mux_client")
        ));
        assert!(!is_transport_failure(
            &transport,
            Some(255),
            Some("remote command deliberately exited 255")
        ));
        assert!(is_transport_failure(
            &transport,
            Some(255),
            Some("mux_client: master is dead")
        ));
    }

    fn ssh_job_request(
        job_id: &str,
        resource: &str,
        command: &str,
    ) -> crate::runner_protocol::RunnerRequest {
        serde_json::from_value(serde_json::json!({
            "request_id": format!("request-{job_id}"),
            "client_id": "ssh-agent",
            "kind": "start_job",
            "job_id": job_id,
            "command": command,
            "timeout_secs": 60,
            "requested_by": "test",
            "created_at": 1,
            "job_context": {
                "runtime_project_id": "agent:ssh-agent:remote-project",
                "workflow_session_id": "wc_sess_ssh_job",
                "ssh_resource": resource,
                "project_cwd": ".",
                "purpose": "other",
                "shell": "remote",
                "command_preview": "remote test command",
                "validation_steps": []
            }
        }))
        .expect("build remote SSH job request")
    }

    fn wait_for_job_update(
        rx: &mut tokio::sync::mpsc::Receiver<crate::runner_protocol::RunnerEnvelope>,
        job_id: &str,
        predicate: impl Fn(&crate::runner_protocol::RunnerJobUpdateRequest) -> bool,
    ) -> crate::runner_protocol::RunnerJobUpdateRequest {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(crate::runner_protocol::RunnerEnvelope::JobUpdate { payload })
                    if payload.job_id == job_id && predicate(&payload) =>
                {
                    return payload;
                }
                Ok(_) | Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    panic!("remote SSH job update channel closed")
                }
            }
        }
        panic!("timed out waiting for remote SSH job {job_id}");
    }

    /// A project-registry directory with a registered `remote-project` owned by the
    /// `ssh-agent` Runner. SSH persistent shells still require the project to
    /// be registered on the Runner (it owns the Runner + resource binding);
    /// only execution happens remotely.
    fn ssh_project_registry_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create ssh projects dir");
        std::fs::write(
            dir.path().join("remote-project.toml"),
            "id = \"remote-project\"\npath = \"/srv/remote-project\"\n",
        )
        .expect("write remote-project.toml");
        dir
    }

    fn ssh_persistent_shell_request(
        action: &str,
        shell_id: &str,
        resource: &str,
        command: Option<&str>,
    ) -> crate::runner_protocol::RunnerRequest {
        ssh_persistent_shell_request_at_cwd(action, shell_id, resource, None, command)
    }

    fn ssh_persistent_shell_request_at_cwd(
        action: &str,
        shell_id: &str,
        resource: &str,
        cwd: Option<&str>,
        command: Option<&str>,
    ) -> crate::runner_protocol::RunnerRequest {
        serde_json::from_value(serde_json::json!({
            "request_id": format!("req-{action}-{shell_id}"),
            "client_id": "ssh-agent",
            "kind": "persistent_shell",
            "command": command.unwrap_or(""),
            "timeout_secs": 30,
            "requested_by": "test",
            "created_at": 1,
            "job_context": {
                "runtime_project_id": "agent:ssh-agent:remote-project",
                "workflow_session_id": "wc_sess_ssh_pshell",
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
                "workflow_session_id": "wc_sess_ssh_pshell",
                "runtime_project_id": "agent:ssh-agent:remote-project",
                "cwd": cwd,
                "shell": "bash",
                "command": command,
                "timeout_secs": 20,
                "purpose": null
            }
        }))
        .expect("build remote SSH persistent shell request")
    }

    fn ssh_config_without_default(server: &TestSshServer) -> SshConfig {
        let mut resources = BTreeMap::new();
        resources.insert(
            "nodefault".to_string(),
            SshResourceConfig {
                host: server.alias.clone(),
                default_cwd: None,
            },
        );
        SshConfig { resources }
    }

    #[test]
    fn remote_persistent_shell_preserves_state_across_execs() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let manager = crate::webcodex_runner::PersistentShellManager::new(
            &crate::webcodex_runner::ShellConfig::default(),
            pool,
        );
        let policy = RunnerPolicy::default();
        let project_registry_dir = ssh_project_registry_dir();
        let projects = project_registry_dir.path().to_path_buf();

        let opened = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("open", "wc_shell_rps", "tmp", None),
        );
        assert_eq!(opened.shell_state, "running", "{opened:?}");
        assert_eq!(opened.error_code, None, "{opened:?}");

        // cd, export, a shell variable, a function, and umask must all persist.
        let setup = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request(
                "exec",
                "wc_shell_rps",
                "tmp",
                Some("cd /tmp; export WC_RPS=kept; WC_LOCAL=v; rps_fn() { printf fn; }; umask 027"),
            ),
        );
        assert_eq!(setup.exit_code, Some(0), "{setup:?}");
        assert_eq!(setup.shell_state, "running", "{setup:?}");

        let observed = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request(
                "exec",
                "wc_shell_rps",
                "tmp",
                Some("printf '%s:%s:' \"$PWD\" \"$WC_RPS\"; rps_fn; printf ':%s' \"$(umask)\""),
            ),
        );
        assert_eq!(observed.exit_code, Some(0), "{observed:?}");
        assert_eq!(observed.shell_state, "running", "{observed:?}");
        assert!(
            observed.stdout.contains("/tmp:kept:fn:0027"),
            "remote state did not persist: {:?}",
            observed.stdout
        );
        // Internal markers must never leak into user output.
        assert!(
            !observed.stdout.contains("WCPS"),
            "marker leaked: {:?}",
            observed.stdout
        );
        assert!(
            !observed.stderr.contains("WCPS"),
            "marker leaked: {:?}",
            observed.stderr
        );

        // unset propagates.
        let unset = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("exec", "wc_shell_rps", "tmp", Some("unset WC_RPS")),
        );
        assert_eq!(unset.exit_code, Some(0), "{unset:?}");
        let gone = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request(
                "exec",
                "wc_shell_rps",
                "tmp",
                Some("printf '%s' \"${WC_RPS-unset}\""),
            ),
        );
        assert_eq!(gone.stdout, "unset", "{gone:?}");

        let closed = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("close", "wc_shell_rps", "tmp", None),
        );
        assert_eq!(closed.shell_state, "closed", "{closed:?}");
    }

    #[test]
    fn remote_persistent_shell_resets_when_resource_is_removed() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let manager = crate::webcodex_runner::PersistentShellManager::new(
            &crate::webcodex_runner::ShellConfig::default(),
            pool,
        );
        let policy = RunnerPolicy::default();
        let project_registry_dir = ssh_project_registry_dir();
        let projects = project_registry_dir.path().to_path_buf();

        let opened = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("open", "wc_shell_rps_reset", "tmp", None),
        );
        assert_eq!(opened.shell_state, "running", "{opened:?}");

        // Removing the resource from the active config invalidates the shell.
        let mut removed = config.clone();
        removed.resources.remove("tmp");
        let exec = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &removed,
            7,
            &projects,
            &ssh_persistent_shell_request(
                "exec",
                "wc_shell_rps_reset",
                "tmp",
                Some("printf forbidden"),
            ),
        );
        assert_eq!(
            exec.error_code.as_deref(),
            Some("shell_reset_required"),
            "{exec:?}"
        );
    }

    #[test]
    fn remote_persistent_shell_resets_when_config_generation_changes() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let manager = crate::webcodex_runner::PersistentShellManager::new(
            &crate::webcodex_runner::ShellConfig::default(),
            pool,
        );
        let policy = RunnerPolicy::default();
        let project_registry_dir = ssh_project_registry_dir();
        let projects = project_registry_dir.path().to_path_buf();
        let shell_id = "wc_shell_rps_gen";
        let resource = "tmp";

        let opened = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("open", shell_id, resource, None),
        );
        assert_eq!(opened.shell_state, "running", "{opened:?}");

        // The first exec runs on the generation-7 binding and reaches the
        // remote host.
        let first = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request(
                "exec",
                shell_id,
                resource,
                Some("printf generation-7-command"),
            ),
        );
        assert_eq!(first.exit_code, Some(0), "{first:?}");
        assert_eq!(first.stdout, "generation-7-command", "{first:?}");
        assert_eq!(first.shell_state, "running", "{first:?}");

        // The Runner config advanced to generation 8 for the same resource
        // name. The old shell must not run the user command.
        let reset = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            8,
            &projects,
            &ssh_persistent_shell_request("exec", shell_id, resource, Some("printf forbidden")),
        );
        assert_eq!(
            reset.error_code.as_deref(),
            Some("shell_reset_required"),
            "{reset:?}"
        );
        assert!(!reset.command_started, "{reset:?}");
        assert_eq!(reset.stdout, "", "{reset:?}");

        // The old shell is closed and can no longer run user commands; a
        // further exec reports its terminal state instead of executing.
        let stale = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            8,
            &projects,
            &ssh_persistent_shell_request("exec", shell_id, resource, Some("printf stale")),
        );
        assert!(
            stale.error_code.is_some(),
            "closed old shell must reject further execs: {stale:?}"
        );
        assert!(!stale.command_started, "{stale:?}");
        assert_eq!(stale.stdout, "", "{stale:?}");
        assert_eq!(manager.active_count(), 0, "{stale:?}");

        // Reopening on generation 8 gives a working shell again.
        let reopened = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            8,
            &projects,
            &ssh_persistent_shell_request("open", "wc_shell_rps_gen_new", resource, None),
        );
        assert_eq!(reopened.shell_state, "running", "{reopened:?}");
        let after = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            8,
            &projects,
            &ssh_persistent_shell_request(
                "exec",
                "wc_shell_rps_gen_new",
                resource,
                Some("printf generation-8-command"),
            ),
        );
        assert_eq!(after.exit_code, Some(0), "{after:?}");
        assert_eq!(after.stdout, "generation-8-command", "{after:?}");
    }

    #[test]
    fn remote_persistent_shell_applies_explicit_cwd_over_resource_default() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let manager = crate::webcodex_runner::PersistentShellManager::new(
            &crate::webcodex_runner::ShellConfig::default(),
            pool,
        );
        let policy = RunnerPolicy::default();
        let project_registry_dir = ssh_project_registry_dir();
        let projects = project_registry_dir.path().to_path_buf();

        // The resource default_cwd is server.remote_cwd; an explicit cwd must
        // win.
        let opened = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request_at_cwd(
                "open",
                "wc_shell_cwd_explicit",
                "tmp",
                Some("/tmp"),
                None,
            ),
        );
        assert_eq!(opened.shell_state, "running", "{opened:?}");
        assert_eq!(opened.cwd.as_deref(), Some("/tmp"), "{opened:?}");
        assert_eq!(opened.initial_cwd.as_deref(), Some("/tmp"), "{opened:?}");
        assert!(
            !opened.stdout.contains("WCPS"),
            "marker leaked: {:?}",
            opened.stdout
        );

        let observed = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("exec", "wc_shell_cwd_explicit", "tmp", Some("pwd -P")),
        );
        assert_eq!(observed.exit_code, Some(0), "{observed:?}");
        assert_eq!(observed.stdout.trim(), "/tmp", "{observed:?}");
        assert_eq!(observed.cwd.as_deref(), Some("/tmp"), "{observed:?}");
        assert!(
            !observed.stdout.contains("WCPS"),
            "marker leaked: {:?}",
            observed.stdout
        );
        assert!(
            !observed.stderr.contains("WCPS"),
            "marker leaked: {:?}",
            observed.stderr
        );
    }

    #[test]
    fn remote_persistent_shell_uses_session_cwd_when_no_explicit_open_cwd() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let manager = crate::webcodex_runner::PersistentShellManager::new(
            &crate::webcodex_runner::ShellConfig::default(),
            pool,
        );
        let policy = RunnerPolicy::default();
        let project_registry_dir = ssh_project_registry_dir();
        let projects = project_registry_dir.path().to_path_buf();
        let session_cwd = server.remote_cwd.join("session-dir");
        std::fs::create_dir(&session_cwd).expect("create session cwd");
        let session_cwd = session_cwd.to_string_lossy().into_owned();

        // The Server sends the Session default_cwd as operation.cwd. It must
        // become the actual remote initial cwd even though the resource also
        // has a default_cwd.
        let opened = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request_at_cwd(
                "open",
                "wc_shell_cwd_session",
                "tmp",
                Some(&session_cwd),
                None,
            ),
        );
        assert_eq!(opened.shell_state, "running", "{opened:?}");
        assert_eq!(
            opened.cwd.as_deref(),
            Some(session_cwd.as_str()),
            "{opened:?}"
        );
        assert_eq!(
            opened.initial_cwd.as_deref(),
            Some(session_cwd.as_str()),
            "{opened:?}"
        );

        let observed = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("exec", "wc_shell_cwd_session", "tmp", Some("pwd -P")),
        );
        assert_eq!(observed.exit_code, Some(0), "{observed:?}");
        assert_eq!(observed.stdout.trim(), session_cwd, "{observed:?}");
        assert_eq!(
            observed.cwd.as_deref(),
            Some(session_cwd.as_str()),
            "{observed:?}"
        );
    }

    #[test]
    fn remote_persistent_shell_uses_resource_default_cwd_when_nothing_else() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let manager = crate::webcodex_runner::PersistentShellManager::new(
            &crate::webcodex_runner::ShellConfig::default(),
            pool,
        );
        let policy = RunnerPolicy::default();
        let project_registry_dir = ssh_project_registry_dir();
        let projects = project_registry_dir.path().to_path_buf();
        let resource_cwd = server.remote_cwd.to_string_lossy().into_owned();

        let opened = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("open", "wc_shell_cwd_resource", "tmp", None),
        );
        assert_eq!(opened.shell_state, "running", "{opened:?}");
        assert_eq!(
            opened.cwd.as_deref(),
            Some(resource_cwd.as_str()),
            "{opened:?}"
        );

        let observed = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("exec", "wc_shell_cwd_resource", "tmp", Some("pwd -P")),
        );
        assert_eq!(observed.exit_code, Some(0), "{observed:?}");
        assert_eq!(observed.stdout.trim(), resource_cwd, "{observed:?}");
        assert_eq!(
            observed.cwd.as_deref(),
            Some(resource_cwd.as_str()),
            "{observed:?}"
        );
    }

    #[test]
    fn remote_persistent_shell_keeps_login_dir_when_no_cwd_is_specified() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config_without_default(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let manager = crate::webcodex_runner::PersistentShellManager::new(
            &crate::webcodex_runner::ShellConfig::default(),
            pool,
        );
        let policy = RunnerPolicy::default();
        let project_registry_dir = ssh_project_registry_dir();
        let projects = project_registry_dir.path().to_path_buf();

        // The remote login directory: what a plain `ssh host pwd -P` reports
        // when no cd is issued.
        let login_dir = Command::new("ssh")
            .arg("-F")
            .arg(&server.client_config)
            .arg(&server.alias)
            .arg("pwd -P")
            .output()
            .expect("resolve SSH test login directory");
        assert!(
            login_dir.status.success(),
            "could not resolve SSH test login directory: {}",
            String::from_utf8_lossy(&login_dir.stderr)
        );
        let login_dir = String::from_utf8_lossy(&login_dir.stdout)
            .trim()
            .to_string();
        assert!(!login_dir.is_empty(), "SSH login directory is empty");

        let opened = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("open", "wc_shell_cwd_login", "nodefault", None),
        );
        assert_eq!(opened.shell_state, "running", "{opened:?}");
        assert!(opened.error.is_none(), "{opened:?}");
        assert_eq!(
            opened.cwd.as_deref(),
            Some(login_dir.as_str()),
            "{opened:?}"
        );
        assert_eq!(
            opened.initial_cwd.as_deref(),
            Some(login_dir.as_str()),
            "without any requested cwd the reported initial cwd is the login directory, not an empty seed"
        );

        let changed = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request(
                "exec",
                "wc_shell_cwd_login",
                "nodefault",
                Some("cd /tmp"),
            ),
        );
        assert_eq!(changed.exit_code, Some(0), "{changed:?}");
        assert_eq!(changed.cwd.as_deref(), Some("/tmp"), "{changed:?}");

        let status = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("status", "wc_shell_cwd_login", "nodefault", None),
        );
        assert_eq!(status.cwd.as_deref(), Some("/tmp"), "{status:?}");
        assert_eq!(
            status.initial_cwd.as_deref(),
            Some(login_dir.as_str()),
            "status must retain the login directory as initial_cwd: {status:?}"
        );

        let closed = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("close", "wc_shell_cwd_login", "nodefault", None),
        );
        assert_eq!(closed.cwd.as_deref(), Some("/tmp"), "{closed:?}");
        assert_eq!(
            closed.initial_cwd.as_deref(),
            Some(login_dir.as_str()),
            "close must retain the login directory as initial_cwd: {closed:?}"
        );
    }

    #[test]
    fn remote_persistent_shell_freezes_physical_cwd_from_symlink_request() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let manager = crate::webcodex_runner::PersistentShellManager::new(
            &crate::webcodex_runner::ShellConfig::default(),
            pool,
        );
        let policy = RunnerPolicy::default();
        let project_registry_dir = ssh_project_registry_dir();
        let projects = project_registry_dir.path().to_path_buf();
        let physical = server.remote_cwd.join("physical");
        let logical = server.remote_cwd.join("logical");
        std::fs::create_dir(&physical).expect("create physical remote cwd");
        symlink(&physical, &logical).expect("create remote cwd symlink");
        let physical = physical.canonicalize().expect("canonicalize physical cwd");
        let physical = physical.to_string_lossy().into_owned();
        let logical = logical.to_string_lossy().into_owned();

        let opened = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request_at_cwd(
                "open",
                "wc_shell_cwd_symlink",
                "tmp",
                Some(&logical),
                None,
            ),
        );
        assert_eq!(opened.shell_state, "running", "{opened:?}");
        assert_eq!(opened.cwd.as_deref(), Some(physical.as_str()), "{opened:?}");
        assert_eq!(
            opened.initial_cwd.as_deref(),
            Some(physical.as_str()),
            "initial_cwd must use pwd -P rather than the requested symlink: {opened:?}"
        );

        let changed = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("exec", "wc_shell_cwd_symlink", "tmp", Some("cd /tmp")),
        );
        assert_eq!(changed.exit_code, Some(0), "{changed:?}");

        let status = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("status", "wc_shell_cwd_symlink", "tmp", None),
        );
        assert_eq!(status.cwd.as_deref(), Some("/tmp"), "{status:?}");
        assert_eq!(
            status.initial_cwd.as_deref(),
            Some(physical.as_str()),
            "{status:?}"
        );

        let closed = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request("close", "wc_shell_cwd_symlink", "tmp", None),
        );
        assert_eq!(closed.cwd.as_deref(), Some("/tmp"), "{closed:?}");
        assert_eq!(
            closed.initial_cwd.as_deref(),
            Some(physical.as_str()),
            "{closed:?}"
        );
    }

    #[test]
    fn remote_persistent_shell_open_fails_when_cwd_is_unavailable() {
        let Some(server) = TestSshServer::start() else {
            eprintln!("skipping SSH integration test because sshd is unavailable");
            return;
        };
        let config = ssh_config(&server);
        let pool = SshConnectionPool::with_test_config(server.client_config.clone());
        let manager = crate::webcodex_runner::PersistentShellManager::new(
            &crate::webcodex_runner::ShellConfig::default(),
            pool,
        );
        let policy = RunnerPolicy::default();
        let project_registry_dir = ssh_project_registry_dir();
        let projects = project_registry_dir.path().to_path_buf();
        let missing = server
            .remote_cwd
            .join("does-not-exist")
            .to_string_lossy()
            .into_owned();

        let opened = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request_at_cwd(
                "open",
                "wc_shell_cwd_missing",
                "tmp",
                Some(&missing),
                None,
            ),
        );
        assert_eq!(
            opened.error_code.as_deref(),
            Some("persistent_shell_initialization_failed"),
            "{opened:?}"
        );
        assert_ne!(opened.shell_state, "running", "{opened:?}");
        assert_eq!(manager.active_count(), 0, "{opened:?}");

        // The failed shell is fully torn down; nothing can be executed against
        // a shell that failed to open.
        let stale = manager.handle(
            &policy,
            &crate::webcodex_runner::ShellConfig::default(),
            &config,
            7,
            &projects,
            &ssh_persistent_shell_request(
                "exec",
                "wc_shell_cwd_missing",
                "tmp",
                Some("printf never-runs"),
            ),
        );
        assert!(
            stale.error_code.is_some(),
            "failed open must leave no usable shell: {stale:?}"
        );
        assert_eq!(stale.stdout, "", "{stale:?}");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::runner_protocol::ShellCommandExecutionState;
    use crate::webcodex_runner::config::{RunnerPolicy, SshResourceConfig};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, Instant};

    struct FakeSsh {
        _temp: tempfile::TempDir,
        path: PathBuf,
    }

    static FAKE_SSH: OnceLock<Arc<FakeSsh>> = OnceLock::new();

    fn fake_ssh() -> Arc<FakeSsh> {
        FAKE_SSH
            .get_or_init(|| {
                let temp = tempfile::tempdir().expect("create fake SSH tempdir");
                let source = temp.path().join("fake_ssh.rs");
                let output = temp
                    .path()
                    .join(format!("fake-ssh{}", std::env::consts::EXE_SUFFIX));
                std::fs::write(
                    &source,
                    r#"
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const PROGRAM_PREFIX: &str = ": WEBCODEX_SSH_PROGRAM_BYTES=";

fn decode_program_frame(frame: &str) -> Option<String> {
    let quoted = frame.strip_prefix("eval ")?;
    if quoted.len() < 2 || !quoted.starts_with('\'') || !quoted.ends_with('\'') {
        return None;
    }
    Some(quoted[1..quoted.len() - 1].replace("'\"'\"'", "'"))
}

fn suffix<'a>(script: &'a str, marker: &str) -> Option<&'a str> {
    script.rfind(marker).map(|index| script[index + marker.len()..].trim())
}

fn append_start(path: &str) {
    let mut file = OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(file, "start").unwrap();
}

fn direct_program_len(args: &[String]) -> Option<usize> {
    let bootstrap = args.last()?;
    let rest = bootstrap.strip_prefix(PROGRAM_PREFIX)?;
    rest.split(';').next()?.parse().ok()
}

fn main() {
    let args = env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("--grandchild") {
        let marker = args.get(2).expect("grandchild marker");
        thread::sleep(Duration::from_secs(3));
        fs::write(marker, format!("{}", std::process::id())).unwrap();
        thread::sleep(Duration::from_secs(60));
        return;
    }

    let host = args.get(5).map(String::as_str).unwrap_or_default();
    if host == "fake-exit-before-program" {
        std::process::exit(0);
    }
    if let Some(marker) = host.strip_prefix("fake-never-read-stop=") {
        fs::write(marker, format!("{}", std::process::id())).unwrap();
        println!("FAKE_SSH_PID={}", std::process::id());
        io::stdout().flush().unwrap();
        thread::sleep(Duration::from_secs(60));
        return;
    }
    if host == "fake-never-read" || host == "fake-never-read-output" {
        println!("FAKE_SSH_PID={}", std::process::id());
        io::stdout().flush().unwrap();
        if host == "fake-never-read-output" {
            io::stdout().write_all(&vec![b'o'; 2 * 1024 * 1024]).unwrap();
            io::stdout().flush().unwrap();
            io::stderr().write_all(&vec![b'e'; 2 * 1024 * 1024]).unwrap();
            io::stderr().flush().unwrap();
        }
        thread::sleep(Duration::from_secs(60));
        return;
    }

    let mut stdin = io::stdin().lock();
    let mut owned_script = None;
    if let Some(program_len) = direct_program_len(&args) {
        let mut program = vec![0_u8; program_len];
        if stdin.read_exact(&mut program).is_err() {
            eprintln!("fake ssh: incomplete program frame");
            std::process::exit(126);
        }
        let frame = String::from_utf8(program).expect("UTF-8 program frame");
        owned_script = Some(decode_program_frame(&frame).expect("valid Windows Direct program frame"));
    }
    let script = owned_script
        .as_deref()
        .unwrap_or_else(|| args.last().map(String::as_str).unwrap_or_default());

    if let Some(base) = suffix(script, "WC_FAKE_CAPTURE_FRAME::") {
        fs::write(format!("{base}.program"), script.as_bytes()).unwrap();
        let mut caller_stdin = Vec::new();
        stdin.read_to_end(&mut caller_stdin).unwrap();
        fs::write(format!("{base}.stdin"), caller_stdin).unwrap();
        print!("capture-ok");
        io::stdout().flush().unwrap();
        return;
    }
    if let Some(path) = suffix(script, "WC_FAKE_EXIT_255::") {
        append_start(path);
        eprintln!("ambiguous direct ssh exit");
        std::process::exit(255);
    }
    if script.contains("WC_FAKE_EXIT_7") {
        eprintln!("remote-like command failure");
        std::process::exit(7);
    }
    if script.contains("WC_FAKE_STDIN_FAILURE") {
        thread::sleep(Duration::from_millis(100));
        return;
    }
    if script.contains("WC_FAKE_OUTPUT") {
        io::stdout().write_all(&vec![b'o'; 32 * 1024]).unwrap();
        io::stdout().flush().unwrap();
        io::stderr().write_all(&vec![b'e'; 32 * 1024]).unwrap();
        io::stderr().flush().unwrap();
        return;
    }
    if let Some(marker) = suffix(script, "WC_FAKE_TREE::") {
        let child = Command::new(env::current_exe().unwrap())
            .arg("--grandchild")
            .arg(marker)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        println!("GRANDCHILD_PID={}", child.id());
        io::stdout().flush().unwrap();
        thread::sleep(Duration::from_secs(60));
        return;
    }
    if let Some(marker) = suffix(script, "WC_FAKE_WAIT_FOR_STOP::") {
        fs::write(marker, format!("{}", std::process::id())).unwrap();
        thread::sleep(Duration::from_secs(60));
        return;
    }
    if script.contains("WC_FAKE_SLEEP") {
        thread::sleep(Duration::from_secs(60));
        return;
    }
    print!("fake-ssh-ok");
    io::stdout().flush().unwrap();
}
"#,
                )
                .expect("write fake SSH source");
                let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
                let result = Command::new(rustc)
                    .arg("--edition=2021")
                    .arg("--crate-name=webcodex_fake_ssh")
                    .arg(&source)
                    .arg("-o")
                    .arg(&output)
                    .output()
                    .expect("run rustc for fake SSH");
                assert!(
                    result.status.success(),
                    "fake SSH compilation failed: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                Arc::new(FakeSsh {
                    _temp: temp,
                    path: output,
                })
            })
            .clone()
    }

    fn ssh_config(host: &str, default_cwd: Option<&str>) -> SshConfig {
        let mut resources = BTreeMap::new();
        resources.insert(
            "spe".to_string(),
            SshResourceConfig {
                host: host.to_string(),
                default_cwd: default_cwd.map(str::to_string),
            },
        );
        SshConfig { resources }
    }

    fn command_args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn direct_windows_ssh_output(
        host: &str,
        program: &str,
        caller_stdin: Option<&[u8]>,
    ) -> std::process::Output {
        let mut command = Command::new("ssh.exe");
        command
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("LogLevel=ERROR")
            .arg(host)
            .arg(program)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if caller_stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        let mut child = command.spawn().expect("spawn direct ssh.exe control");
        if let Some(caller_stdin) = caller_stdin {
            let mut stdin = child.stdin.take().expect("direct ssh.exe control stdin");
            stdin
                .write_all(caller_stdin)
                .expect("write direct ssh.exe control stdin");
            drop(stdin);
        }
        child
            .wait_with_output()
            .expect("wait direct ssh.exe control")
    }

    fn assert_direct_args(args: &[String], host: &str) {
        assert_eq!(
            &args[..5],
            ["-o", "BatchMode=yes", "-o", "LogLevel=ERROR", host]
        );
        assert!(!args.iter().any(|arg| arg == "-S"), "{args:?}");
        assert!(
            !args.iter().any(|arg| arg.contains("ControlMaster")),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("ControlPersist")),
            "{args:?}"
        );
    }

    fn stdin_frame(delivery: &PreparedSshProgramDelivery) -> &str {
        let PreparedSshProgramDelivery::StdinFramed { program } = delivery else {
            panic!("Windows Direct SSH must use framed stdin program delivery");
        };
        std::str::from_utf8(program).expect("prepared transport frame is UTF-8")
    }

    fn conservative_windows_command_line_utf16_bound(command: &Command) -> usize {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|value| {
                let units = value.to_string_lossy().encode_utf16().count();
                // Rust/Windows argv quoting cannot more than double every input
                // code unit plus a pair of surrounding quotes and a separator.
                units * 2 + 3
            })
            .sum::<usize>()
            + 1
    }

    fn explicit_bash_wire_command(authored: &str) -> String {
        let escaped = authored.replace('\'', "'\\''");
        format!("exec bash -c '{escaped}'")
    }

    fn fake_pool() -> SshConnectionPool {
        SshConnectionPool::with_test_executable(fake_ssh().path.clone())
    }

    fn grandchild_pid(text: &str) -> u32 {
        text.lines()
            .find_map(|line| line.trim().strip_prefix("GRANDCHILD_PID="))
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or_else(|| panic!("missing GRANDCHILD_PID in {text:?}"))
    }

    fn wait_for_process_exit(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !crate::job_manager_tests::process_running(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        !crate::job_manager_tests::process_running(pid)
    }

    fn ssh_job_request(
        job_id: &str,
        command: &str,
        timeout_secs: u64,
    ) -> crate::runner_protocol::RunnerRequest {
        serde_json::from_value(serde_json::json!({
            "request_id": format!("request-{job_id}"),
            "client_id": "ssh-agent",
            "kind": "start_job",
            "job_id": job_id,
            "command": command,
            "timeout_secs": timeout_secs,
            "requested_by": "test",
            "created_at": 1,
            "job_context": {
                "runtime_project_id": "agent:ssh-agent:remote-project",
                "workflow_session_id": "wc_sess_windows_ssh_job",
                "ssh_resource": "spe",
                "project_cwd": ".",
                "purpose": "other",
                "shell": "remote",
                "command_preview": "remote test command",
                "validation_steps": []
            }
        }))
        .expect("build Windows SSH job request")
    }

    fn wait_for_job_update(
        rx: &mut tokio::sync::mpsc::Receiver<crate::runner_protocol::RunnerEnvelope>,
        job_id: &str,
        predicate: impl Fn(&crate::runner_protocol::RunnerJobUpdateRequest) -> bool,
    ) -> crate::runner_protocol::RunnerJobUpdateRequest {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(crate::runner_protocol::RunnerEnvelope::JobUpdate { payload })
                    if payload.job_id == job_id && predicate(&payload) =>
                {
                    return payload;
                }
                Ok(_) | Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    panic!("Windows SSH job update channel closed")
                }
            }
        }
        panic!("timed out waiting for Windows SSH job {job_id}");
    }

    fn enqueue_job(
        manager: &crate::JobManager,
        sink: crate::webcodex_runner::RunnerSink,
        config: SshConfig,
        policy: RunnerPolicy,
        job_id: &str,
        command: &str,
        timeout_secs: u64,
    ) {
        manager.enqueue(
            sink,
            crate::PendingJobStart::from_wire(
                11,
                policy,
                crate::webcodex_runner::ShellConfig::default(),
                config,
                PathBuf::new(),
                ssh_job_request(job_id, command, timeout_secs),
            ),
        );
    }

    #[test]
    fn windows_ssh_capabilities_track_ssh_exe_discovery() {
        let executable_available = Command::new("ssh.exe")
            .arg("-V")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        assert_eq!(SshConnectionPool::is_available(), executable_available);
        let nonzero = Command::new("cmd.exe")
            .args(["/d", "/c", "exit /b 7"])
            .status()
            .expect("start Windows capability probe fixture");
        assert!(!nonzero.success());
        assert!(!ssh_shell_probe_available(Ok(nonzero)));
        assert_eq!(
            SshConnectionPool::persistent_shell_available(),
            executable_available
        );
    }

    #[test]
    fn windows_one_shot_and_job_keep_remote_program_out_of_createprocess_argv() {
        let config = ssh_config("spe", Some("/srv/webcodex"));
        let pool = SshConnectionPool::default();
        let prepared = pool
            .prepare_command(
                7,
                &config,
                "spe",
                "wc_sess_windows_ssh",
                Some("/srv/override"),
                "printf test",
            )
            .expect("prepare Windows one-shot SSH command");
        assert_eq!(prepared.command.get_program(), "ssh.exe");
        assert!(matches!(prepared.transport, PreparedSshTransport::Direct));
        let args = command_args(&prepared.command);
        assert_direct_args(&args, "spe");
        assert_eq!(args.len(), 6, "{args:?}");
        assert!(
            args.last()
                .is_some_and(|arg| arg.starts_with(WINDOWS_DIRECT_BOOTSTRAP_PREFIX)),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("printf test")),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("/srv/override")),
            "{args:?}"
        );
        assert_eq!(
            stdin_frame(&prepared.program_delivery),
            windows_direct_program_frame("if ! cd '/srv/override'; then printf >&2 '%s\\n' 'webcodex: remote cwd is unavailable'; exit 125; fi\nprintf test")
        );
        assert_eq!(pool.connection_count(), 0);

        let job = pool
            .prepare_job_command(7, &config, "spe", "wc_sess_windows_ssh", None, "printf job")
            .expect("prepare Windows background SSH command");
        assert_eq!(job.command.get_program(), "ssh.exe");
        assert!(matches!(job.transport, PreparedSshTransport::Direct));
        let job_args = command_args(&job.command);
        assert_direct_args(&job_args, "spe");
        assert!(
            !job_args.iter().any(|arg| arg.contains("printf job")),
            "{job_args:?}"
        );
        assert!(
            !job_args.iter().any(|arg| arg.contains("/srv/webcodex")),
            "{job_args:?}"
        );
        assert_eq!(
            stdin_frame(&job.program_delivery),
            windows_direct_program_frame("if ! cd '/srv/webcodex'; then printf >&2 '%s\\n' 'webcodex: remote cwd is unavailable'; exit 125; fi\nprintf job")
        );
        assert_eq!(pool.connection_count(), 0);
    }

    #[test]
    fn windows_direct_large_program_and_cwd_contract_fit_bounded_argv() {
        let pool = SshConnectionPool::default();
        let authored = "'".repeat(crate::runner_protocol::RAW_SHELL_COMMAND_MAX_BYTES);
        let wrapped = explicit_bash_wire_command(&authored);
        assert_eq!(wrapped.len(), 64_015);
        assert!(wrapped.len() <= crate::runner_protocol::RAW_SHELL_WIRE_MAX_BYTES);

        let max_host = "h".repeat(512);
        let prepared = pool
            .prepare_command(
                7,
                &ssh_config(&max_host, None),
                "spe",
                "wc_sess_windows_long_program",
                None,
                &wrapped,
            )
            .expect("prepare 16K quote-dense explicit bash command");
        let args = command_args(&prepared.command);
        assert_direct_args(&args, &max_host);
        assert_eq!(
            stdin_frame(&prepared.program_delivery),
            windows_direct_program_frame(&wrapped)
        );
        assert!(stdin_frame(&prepared.program_delivery).len() <= WINDOWS_DIRECT_FRAME_MAX_BYTES);
        assert!(!args.iter().any(|arg| arg.contains(&authored)));
        let argv_bound = conservative_windows_command_line_utf16_bound(&prepared.command);
        assert!(
            argv_bound < 8 * 1024,
            "conservative UTF-16 argv bound={argv_bound}"
        );
        assert!(
            argv_bound < 32_767,
            "CreateProcessW argv bound={argv_bound}"
        );

        let result = run_ssh_shell_with_execution_state(
            &fake_pool(),
            7,
            &ssh_config("spe", None),
            &RunnerPolicy::default(),
            "spe",
            "wc_sess_windows_long_program",
            None,
            &wrapped,
            None,
            5,
            None,
        );
        assert_eq!(
            result.execution_state,
            ShellCommandExecutionState::Completed,
            "{result:?}"
        );
        assert_eq!(result.result.exit_code, Some(0), "{result:?}");

        let max_wire = "x".repeat(crate::runner_protocol::RAW_SHELL_WIRE_MAX_BYTES);
        crate::runner_protocol::validate_raw_shell_wire_command(&max_wire).unwrap();
        assert!(
            crate::runner_protocol::validate_raw_shell_wire_command(&format!("{max_wire}x"))
                .is_err()
        );
        assert!(
            crate::runner_protocol::validate_raw_shell_wire_command("printf ok\0printf never")
                .is_err(),
            "raw-shell NUL rejection must remain platform-neutral"
        );
        let max_prepared = pool
            .prepare_command(
                7,
                &ssh_config("spe", None),
                "spe",
                "wc_sess_windows_max_wire",
                None,
                &max_wire,
            )
            .expect("prepare exact max wire command");
        assert_eq!(
            stdin_frame(&max_prepared.program_delivery),
            windows_direct_program_frame(&max_wire)
        );

        let quote_dense_cwd = "'".repeat(SSH_REMOTE_CWD_MAX_BYTES);
        let cwd_prepared = pool
            .prepare_command(
                7,
                &ssh_config("spe", None),
                "spe",
                "wc_sess_windows_max_cwd",
                Some(&quote_dense_cwd),
                "printf cwd",
            )
            .expect("prepare max quote-dense cwd");
        assert_eq!(
            stdin_frame(&cwd_prepared.program_delivery),
            windows_direct_program_frame(&remote_script(Some(&quote_dense_cwd), "printf cwd"))
        );
        let cwd_args = command_args(&cwd_prepared.command);
        assert!(!cwd_args.iter().any(|arg| arg.contains(&quote_dense_cwd)));
        assert!(conservative_windows_command_line_utf16_bound(&cwd_prepared.command) < 8 * 1024);

        let job = pool
            .prepare_job_command(
                7,
                &ssh_config("spe", None),
                "spe",
                "wc_sess_windows_long_job",
                None,
                &wrapped,
            )
            .expect("prepare long Windows SSH Job payload");
        assert_eq!(
            stdin_frame(&job.program_delivery),
            windows_direct_program_frame(&wrapped)
        );
        assert!(!command_args(&job.command)
            .iter()
            .any(|arg| arg.contains(&authored)));
    }

    #[test]
    fn windows_shell_quote_handles_leading_and_dense_single_quotes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a'b"), "'a'\"'\"'b'");
        assert_eq!(shell_quote("'a"), "''\"'\"'a'");
        assert_eq!(shell_quote("''"), "''\"'\"''\"'\"''");
        let dense = "'".repeat(SSH_REMOTE_CWD_MAX_BYTES);
        assert_eq!(shell_quote(&dense).len(), 2 + dense.len() * 5);
    }

    #[test]
    fn windows_direct_preparation_uses_current_generation_resource_mapping() {
        let pool = SshConnectionPool::default();
        let generation_7 = ssh_config("host-a", None);
        let old = pool
            .prepare_command(
                7,
                &generation_7,
                "spe",
                "wc_sess_windows_generation",
                None,
                "printf old",
            )
            .unwrap();
        assert_direct_args(&command_args(&old.command), "host-a");

        let generation_8 = ssh_config("host-b", None);
        let current = pool
            .prepare_command(
                8,
                &generation_8,
                "spe",
                "wc_sess_windows_generation",
                None,
                "printf current",
            )
            .unwrap();
        assert_direct_args(&command_args(&current.command), "host-b");
        assert_eq!(
            pool.connection_count(),
            0,
            "Windows direct SSH must not create mux state"
        );
    }

    #[test]
    fn windows_direct_transport_classifies_only_exit_255_as_unknown() {
        let transport = PreparedSshTransport::Direct;
        assert!(!is_transport_failure(&transport, Some(0), None));
        assert!(!is_transport_failure(&transport, Some(7), Some("anything")));
        assert!(is_transport_failure(
            &transport,
            Some(255),
            Some("remote command deliberately exited 255")
        ));
    }

    #[test]
    fn windows_direct_framing_keeps_program_and_caller_stdin_exactly_separate() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("frame-capture");
        let program = format!("WC_FAKE_CAPTURE_FRAME::{}", base.display());
        let caller_stdin = "caller stdin\nwith 'quotes' and WC_FAKE markers\n";
        let result = run_ssh_shell_with_execution_state(
            &fake_pool(),
            7,
            &ssh_config("spe", None),
            &RunnerPolicy::default(),
            "spe",
            "wc_sess_windows_frame",
            None,
            &program,
            Some(caller_stdin),
            5,
            None,
        );
        assert_eq!(
            result.execution_state,
            ShellCommandExecutionState::Completed,
            "{result:?}"
        );
        assert_eq!(
            result.result.stdout.as_deref(),
            Some("capture-ok"),
            "{result:?}"
        );
        assert_eq!(
            std::fs::read(format!("{}.program", base.display())).unwrap(),
            program.as_bytes()
        );
        assert_eq!(
            std::fs::read(format!("{}.stdin", base.display())).unwrap(),
            caller_stdin.as_bytes()
        );
    }

    #[test]
    fn windows_program_writer_failure_after_spawn_is_outcome_unknown() {
        let command = "x".repeat(crate::runner_protocol::RAW_SHELL_WIRE_MAX_BYTES);
        let cwd = "'".repeat(SSH_REMOTE_CWD_MAX_BYTES);
        let result = run_ssh_shell_with_execution_state(
            &fake_pool(),
            7,
            &ssh_config("fake-exit-before-program", None),
            &RunnerPolicy::default(),
            "spe",
            "wc_sess_windows_program_writer_failure",
            Some(&cwd),
            &command,
            None,
            5,
            None,
        );
        assert_eq!(
            result.execution_state,
            ShellCommandExecutionState::OutcomeUnknown,
            "{result:?}"
        );
        assert_eq!(result.result.exit_code, None, "{result:?}");
        assert!(
            result
                .result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_program_write_failed")),
            "{result:?}"
        );
    }

    #[test]
    #[ignore = "runner real-process lane: fake SSH blocks program delivery until timeout"]
    fn runner_real_process_windows_blocked_program_writer_timeout_drains_output_and_returns_bounded(
    ) {
        let policy = RunnerPolicy {
            max_output_bytes: 4 * 1024,
            ..RunnerPolicy::default()
        };
        let program = "x".repeat(crate::runner_protocol::RAW_SHELL_WIRE_MAX_BYTES);
        let started = Instant::now();
        let result = run_ssh_shell_with_execution_state(
            &fake_pool(),
            7,
            &ssh_config("fake-never-read-output", None),
            &policy,
            "spe",
            "wc_sess_windows_blocked_timeout",
            None,
            &program,
            None,
            1,
            None,
        );
        assert!(started.elapsed() < Duration::from_secs(4), "{result:?}");
        assert_eq!(
            result.execution_state,
            ShellCommandExecutionState::TimedOut,
            "{result:?}"
        );
        assert!(result.result.stdout.as_deref().unwrap_or_default().len() <= 4 * 1024);
        assert!(result.result.stderr.as_deref().unwrap_or_default().len() <= 4 * 1024);
    }

    #[test]
    #[ignore = "runner real-process lane: fake SSH blocked writer is stopped and reaped"]
    fn runner_real_process_windows_blocked_program_writer_stop_is_outcome_unknown_and_reaps_client()
    {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("blocked-writer.pid");
        let host = format!("fake-never-read-stop={}", marker.display());
        let stop_requested = Arc::new(AtomicBool::new(false));
        let stop_signal = Arc::clone(&stop_requested);
        let marker_for_stop = marker.clone();
        let stopper = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker_for_stop.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                marker_for_stop.exists(),
                "fake SSH never reached blocked-writer state"
            );
            stop_signal.store(true, Ordering::SeqCst);
        });
        let program = "x".repeat(crate::runner_protocol::RAW_SHELL_WIRE_MAX_BYTES);
        let started = Instant::now();
        let result = run_ssh_shell_with_execution_state(
            &fake_pool(),
            7,
            &ssh_config(&host, None),
            &RunnerPolicy::default(),
            "spe",
            "wc_sess_windows_blocked_stop",
            None,
            &program,
            None,
            30,
            Some(stop_requested.as_ref()),
        );
        stopper.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(4), "{result:?}");
        assert_eq!(
            result.execution_state,
            ShellCommandExecutionState::OutcomeUnknown,
            "{result:?}"
        );
        assert!(
            result
                .result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_command_stopped_after_dispatch")),
            "{result:?}"
        );
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert!(
            wait_for_process_exit(pid),
            "blocked-writer SSH client survived stop: {pid}"
        );
    }

    #[test]
    fn windows_one_shot_direct_ssh_preserves_execution_state_and_no_retry() {
        let config = ssh_config("spe", None);
        let policy = RunnerPolicy::default();
        let pool = fake_pool();

        let ok = run_ssh_shell_with_execution_state(
            &pool,
            7,
            &config,
            &policy,
            "spe",
            "wc_sess_windows_one_shot",
            None,
            "WC_FAKE_EXIT_0",
            None,
            5,
            None,
        );
        assert_eq!(
            ok.execution_state,
            ShellCommandExecutionState::Completed,
            "{ok:?}"
        );
        assert_eq!(ok.result.exit_code, Some(0), "{ok:?}");

        let failed = run_ssh_shell_with_execution_state(
            &pool,
            7,
            &config,
            &policy,
            "spe",
            "wc_sess_windows_one_shot",
            None,
            "WC_FAKE_EXIT_7",
            None,
            5,
            None,
        );
        assert_eq!(
            failed.execution_state,
            ShellCommandExecutionState::Completed,
            "{failed:?}"
        );
        assert_eq!(failed.result.exit_code, Some(7), "{failed:?}");

        let large_stdin = "x".repeat(1024 * 1024);
        let stdin_unknown = run_ssh_shell_with_execution_state(
            &pool,
            7,
            &config,
            &policy,
            "spe",
            "wc_sess_windows_one_shot",
            None,
            "WC_FAKE_STDIN_FAILURE",
            Some(&large_stdin),
            5,
            None,
        );
        assert_eq!(
            stdin_unknown.execution_state,
            ShellCommandExecutionState::OutcomeUnknown,
            "{stdin_unknown:?}"
        );
        assert!(
            stdin_unknown
                .result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_command_stdin_failed")),
            "{stdin_unknown:?}"
        );

        let temp = tempfile::tempdir().unwrap();
        let starts = temp.path().join("exit255.starts");
        let unknown = run_ssh_shell_with_execution_state(
            &pool,
            7,
            &config,
            &policy,
            "spe",
            "wc_sess_windows_one_shot",
            None,
            &format!("WC_FAKE_EXIT_255::{}", starts.display()),
            None,
            5,
            None,
        );
        assert_eq!(
            unknown.execution_state,
            ShellCommandExecutionState::OutcomeUnknown,
            "{unknown:?}"
        );
        assert_eq!(unknown.result.exit_code, Some(255), "{unknown:?}");
        assert!(
            unknown
                .result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_transport_failed")),
            "{unknown:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&starts).unwrap().lines().count(),
            1,
            "direct exit 255 was retried"
        );
    }

    #[test]
    #[ignore = "runner real-process lane: one-shot fake SSH stop reaps a real process"]
    fn runner_real_process_windows_one_shot_post_dispatch_stop_is_outcome_unknown_and_reaps_tree() {
        let temp = tempfile::tempdir().unwrap();
        let started_marker = temp.path().join("one-shot-stop.started");
        let stop_requested = Arc::new(AtomicBool::new(false));
        let stop_signal = Arc::clone(&stop_requested);
        let marker_for_stop = started_marker.clone();
        let stopper = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker_for_stop.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                marker_for_stop.exists(),
                "fake SSH never reached dispatch point"
            );
            stop_signal.store(true, Ordering::SeqCst);
        });

        let result = run_ssh_shell_with_execution_state(
            &fake_pool(),
            7,
            &ssh_config("spe", None),
            &RunnerPolicy::default(),
            "spe",
            "wc_sess_windows_one_shot_stop",
            None,
            &format!("WC_FAKE_WAIT_FOR_STOP::{}", started_marker.display()),
            None,
            30,
            Some(stop_requested.as_ref()),
        );
        stopper.join().expect("join one-shot SSH stopper");
        let pid = std::fs::read_to_string(&started_marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert_eq!(
            result.execution_state,
            ShellCommandExecutionState::OutcomeUnknown,
            "{result:?}"
        );
        assert_eq!(result.result.exit_code, None, "{result:?}");
        assert!(
            result
                .result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_command_stopped_after_dispatch")),
            "{result:?}"
        );
        assert!(
            wait_for_process_exit(pid),
            "one-shot SSH client survived post-dispatch stop: {pid}"
        );
    }

    #[test]
    fn windows_one_shot_spawn_failure_is_not_started() {
        let temp = tempfile::tempdir().unwrap();
        let pool = SshConnectionPool::with_test_executable(temp.path().join("missing-ssh.exe"));
        let result = run_ssh_shell_with_execution_state(
            &pool,
            7,
            &ssh_config("spe", None),
            &RunnerPolicy::default(),
            "spe",
            "wc_sess_windows_spawn_failure",
            None,
            "printf never",
            None,
            5,
            None,
        );
        assert_eq!(
            result.execution_state,
            ShellCommandExecutionState::NotStarted,
            "{result:?}"
        );
        assert!(
            result
                .result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_command_spawn_failed")),
            "{result:?}"
        );
    }

    #[test]
    fn windows_background_ssh_spawn_failure_is_not_started() {
        let temp = tempfile::tempdir().unwrap();
        let mut manager = crate::JobManager::new(1);
        manager.ssh_pool =
            SshConnectionPool::with_test_executable(temp.path().join("missing-ssh.exe"));
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let sink = crate::webcodex_runner::RunnerSink::WebSocket {
            tx,
            client_id: "ssh-agent".to_string(),
            runner_instance_id: "ssh-instance".to_string(),
        };
        enqueue_job(
            &manager,
            sink,
            ssh_config("spe", None),
            RunnerPolicy::default(),
            "win-ssh-spawn-failure",
            "printf never",
            5,
        );
        let failed =
            wait_for_job_update(&mut rx, "win-ssh-spawn-failure", |update| update.finished);
        assert_eq!(failed.status, "failed", "{failed:?}");
        assert_eq!(
            failed.command_execution_state,
            Some(ShellCommandExecutionState::NotStarted),
            "{failed:?}"
        );
        assert!(
            failed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_command_spawn_failed")),
            "{failed:?}"
        );
    }

    #[test]
    fn windows_background_ssh_post_spawn_rejections_are_outcome_unknown() {
        for (shutting_down, stop_requested, job_record_present, expected_error) in [
            (
                true,
                false,
                true,
                "runner began shutdown after command start",
            ),
            (false, true, true, "job stop requested after command start"),
            (
                false,
                false,
                false,
                "runner lost the Job record after command start",
            ),
        ] {
            let error = crate::post_spawn_interruption_reason(
                shutting_down,
                stop_requested,
                job_record_present,
            )
            .expect("post-spawn interruption is rejected");
            assert_eq!(error, expected_error);
            let delta = crate::post_spawn_interruption_delta("start_job", 7, error);
            assert_eq!(delta.status, "failed");
            assert_eq!(delta.exit_code, None);
            assert_eq!(delta.duration_ms, Some(7));
            assert_eq!(
                delta.command_execution_state,
                Some(ShellCommandExecutionState::OutcomeUnknown)
            );
            assert!(delta.finished);
        }
        assert!(crate::post_spawn_interruption_reason(false, false, true).is_none());
    }

    #[test]
    #[ignore = "runner real-process lane: post-spawn rejection terminates a fake SSH process tree"]
    fn runner_real_process_windows_background_ssh_post_spawn_rejection_reaps_owned_tree() {
        use std::io::{BufRead, BufReader};

        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("post-spawn-grandchild.marker");
        let fake = fake_ssh();
        let mut command = Command::new(&fake.path);
        command
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("LogLevel=ERROR")
            .arg("spe")
            .arg(format!("WC_FAKE_TREE::{}", marker.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ManagedChild::spawn(&mut command).expect("spawn owned fake SSH tree");
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .expect("fake SSH stdout pipe");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("read fake SSH grandchild pid");
        let pid = grandchild_pid(&line);
        let child = Arc::new(Mutex::new(child));

        let error = crate::post_spawn_interruption_reason(true, false, true)
            .expect("shutdown after spawn is rejected");
        crate::terminate_managed_tree(&child).expect("terminate owned SSH tree");
        assert!(
            wait_for_process_exit(pid),
            "post-spawn SSH grandchild survived owned-tree termination: {pid}"
        );
        assert!(!marker.exists(), "terminated grandchild reached marker");

        let delta = crate::post_spawn_interruption_delta("start_job", 0, error);
        assert_eq!(
            delta.command_execution_state,
            Some(ShellCommandExecutionState::OutcomeUnknown)
        );
        assert_eq!(delta.status, "failed");
        assert_eq!(delta.exit_code, None);
    }

    #[test]
    #[ignore = "runner real-process lane: one-shot fake SSH timeout reaps a real process tree"]
    fn runner_real_process_windows_one_shot_timeout_reaps_the_managed_ssh_tree() {
        let temp = tempfile::tempdir().unwrap();
        let delayed_marker = temp.path().join("one-shot-grandchild.marker");
        let result = run_ssh_shell_with_execution_state(
            &fake_pool(),
            7,
            &ssh_config("spe", None),
            &RunnerPolicy::default(),
            "spe",
            "wc_sess_windows_tree",
            None,
            &format!("WC_FAKE_TREE::{}", delayed_marker.display()),
            None,
            2,
            None,
        );
        assert_eq!(
            result.execution_state,
            ShellCommandExecutionState::TimedOut,
            "{result:?}"
        );
        let pid = grandchild_pid(result.result.stdout.as_deref().unwrap_or_default());
        assert!(
            wait_for_process_exit(pid),
            "one-shot SSH grandchild survived timeout cleanup: {pid}"
        );
        assert!(
            !delayed_marker.exists(),
            "terminated grandchild reached its delayed marker"
        );
    }

    #[test]
    #[ignore = "runner real-process lane: background fake SSH exercises repeated child-process lifecycle"]
    fn runner_real_process_windows_background_ssh_reuses_job_manager_lifecycle_and_bounds_output() {
        let config = ssh_config("spe", None);
        let mut manager = crate::JobManager::new(1);
        manager.ssh_pool = fake_pool();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let sink = crate::webcodex_runner::RunnerSink::WebSocket {
            tx,
            client_id: "ssh-agent".to_string(),
            runner_instance_id: "ssh-instance".to_string(),
        };

        enqueue_job(
            &manager,
            sink.clone(),
            config.clone(),
            RunnerPolicy::default(),
            "win-ssh-0",
            "WC_FAKE_EXIT_0",
            5,
        );
        let ok = wait_for_job_update(&mut rx, "win-ssh-0", |update| update.finished);
        assert_eq!(ok.status, "completed", "{ok:?}");
        assert_eq!(ok.exit_code, Some(0), "{ok:?}");
        assert_eq!(
            ok.command_execution_state,
            Some(ShellCommandExecutionState::Completed),
            "{ok:?}"
        );

        enqueue_job(
            &manager,
            sink.clone(),
            config.clone(),
            RunnerPolicy::default(),
            "win-ssh-7",
            "WC_FAKE_EXIT_7",
            5,
        );
        let failed = wait_for_job_update(&mut rx, "win-ssh-7", |update| update.finished);
        assert_eq!(failed.status, "failed", "{failed:?}");
        assert_eq!(failed.exit_code, Some(7), "{failed:?}");
        assert_eq!(
            failed.command_execution_state,
            Some(ShellCommandExecutionState::Completed),
            "{failed:?}"
        );

        let starts_dir = tempfile::tempdir().unwrap();
        let starts = starts_dir.path().join("job-255.starts");
        enqueue_job(
            &manager,
            sink.clone(),
            config.clone(),
            RunnerPolicy::default(),
            "win-ssh-255",
            &format!("WC_FAKE_EXIT_255::{}", starts.display()),
            5,
        );
        let unknown = wait_for_job_update(&mut rx, "win-ssh-255", |update| update.finished);
        assert_eq!(unknown.status, "failed", "{unknown:?}");
        assert_eq!(unknown.exit_code, Some(255), "{unknown:?}");
        assert_eq!(
            unknown.command_execution_state,
            Some(ShellCommandExecutionState::OutcomeUnknown),
            "{unknown:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&starts).unwrap().lines().count(),
            1,
            "background direct exit 255 was retried"
        );

        let long_authored = "'".repeat(crate::runner_protocol::RAW_SHELL_COMMAND_MAX_BYTES);
        let long_wire = explicit_bash_wire_command(&long_authored);
        enqueue_job(
            &manager,
            sink.clone(),
            config.clone(),
            RunnerPolicy::default(),
            "win-ssh-long-program",
            &long_wire,
            5,
        );
        let long = wait_for_job_update(&mut rx, "win-ssh-long-program", |update| update.finished);
        assert_eq!(long.status, "completed", "{long:?}");
        assert_eq!(long.exit_code, Some(0), "{long:?}");
        assert_eq!(
            long.command_execution_state,
            Some(ShellCommandExecutionState::Completed),
            "{long:?}"
        );

        let bounded_policy = RunnerPolicy {
            max_output_bytes: 4 * 1024,
            ..RunnerPolicy::default()
        };
        enqueue_job(
            &manager,
            sink.clone(),
            config.clone(),
            bounded_policy,
            "win-ssh-output",
            "WC_FAKE_OUTPUT",
            5,
        );
        let output = wait_for_job_update(&mut rx, "win-ssh-output", |update| update.finished);
        let snapshot = output
            .log_snapshot
            .as_ref()
            .expect("bounded SSH job log snapshot");
        assert!(snapshot.stdout.tail.len() <= 4 * 1024, "{snapshot:?}");
        assert!(snapshot.stderr.tail.len() <= 4 * 1024, "{snapshot:?}");
        assert!(snapshot.stdout.truncated, "{snapshot:?}");
        assert!(snapshot.stderr.truncated, "{snapshot:?}");

        enqueue_job(
            &manager,
            sink,
            config,
            RunnerPolicy::default(),
            "win-ssh-slot",
            "WC_FAKE_EXIT_0",
            5,
        );
        let slot = wait_for_job_update(&mut rx, "win-ssh-slot", |update| update.finished);
        assert_eq!(
            slot.status, "completed",
            "terminal SSH jobs must release the Job slot: {slot:?}"
        );
    }

    #[test]
    #[ignore = "runner real-process lane: background fake SSH stop and timeout reap process trees"]
    fn runner_real_process_windows_background_ssh_stop_and_timeout_reap_owned_trees() {
        let temp = tempfile::tempdir().unwrap();
        let config = ssh_config("spe", None);
        let mut manager = crate::JobManager::new(1);
        manager.ssh_pool = fake_pool();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let sink = crate::webcodex_runner::RunnerSink::WebSocket {
            tx,
            client_id: "ssh-agent".to_string(),
            runner_instance_id: "ssh-instance".to_string(),
        };

        let stop_marker = temp.path().join("stop-grandchild.marker");
        enqueue_job(
            &manager,
            sink.clone(),
            config.clone(),
            RunnerPolicy::default(),
            "win-ssh-stop",
            &format!("WC_FAKE_TREE::{}", stop_marker.display()),
            30,
        );
        let observed = wait_for_job_update(&mut rx, "win-ssh-stop", |update| {
            update
                .log_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.stdout.tail.contains("GRANDCHILD_PID="))
        });
        let stop_pid = grandchild_pid(&observed.log_snapshot.as_ref().unwrap().stdout.tail);
        manager.stop("win-ssh-stop").expect("stop Windows SSH job");
        let stopped = wait_for_job_update(&mut rx, "win-ssh-stop", |update| update.finished);
        assert_eq!(stopped.status, "failed", "{stopped:?}");
        assert_eq!(stopped.exit_code, None, "{stopped:?}");
        assert_eq!(
            stopped.command_execution_state,
            Some(ShellCommandExecutionState::OutcomeUnknown),
            "{stopped:?}"
        );
        assert!(
            stopped
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_command_stopped_after_dispatch")),
            "{stopped:?}"
        );
        assert!(
            wait_for_process_exit(stop_pid),
            "background SSH grandchild survived stop: {stop_pid}"
        );
        assert!(!stop_marker.exists());

        let timeout_marker = temp.path().join("timeout-grandchild.marker");
        enqueue_job(
            &manager,
            sink,
            config,
            RunnerPolicy::default(),
            "win-ssh-timeout",
            &format!("WC_FAKE_TREE::{}", timeout_marker.display()),
            1,
        );
        let timed_out = wait_for_job_update(&mut rx, "win-ssh-timeout", |update| update.finished);
        assert_eq!(timed_out.status, "timeout", "{timed_out:?}");
        assert_eq!(
            timed_out.command_execution_state,
            Some(ShellCommandExecutionState::TimedOut),
            "{timed_out:?}"
        );
        let timeout_pid = grandchild_pid(&timed_out.log_snapshot.as_ref().unwrap().stdout.tail);
        assert!(
            wait_for_process_exit(timeout_pid),
            "background SSH grandchild survived timeout: {timeout_pid}"
        );
        assert!(!timeout_marker.exists());
    }

    #[test]
    #[ignore = "runner real-process lane: Runner shutdown drains and reaps a fake SSH tree"]
    fn runner_real_process_windows_background_ssh_shutdown_drain_is_bounded_and_reaps_tree() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("shutdown-grandchild.marker");
        let mut manager = crate::JobManager::new(1);
        manager.ssh_pool = fake_pool();
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let sink = crate::webcodex_runner::RunnerSink::WebSocket {
            tx,
            client_id: "ssh-agent".to_string(),
            runner_instance_id: "ssh-instance".to_string(),
        };
        enqueue_job(
            &manager,
            sink,
            ssh_config("spe", None),
            RunnerPolicy::default(),
            "win-ssh-shutdown",
            &format!("WC_FAKE_TREE::{}", marker.display()),
            30,
        );
        let observed = wait_for_job_update(&mut rx, "win-ssh-shutdown", |update| {
            update
                .log_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.stdout.tail.contains("GRANDCHILD_PID="))
        });
        let pid = grandchild_pid(&observed.log_snapshot.as_ref().unwrap().stdout.tail);
        let started = Instant::now();
        manager.stop_accepting_work();
        let batch = manager.signal_all_for_shutdown();
        let outcome = manager.drain_shutdown(batch, Instant::now() + Duration::from_secs(2));
        assert_eq!(outcome.timed_out, 0, "{outcome:?}");
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "SSH shutdown drain exceeded its bound"
        );
        assert!(
            wait_for_process_exit(pid),
            "background SSH grandchild survived Runner shutdown: {pid}"
        );
        assert!(!marker.exists());
        let terminal = wait_for_job_update(&mut rx, "win-ssh-shutdown", |update| update.finished);
        assert_eq!(terminal.status, "failed", "{terminal:?}");
        assert_eq!(terminal.exit_code, None, "{terminal:?}");
        assert_eq!(
            terminal.command_execution_state,
            Some(ShellCommandExecutionState::OutcomeUnknown),
            "{terminal:?}"
        );
        assert!(
            terminal
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_command_stopped_after_dispatch")),
            "{terminal:?}"
        );
    }

    #[test]
    fn windows_persistent_shell_prepares_direct_literal_ssh_exe_argv() {
        let config = ssh_config("spe", Some("/srv/webcodex"));
        let pool = SshConnectionPool::default();
        let prepared = pool
            .prepare_persistent_shell_command(7, &config, "spe", "wc_sess_windows_ssh", "bash")
            .expect("prepare Windows persistent SSH command");

        assert_eq!(prepared.command.get_program(), "ssh.exe");
        let args = command_args(&prepared.command);
        assert_eq!(
            args,
            vec!["-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "spe", "bash"]
        );
        assert!(!args.iter().any(|arg| arg.contains("ControlMaster")));
        assert!(!args.iter().any(|arg| arg == "-S"));
        assert_eq!(prepared.default_cwd.as_deref(), Some("/srv/webcodex"));
    }

    #[test]
    fn windows_real_host_one_shot_and_background_are_opt_in() {
        let Ok(host) = std::env::var("WEBCODEX_TEST_WINDOWS_SSH_HOST") else {
            eprintln!("skipping Windows real-host SSH integration: WEBCODEX_TEST_WINDOWS_SSH_HOST is unset");
            return;
        };
        let host = host.trim();
        if host.is_empty() {
            eprintln!("skipping Windows real-host SSH integration: WEBCODEX_TEST_WINDOWS_SSH_HOST is empty");
            return;
        }
        let config = ssh_config(host, None);
        let pool = SshConnectionPool::default();
        let run = |cwd: Option<&str>, command: &str| {
            run_ssh_shell_with_execution_state(
                &pool,
                7,
                &config,
                &RunnerPolicy::default(),
                "spe",
                "wc_sess_windows_real_ssh",
                cwd,
                command,
                None,
                15,
                None,
            )
        };
        let assert_direct_equivalent = |program: &str| {
            let direct = direct_windows_ssh_output(host, program, None);
            let candidate = run(None, program);
            assert_eq!(
                candidate.execution_state,
                ShellCommandExecutionState::Completed,
                "real-host EOF candidate was not definite for {program:?}: {candidate:?}"
            );
            assert_eq!(
                candidate.result.exit_code,
                direct.status.code(),
                "real-host EOF exit mismatch for {program:?}: direct={direct:?} candidate={candidate:?}"
            );
            assert_eq!(
                candidate.result.stdout.as_deref().unwrap_or_default().as_bytes(),
                direct.stdout.as_slice(),
                "real-host EOF stdout mismatch for {program:?}: direct={direct:?} candidate={candidate:?}"
            );
            assert_eq!(
                candidate.result.stderr.as_deref().unwrap_or_default().as_bytes(),
                direct.stderr.as_slice(),
                "real-host EOF stderr mismatch for {program:?}: direct={direct:?} candidate={candidate:?}"
            );
        };

        let backslash = '\\';
        for program in [
            "printf wc-eof-control".to_string(),
            "printf wc-one-newline\n".to_string(),
            "printf wc-many-newlines\n\n\n".to_string(),
            format!("printf x{backslash}"),
            format!("printf x{backslash}{backslash}"),
            format!("printf x{backslash}{backslash}{backslash}"),
            "exit 7".to_string(),
            "exec printf wc-real-exec-control".to_string(),
        ] {
            assert_direct_equivalent(&program);
        }

        let one_shot = run(None, "printf wc-windows-one-shot");
        assert_eq!(
            one_shot.execution_state,
            ShellCommandExecutionState::Completed,
            "{one_shot:?}"
        );
        assert_eq!(one_shot.result.exit_code, Some(0), "{one_shot:?}");
        assert_eq!(
            one_shot.result.stdout.as_deref(),
            Some("wc-windows-one-shot")
        );

        let exit_7 = run(None, "exit 7");
        assert_eq!(
            exit_7.execution_state,
            ShellCommandExecutionState::Completed,
            "{exit_7:?}"
        );
        assert_eq!(exit_7.result.exit_code, Some(7), "{exit_7:?}");

        let exit_255 = run(None, "exit 255");
        assert_eq!(
            exit_255.execution_state,
            ShellCommandExecutionState::OutcomeUnknown,
            "{exit_255:?}"
        );
        assert_eq!(exit_255.result.exit_code, Some(255), "{exit_255:?}");

        let explicit_sh = run(None, "exec sh -c 'printf wc-explicit-sh'");
        assert_eq!(
            explicit_sh.execution_state,
            ShellCommandExecutionState::Completed,
            "{explicit_sh:?}"
        );
        assert_eq!(explicit_sh.result.stdout.as_deref(), Some("wc-explicit-sh"));

        let explicit_bash = run(None, "exec bash -c 'printf wc-explicit-bash'");
        assert_eq!(
            explicit_bash.execution_state,
            ShellCommandExecutionState::Completed,
            "{explicit_bash:?}"
        );
        assert_eq!(
            explicit_bash.result.stdout.as_deref(),
            Some("wc-explicit-bash")
        );

        let execed = run(None, "exec printf wc-top-level-exec");
        assert_eq!(
            execed.execution_state,
            ShellCommandExecutionState::Completed,
            "{execed:?}"
        );
        assert_eq!(execed.result.exit_code, Some(0), "{execed:?}");
        assert_eq!(execed.result.stdout.as_deref(), Some("wc-top-level-exec"));

        let umask = run(None, "umask");
        assert_eq!(
            umask.execution_state,
            ShellCommandExecutionState::Completed,
            "{umask:?}"
        );
        assert_eq!(umask.result.exit_code, Some(0), "{umask:?}");
        let observed_umask = umask.result.stdout.as_deref().unwrap_or_default().trim();
        assert!(
            matches!(observed_umask.len(), 3 | 4)
                && observed_umask.chars().all(|ch| matches!(ch, '0'..='7')),
            "unexpected remote umask projection: {umask:?}"
        );

        let cwd_ok = run(Some("/tmp"), "pwd");
        assert_eq!(
            cwd_ok.execution_state,
            ShellCommandExecutionState::Completed,
            "{cwd_ok:?}"
        );
        assert_eq!(cwd_ok.result.exit_code, Some(0), "{cwd_ok:?}");

        let missing_cwd = format!("/tmp/webcodex-missing-{}", uuid::Uuid::new_v4().simple());
        let cwd_missing = run(Some(&missing_cwd), "printf never");
        assert_eq!(
            cwd_missing.execution_state,
            ShellCommandExecutionState::Completed,
            "{cwd_missing:?}"
        );
        assert_eq!(cwd_missing.result.exit_code, Some(125), "{cwd_missing:?}");
        assert!(
            cwd_missing
                .result
                .stderr
                .as_deref()
                .is_some_and(|stderr| stderr.contains("webcodex: remote cwd is unavailable")),
            "{cwd_missing:?}"
        );

        let caller_stdin = "wc-real-caller-stdin\n";
        let stdin_result = run_ssh_shell_with_execution_state(
            &pool,
            7,
            &config,
            &RunnerPolicy::default(),
            "spe",
            "wc_sess_windows_real_ssh",
            None,
            "cat",
            Some(caller_stdin),
            15,
            None,
        );
        assert_eq!(
            stdin_result.execution_state,
            ShellCommandExecutionState::Completed,
            "{stdin_result:?}"
        );
        assert_eq!(stdin_result.result.exit_code, Some(0), "{stdin_result:?}");
        assert_eq!(stdin_result.result.stdout.as_deref(), Some(caller_stdin));
        let direct_stdin = direct_windows_ssh_output(host, "cat", Some(caller_stdin.as_bytes()));
        assert_eq!(stdin_result.result.exit_code, direct_stdin.status.code());
        assert_eq!(
            stdin_result
                .result
                .stdout
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
            direct_stdin.stdout.as_slice()
        );
        assert_eq!(
            stdin_result
                .result
                .stderr
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
            direct_stdin.stderr.as_slice()
        );

        let prefix = "printf wc-max-wire; #";
        let max_wire = format!(
            "{prefix}{}",
            "x".repeat(crate::runner_protocol::RAW_SHELL_WIRE_MAX_BYTES - prefix.len())
        );
        assert_eq!(
            max_wire.len(),
            crate::runner_protocol::RAW_SHELL_WIRE_MAX_BYTES
        );
        let max_wire_result = run(None, &max_wire);
        assert_eq!(
            max_wire_result.execution_state,
            ShellCommandExecutionState::Completed,
            "{max_wire_result:?}"
        );
        assert_eq!(
            max_wire_result.result.exit_code,
            Some(0),
            "{max_wire_result:?}"
        );
        assert_eq!(
            max_wire_result.result.stdout.as_deref(),
            Some("wc-max-wire")
        );

        let manager = crate::JobManager::new(1);
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let sink = crate::webcodex_runner::RunnerSink::WebSocket {
            tx,
            client_id: "ssh-agent".to_string(),
            runner_instance_id: "ssh-instance".to_string(),
        };
        enqueue_job(
            &manager,
            sink.clone(),
            config.clone(),
            RunnerPolicy::default(),
            "win-ssh-real-background",
            "printf 'wc-windows-background|'; umask",
            15,
        );
        let background =
            wait_for_job_update(&mut rx, "win-ssh-real-background", |update| update.finished);
        assert_eq!(background.status, "completed", "{background:?}");
        assert_eq!(background.exit_code, Some(0), "{background:?}");
        assert_eq!(
            background.command_execution_state,
            Some(ShellCommandExecutionState::Completed),
            "{background:?}"
        );
        let background_snapshot = background
            .log_snapshot
            .as_ref()
            .expect("real Windows SSH background log snapshot");
        assert!(
            background_snapshot
                .stdout
                .tail
                .contains("wc-windows-background"),
            "{background:?}"
        );
        assert!(
            background_snapshot.stdout.tail.contains(observed_umask),
            "background loader changed remote umask: one-shot={observed_umask:?}, background={background:?}"
        );

        let background_eof_program = format!("printf x{}", '\\');
        let background_eof_control = direct_windows_ssh_output(host, &background_eof_program, None);
        enqueue_job(
            &manager,
            sink,
            config,
            RunnerPolicy::default(),
            "win-ssh-real-background-eof",
            &background_eof_program,
            15,
        );
        let background_eof =
            wait_for_job_update(&mut rx, "win-ssh-real-background-eof", |update| {
                update.finished
            });
        assert_eq!(
            background_eof.exit_code,
            background_eof_control.status.code(),
            "background EOF exit mismatch: control={background_eof_control:?} job={background_eof:?}"
        );
        assert_eq!(
            background_eof.command_execution_state,
            Some(ShellCommandExecutionState::Completed),
            "{background_eof:?}"
        );
        let background_eof_snapshot = background_eof
            .log_snapshot
            .as_ref()
            .expect("background EOF log snapshot");
        assert_eq!(
            background_eof_snapshot.stdout.tail.as_bytes(),
            background_eof_control.stdout.as_slice(),
            "background EOF stdout mismatch: control={background_eof_control:?} job={background_eof:?}"
        );
        assert_eq!(
            background_eof_snapshot.stderr.tail.as_bytes(),
            background_eof_control.stderr.as_slice(),
            "background EOF stderr mismatch: control={background_eof_control:?} job={background_eof:?}"
        );
    }

    #[test]
    fn windows_remote_cwd_validation_stays_prestart() {
        let result = run_ssh_shell_with_execution_state(
            &fake_pool(),
            7,
            &ssh_config("spe", None),
            &RunnerPolicy::default(),
            "spe",
            "wc_sess_windows_invalid_cwd",
            Some("/tmp\ninvalid"),
            "printf never",
            None,
            5,
            None,
        );
        assert_eq!(
            result.execution_state,
            ShellCommandExecutionState::NotStarted,
            "{result:?}"
        );
        assert!(
            result
                .result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh_remote_cwd_invalid")),
            "{result:?}"
        );
    }
}
