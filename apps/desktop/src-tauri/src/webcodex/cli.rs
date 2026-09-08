use crate::deadline::Deadline;
use crate::error::{DesktopError, DesktopResult};
use crate::models::BinaryInfo;
use crate::operation::{cancelled_error, CancellationContext};
use crate::platform;
use serde::de::DeserializeOwned;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use webcodex_process::{GracefulTermination, ManagedChild};

const CLI_OUTPUT_BYTES: usize = 256 * 1024;
const CLI_INPUT_BYTES: usize = 64 * 1024;
const CLI_TIMEOUT: Duration = Duration::from_secs(30);
const CLI_CLEANUP_SLACK: Duration = Duration::from_secs(2);
const CLI_POLL_INTERVAL: Duration = Duration::from_millis(20);
const CLI_GRACEFUL_CLEANUP: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedBinarySource {
    Bundled,
    Environment,
    SourceDogfoodTarget,
}

impl ResolvedBinarySource {
    fn label(self) -> &'static str {
        match self {
            Self::Bundled => "Bundled",
            Self::Environment => "WEBCODEX_DESKTOP_BIN_DIR",
            Self::SourceDogfoodTarget => "source target/dogfood",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedBinaries {
    pub directory: PathBuf,
    pub webcodex: PathBuf,
    pub server: PathBuf,
    pub runner: PathBuf,
    pub version: String,
    pub git_commit: String,
    pub source: ResolvedBinarySource,
}

impl ResolvedBinaries {
    pub async fn resolve(
        bundled_runtime_dir: Option<&Path>,
        cancellation: &CancellationContext,
    ) -> DesktopResult<Self> {
        Self::resolve_until(
            bundled_runtime_dir,
            cancellation,
            Deadline::after(CLI_TIMEOUT),
        )
        .await
    }

    pub async fn resolve_until(
        bundled_runtime_dir: Option<&Path>,
        cancellation: &CancellationContext,
        deadline: Deadline,
    ) -> DesktopResult<Self> {
        cancellation.check()?;
        if deadline.is_elapsed() {
            return Err(timeout_error());
        }
        let (directory, source) =
            if let Some(directory) = bundled_runtime_dir.filter(|path| path.is_dir()) {
                (directory.to_path_buf(), ResolvedBinarySource::Bundled)
            } else if !cfg!(debug_assertions) {
                let expected = bundled_runtime_dir
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<Desktop resource directory>/webcodex-runtime".to_string());
                return Err(DesktopError::new(
                    "bundled_runtime_missing",
                    format!("The installed WebCodex runtime is missing: {expected}"),
                    "Reinstall WebCodex Desktop from the matching release installer.",
                ));
            } else if let Some(value) = std::env::var_os("WEBCODEX_DESKTOP_BIN_DIR") {
                let directory = PathBuf::from(value);
                if directory.as_os_str().is_empty() {
                    return Err(DesktopError::new(
                        "binary_directory_invalid",
                        "WEBCODEX_DESKTOP_BIN_DIR is empty",
                        "Set it to the directory containing the source-matched WebCodex binaries.",
                    ));
                }
                (directory, ResolvedBinarySource::Environment)
            } else {
                let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                let repo = manifest
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .ok_or_else(|| {
                        DesktopError::new(
                            "binary_directory_invalid",
                            "Could not derive the WebCodex source root",
                            "Set WEBCODEX_DESKTOP_BIN_DIR explicitly.",
                        )
                    })?;
                (
                    repo.join("target").join("dogfood"),
                    ResolvedBinarySource::SourceDogfoodTarget,
                )
            };
        let missing_directory_action = match source {
            ResolvedBinarySource::Bundled => {
                "Reinstall WebCodex Desktop from the matching release installer."
            }
            ResolvedBinarySource::Environment | ResolvedBinarySource::SourceDogfoodTarget => {
                "Build `cargo build --profile dogfood -p webcodex -p webcodex-cli -p webcodex-runner` from this source baseline or set WEBCODEX_DESKTOP_BIN_DIR."
            }
        };
        let directory = directory.canonicalize().map_err(|_| {
            DesktopError::new(
                "binary_directory_missing",
                format!(
                    "WebCodex binary directory does not exist: {}",
                    directory.display()
                ),
                missing_directory_action,
            )
        })?;
        let webcodex = directory.join(executable_name("webcodex"));
        let server = directory.join(executable_name("webcodex-server"));
        let runner = directory.join(executable_name("webcodex-runner"));
        for path in [&webcodex, &server, &runner] {
            if !path.is_file() {
                return Err(DesktopError::new(
                    "binary_missing",
                    format!("Required WebCodex binary is missing: {}", path.display()),
                    match source {
                        ResolvedBinarySource::Bundled => {
                            "Reinstall WebCodex Desktop from the matching release installer."
                        }
                        ResolvedBinarySource::Environment
                        | ResolvedBinarySource::SourceDogfoodTarget => {
                            "Build all WebCodex dogfood binaries from the current source baseline."
                        }
                    },
                ));
            }
        }

        let cli_version = binary_version(&webcodex, cancellation, deadline).await?;
        let server_version = binary_version(&server, cancellation, deadline).await?;
        let runner_version = binary_version(&runner, cancellation, deadline).await?;
        if cli_version.version != server_version.version
            || cli_version.version != runner_version.version
            || cli_version.git_commit != server_version.git_commit
            || cli_version.git_commit != runner_version.git_commit
        {
            return Err(DesktopError::new(
                "binary_version_mismatch",
                "CLI, Server, and Runner were built from different baselines",
                "Rebuild all dogfood binaries from one WebCodex checkout.",
            ));
        }
        if cli_version.git_commit == "unknown" {
            return Err(DesktopError::new(
                "binary_version_unverifiable",
                "WebCodex binaries do not carry a source revision",
                "Rebuild dogfood binaries with normal WebCodex build metadata enabled.",
            ));
        }
        Ok(Self {
            directory,
            webcodex,
            server,
            runner,
            version: cli_version.version,
            git_commit: cli_version.git_commit,
            source,
        })
    }

    pub fn info(&self) -> BinaryInfo {
        BinaryInfo {
            directory: self.directory.to_string_lossy().to_string(),
            version: self.version.clone(),
            git_commit: self.git_commit.clone(),
            source: self.source.label().to_string(),
        }
    }
}

#[derive(Debug)]
struct VersionLine {
    version: String,
    git_commit: String,
}

async fn binary_version(
    path: &Path,
    cancellation: &CancellationContext,
    deadline: Deadline,
) -> DesktopResult<VersionLine> {
    let output = run_bounded_until(
        path,
        &["--version".to_string()],
        None,
        false,
        cancellation,
        deadline,
    )
    .await?;
    if output.exit_code != Some(0) {
        return Err(DesktopError::new(
            "binary_probe_failed",
            format!("Could not read build identity from {}", path.display()),
            "Rebuild the WebCodex dogfood binaries.",
        ));
    }
    parse_version_line(&output.stdout).ok_or_else(|| {
        DesktopError::new(
            "binary_probe_failed",
            format!("{} returned an invalid version identity", path.display()),
            "Rebuild the WebCodex dogfood binaries from a compatible source baseline.",
        )
    })
}

fn parse_version_line(output: &[u8]) -> Option<VersionLine> {
    let text = std::str::from_utf8(output).ok()?.trim();
    let mut fields = text.split_whitespace();
    fields.next()?;
    let version = fields.next()?.to_string();
    let commit_marker = text.find("(commit ")? + "(commit ".len();
    let tail = &text[commit_marker..];
    let end = tail.find([',', ')']).unwrap_or(tail.len());
    let git_commit = tail[..end].trim().to_string();
    if version.is_empty() || git_commit.is_empty() {
        return None;
    }
    Some(VersionLine {
        version,
        git_commit,
    })
}

pub async fn run_json<T: DeserializeOwned>(
    executable: &Path,
    args: &[String],
    stdin: Option<&[u8]>,
    secret_output: bool,
    cancellation: &CancellationContext,
) -> DesktopResult<T> {
    run_json_until(
        executable,
        args,
        stdin,
        secret_output,
        cancellation,
        Deadline::after(CLI_TIMEOUT),
    )
    .await
}

pub async fn run_json_until<T: DeserializeOwned>(
    executable: &Path,
    args: &[String],
    stdin: Option<&[u8]>,
    secret_output: bool,
    cancellation: &CancellationContext,
    deadline: Deadline,
) -> DesktopResult<T> {
    let output = run_bounded_until(
        executable,
        args,
        stdin,
        secret_output,
        cancellation,
        deadline,
    )
    .await?;
    if output.exit_code != Some(0) {
        return Err(DesktopError::new(
            "webcodex_command_failed",
            "WebCodex did not complete the requested operation",
            "Open Activity for safe diagnostics, correct the configuration, and retry.",
        )
        .with_details(serde_json::json!({ "exit_code": output.exit_code })));
    }
    serde_json::from_slice(&output.stdout).map_err(|_| {
        DesktopError::new(
            "webcodex_contract_invalid",
            "WebCodex returned invalid machine-readable output",
            "Verify that Desktop and WebCodex binaries come from the same source baseline.",
        )
    })
}

#[derive(Debug)]
struct BoundedOutput {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    #[allow(dead_code)]
    stderr: Vec<u8>,
}

#[cfg(test)]
async fn run_bounded_with_timeout(
    executable: &Path,
    args: &[String],
    stdin_payload: Option<&[u8]>,
    _secret_output: bool,
    cancellation: &CancellationContext,
    timeout: Duration,
) -> DesktopResult<BoundedOutput> {
    run_bounded_until(
        executable,
        args,
        stdin_payload,
        _secret_output,
        cancellation,
        Deadline::after(timeout),
    )
    .await
}

async fn run_bounded_until(
    executable: &Path,
    args: &[String],
    stdin_payload: Option<&[u8]>,
    _secret_output: bool,
    cancellation: &CancellationContext,
    deadline: Deadline,
) -> DesktopResult<BoundedOutput> {
    cancellation.check()?;
    if deadline.is_elapsed() {
        return Err(timeout_error());
    }
    if stdin_payload.is_some_and(|payload| payload.len() > CLI_INPUT_BYTES) {
        return Err(DesktopError::new(
            "webcodex_command_input_failed",
            "Protected WebCodex command input exceeds the Desktop safety limit",
            "Retry with a valid bounded Desktop input.",
        ));
    }

    let mut command = bounded_command(executable, args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin_payload.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child =
        ManagedChild::spawn_with_options(&mut command, platform::managed_spawn_options(false))
            .map_err(|error| {
                DesktopError::new(
                    "webcodex_command_start_failed",
                    "Could not start a safely owned WebCodex command",
                    "Check the Desktop binary directory and execution permissions.",
                )
                .with_details(serde_json::json!({ "io_kind": format!("{:?}", error.kind()) }))
            })?;

    let stdout = match child.child_mut().stdout.take() {
        Some(stdout) => stdout,
        None => {
            cleanup_child_only(&mut child, deadline).await;
            return Err(DesktopError::new(
                "webcodex_command_start_failed",
                "Could not capture WebCodex output",
                "Retry the operation.",
            ));
        }
    };
    let stderr = match child.child_mut().stderr.take() {
        Some(stderr) => stderr,
        None => {
            cleanup_child_only(&mut child, deadline).await;
            return Err(DesktopError::new(
                "webcodex_command_start_failed",
                "Could not capture WebCodex diagnostics",
                "Retry the operation.",
            ));
        }
    };
    let mut stdout_task = Some(tokio::task::spawn_blocking(move || read_bounded(stdout)));
    let mut stderr_task = Some(tokio::task::spawn_blocking(move || read_bounded(stderr)));
    let mut stdin_task = None;

    if let Some(payload) = stdin_payload {
        let Some(stdin) = child.child_mut().stdin.take() else {
            cleanup_command(
                &mut child,
                &mut stdin_task,
                &mut stdout_task,
                &mut stderr_task,
                deadline,
            )
            .await;
            return Err(DesktopError::new(
                "webcodex_command_input_failed",
                "Could not open protected input for the WebCodex command",
                "Retry the operation.",
            ));
        };
        let payload = payload.to_vec();
        stdin_task = Some(tokio::task::spawn_blocking(move || {
            write_and_close_stdin(stdin, payload)
        }));
        match await_stdin_writer(&mut stdin_task, deadline.instant(), cancellation).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                cleanup_command(
                    &mut child,
                    &mut stdin_task,
                    &mut stdout_task,
                    &mut stderr_task,
                    deadline,
                )
                .await;
                return Err(DesktopError::new(
                    "webcodex_command_input_failed",
                    "Could not pass protected input to WebCodex",
                    "Retry the operation.",
                ));
            }
            Err(WaitInterruption::Timeout) => {
                cleanup_command(
                    &mut child,
                    &mut stdin_task,
                    &mut stdout_task,
                    &mut stderr_task,
                    deadline,
                )
                .await;
                return Err(timeout_error());
            }
            Err(WaitInterruption::Cancelled) => {
                cleanup_command(
                    &mut child,
                    &mut stdin_task,
                    &mut stdout_task,
                    &mut stderr_task,
                    deadline,
                )
                .await;
                return Err(cancelled_error());
            }
        }
    }

    let status = match wait_for_direct_child(&mut child, deadline.instant(), cancellation).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            cleanup_command(
                &mut child,
                &mut stdin_task,
                &mut stdout_task,
                &mut stderr_task,
                deadline,
            )
            .await;
            return Err(DesktopError::new(
                "webcodex_command_wait_failed",
                "Could not observe the WebCodex command result",
                "Retry the operation.",
            ));
        }
        Err(WaitInterruption::Timeout) => {
            cleanup_command(
                &mut child,
                &mut stdin_task,
                &mut stdout_task,
                &mut stderr_task,
                deadline,
            )
            .await;
            return Err(timeout_error());
        }
        Err(WaitInterruption::Cancelled) => {
            cleanup_command(
                &mut child,
                &mut stdin_task,
                &mut stdout_task,
                &mut stderr_task,
                deadline,
            )
            .await;
            return Err(cancelled_error());
        }
    };

    let stdout = match await_reader(&mut stdout_task, deadline.instant(), cancellation).await {
        Ok(output) => output,
        Err(interruption) => {
            cleanup_command(
                &mut child,
                &mut stdin_task,
                &mut stdout_task,
                &mut stderr_task,
                deadline,
            )
            .await;
            return Err(interruption.into_error());
        }
    };
    let stderr = match await_reader(&mut stderr_task, deadline.instant(), cancellation).await {
        Ok(output) => output,
        Err(interruption) => {
            cleanup_command(
                &mut child,
                &mut stdin_task,
                &mut stdout_task,
                &mut stderr_task,
                deadline,
            )
            .await;
            return Err(interruption.into_error());
        }
    };

    match wait_for_tree_exit(&child, deadline.instant(), cancellation).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            cleanup_command(
                &mut child,
                &mut stdin_task,
                &mut stdout_task,
                &mut stderr_task,
                deadline,
            )
            .await;
            return Err(DesktopError::new(
                "webcodex_command_wait_failed",
                "Could not verify that the Desktop-owned WebCodex process tree exited",
                "Retry the operation after checking Activity.",
            ));
        }
        Err(WaitInterruption::Timeout) => {
            cleanup_command(
                &mut child,
                &mut stdin_task,
                &mut stdout_task,
                &mut stderr_task,
                deadline,
            )
            .await;
            return Err(timeout_error());
        }
        Err(WaitInterruption::Cancelled) => {
            cleanup_command(
                &mut child,
                &mut stdin_task,
                &mut stdout_task,
                &mut stderr_task,
                deadline,
            )
            .await;
            return Err(cancelled_error());
        }
    }

    Ok(BoundedOutput {
        exit_code: status.code(),
        stdout,
        stderr,
    })
}

