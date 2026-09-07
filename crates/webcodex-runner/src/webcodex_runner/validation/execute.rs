//! Bounded process execution for validation adapters.

use crate::validation_bridge::{
    sanitize_bridge_text, MAX_VALIDATION_STDERR_CAPTURE_BYTES, MAX_VALIDATION_STDERR_SUMMARY_CHARS,
    MAX_VALIDATION_STDOUT_BYTES,
};
use crate::webcodex_runner::output_text::{normalize_output_text, OutputTextSource};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use webcodex_process::{GracefulTermination, ManagedChild};

#[derive(Debug)]
pub(crate) struct CapturedProcess {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stdout_capped: bool,
    pub(crate) stderr_capped: bool,
    pub(crate) stderr_summary: Option<String>,
    pub(crate) duration_ms: u64,
    pub(crate) timed_out: bool,
    pub(crate) spawn_error: Option<String>,
    pub(crate) wait_error: Option<String>,
}

/// Run argv with bounded stdout capture. When stdout exceeds the hard byte cap,
/// `stdout_capped` is true and `stdout` is empty (complete JSON only — never a
/// truncated body intended for parsing).
pub(crate) fn run_bounded(
    program: &Path,
    args: &[String],
    cwd: &Path,
    timeout_secs: u64,
    shutdown: Option<&AtomicBool>,
) -> CapturedProcess {
    let start = Instant::now();
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("PYTHONSTARTUP")
        .env_remove("PYTHONPATH");

    // ManagedChild owns the whole validation process tree: a private process
    // group on Unix, a kill-on-close Job Object on Windows.
    let mut child = match ManagedChild::spawn(&mut command) {
        Ok(child) => child,
        Err(error) => {
            return CapturedProcess {
                exit_code: None,
                stdout: Vec::new(),
                stdout_capped: false,
                stderr_capped: false,
                stderr_summary: Some(bound_stderr(&format!("spawn failed: {error}"))),
                duration_ms: start.elapsed().as_millis() as u64,
                timed_out: false,
                spawn_error: Some(format!("spawn failed: {error}")),
                wait_error: None,
            };
        }
    };

    let stdout = child.child_mut().stdout.take();
    let stderr = child.child_mut().stderr.take();

    let (stdout_tx, stdout_rx) = mpsc::channel::<(Vec<u8>, bool)>();
    if let Some(mut out) = stdout {
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut collected = Vec::new();
            let mut capped = false;
            loop {
                match out.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if collected.len() + n > MAX_VALIDATION_STDOUT_BYTES {
                            capped = true;
                            let mut discard = [0u8; 8192];
                            while let Ok(m) = out.read(&mut discard) {
                                if m == 0 {
                                    break;
                                }
                            }
                            break;
                        }
                        collected.extend_from_slice(&buf[..n]);
                    }
                    Err(_) => break,
                }
            }
            let _ = stdout_tx.send((if capped { Vec::new() } else { collected }, capped));
        });
    } else {
        let _ = stdout_tx.send((Vec::new(), false));
    }

    let (stderr_tx, stderr_rx) = mpsc::channel::<(Vec<u8>, bool)>();
    if let Some(mut err) = stderr {
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut collected = Vec::with_capacity(MAX_VALIDATION_STDERR_CAPTURE_BYTES);
            let mut capped = false;
            loop {
                match err.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let remaining =
                            MAX_VALIDATION_STDERR_CAPTURE_BYTES.saturating_sub(collected.len());
                        let retained = remaining.min(n);
                        collected.extend_from_slice(&buf[..retained]);
                        if retained < n {
                            capped = true;
                        }
                        // Keep draining after the cap so the child cannot block
                        // on a full stderr pipe. No further bytes are retained.
                    }
                    Err(_) => break,
                }
            }
            let _ = stderr_tx.send((collected, capped));
        });
    } else {
        let _ = stderr_tx.send((Vec::new(), false));
    }

    let timeout = Duration::from_secs(timeout_secs.max(1));
    let mut wait_error = None;
    let mut exit_status = None;
    let mut stopped = false;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break false;
            }
            Ok(None) => {
                if shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                    stopped = true;
                    break false;
                }
                if start.elapsed() >= timeout {
                    break true;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                wait_error = Some(format!("wait failed: {error}"));
                break false;
            }
        }
    };

    let cleanup_deadline = Instant::now() + Duration::from_secs(1);
    match terminate_validation_child(&mut child, cleanup_deadline) {
        Ok(status) => {
            if exit_status.is_none() {
                exit_status = status;
            }
        }
        Err(error) => {
            if wait_error.is_none() {
                wait_error = Some(error);
            }
        }
    }
    if stopped && wait_error.is_none() {
        wait_error = Some("validation stopped during runner shutdown".to_string());
    }

    let exit_code = if timed_out {
        Some(-1)
    } else if wait_error.is_some() {
        None
    } else {
        exit_status.and_then(|status| status.code())
    };

    // Drain readers against one shared cleanup deadline rather than giving
    // stdout and stderr independent five-second waits.
    let remaining = cleanup_deadline.saturating_duration_since(Instant::now());
    let (stdout_bytes, stdout_capped) = stdout_rx
        .recv_timeout(remaining)
        .unwrap_or((Vec::new(), false));
    let remaining = cleanup_deadline.saturating_duration_since(Instant::now());
    let (stderr_bytes, stderr_capped) = stderr_rx
        .recv_timeout(remaining)
        .unwrap_or((Vec::new(), false));
    // Validation stdout remains the exact bounded byte buffer consumed by
    // structured diagnostic parsers. Only the human/model-facing stderr
    // summary crosses the local process-text normalization boundary.
    let stderr_text = normalize_output_text(
        &stderr_bytes,
        stderr_capped,
        MAX_VALIDATION_STDERR_CAPTURE_BYTES,
        OutputTextSource::LocalProcess,
    );

    if timed_out {
        return CapturedProcess {
            exit_code,
            stdout: Vec::new(),
            stdout_capped: false,
            stderr_capped,
            stderr_summary: Some(bound_stderr(&format!(
                "command timed out after {timeout_secs} seconds"
            ))),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: true,
            spawn_error: None,
            wait_error: None,
        };
    }

    if let Some(error) = wait_error {
        return CapturedProcess {
            exit_code: None,
            stdout: stdout_bytes,
            stdout_capped,
            stderr_capped,
            stderr_summary: Some(bound_stderr(&error)),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: false,
            spawn_error: None,
            wait_error: Some(error),
        };
    }

    CapturedProcess {
        exit_code,
        stdout: stdout_bytes,
        stdout_capped,
        stderr_capped,
        stderr_summary: if stderr_text.trim().is_empty() {
            None
        } else {
            Some(bound_stderr(&stderr_text))
        },
        duration_ms: start.elapsed().as_millis() as u64,
        timed_out: false,
        spawn_error: None,
        wait_error: None,
    }
}