fn bounded_command(executable: &Path, args: &[String]) -> Command {
    #[cfg(debug_assertions)]
    if std::env::var_os("WEBCODEX_DESKTOP_STUCK_OPERATION_FIXTURE").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        #[cfg(target_os = "windows")]
        {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$child = Start-Process powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 120' -PassThru; Wait-Process -Id $child.Id",
            ]);
            return command;
        }
        #[cfg(not(target_os = "windows"))]
        {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "sleep 120 & descendant=$!; wait \"$descendant\""]);
            return command;
        }
    }

    let mut command = Command::new(executable);
    command.args(args);
    command
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitInterruption {
    Timeout,
    Cancelled,
}

impl WaitInterruption {
    fn into_error(self) -> DesktopError {
        match self {
            Self::Timeout => timeout_error(),
            Self::Cancelled => cancelled_error(),
        }
    }
}

async fn wait_for_direct_child(
    child: &mut ManagedChild,
    deadline: Instant,
    cancellation: &CancellationContext,
) -> Result<std::io::Result<ExitStatus>, WaitInterruption> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Ok(status)),
            Ok(None) => {}
            Err(error) => return Ok(Err(error)),
        }
        wait_for_poll(deadline, cancellation).await?;
    }
}

async fn wait_for_tree_exit(
    child: &ManagedChild,
    deadline: Instant,
    cancellation: &CancellationContext,
) -> Result<std::io::Result<()>, WaitInterruption> {
    loop {
        match child.try_tree_exit() {
            Ok(true) => return Ok(Ok(())),
            Ok(false) => {}
            Err(error) => return Ok(Err(error)),
        }
        wait_for_poll(deadline, cancellation).await?;
    }
}