/// Terminate the whole validation process tree within one shared deadline.
///
/// The direct child and its descendants are owned together by the
/// [`ManagedChild`]: a private process group on Unix, a kill-on-close Job
/// Object on Windows. Cleanup never touches a pid/pgid directly.
///
/// Phase 1 (Unix only) requests graceful tree termination and gives the tree a
/// bounded 100ms grace to exit on its own. Windows reports
/// [`GracefulTermination::Unsupported`] and skips straight to phase 2. Phase 2
/// forcefully terminates any tree that is still alive. Then the direct child
/// is reaped and the complete tree (not just the direct child) is confirmed
/// exited — all within `deadline`. The direct child's `ExitStatus`, when it
/// can still be obtained, is returned; failures are joined into one error
/// string, but cleanup never gives up early because a graceful request failed.
fn terminate_validation_child(
    child: &mut ManagedChild,
    deadline: Instant,
) -> Result<Option<ExitStatus>, String> {
    let mut errors = Vec::new();

    match child.request_terminate_tree() {
        Ok(GracefulTermination::Requested) => {
            // The whole tree received a graceful termination request. Give it
            // a bounded grace to exit on its own; the grace never extends past
            // the overall cleanup deadline.
            let grace_deadline = deadline.min(Instant::now() + Duration::from_millis(100));
            let remaining = grace_deadline.saturating_duration_since(Instant::now());
            match child.wait_tree_exit(remaining) {
                Ok(true) => {}
                Ok(false) => {} // grace expired, tree still alive: escalate
                Err(error) => {
                    errors.push(format!(
                        "validation graceful termination wait failed: {error}"
                    ));
                }
            }
        }
        Ok(GracefulTermination::AlreadyExited) => {
            // The owned tree was already fully gone; nothing to signal or wait for.
        }
        Ok(GracefulTermination::Unsupported) => {
            // Windows: no generic graceful tree termination. Escalate below.
        }
        Err(error) => {
            errors.push(format!(
                "validation graceful termination request failed: {error}"
            ));
        }
    }

    // Forceful phase: any tree still alive is terminated as a whole.
    let tree_alive = match child.try_tree_exit() {
        Ok(exited) => !exited,
        Err(error) => {
            errors.push(format!("validation tree liveness probe failed: {error}"));
            true
        }
    };
    if tree_alive {
        if let Err(error) = child.terminate_tree() {
            errors.push(format!("validation tree termination failed: {error}"));
        }
    }

    // Reap the direct child within the remaining deadline.
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(exit_status)) => {
                status = Some(exit_status);
                break;
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    errors.push("validation child reap timed out".to_string());
                    break;
                }
                thread::sleep(Duration::from_millis(10).min(remaining));
            }
            Err(error) => {
                errors.push(format!("validation child reap failed: {error}"));
                break;
            }
        }
    }

    // Confirm the complete tree exited, not just the direct child. Forceful
    // termination can complete asynchronously (notably Job Object teardown on
    // Windows), so use the remaining shared cleanup budget rather than a
    // single instantaneous probe.
    let remaining = deadline.saturating_duration_since(Instant::now());
    match child.wait_tree_exit(remaining) {
        Ok(true) => {}
        Ok(false) => {
            errors.push("validation process tree did not exit before deadline".to_string())
        }
        Err(error) => errors.push(format!("validation tree exit wait failed: {error}")),
    }

    if errors.is_empty() {
        Ok(status)
    } else {
        Err(errors.join("; "))
    }
}

fn bound_stderr(text: &str) -> String {
    sanitize_bridge_text(text)
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(MAX_VALIDATION_STDERR_SUMMARY_CHARS)
        .collect()
}

/// Resolve an executable by env override then PATH search. Callers must not
/// expose the absolute executable path across the bridge.
pub(crate) fn resolve_executable(env_override: &str, executable_name: &str) -> Option<PathBuf> {
    if let Ok(value) = std::env::var(env_override) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            #[cfg(windows)]
            {
                // The override may be a bare name ("pyright"), a batch shim
                // ("pyright.cmd") or a path; resolve through the platform
                // rules so extensionless POSIX shims are never selected.
                let path_var = std::env::var_os("PATH").unwrap_or_default();
                return crate::webcodex_runner::util::resolve_program_in_path(trimmed, &path_var)
                    .map(|program| program.path().to_path_buf());
            }
            #[cfg(not(windows))]
            {
                let path = PathBuf::from(trimmed);
                return crate::webcodex_runner::util::is_executable_file(&path).then_some(path);
            }
        }
    }
    let path_var = std::env::var_os("PATH")?;
    crate::webcodex_runner::util::find_executable_in_path(executable_name, &path_var)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, OnceLock};
    use std::time::SystemTime;

    #[cfg(unix)]
    #[test]
    fn env_override_requires_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        const ENV: &str = "WEBCODEX_TEST_VALIDATION_EXECUTABLE";
        const MISSING_NAME: &str = "webcodex-validation-executable-that-does-not-exist";
        let _env_lock = crate::tests::test_env_lock();
        let temp = tempfile::tempdir().unwrap();

        let env = crate::tests::EnvGuard::new().set(ENV, temp.path());
        assert!(resolve_executable(ENV, MISSING_NAME).is_none());

        let file = temp.path().join("tool");
        std::fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
        let _env = env.set(ENV, &file);
        assert!(resolve_executable(ENV, MISSING_NAME).is_none());

        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&file, permissions).unwrap();
        assert_eq!(resolve_executable(ENV, MISSING_NAME), Some(file));
    }

    // -----------------------------------------------------------------------
    // Lifecycle regression coverage for run_bounded's ManagedChild migration.
    // Each "whole tree" test tracks a real descendant pid (written by the
    // validation_tree_helper to a marker file) and probes that pid directly.
    // -----------------------------------------------------------------------

    /// Compiled copy of the `validation_tree_helper` fixture, kept alive for
    /// the whole test process so its binary path never disappears under a
    /// running descendant (same pattern as the MCP/LSP/job-tree fixtures).
    struct ValidationTreeHelper {
        _temp: tempfile::TempDir,
        path: PathBuf,
    }

    static VALIDATION_TREE_HELPER: OnceLock<Arc<ValidationTreeHelper>> = OnceLock::new();

    fn helper_binary() -> PathBuf {
        VALIDATION_TREE_HELPER
            .get_or_init(|| {
                let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("src/webcodex_runner/validation/validation_tree_helper.rs");
                let temp = tempfile::tempdir().unwrap();
                let output = temp.path().join(format!(
                    "validation-tree-helper{}",
                    std::env::consts::EXE_SUFFIX
                ));
                let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
                let result = Command::new(rustc)
                    .arg("--edition=2021")
                    .arg("--crate-name=webcodex_validation_tree_helper")
                    .arg(&source)
                    .arg("-o")
                    .arg(&output)
                    .output()
                    .expect("run rustc for validation tree helper");
                assert!(
                    result.status.success(),
                    "validation tree helper compilation failed: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                Arc::new(ValidationTreeHelper {
                    _temp: temp,
                    path: output,
                })
            })
            .path
            .clone()
    }

    fn str_args(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// A unique temp file, removed on drop.
    struct CleanupPath(PathBuf);

    impl std::ops::Deref for CleanupPath {
        type Target = PathBuf;
        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for CleanupPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn unique_temp_path(tag: &str) -> CleanupPath {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "wc-validation-{tag}-{}-{nanos}",
            std::process::id()
        ));
        CleanupPath(path)
    }

    /// Poll until `path` exists or `timeout` elapses. The markers are written
    /// by freshly spawned helper processes; under a fully parallel test suite
    /// on the small GitHub CI runners, process startup can take well over a
    /// few seconds, so callers pass a generous 30s budget (matching the
    /// helper's own 30s gate).
    fn wait_until_file(path: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if path.exists() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Parse `KEY=<pid>` from a marker file written by the helper.
    fn read_pid(marker: &Path, key: &str) -> u32 {
        let text = std::fs::read_to_string(marker).expect("read pid marker");
        text.lines()
            .find_map(|line| {
                line.strip_prefix(key)
                    .and_then(|rest| rest.strip_prefix('='))
                    .and_then(|value| value.trim().parse().ok())
            })
            .unwrap_or_else(|| panic!("marker {marker:?} missing {key}: {text}"))
    }

    #[cfg(windows)]
    fn process_alive(pid: u32) -> bool {
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: OpenProcess returns a handle or NULL; NULL means the pid no
        // longer exists (or is inaccessible, which also means not ours).
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0u32;
        // SAFETY: `handle` is valid; `exit_code` is a valid out-param.
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        // SAFETY: close the handle we opened.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        ok == 1 && exit_code == 259 // 259 == STILL_ACTIVE
    }

    #[cfg(target_os = "linux")]
    fn process_alive(pid: u32) -> bool {
        // `kill(pid, 0)` also succeeds for zombies, while ManagedChild's Linux
        // tree-liveness contract deliberately treats zombies as unable to run.
        // Use /proc to align this test probe with that contract, but fall back
        // conservatively if procfs cannot be read or parsed.
        // SAFETY: signal 0 is an existence probe; the pid comes from our own
        // test helper.
        if (unsafe { libc::kill(pid as i32, 0) }) != 0 {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return true;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return true;
        };
        let state = rest.split_whitespace().next().unwrap_or("");
        state != "Z" && state != "X"
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn process_alive(pid: u32) -> bool {
        // SAFETY: signal 0 is an existence probe; the pid comes from our own
        // test helper.
        (unsafe { libc::kill(pid as i32, 0) }) == 0
    }

    /// Upper bound for the whole test body including cleanup; the child sleeps
    /// far longer, so any run exceeding this is a cleanup hang, not a slow exit.
    const BOUNDEDNESS_LIMIT: Duration = Duration::from_secs(30);

    /// A. Normal completion: real exit code, stdout/stderr capture, no errors.
    #[test]
    #[ignore = "runner real-process lane: spawns the validation process-tree fixture"]
    fn runner_real_process_validation_normal_completion_preserves_exit_code_and_capture() {
        let cwd = tempfile::tempdir().unwrap();
        let captured = run_bounded(
            &helper_binary(),
            &str_args(&["sleep", "0", "7"]),
            cwd.path(),
            30,
            None,
        );
        assert_eq!(captured.exit_code, Some(7));
        assert!(!captured.timed_out);
        assert!(captured.spawn_error.is_none());
        assert!(captured.wait_error.is_none(), "{:?}", captured.wait_error);
        assert!(!captured.stdout_capped);
        assert!(!captured.stderr_capped);
        let stdout = String::from_utf8_lossy(&captured.stdout);
        assert!(stdout.contains("VALIDATION_HELPER_STDOUT"), "{stdout}");
        let stderr = captured.stderr_summary.as_deref().unwrap_or_default();
        assert!(stderr.contains("VALIDATION_HELPER_STDERR"), "{stderr}");
    }

    /// B. Timeout terminates the entire tree: parent AND descendant must both
    /// be gone, with the timeout semantics unchanged.
    #[test]
    #[ignore = "runner real-process lane: spawns the validation process-tree fixture"]
    fn runner_real_process_validation_timeout_terminates_entire_tree() {
        let parent_marker = unique_temp_path("timeout-parent");
        let alive_marker = unique_temp_path("timeout-desc");
        let cwd = tempfile::tempdir().unwrap();
        let program = helper_binary();
        let args = str_args(&[
            "spawn-descendant-keepalive",
            parent_marker.to_str().unwrap(),
            alive_marker.to_str().unwrap(),
            "600",
        ]);
        let started = Instant::now();
        let captured = thread::scope(|scope| {
            let handle = scope.spawn(|| run_bounded(&program, &args, cwd.path(), 10, None));
            assert!(
                wait_until_file(&parent_marker, Duration::from_secs(30)),
                "parent marker never appeared"
            );
            assert!(
                wait_until_file(&alive_marker, Duration::from_secs(30)),
                "descendant marker never appeared"
            );
            let parent_pid = read_pid(&parent_marker, "PARENT_PID");
            let descendant_pid = read_pid(&parent_marker, "DESCENDANT_PID");
            // Both sleep 600s while the timeout is 10s, so they must still be
            // alive when the timeout fires.
            assert!(process_alive(parent_pid), "parent not alive before timeout");
            assert!(
                process_alive(descendant_pid),
                "descendant not alive before timeout"
            );
            handle.join().expect("run_bounded panicked")
        });
        let elapsed = started.elapsed();
        assert!(captured.timed_out);
        assert_eq!(captured.exit_code, Some(-1));
        assert!(captured.spawn_error.is_none());
        assert!(captured.wait_error.is_none(), "{:?}", captured.wait_error);
        assert!(
            elapsed < BOUNDEDNESS_LIMIT,
            "timeout cleanup not bounded: {elapsed:?}"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "PARENT_PID")),
            "validation parent survived timeout cleanup"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "DESCENDANT_PID")),
            "validation descendant survived timeout cleanup"
        );
    }

    /// C. The direct child exits before its descendant, which keeps the
    /// captured stdout pipe open. Direct-child exit alone must not finish
    /// cleanup: the descendant is terminated, the reader reaches EOF, and
    /// run_bounded stays bounded.
    #[test]
    #[ignore = "runner real-process lane: spawns the validation process-tree fixture"]
    fn runner_real_process_validation_parent_exit_alone_does_not_finish_cleanup() {
        let parent_marker = unique_temp_path("parent-first");
        let alive_marker = unique_temp_path("parent-first-desc");
        let cwd = tempfile::tempdir().unwrap();
        let program = helper_binary();
        let args = str_args(&[
            "spawn-descendant",
            parent_marker.to_str().unwrap(),
            alive_marker.to_str().unwrap(),
            "600",
        ]);
        let started = Instant::now();
        let captured = thread::scope(|scope| {
            let handle = scope.spawn(|| run_bounded(&program, &args, cwd.path(), 30, None));
            // The parent exits almost immediately after spawning its
            // descendant. The descendant's marker appears only if it actually
            // ran, so its existence proves the descendant was alive after the
            // parent exited.
            assert!(
                wait_until_file(&alive_marker, Duration::from_secs(30)),
                "descendant marker never appeared"
            );
            handle.join().expect("run_bounded panicked")
        });
        let elapsed = started.elapsed();
        assert_eq!(captured.exit_code, Some(0), "{:?}", captured.wait_error);
        assert!(!captured.timed_out);
        assert!(captured.wait_error.is_none(), "{:?}", captured.wait_error);
        assert!(captured.spawn_error.is_none());
        // The captured stdout contains the helper's pid line only when the
        // reader hit EOF, which requires every descendant holding the pipe to
        // be gone. A cleanup that stops at the direct child leaves stdout empty.
        let stdout = String::from_utf8_lossy(&captured.stdout);
        assert!(
            stdout.contains("DESCENDANT_PID="),
            "stdout reader never reached EOF: {stdout}"
        );
        assert!(
            !process_alive(read_pid(&alive_marker, "DESCENDANT_PID")),
            "descendant survived cleanup after direct child exit"
        );
        assert!(
            elapsed < BOUNDEDNESS_LIMIT,
            "parent-exit cleanup not bounded: {elapsed:?}"
        );
    }

    /// D. Runner shutdown terminates the whole tree with the shutdown
    /// semantics unchanged.
    #[test]
    #[ignore = "runner real-process lane: spawns the validation process-tree fixture"]
    fn runner_real_process_validation_runner_shutdown_terminates_whole_tree() {
        let parent_marker = unique_temp_path("shutdown-parent");
        let alive_marker = unique_temp_path("shutdown-desc");
        let cwd = tempfile::tempdir().unwrap();
        let program = helper_binary();
        let args = str_args(&[
            "spawn-descendant-keepalive",
            parent_marker.to_str().unwrap(),
            alive_marker.to_str().unwrap(),
            "600",
        ]);
        let shutdown = AtomicBool::new(false);
        let started = Instant::now();
        let captured = thread::scope(|scope| {
            let handle =
                scope.spawn(|| run_bounded(&program, &args, cwd.path(), 60, Some(&shutdown)));
            assert!(
                wait_until_file(&parent_marker, Duration::from_secs(30)),
                "parent marker never appeared"
            );
            assert!(
                wait_until_file(&alive_marker, Duration::from_secs(30)),
                "descendant marker never appeared"
            );
            let parent_pid = read_pid(&parent_marker, "PARENT_PID");
            let descendant_pid = read_pid(&parent_marker, "DESCENDANT_PID");
            assert!(
                process_alive(parent_pid),
                "parent not alive before shutdown"
            );
            assert!(
                process_alive(descendant_pid),
                "descendant not alive before shutdown"
            );
            shutdown.store(true, Ordering::SeqCst);
            handle.join().expect("run_bounded panicked")
        });
        let elapsed = started.elapsed();
        assert!(!captured.timed_out);
        assert!(captured.spawn_error.is_none());
        assert_eq!(
            captured.wait_error.as_deref(),
            Some("validation stopped during runner shutdown"),
            "runner shutdown must not be misreported"
        );
        assert_eq!(captured.exit_code, None);
        assert!(
            elapsed < BOUNDEDNESS_LIMIT,
            "shutdown cleanup not bounded: {elapsed:?}"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "PARENT_PID")),
            "validation parent survived runner shutdown"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "DESCENDANT_PID")),
            "validation descendant survived runner shutdown"
        );
    }

    /// E. A SIGTERM-resistant tree is escalated to force: the graceful request
    /// gets a bounded grace, then the whole tree is killed. Never unbounded.
    #[cfg(unix)]
    #[test]
    #[ignore = "runner real-process lane: spawns the validation process-tree fixture"]
    fn runner_real_process_validation_sigterm_resistant_tree_is_forcefully_escalated() {
        let parent_marker = unique_temp_path("resist-parent");
        let alive_marker = unique_temp_path("resist-desc");
        let cwd = tempfile::tempdir().unwrap();
        let program = helper_binary();
        let args = str_args(&[
            "ignore-term-keepalive",
            parent_marker.to_str().unwrap(),
            alive_marker.to_str().unwrap(),
            "600",
        ]);
        let started = Instant::now();
        let captured = thread::scope(|scope| {
            let handle = scope.spawn(|| run_bounded(&program, &args, cwd.path(), 3, None));
            assert!(
                wait_until_file(&parent_marker, Duration::from_secs(30)),
                "parent marker never appeared"
            );
            assert!(
                wait_until_file(&alive_marker, Duration::from_secs(30)),
                "descendant marker never appeared"
            );
            let parent_pid = read_pid(&parent_marker, "PARENT_PID");
            let descendant_pid = read_pid(&parent_marker, "DESCENDANT_PID");
            assert!(process_alive(parent_pid), "parent not alive before timeout");
            assert!(
                process_alive(descendant_pid),
                "descendant not alive before timeout"
            );
            handle.join().expect("run_bounded panicked")
        });
        let elapsed = started.elapsed();
        assert!(captured.timed_out);
        assert_eq!(captured.exit_code, Some(-1));
        assert!(captured.wait_error.is_none(), "{:?}", captured.wait_error);
        // Both processes ignore SIGTERM (inherited SIG_IGN), so only the
        // forceful escalation can have ended them; the ~100ms grace plus
        // escalation must stay well within the bound.
        assert!(
            elapsed < BOUNDEDNESS_LIMIT,
            "SIGTERM-resistant cleanup not bounded: {elapsed:?}"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "PARENT_PID")),
            "SIGTERM-resistant parent survived escalation"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "DESCENDANT_PID")),
            "SIGTERM-resistant descendant survived escalation"
        );
    }

    /// F. Cleanup of an already-exited tree: no panic, no false infrastructure
    /// error, and it is idempotent.
    #[test]
    #[ignore = "runner real-process lane: spawns the validation process-tree fixture"]
    fn runner_real_process_validation_already_exited_cleanup_is_not_an_error() {
        // Normal completion runs cleanup after the tree already exited; the
        // AlreadyExited graceful path must not surface as a wait error.
        let cwd = tempfile::tempdir().unwrap();
        let captured = run_bounded(
            &helper_binary(),
            &str_args(&["sleep", "0", "0"]),
            cwd.path(),
            30,
            None,
        );
        assert_eq!(captured.exit_code, Some(0));
        assert!(!captured.timed_out);
        assert!(captured.wait_error.is_none(), "{:?}", captured.wait_error);

        // Directly repeated cleanup of an already-exited tree.
        let mut command = Command::new(helper_binary());
        command
            .args(["sleep", "0", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = ManagedChild::spawn(&mut command).expect("spawn helper");
        let status = child.wait().expect("wait helper");
        let deadline = Instant::now() + Duration::from_secs(1);
        let first = terminate_validation_child(&mut child, deadline)
            .expect("cleanup of an exited tree must not report an error");
        assert_eq!(
            first,
            Some(status),
            "cleanup must keep the real exit status"
        );
        let second = terminate_validation_child(&mut child, deadline)
            .expect("repeated cleanup must not report an error");
        assert_eq!(second, Some(status));
    }

    /// G. Spawn failure keeps the spawn-error semantics and has no tree to leak.
    #[test]
    fn spawn_failure_reports_spawn_error_only() {
        let cwd = tempfile::tempdir().unwrap();
        let captured = run_bounded(
            Path::new("webcodex-validation-executable-that-does-not-exist"),
            &str_args(&["sleep", "0", "0"]),
            cwd.path(),
            30,
            None,
        );
        assert!(
            captured
                .spawn_error
                .as_deref()
                .is_some_and(|e| e.contains("spawn failed")),
            "{:?}",
            captured.spawn_error
        );
        assert!(!captured.timed_out);
        assert!(captured.wait_error.is_none(), "{:?}", captured.wait_error);
        assert_eq!(captured.exit_code, None);
        assert!(captured.stdout.is_empty());
        assert!(
            captured.duration_ms < 5_000,
            "spawn failure was not immediate"
        );
    }
}