async fn wait_for_poll(
    deadline: Instant,
    cancellation: &CancellationContext,
) -> Result<(), WaitInterruption> {
    if cancellation.is_cancelled() {
        return Err(WaitInterruption::Cancelled);
    }
    let now = Instant::now();
    if now >= deadline {
        return Err(WaitInterruption::Timeout);
    }
    let next_poll = std::cmp::min(deadline, now + CLI_POLL_INTERVAL);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(WaitInterruption::Cancelled),
        _ = tokio::time::sleep_until(next_poll) => Ok(()),
    }
}

async fn await_stdin_writer(
    task: &mut Option<JoinHandle<std::io::Result<()>>>,
    deadline: Instant,
    cancellation: &CancellationContext,
) -> Result<std::io::Result<()>, WaitInterruption> {
    let Some(handle) = task.as_mut() else {
        return Ok(Ok(()));
    };
    if cancellation.is_cancelled() {
        return Err(WaitInterruption::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(WaitInterruption::Timeout);
    }
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(WaitInterruption::Cancelled),
        _ = tokio::time::sleep_until(deadline) => return Err(WaitInterruption::Timeout),
        result = handle => result,
    };
    task.take();
    Ok(result.unwrap_or_else(|error| Err(std::io::Error::other(error.to_string()))))
}

async fn await_reader(
    task: &mut Option<JoinHandle<Vec<u8>>>,
    deadline: Instant,
    cancellation: &CancellationContext,
) -> Result<Vec<u8>, WaitInterruption> {
    let Some(handle) = task.as_mut() else {
        return Ok(Vec::new());
    };
    if cancellation.is_cancelled() {
        return Err(WaitInterruption::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(WaitInterruption::Timeout);
    }
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(WaitInterruption::Cancelled),
        _ = tokio::time::sleep_until(deadline) => return Err(WaitInterruption::Timeout),
        result = handle => result,
    };
    task.take();
    Ok(result.unwrap_or_default())
}

async fn cleanup_child_only(child: &mut ManagedChild, operation_deadline: Deadline) {
    terminate_managed_tree(
        child,
        operation_deadline.cleanup_deadline(CLI_CLEANUP_SLACK),
    )
    .await;
}

async fn cleanup_command(
    child: &mut ManagedChild,
    stdin_task: &mut Option<JoinHandle<std::io::Result<()>>>,
    stdout_task: &mut Option<JoinHandle<Vec<u8>>>,
    stderr_task: &mut Option<JoinHandle<Vec<u8>>>,
    operation_deadline: Deadline,
) {
    let deadline = operation_deadline.cleanup_deadline(CLI_CLEANUP_SLACK);
    terminate_managed_tree(child, deadline).await;
    finish_task(stdin_task, deadline).await;
    finish_task(stdout_task, deadline).await;
    finish_task(stderr_task, deadline).await;
}

async fn terminate_managed_tree(child: &mut ManagedChild, deadline: Instant) {
    if matches!(
        child.request_terminate_tree(),
        Ok(GracefulTermination::Requested)
    ) {
        let now = Instant::now();
        if now < deadline {
            let graceful_deadline = std::cmp::min(deadline, now + CLI_GRACEFUL_CLEANUP);
            let _ = wait_for_tree_exit_during_cleanup(child, graceful_deadline).await;
        }
    }
    if !child.try_tree_exit().unwrap_or(false) {
        let _ = child.terminate_tree();
        let _ = wait_for_tree_exit_during_cleanup(child, deadline).await;
    }
    let _ = child.try_wait();
}

async fn wait_for_tree_exit_during_cleanup(child: &ManagedChild, deadline: Instant) -> bool {
    loop {
        if child.try_tree_exit().unwrap_or(false) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep_until(std::cmp::min(deadline, now + CLI_POLL_INTERVAL)).await;
    }
}

async fn finish_task<T>(task: &mut Option<JoinHandle<T>>, deadline: Instant) {
    if let Some(mut task) = task.take() {
        task.abort();
        if Instant::now() < deadline {
            let _ = tokio::time::timeout_at(deadline, &mut task).await;
        }
    }
}

fn timeout_error() -> DesktopError {
    DesktopError::new(
        "webcodex_command_timeout",
        "WebCodex did not finish within the Desktop command timeout",
        "Check Server reachability and retry.",
    )
}

#[cfg(test)]
pub(crate) async fn run_test_bounded(
    executable: &Path,
    args: &[String],
    stdin_payload: Option<&[u8]>,
    cancellation: &CancellationContext,
    timeout: Duration,
) -> DesktopResult<()> {
    run_bounded_with_timeout(
        executable,
        args,
        stdin_payload,
        false,
        cancellation,
        timeout,
    )
    .await
    .map(|_| ())
}

fn write_and_close_stdin(
    mut stdin: std::process::ChildStdin,
    payload: Vec<u8>,
) -> std::io::Result<()> {
    stdin.write_all(&payload)?;
    stdin.flush()?;
    drop(stdin);
    Ok(())
}

fn read_bounded<R: Read>(mut reader: R) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        if output.len() < CLI_OUTPUT_BYTES {
            let remaining = CLI_OUTPUT_BYTES - output.len();
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
    }
    output
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{CancellationContext, CancellationSignal};

    #[test]
    fn version_parser_requires_commit_identity() {
        let parsed =
            parse_version_line(b"webcodex 0.3.9 (commit 0123456789abcdef, dirty=false)\n").unwrap();
        assert_eq!(parsed.version, "0.3.9");
        assert_eq!(parsed.git_commit, "0123456789abcdef");
        assert!(parse_version_line(b"webcodex 0.3.9\n").is_none());
    }

    #[test]
    fn bundled_source_label_is_stable() {
        assert_eq!(ResolvedBinarySource::Bundled.label(), "Bundled");
    }

    #[cfg(unix)]
    fn process_can_execute(pid: u32) -> bool {
        // Match ManagedChild's tree-liveness contract: an unreaped zombie still
        // occupies a PID, but it cannot execute code and must not make cleanup
        // look incomplete. Keep unknown probe failures conservative.
        #[cfg(target_os = "linux")]
        let raw_pid = pid;
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if unsafe { libc::kill(pid, 0) } != 0 {
            return !matches!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        #[cfg(target_os = "linux")]
        {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{raw_pid}/stat")) else {
                return true;
            };
            let Some((_, rest)) = stat.rsplit_once(')') else {
                return true;
            };
            let state = rest.split_whitespace().next().unwrap_or("");
            state != "Z" && state != "X"
        }
        #[cfg(target_os = "macos")]
        {
            let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>();
            let bytes = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size as libc::c_int,
                )
            };
            if bytes == size as libc::c_int {
                return unsafe { info.assume_init() }.pbi_status != libc::SZOMB;
            }
            if bytes == 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return false;
            }
            true
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            true
        }
    }

    #[cfg(unix)]
    fn unique_marker(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "webcodex-cli-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[cfg(unix)]
    fn read_fixture_pids(marker: &Path) -> Vec<u32> {
        std::fs::read_to_string(marker)
            .expect("fixture must publish owned pids")
            .split_whitespace()
            .map(|value| value.parse().expect("fixture pid"))
            .collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blocked_stdin_uses_one_total_deadline_and_reclaims_owned_tree() {
        let marker = unique_marker("blocked-stdin");
        let args = vec![
            "-c".to_string(),
            "sleep 8 & descendant=$!; printf '%s %s\\n' \"$$\" \"$descendant\" > \"$1\"; wait \"$descendant\"".to_string(),
            "webcodex-blocked-stdin".to_string(),
            marker.to_string_lossy().to_string(),
        ];
        let payload = vec![b'x'; CLI_INPUT_BYTES];
        let cancellation = CancellationContext::never();
        let started = Instant::now();
        let error = run_bounded_with_timeout(
            Path::new("/bin/sh"),
            &args,
            Some(&payload),
            false,
            &cancellation,
            Duration::from_millis(350),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "webcodex_command_timeout");
        assert!(started.elapsed() < Duration::from_secs(4));
        for pid in read_fixture_pids(&marker) {
            assert!(
                !process_can_execute(pid),
                "owned PID {pid} survived timeout cleanup"
            );
        }
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inherited_output_pipe_cannot_extend_the_command_lifetime() {
        let marker = unique_marker("inherited-pipe");
        let args = vec![
            "-c".to_string(),
            "sleep 8 & descendant=$!; printf '%s\\n' \"$descendant\" > \"$1\"; exit 0".to_string(),
            "webcodex-inherited-pipe".to_string(),
            marker.to_string_lossy().to_string(),
        ];
        let cancellation = CancellationContext::never();
        let started = Instant::now();
        let error = run_bounded_with_timeout(
            Path::new("/bin/sh"),
            &args,
            None,
            false,
            &cancellation,
            Duration::from_millis(350),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "webcodex_command_timeout");
        assert!(started.elapsed() < Duration::from_secs(4));
        for pid in read_fixture_pids(&marker) {
            assert!(
                !process_can_execute(pid),
                "pipe-holding PID {pid} survived cleanup"
            );
        }
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_cancel_interrupts_blocked_stdin_and_keeps_cancel_classification() {
        let marker = unique_marker("cancelled-stdin");
        let args = vec![
            "-c".to_string(),
            "sleep 8 & descendant=$!; printf '%s %s\\n' \"$$\" \"$descendant\" > \"$1\"; wait \"$descendant\"".to_string(),
            "webcodex-cancelled-stdin".to_string(),
            marker.to_string_lossy().to_string(),
        ];
        let operation = CancellationSignal::new();
        let cancellation = CancellationContext::new(operation.clone(), CancellationSignal::new());
        let payload = vec![b'x'; CLI_INPUT_BYTES];
        let command = tokio::spawn(async move {
            run_bounded_with_timeout(
                Path::new("/bin/sh"),
                &args,
                Some(&payload),
                false,
                &cancellation,
                Duration::from_secs(8),
            )
            .await
        });
        let marker_deadline = Instant::now() + Duration::from_secs(2);
        while !marker.is_file() {
            assert!(Instant::now() < marker_deadline, "fixture did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        operation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(4), command)
            .await
            .expect("cancelled command must return promptly")
            .expect("fixture task")
            .unwrap_err();
        assert_eq!(error.code, "desktop_operation_cancelled");
        for pid in read_fixture_pids(&marker) {
            assert!(
                !process_can_execute(pid),
                "owned PID {pid} survived cancellation"
            );
        }
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn absolute_deadline_is_not_reset_by_nested_cli_work() {
        let cancellation = CancellationContext::never();
        let outer_started = Instant::now();
        let deadline = Deadline::after(Duration::from_millis(350));
        tokio::time::sleep(Duration::from_millis(150)).await;
        let error = run_bounded_until(
            Path::new("/bin/sh"),
            &["-c".to_string(), "sleep 8".to_string()],
            None,
            false,
            &cancellation,
            deadline,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "webcodex_command_timeout");
        assert!(
            outer_started.elapsed() < Duration::from_secs(3),
            "nested CLI work must consume the original deadline plus only bounded cleanup slack"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn elapsed_deadline_does_not_start_a_new_cli_command() {
        let marker = unique_marker("expired-before-spawn");
        let cancellation = CancellationContext::never();
        let deadline = Deadline::after(Duration::from_millis(20));
        tokio::time::sleep(Duration::from_millis(30)).await;
        let args = vec![
            "-c".to_string(),
            "printf started > \"$1\"".to_string(),
            "webcodex-expired-deadline".to_string(),
            marker.to_string_lossy().to_string(),
        ];
        let error = run_bounded_until(
            Path::new("/bin/sh"),
            &args,
            None,
            false,
            &cancellation,
            deadline,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "webcodex_command_timeout");
        assert!(!marker.exists(), "expired deadline must prevent spawn");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_during_output_drain_reclaims_the_owned_tree() {
        let marker = unique_marker("cancel-output-drain");
        let args = vec![
            "-c".to_string(),
            "sleep 8 & descendant=$!; printf '%s\\n' \"$descendant\" > \"$1\"; exit 0".to_string(),
            "webcodex-cancel-output-drain".to_string(),
            marker.to_string_lossy().to_string(),
        ];
        let operation = CancellationSignal::new();
        let cancellation = CancellationContext::new(operation.clone(), CancellationSignal::new());
        let command = tokio::spawn(async move {
            run_bounded_with_timeout(
                Path::new("/bin/sh"),
                &args,
                None,
                false,
                &cancellation,
                Duration::from_secs(8),
            )
            .await
        });
        let marker_deadline = Instant::now() + Duration::from_secs(2);
        while !marker.is_file() {
            assert!(Instant::now() < marker_deadline, "fixture did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        operation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(4), command)
            .await
            .expect("cancelled drain must return promptly")
            .expect("fixture task")
            .unwrap_err();
        assert_eq!(error.code, "desktop_operation_cancelled");
        for pid in read_fixture_pids(&marker) {
            assert!(
                !process_can_execute(pid),
                "pipe-holding PID {pid} survived output-drain cancellation"
            );
        }
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(target_os = "windows")]
    fn windows_process_exists(pid: u32) -> bool {
        let filter = format!("PID eq {pid}");
        let Ok(output) = std::process::Command::new("tasklist.exe")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_blocked_stdin_reclaims_the_owned_process_tree() {
        let marker = std::env::temp_dir().join(format!(
            "webcodex-cli-blocked-stdin-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let escaped_marker = marker.to_string_lossy().replace('\'', "''");
        let script = format!(
            "$child = Start-Process ping.exe -ArgumentList '-n','31','127.0.0.1' -PassThru; Set-Content -LiteralPath '{escaped_marker}' -Value \"$PID $($child.Id)\" -NoNewline; Start-Sleep -Seconds 30"
        );
        let args = vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script,
        ];
        let payload = vec![b'x'; CLI_INPUT_BYTES];
        let started = Instant::now();
        let command = tokio::spawn(async move {
            let cancellation = CancellationContext::never();
            run_bounded_with_timeout(
                Path::new("powershell.exe"),
                &args,
                Some(&payload),
                false,
                &cancellation,
                Duration::from_secs(8),
            )
            .await
        });
        let marker_deadline = Instant::now() + Duration::from_secs(6);
        let pids = loop {
            if let Ok(contents) = std::fs::read_to_string(&marker) {
                let parsed = contents
                    .split_whitespace()
                    .map(str::parse::<u32>)
                    .collect::<Result<Vec<_>, _>>();
                if let Ok(pids) = parsed {
                    if pids.len() == 2 {
                        break pids;
                    }
                }
            }
            assert!(
                Instant::now() < marker_deadline,
                "blocked-stdin fixture must publish owned pids before timeout"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let error = tokio::time::timeout(Duration::from_secs(12), command)
            .await
            .expect("blocked-stdin command must finish within its bounded cleanup")
            .expect("blocked-stdin fixture task")
            .unwrap_err();
        assert_eq!(error.code, "webcodex_command_timeout");
        assert!(started.elapsed() < Duration::from_secs(12));
        for pid in pids {
            assert!(
                !windows_process_exists(pid),
                "owned PID {pid} survived timeout cleanup"
            );
        }
        let _ = std::fs::remove_file(marker);
    }
}
