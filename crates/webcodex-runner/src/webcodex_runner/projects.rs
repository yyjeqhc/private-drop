use super::config::{
    default_true, project_registry_dir, validate_shell_profile_name, RunnerConfig, RunnerPolicy,
};
use super::shell::canonicalize_existing;
use crate::runner_protocol::RunnerProjectSummary;
#[cfg(test)]
use crate::runner_protocol::RunnerRequest;
use crate::{err_cmd, ok_cmd, write_created_file};
use crate::{CommandResult, CreatedProjectPaths};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(test)]
use webcodex_core::runner_operation::RunnerOperation;
use webcodex_core::runner_operation::{RunnerProjectOperation, RunnerProjectOperationKind};
use webcodex_process::{GracefulTermination, ManagedChild};
use webcodex_runner_config::paths::paths_equal;

const PROJECT_SCAN_CACHE_MS: u64 = 5000;
const PROJECT_GIT_TIMEOUT: Duration = Duration::from_secs(2);
const MANAGED_WORKTREE_GIT_TIMEOUT: Duration = Duration::from_secs(20);
// Tree shutdown also has to let the bounded stdout/stderr readers observe EOF.
// Darwin process-group teardown and reader scheduling can legitimately take
// longer than 500ms on loaded native CI hosts, so keep a short but realistic
// bounded cleanup budget rather than turning successful direct-child exit into
// a spurious reader-timeout failure.
const PROJECT_GIT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const PROJECT_GIT_OUTPUT_MAX_BYTES: usize = 64 * 1024;
const EXPLICIT_REGISTRATION_SOURCE: &str = "explicit";
const AUTO_REGISTERED_REGISTRATION_SOURCE: &str = "auto_registered";
const LEGACY_AUTO_REGISTERED_PROJECT_KIND: &str = "auto_registered";
const AUTO_PROJECT_HASH_PREFIX_LENGTHS: &[usize] = &[8, 12, 16, 24, 32, 48, 64];
static PROJECT_REGISTRY_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn project_registry_write_lock() -> &'static Mutex<()> {
    PROJECT_REGISTRY_WRITE_LOCK.get_or_init(|| Mutex::new(()))
}

fn project_error_cmd(start: Instant, error_code: &'static str) -> CommandResult {
    CommandResult {
        exit_code: Some(1),
        stdout: Some(
            serde_json::to_string(&serde_json::json!({"error_code": error_code}))
                .unwrap_or_else(|_| r#"{"error_code":"operation_failed"}"#.to_string()),
        ),
        stderr: Some(String::new()),
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
    }
}

fn structured_project_error_cmd(
    start: Instant,
    error_kind: &'static str,
    state_changed: bool,
    fields: serde_json::Value,
) -> CommandResult {
    let mut output = serde_json::json!({
        "error_code": error_kind,
        "error_kind": error_kind,
        "failure_kind": error_kind,
        "state_changed": state_changed,
    });
    if let (Some(output), Some(fields)) = (output.as_object_mut(), fields.as_object()) {
        output.extend(fields.clone());
    }
    CommandResult {
        exit_code: Some(1),
        stdout: Some(
            serde_json::to_string(&output)
                .unwrap_or_else(|_| r#"{"error_code":"operation_failed"}"#.to_string()),
        ),
        stderr: Some(String::new()),
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunnerProjectFile {
    pub(crate) id: String,
    pub(crate) path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) shell_profile: Option<String>,
    #[serde(default = "default_true")]
    pub(crate) allow_patch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) registration_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) disabled: bool,
    #[serde(default)]
    pub(crate) hooks: HashMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) managed_worktree: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) managed_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) managed_base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) managed_base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) managed_operation_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RunnerProjectCache {
    projects: Vec<RunnerProjectSummary>,
    refreshed_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub(crate) struct RunnerProjectShellContext {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) shell_profile: Option<String>,
}

fn validate_project_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("id cannot be empty".to_string());
    }
    if id == "." || id == ".." {
        return Err("id cannot be '.' or '..'".to_string());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err("id may only contain ASCII letters, digits, '-', '_', and '.'".to_string());
    }
    Ok(())
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn runner_project_server_format_hint(content: &str, err: &str) -> Option<String> {
    let normalized = err.replace('`', "");
    if normalized.contains("missing field id") && content.contains("[projects.") {
        Some(
            "looks like a server projects.toml entry. Runner project registration records must use top-level fields:\n\
             id = \"smoke\"\n\
             path = \"/path/to/repo\""
                .to_string(),
        )
    } else {
        None
    }
}

pub(crate) fn parse_runner_project_toml(content: &str) -> Result<RunnerProjectFile, String> {
    let mut project: RunnerProjectFile = toml::from_str(content).map_err(|e| {
        let err = e.to_string();
        let base = format!("failed to parse project toml: {}", err);
        match runner_project_server_format_hint(content, &err) {
            Some(hint) => format!("{}; {}", base, hint),
            None => base,
        }
    })?;
    project.id = project.id.trim().to_string();
    validate_project_id(&project.id)?;
    project.path = project.path.trim().to_string();
    if project.path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    project.name = trim_optional(project.name);
    project.kind = trim_optional(project.kind);
    project.registration_source = trim_optional(project.registration_source);
    project.description = trim_optional(project.description);
    if let Some(shell_profile) = &project.shell_profile {
        validate_shell_profile_name("project.shell_profile", shell_profile)?;
    }
    let mut hooks = HashMap::new();
    for (name, commands) in project.hooks {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("hook name cannot be empty".to_string());
        }
        hooks.insert(name, commands);
    }
    project.hooks = hooks;
    Ok(project)
}

fn load_runner_project_shell_contexts_from_dir(dir: &Path) -> Vec<RunnerProjectShellContext> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            files.push(path);
        }
    }
    files.sort();
    let mut seen = HashSet::new();
    let mut projects = Vec::new();
    for file in files {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(project) = parse_runner_project_toml(&content) else {
            continue;
        };
        if project.disabled || !seen.insert(project.id.clone()) {
            continue;
        }
        projects.push(RunnerProjectShellContext {
            id: project.id,
            path: project.path,
            shell_profile: project.shell_profile,
        });
    }
    projects
}

pub(crate) fn find_project_shell_context(
    project_registry_dir: &Path,
    cwd_path: &Path,
) -> Option<RunnerProjectShellContext> {
    let cwd = cwd_path.canonicalize().ok()?;
    load_runner_project_shell_contexts_from_dir(project_registry_dir)
        .into_iter()
        .filter_map(|project| {
            let project_path = PathBuf::from(&project.path).canonicalize().ok()?;
            // Windows filesystems are case-insensitive and `canonicalize` may
            // return `\\?\`-prefixed paths, so containment uses the shared
            // path identity rules instead of raw `==`/`starts_with`.
            if webcodex_runner_config::paths::path_is_within(&cwd, &project_path) {
                Some((project_path.components().count(), project))
            } else {
                None
            }
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, project)| project)
}

/// Resolve one enabled project by its Runner-local id. Persistent shells use
/// the id from the authenticated runtime-project binding rather than choosing
/// a project solely from a caller-controlled cwd.
pub(crate) fn find_project_shell_context_by_id(
    project_registry_dir: &Path,
    project_id: &str,
) -> Option<RunnerProjectShellContext> {
    load_runner_project_shell_contexts_from_dir(project_registry_dir)
        .into_iter()
        .find(|project| project.id == project_id)
}

struct BoundedGitOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_capped: bool,
    stderr_capped: bool,
}

fn spawn_bounded_git_reader(
    mut pipe: impl Read + Send + 'static,
) -> (mpsc::Receiver<(Vec<u8>, bool)>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let mut retained = Vec::with_capacity(PROJECT_GIT_OUTPUT_MAX_BYTES.min(8192));
        let mut chunk = [0_u8; 8192];
        let mut capped = false;
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    let remaining = PROJECT_GIT_OUTPUT_MAX_BYTES.saturating_sub(retained.len());
                    let keep = remaining.min(read);
                    retained.extend_from_slice(&chunk[..keep]);
                    capped |= keep < read;
                }
                Err(_) => break,
            }
        }
        let _ = tx.send((retained, capped));
    });
    (rx, handle)
}

/// Terminate the whole Git process tree within one shared cleanup deadline,
/// then reap the direct child and confirm the complete tree exited.
///
/// The platform tree isolation lives in [`ManagedChild`]: a private process
/// group on Unix, a kill-on-close Job Object on Windows. Phase 1 (Unix only)
/// requests graceful tree termination and gives the tree a short bounded grace
/// to exit on its own; Windows reports [`GracefulTermination::Unsupported`]
/// and skips straight to phase 2. Phase 2 forcefully terminates any tree that
/// is still alive. Then the direct child is reaped and the complete tree (not
/// just the direct child) is confirmed exited — all within `deadline`. The
/// direct child's `ExitStatus`, when it can still be obtained, is returned;
/// failures are joined into one error string, but cleanup never gives up early
/// because a graceful request failed.
fn terminate_project_git_tree(
    child: &mut ManagedChild,
    deadline: Instant,
) -> Result<Option<ExitStatus>, String> {
    let mut errors = Vec::new();

    match child.request_terminate_tree() {
        Ok(GracefulTermination::Requested) => {
            // The whole tree received a graceful termination request. Give it
            // a short bounded grace to exit on its own; the grace never
            // extends past the overall cleanup deadline.
            let grace_deadline = deadline.min(Instant::now() + Duration::from_millis(50));
            let remaining = grace_deadline.saturating_duration_since(Instant::now());
            match child.wait_tree_exit(remaining) {
                Ok(_) => {}
                Err(error) => {
                    errors.push(format!("git graceful termination wait failed: {error}"));
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
            errors.push(format!("git graceful termination request failed: {error}"));
        }
    }

    // Forceful phase: any tree still alive is terminated as a whole.
    let tree_alive = match child.try_tree_exit() {
        Ok(exited) => !exited,
        Err(error) => {
            errors.push(format!("git tree liveness probe failed: {error}"));
            true
        }
    };
    if tree_alive {
        if let Err(error) = child.terminate_tree() {
            errors.push(format!("git tree termination failed: {error}"));
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
                    errors.push("git child reap timed out".to_string());
                    break;
                }
                thread::sleep(Duration::from_millis(10).min(remaining));
            }
            Err(error) => {
                errors.push(format!("git child reap failed: {error}"));
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
        Ok(false) => errors.push("git process tree did not exit before deadline".to_string()),
        Err(error) => errors.push(format!("git tree exit wait failed: {error}")),
    }

    if errors.is_empty() {
        Ok(status)
    } else {
        Err(errors.join("; "))
    }
}

fn run_git_bounded(
    path: &Path,
    args: &[&str],
    timeout: Duration,
    shutdown: Option<&AtomicBool>,
) -> Result<BoundedGitOutput, String> {
    run_git_bounded_with_program("git", path, args, timeout, shutdown)
}

/// Test seam over `run_git_bounded`: the program name is passed in instead of
/// being hardcoded to `"git"`, so lifecycle tests can drive a cross-platform
/// fixture binary through the same bounded tree lifecycle. Production always
/// calls [`run_git_bounded`], which passes `"git"`.
fn run_git_bounded_with_program(
    program: &str,
    path: &Path,
    args: &[&str],
    timeout: Duration,
    shutdown: Option<&AtomicBool>,
) -> Result<BoundedGitOutput, String> {
    if shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err("git stopped during runner shutdown".to_string());
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // ManagedChild owns the whole Git process tree: a private process group on
    // Unix, a kill-on-close Job Object on Windows. Spawn remains direct process
    // spawning with the standard Command spawn failure semantics.
    let mut child = match ManagedChild::spawn(&mut command) {
        Ok(child) => child,
        Err(error) => return Err(format!("failed to spawn git: {error}")),
    };
    let Some(stdout) = child.child_mut().stdout.take() else {
        let cleanup_deadline = Instant::now() + PROJECT_GIT_CLEANUP_TIMEOUT;
        let _ = terminate_project_git_tree(&mut child, cleanup_deadline);
        return Err("git stdout pipe was unavailable".to_string());
    };
    let Some(stderr) = child.child_mut().stderr.take() else {
        drop(stdout);
        let cleanup_deadline = Instant::now() + PROJECT_GIT_CLEANUP_TIMEOUT;
        let _ = terminate_project_git_tree(&mut child, cleanup_deadline);
        return Err("git stderr pipe was unavailable".to_string());
    };
    let (stdout_rx, stdout_reader) = spawn_bounded_git_reader(stdout);
    let (stderr_rx, stderr_reader) = spawn_bounded_git_reader(stderr);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let stopping = shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst));
                if stopping || Instant::now() >= deadline {
                    // Cleanup and then report the stopping cause: the cleanup
                    // outcome is deliberately not allowed to replace the
                    // user-visible timeout/shutdown error.
                    let _ = terminate_project_git_tree(
                        &mut child,
                        Instant::now() + PROJECT_GIT_CLEANUP_TIMEOUT,
                    );
                    return Err(if stopping {
                        "git stopped during runner shutdown".to_string()
                    } else {
                        "git command timed out".to_string()
                    });
                }
                thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(error) => {
                let _ = terminate_project_git_tree(
                    &mut child,
                    Instant::now() + PROJECT_GIT_CLEANUP_TIMEOUT,
                );
                return Err(format!("failed to wait for git: {error}"));
            }
        }
    };

    // A helper descendant must not keep either pipe open after Git itself
    // exits. Direct-child exit alone is not tree exit: if descendants remain,
    // clean up the surviving tree, then drain the bounded readers — all within
    // one shared cleanup deadline so no operation gets a fresh independent one.
    let cleanup_deadline = Instant::now() + PROJECT_GIT_CLEANUP_TIMEOUT;
    match child.try_tree_exit() {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            let _ = terminate_project_git_tree(&mut child, cleanup_deadline);
        }
    }
    let stdout = stdout_rx
        .recv_timeout(cleanup_deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "git stdout reader timed out".to_string())?;
    let stderr = stderr_rx
        .recv_timeout(cleanup_deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "git stderr reader timed out".to_string())?;
    if stdout_reader.is_finished() {
        let _ = stdout_reader.join();
    }
    if stderr_reader.is_finished() {
        let _ = stderr_reader.join();
    }
    Ok(BoundedGitOutput {
        status,
        stdout: stdout.0,
        stderr: stderr.0,
        stdout_capped: stdout.1,
        stderr_capped: stderr.1,
    })
}

fn run_git_capture(path: &str, args: &[&str], shutdown: Option<&AtomicBool>) -> Option<String> {
    let output = run_git_bounded(Path::new(path), args, PROJECT_GIT_TIMEOUT, shutdown).ok()?;
    if !output.status.success() || output.stdout_capped {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn project_revision(project: &RunnerProjectFile) -> String {
    let normalized = toml::to_string(project).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(normalized.as_bytes()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectRegistrationSource {
    Explicit,
    AutoRegistered,
}

impl ProjectRegistrationSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => EXPLICIT_REGISTRATION_SOURCE,
            Self::AutoRegistered => AUTO_REGISTERED_REGISTRATION_SOURCE,
        }
    }
}

/// Interpret registration provenance without changing the parsed persisted
/// representation. Keeping this compatibility projection separate is
/// important because `project_revision` hashes the raw normalized record.
fn effective_registration_source(project: &RunnerProjectFile) -> ProjectRegistrationSource {
    match project.registration_source.as_deref() {
        Some(AUTO_REGISTERED_REGISTRATION_SOURCE) => ProjectRegistrationSource::AutoRegistered,
        // A present new field is authoritative. `explicit` and unknown future
        // values therefore fail closed to ordinary explicit registration rather
        // than allowing a legacy `kind` value to override newer semantics.
        Some(_) => ProjectRegistrationSource::Explicit,
        None if project.kind.as_deref() == Some(LEGACY_AUTO_REGISTERED_PROJECT_KIND) => {
            ProjectRegistrationSource::AutoRegistered
        }
        None => ProjectRegistrationSource::Explicit,
    }
}

/// Preserve the historical auto-registration sentinel only at Runner→Server
/// compatibility boundaries. Persisted `kind` remains genuine project metadata.
fn project_wire_kind(project: &RunnerProjectFile) -> Option<String> {
    if project.kind.is_none()
        && project.registration_source.as_deref() == Some(AUTO_REGISTERED_REGISTRATION_SOURCE)
    {
        Some(LEGACY_AUTO_REGISTERED_PROJECT_KIND.to_string())
    } else {
        project.kind.clone()
    }
}

fn runner_project_summary_with_shutdown(
    project: &RunnerProjectFile,
    updated_at: i64,
    include_git: bool,
    shutdown: Option<&AtomicBool>,
) -> RunnerProjectSummary {
    let mut hooks = project.hooks.keys().cloned().collect::<Vec<_>>();
    hooks.sort();
    // The server uses the reported path as part of its repository continuity
    // identity. Report the actual root, not a mutable symlink alias, so a
    // retargeted project registration cannot inherit another repository's
    // current Workflow Session.
    let resolved_path = canonicalize_existing(Path::new(&project.path))
        .ok()
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| PathBuf::from(&project.path));
    let resolved_path = resolved_path.to_string_lossy().to_string();
    let (git_branch, git_head, git_dirty) = if include_git {
        let branch = run_git_capture(
            &resolved_path,
            &["rev-parse", "--abbrev-ref", "HEAD"],
            shutdown,
        );
        let head = run_git_capture(
            &resolved_path,
            &["log", "-1", "--pretty=format:%h"],
            shutdown,
        );
        let dirty = run_git_capture(&resolved_path, &["status", "--short"], shutdown)
            .map(|status| !status.trim().is_empty());
        (branch, head, dirty)
    } else {
        (None, None, None)
    };
    let registration_source = effective_registration_source(project);
    // Rolling-upgrade shim: old Servers only know the historical `kind`
    // sentinel. New auto-registered records keep persisted `kind` empty, but
    // temporarily project that sentinel on the wire when there is no genuine
    // project kind. New Servers ignore it in favor of `registration_source`.
    RunnerProjectSummary {
        id: project.id.clone(),
        name: project.name.clone().or_else(|| Some(project.id.clone())),
        path: resolved_path,
        allow_patch: project.allow_patch,
        kind: project_wire_kind(project),
        registration_source: Some(registration_source.as_str().to_string()),
        description: project.description.clone(),
        hooks,
        disabled: project.disabled,
        revision: Some(project_revision(project)),
        git_branch,
        git_head,
        git_dirty,
        updated_at,
        shell_profile: project.shell_profile.clone(),
    }
}

#[cfg(test)]
pub(crate) fn runner_project_summary(
    project: &RunnerProjectFile,
    updated_at: i64,
    include_git: bool,
) -> RunnerProjectSummary {
    runner_project_summary_with_shutdown(project, updated_at, include_git, None)
}

fn warn_empty_hook_commands(source: &Path, project: &RunnerProjectFile) {
    for (hook, commands) in &project.hooks {
        for (idx, command) in commands.iter().enumerate() {
            if command.trim().is_empty() {
                eprintln!(
                    "webcodex-runner project warning: {} hook {} command {} is empty",
                    source.display(),
                    hook,
                    idx
                );
            }
        }
    }
}

fn load_runner_project_summaries_from_dir_with_shutdown(
    dir: &Path,
    shutdown: Option<&AtomicBool>,
) -> Vec<RunnerProjectSummary> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!(
                "webcodex-runner project warning: failed to read {}: {}",
                dir.display(),
                e
            );
            return Vec::new();
        }
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            files.push(path);
        }
    }
    files.sort();

    let updated_at = chrono::Utc::now().timestamp();
    let mut seen = HashSet::new();
    let mut projects = Vec::new();
    for file in files {
        if shutdown.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            break;
        }
        let content = match std::fs::read_to_string(&file) {
            Ok(content) => content,
            Err(e) => {
                eprintln!(
                    "webcodex-runner project warning: failed to read {}: {}",
                    file.display(),
                    e
                );
                continue;
            }
        };
        let project = match parse_runner_project_toml(&content) {
            Ok(project) => project,
            Err(e) => {
                eprintln!(
                    "webcodex-runner project warning: skipping {}: {}",
                    file.display(),
                    e
                );
                continue;
            }
        };
        if !seen.insert(project.id.clone()) {
            eprintln!(
                "webcodex-runner project warning: duplicate project id {} in {}; skipping",
                project.id,
                file.display()
            );
            continue;
        }
        warn_empty_hook_commands(&file, &project);
        projects.push(runner_project_summary_with_shutdown(
            &project, updated_at, true, shutdown,
        ));
    }
    projects.sort_by(|a, b| a.id.cmp(&b.id));
    projects
}

pub(crate) fn load_runner_project_summaries_from_dir(dir: &Path) -> Vec<RunnerProjectSummary> {
    load_runner_project_summaries_from_dir_with_shutdown(dir, None)
}

fn load_runner_project_summaries(
    cfg: &RunnerConfig,
    shutdown: Option<&AtomicBool>,
) -> Vec<RunnerProjectSummary> {
    // Loaded configs always carry a materialized project_registry_dir; a bare
    // test-built config that cannot derive one reports the error instead of
    // silently scanning a relative path.
    let dir = match project_registry_dir(cfg) {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("webcodex-runner: {error}");
            return Vec::new();
        }
    };
    load_runner_project_summaries_from_dir_with_shutdown(&dir, shutdown)
}

impl RunnerProjectCache {
    #[cfg(test)]
    pub(crate) fn get(&mut self, cfg: &RunnerConfig) -> Vec<RunnerProjectSummary> {
        self.get_with_shutdown(cfg, None)
    }

    pub(crate) fn get_with_shutdown(
        &mut self,
        cfg: &RunnerConfig,
        shutdown: Option<&AtomicBool>,
    ) -> Vec<RunnerProjectSummary> {
        if self.refreshed_at.is_some_and(|refreshed_at| {
            refreshed_at.elapsed() < Duration::from_millis(PROJECT_SCAN_CACHE_MS)
        }) {
            return self.projects.clone();
        }
        self.projects = load_runner_project_summaries(cfg, shutdown);
        self.refreshed_at = Some(Instant::now());
        self.projects.clone()
    }

    pub(crate) fn needs_refresh(&self) -> bool {
        self.refreshed_at.is_none()
    }

    pub(crate) fn invalidate(&mut self) {
        self.projects.clear();
        self.refreshed_at = None;
    }
}

/// Windows-only raw/canonical namespace fence. Local disks plus UNC and
/// verbatim-UNC shares may proceed; device namespaces and generic verbatim
/// namespaces fail closed with a stable error before filesystem access.
///
/// The shared `webcodex_runner_config::paths::validate_project_path_ingress`
/// owns the grammar-based prefix rule; it never falls back to a string
/// `starts_with` check.
fn validate_windows_project_root(path: &Path) -> Result<(), &'static str> {
    webcodex_runner_config::paths::validate_project_path_ingress(path)
        .map_err(|_| "windows_project_path_unsupported")
}

/// Before a model-facing request dereferences a raw Windows network path, require
/// it to fall under Runner authority that was already configured by the user.
/// This prevents an untrusted `\\server\share` spelling from triggering SMB I/O
/// (and possible OS authentication) merely to discover that policy rejects it.
fn validate_model_network_project_ingress_authority(
    policy: &RunnerPolicy,
    path: &Path,
) -> Result<(), &'static str> {
    #[cfg(windows)]
    {
        if !webcodex_runner_config::paths::is_windows_network_share_path(path) {
            return Ok(());
        }
        if policy
            .allowed_roots
            .iter()
            .any(|root| webcodex_runner_config::paths::path_is_within(path, root))
        {
            return Ok(());
        }
        // A configured mapped drive may canonicalize to the same UNC share. It is
        // safe to resolve configured roots here because those roots are already
        // user-authorized; never canonicalize the untrusted target before this gate.
        let canonical_roots =
            webcodex_runner_config::paths::canonicalize_usable_allowed_roots(&policy.allowed_roots);
        if canonical_roots
            .iter()
            .any(|root| webcodex_runner_config::paths::path_is_within(path, root))
        {
            return Ok(());
        }
        return Err("path_outside_allowed_roots");
    }

    #[cfg(not(windows))]
    {
        let _ = (policy, path);
        Ok(())
    }
}

/// Escape a string for use as a TOML basic string (double-quoted). NUL is
/// rejected up front by validation, so we only handle backslash, quote, and
/// common control characters.
fn toml_basic_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{}\"", escaped)
}

/// Build a deterministic project TOML string compatible with the existing
/// `parse_runner_project_toml` parser. The field order is fixed so the output
/// is reproducible.
fn build_project_toml(
    id: &str,
    name: &str,
    path: &str,
    description: &Option<String>,
    allow_patch: bool,
) -> String {
    build_project_toml_with_registration_source(id, name, path, None, description, allow_patch)
}

fn build_project_toml_with_registration_source(
    id: &str,
    name: &str,
    path: &str,
    registration_source: Option<&str>,
    description: &Option<String>,
    allow_patch: bool,
) -> String {
    let mut toml = String::new();
    toml.push_str(&format!("id = {}\n", toml_basic_string(id)));
    toml.push_str(&format!("name = {}\n", toml_basic_string(name)));
    toml.push_str(&format!("path = {}\n", toml_basic_string(path)));
    if let Some(registration_source) = registration_source {
        toml.push_str(&format!(
            "registration_source = {}\n",
            toml_basic_string(registration_source)
        ));
    }
    if let Some(desc) = description {
        toml.push_str(&format!("description = {}\n", toml_basic_string(desc)));
    }
    toml.push_str(&format!("allow_patch = {}\n", allow_patch));
    toml
}

/// Validate the project `id` for project-management operations. Stricter than
/// the existing `validate_project_id`: no dots (prevents any path-like
/// interpretation), only ASCII letters/digits/dash/underscore.
fn validate_project_op_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("id cannot be empty".to_string());
    }
    if id.contains('\0') {
        return Err("id must not contain NUL".to_string());
    }
    if id.len() > 64 {
        return Err("id must be at most 64 characters".to_string());
    }
    if id.contains('/') || id.contains('\\') {
        return Err("id must not contain slash or backslash".to_string());
    }
    if id == ".." || id == "." || id.contains("..") {
        return Err("id must not contain dot-dot traversal".to_string());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("id may only contain ASCII letters, digits, '-', and '_'".to_string());
    }
    Ok(())
}

/// Validate the project `name`: non-empty after trim, <= 120 UTF-8 bytes, no NUL.
fn validate_project_op_name(name: &str) -> Result<(), String> {
    if name.contains('\0') {
        return Err("name must not contain NUL".to_string());
    }
    if name.trim().is_empty() {
        return Err("name cannot be empty".to_string());
    }
    if name.len() > 120 {
        return Err("name must be at most 120 UTF-8 bytes".to_string());
    }
    Ok(())
}

/// Validate the optional `description`: <= 500 UTF-8 bytes, no NUL.
fn validate_project_op_description(desc: &str) -> Result<(), String> {
    if desc.contains('\0') {
        return Err("description must not contain NUL".to_string());
    }
    if desc.len() > 500 {
        return Err("description must be at most 500 UTF-8 bytes".to_string());
    }
    Ok(())
}

/// Thin Runner adapter over the shared authoritative project-path policy.
/// Runner config loading has already materialized HOME-derived effective roots;
/// this layer only canonicalizes currently usable roots before applying the
/// same pure path semantics used by local onboarding.
pub(crate) fn validate_project_path_policy(
    policy: &RunnerPolicy,
    canonical_path: &Path,
) -> Result<(), String> {
    let canonical_roots =
        webcodex_runner_config::paths::canonicalize_usable_allowed_roots(&policy.allowed_roots);
    webcodex_runner_config::paths::validate_project_path_policy(
        canonical_path,
        &canonical_roots,
        policy.allow_cwd_anywhere,
    )
}

#[derive(Debug, Clone)]
struct ProjectTomlWriteResult {
    config_path: PathBuf,
    created_config: bool,
    overwritten: bool,
}

#[derive(Debug)]
enum ProjectTomlWriteError {
    BeforeRename,
    AfterRename,
}

#[derive(Debug)]
enum ProjectUnregisterError {
    BeforeRename,
    AfterRename,
}

#[cfg(test)]
thread_local! {
    static FAIL_PARENT_SYNC_AFTER_PROJECT_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_PROJECT_PUBLISH_BEFORE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_project_parent_sync_after_rename() {
    FAIL_PARENT_SYNC_AFTER_PROJECT_RENAME.set(true);
}

#[cfg(test)]
pub(crate) fn fail_next_project_publish_before_rename() {
    FAIL_PROJECT_PUBLISH_BEFORE_RENAME.set(true);
}

fn sync_project_parent_after_rename(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_PARENT_SYNC_AFTER_PROJECT_RENAME.replace(false) {
        return Err("injected parent directory sync failure".to_string());
    }
    sync_parent_dir(path)
}

/// Write a project TOML file atomically into `project_registry_dir`. Creates
/// `project_registry_dir` if missing. Returns write metadata on success.
/// The temp file is written and fsynced, then atomically published as
/// `<id>.toml`.
fn sync_parent_dir(path: &Path) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "project config has no parent".to_string())?;
    sync_dir(dir)
}

fn sync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        // Opening a directory with `File::open` fails on Windows (it needs
        // FILE_FLAG_BACKUP_SEMANTICS, which std does not expose), and NTFS
        // metadata durability does not rely on directory fsync the way
        // POSIX filesystems do. The rename is already atomic; skip the
        // directory sync here.
        let _ = dir;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::File::open(dir)
            .and_then(|file| file.sync_all())
            .map_err(|e| format!("failed to sync project registry directory: {e}"))
    }
}

fn unique_registry_temp(dir: &Path, id: &str, suffix: &str) -> PathBuf {
    dir.join(format!(".{id}.{}.{}", uuid::Uuid::new_v4(), suffix))
}

fn write_project_toml_atomic(
    project_registry_dir: &Path,
    id: &str,
    toml_content: &str,
    overwrite: bool,
) -> Result<ProjectTomlWriteResult, ProjectTomlWriteError> {
    std::fs::create_dir_all(project_registry_dir)
        .map_err(|_| ProjectTomlWriteError::BeforeRename)?;
    let canonical_dir = canonicalize_existing(project_registry_dir)
        .map_err(|_| ProjectTomlWriteError::BeforeRename)?;
    let config_path = canonical_dir.join(format!("{id}.toml"));
    if !config_path.starts_with(&canonical_dir) {
        return Err(ProjectTomlWriteError::BeforeRename);
    }
    let existed_before = config_path.exists();
    if existed_before && !overwrite {
        return Err(ProjectTomlWriteError::BeforeRename);
    }
    let temp_path = unique_registry_temp(&canonical_dir, id, "toml.tmp");
    let mut published = false;
    let before = (|| -> Result<(), String> {
        let mut file = std::fs::File::create(&temp_path).map_err(|e| e.to_string())?;
        file.write_all(toml_content.as_bytes())
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        #[cfg(test)]
        if FAIL_PROJECT_PUBLISH_BEFORE_RENAME.replace(false) {
            return Err("injected project publish failure".to_string());
        }
        if overwrite {
            std::fs::rename(&temp_path, &config_path).map_err(|e| e.to_string())?;
            published = true;
        } else {
            // Publish a complete, synced same-directory temp file without the
            // overwrite-on-rename race. A concurrent creator wins cleanly and
            // the caller can rescan the registry to converge.
            std::fs::hard_link(&temp_path, &config_path).map_err(|e| e.to_string())?;
            published = true;
            std::fs::remove_file(&temp_path).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    if before.is_err() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(if published {
            ProjectTomlWriteError::AfterRename
        } else {
            ProjectTomlWriteError::BeforeRename
        });
    }
    sync_project_parent_after_rename(&config_path)
        .map_err(|_| ProjectTomlWriteError::AfterRename)?;
    Ok(ProjectTomlWriteResult {
        config_path,
        created_config: !existed_before,
        overwritten: existed_before && overwrite,
    })
}

fn load_project_files_for_path_resolution(
    project_registry_dir: &Path,
) -> Result<Vec<RunnerProjectFile>, &'static str> {
    let entries = match std::fs::read_dir(project_registry_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("project_registry_unavailable"),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| "project_registry_unavailable")?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("toml") {
            files.push(path);
        }
    }
    files.sort();

    let mut projects = Vec::with_capacity(files.len());
    for file in files {
        let content = std::fs::read_to_string(&file).map_err(|_| "project_registry_unavailable")?;
        let project =
            parse_runner_project_toml(&content).map_err(|_| "project_registry_unavailable")?;
        projects.push(project);
    }
    Ok(projects)
}

fn projects_matching_canonical_path(
    projects: &[RunnerProjectFile],
    canonical_path: &Path,
) -> Vec<RunnerProjectFile> {
    projects
        .iter()
        .filter_map(|project| {
            let registered_path = canonicalize_existing(Path::new(&project.path)).ok()?;
            (registered_path.is_dir() && paths_equal(&registered_path, canonical_path))
                .then(|| project.clone())
        })
        .collect()
}

fn bounded_project_name(canonical_path: &Path) -> String {
    let raw = canonical_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Project")
        .trim();
    let mut name = String::new();
    for character in raw.chars() {
        if name.len() + character.len_utf8() > 120 {
            break;
        }
        name.push(character);
    }
    if name.is_empty() {
        "Project".to_string()
    } else {
        name
    }
}

fn sanitized_project_basename(canonical_path: &Path) -> String {
    let raw = canonical_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let mut sanitized = String::new();
    let mut separator_pending = false;
    for character in raw.chars() {
        if character.is_ascii_alphanumeric() {
            if separator_pending && !sanitized.is_empty() {
                sanitized.push('-');
            }
            sanitized.push(character.to_ascii_lowercase());
            separator_pending = false;
        } else {
            separator_pending = true;
        }
    }
    if sanitized.is_empty() {
        "project".to_string()
    } else {
        sanitized
    }
}

fn canonical_project_path_hash(canonical_path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        format!(
            "{:x}",
            Sha256::digest(canonical_path.as_os_str().as_bytes())
        )
    }
    #[cfg(not(unix))]
    format!(
        "{:x}",
        Sha256::digest(
            webcodex_runner_config::paths::normalize_path_identity(canonical_path).as_bytes()
        )
    )
}

fn auto_project_id_candidate(
    canonical_path: &Path,
    hash_prefix_length: usize,
) -> Result<String, &'static str> {
    let digest = canonical_project_path_hash(canonical_path);
    let hash_prefix = digest
        .get(..hash_prefix_length.min(digest.len()))
        .ok_or("project_id_collision")?;
    let max_basename_length = 64usize.saturating_sub(hash_prefix.len() + 1);
    if max_basename_length == 0 {
        return Err("project_id_collision");
    }
    let basename = sanitized_project_basename(canonical_path);
    let basename = basename
        .chars()
        .take(max_basename_length)
        .collect::<String>();
    let candidate = format!("{basename}-{hash_prefix}");
    validate_project_op_id(&candidate).map_err(|_| "project_id_collision")?;
    Ok(candidate)
}

fn choose_auto_project_id(
    project_registry_dir: &Path,
    projects: &[RunnerProjectFile],
    canonical_path: &Path,
) -> Result<String, &'static str> {
    let configured_ids = projects
        .iter()
        .map(|project| project.id.as_str())
        .collect::<HashSet<_>>();
    for &prefix_length in AUTO_PROJECT_HASH_PREFIX_LENGTHS {
        let candidate = auto_project_id_candidate(canonical_path, prefix_length)?;
        if configured_ids.contains(candidate.as_str())
            || project_registry_dir
                .join(format!("{candidate}.toml"))
                .exists()
        {
            continue;
        }
        return Ok(candidate);
    }
    Err("project_id_collision")
}

fn path_resolution_success(
    client_id: &str,
    project: &RunnerProjectFile,
    canonical_path: &Path,
    outcome: &'static str,
    registered: bool,
    project_record_path: Option<&Path>,
) -> serde_json::Value {
    serde_json::json!({
        "id": format!("agent:{}:{}", client_id, project.id),
        "agent_project_id": project.id,
        "client_id": client_id,
        "name": project.name,
        "path": canonical_path.to_string_lossy(),
        "kind": project_wire_kind(project),
        "registration_source": effective_registration_source(project).as_str(),
        "description": project.description,
        "allow_patch": project.allow_patch,
        "disabled": project.disabled,
        "revision": project_revision(project),
        "source": "path",
        "outcome": outcome,
        "registered": registered,
        "created_config": registered,
        "changed": registered,
        "recovered": !registered,
        "project_record_path": project_record_path.map(|path| path.to_string_lossy().to_string()),
        "projects_config_path": project_record_path.map(|path| path.to_string_lossy().to_string()),
    })
}

fn existing_path_resolution_result(
    start: Instant,
    client_id: &str,
    canonical_path: &Path,
    matches: Vec<RunnerProjectFile>,
) -> Option<CommandResult> {
    if matches.len() > 1 {
        let mut matching_project_ids = matches
            .iter()
            .map(|project| project.id.clone())
            .collect::<Vec<_>>();
        matching_project_ids.sort();
        matching_project_ids.dedup();
        return Some(structured_project_error_cmd(
            start,
            "ambiguous_project_path",
            false,
            serde_json::json!({"matching_project_ids": matching_project_ids}),
        ));
    }
    let project = matches.into_iter().next()?;
    if project.disabled {
        return Some(structured_project_error_cmd(
            start,
            "project_disabled",
            false,
            serde_json::json!({"matching_project_id": project.id}),
        ));
    }
    Some(ok_cmd(
        start,
        path_resolution_success(
            client_id,
            &project,
            canonical_path,
            "reused_existing_registration",
            false,
            None,
        ),
    ))
}

/// Resolve an existing Runner registration by canonical path or atomically
/// persist a new one. This is an internal Server↔Runner operation, not a
/// model-visible runtime tool.
pub(crate) fn handle_resolve_or_register_project_operation(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    client_id: &str,
    operation: &RunnerProjectOperation,
) -> CommandResult {
    let start = Instant::now();
    let _registry_guard = match project_registry_write_lock().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return structured_project_error_cmd(
                start,
                "operation_failed",
                false,
                serde_json::json!({}),
            )
        }
    };
    let payload = match serde_json::from_str::<serde_json::Value>(&operation.payload)
        .ok()
        .and_then(|payload| payload.as_object().cloned())
    {
        Some(payload) => payload,
        None => {
            return structured_project_error_cmd(
                start,
                "invalid_request",
                false,
                serde_json::json!({}),
            )
        }
    };
    if payload.len() != 1 {
        return structured_project_error_cmd(
            start,
            "invalid_request",
            false,
            serde_json::json!({}),
        );
    }
    let path = match payload.get("path").and_then(serde_json::Value::as_str) {
        Some(path) if !path.is_empty() && !path.contains('\0') && Path::new(path).is_absolute() => {
            path
        }
        _ => {
            return structured_project_error_cmd(
                start,
                "invalid_project_path",
                false,
                serde_json::json!({"field": "path"}),
            )
        }
    };
    // Reject unsupported namespaces before filesystem access. Raw UNC/VerbatimUNC
    // inputs must also be covered by pre-existing Runner authority before they may
    // trigger SMB I/O; local CLI/Desktop onboarding owns any authority extension.
    if let Err(error_kind) = validate_windows_project_root(Path::new(path)) {
        return structured_project_error_cmd(
            start,
            error_kind,
            false,
            serde_json::json!({"field": "path"}),
        );
    }
    if let Err(error_kind) =
        validate_model_network_project_ingress_authority(policy, Path::new(path))
    {
        return structured_project_error_cmd(
            start,
            error_kind,
            false,
            serde_json::json!({"field": "path"}),
        );
    }
    let canonical_path = match canonicalize_existing(Path::new(path)) {
        Ok(path) => path,
        Err(_) => {
            return structured_project_error_cmd(
                start,
                "project_path_not_found",
                false,
                serde_json::json!({"field": "path"}),
            )
        }
    };
    // Re-check the canonical form so canonicalization cannot introduce a device
    // or other unsupported Windows namespace. VerbatimUNC remains supported.
    if let Err(error_kind) = validate_windows_project_root(&canonical_path) {
        return structured_project_error_cmd(
            start,
            error_kind,
            false,
            serde_json::json!({"field": "path"}),
        );
    }
    if !canonical_path.is_dir() {
        return structured_project_error_cmd(
            start,
            "project_path_not_directory",
            false,
            serde_json::json!({"field": "path"}),
        );
    }
    if canonical_path.to_str().is_none() {
        return structured_project_error_cmd(
            start,
            "invalid_project_path",
            false,
            serde_json::json!({"field": "path"}),
        );
    }
    if validate_project_path_policy(policy, &canonical_path).is_err() {
        return structured_project_error_cmd(
            start,
            "path_outside_allowed_roots",
            false,
            serde_json::json!({"field": "path"}),
        );
    }

    let projects = match load_project_files_for_path_resolution(project_registry_dir) {
        Ok(projects) => projects,
        Err(error_kind) => {
            return structured_project_error_cmd(start, error_kind, false, serde_json::json!({}))
        }
    };
    let matches = projects_matching_canonical_path(&projects, &canonical_path);
    if let Some(result) =
        existing_path_resolution_result(start, client_id, &canonical_path, matches)
    {
        return result;
    }

    let project_id = match choose_auto_project_id(project_registry_dir, &projects, &canonical_path)
    {
        Ok(project_id) => project_id,
        Err(error_kind) => {
            return structured_project_error_cmd(start, error_kind, false, serde_json::json!({}))
        }
    };
    let canonical_path_string = canonical_path
        .to_str()
        .expect("validated UTF-8 canonical project path")
        .to_string();
    let name = bounded_project_name(&canonical_path);
    let description = None;
    let toml_content = build_project_toml_with_registration_source(
        &project_id,
        &name,
        &canonical_path_string,
        Some(AUTO_REGISTERED_REGISTRATION_SOURCE),
        &description,
        true,
    );
    let write_result =
        match write_project_toml_atomic(project_registry_dir, &project_id, &toml_content, false) {
            Ok(result) => result,
            Err(ProjectTomlWriteError::BeforeRename) => {
                // A different process may have won publication. Rescan under
                // our process-local lock and converge if it registered the
                // same canonical directory.
                if let Ok(projects) = load_project_files_for_path_resolution(project_registry_dir) {
                    let matches = projects_matching_canonical_path(&projects, &canonical_path);
                    if let Some(result) =
                        existing_path_resolution_result(start, client_id, &canonical_path, matches)
                    {
                        return result;
                    }
                }
                return structured_project_error_cmd(
                    start,
                    "operation_failed",
                    false,
                    serde_json::json!({}),
                );
            }
            Err(ProjectTomlWriteError::AfterRename) => {
                return structured_project_error_cmd(
                    start,
                    "operation_indeterminate",
                    true,
                    serde_json::json!({}),
                )
            }
        };
    let project = match parse_runner_project_toml(&toml_content) {
        Ok(project) => project,
        Err(_) => {
            return structured_project_error_cmd(
                start,
                "operation_indeterminate",
                true,
                serde_json::json!({}),
            )
        }
    };
    ok_cmd(
        start,
        path_resolution_success(
            client_id,
            &project,
            &canonical_path,
            "auto_registered",
            true,
            Some(&write_result.config_path),
        ),
    )
}

fn managed_worktree_error(
    start: Instant,
    error_kind: &'static str,
    state_changed: bool,
    base_ref: Option<&str>,
    base_sha: Option<&str>,
    source_dirty: Option<bool>,
) -> CommandResult {
    structured_project_error_cmd(
        start,
        error_kind,
        state_changed,
        serde_json::json!({
            "base_ref": base_ref,
            "base_sha": base_sha,
            "source_dirty": source_dirty,
        }),
    )
}

fn managed_worktree_git_text(path: &Path, args: &[&str]) -> Result<String, &'static str> {
    let output = run_git_bounded(path, args, MANAGED_WORKTREE_GIT_TIMEOUT, None)
        .map_err(|_| "worktree_git_failed")?;
    if !output.status.success() || output.stdout_capped || output.stderr_capped {
        return Err("worktree_git_failed");
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_string())
        .map_err(|_| "worktree_git_failed")
}

fn exact_git_commit(source: &Path, base_ref: &str) -> Result<String, &'static str> {
    let commit_ref = format!("{base_ref}^{{commit}}");
    let output = run_git_bounded(
        source,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            commit_ref.as_str(),
        ],
        MANAGED_WORKTREE_GIT_TIMEOUT,
        None,
    )
    .map_err(|_| "base_ref_resolution_failed")?;
    if !output.status.success() || output.stdout_capped {
        return Err("base_ref_resolution_failed");
    }
    let sha = String::from_utf8(output.stdout)
        .map_err(|_| "base_ref_resolution_failed")?
        .trim()
        .to_string();
    if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("base_ref_resolution_failed");
    }
    Ok(sha.to_ascii_lowercase())
}

fn choose_managed_worktree_root(
    policy: &RunnerPolicy,
    source: &Path,
) -> Result<PathBuf, &'static str> {
    let mut roots =
        webcodex_runner_config::paths::canonicalize_usable_allowed_roots(&policy.allowed_roots);
    roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
    for root in roots {
        if !webcodex_runner_config::paths::path_is_within(source, &root) {
            continue;
        }
        let candidate = root.join(".webcodex-managed-worktrees");
        if webcodex_runner_config::paths::path_is_within(&candidate, source) {
            continue;
        }
        return Ok(candidate);
    }
    if policy.allow_cwd_anywhere {
        if let Some(parent) = source.parent() {
            let candidate = parent.join(".webcodex-managed-worktrees");
            if !webcodex_runner_config::paths::path_is_within(&candidate, source) {
                return Ok(candidate);
            }
        }
    }
    Err("managed_worktree_root_unavailable")
}

/// Render an already-authorized managed-worktree destination for Git's CLI.
///
/// Rust canonicalization commonly returns `\\?\C:\...` on Windows. Win32
/// filesystem APIs accept that identity, but Git for Windows does not reliably
/// accept the verbatim-disk spelling as a `git worktree add` destination. Keep
/// canonical PathBuf values for policy, registry, recovery, and identity checks;
/// only the child-process argv gets the equivalent ordinary local-disk spelling.
fn managed_worktree_git_cli_path(path: &Path) -> String {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};

        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next() {
            if let Prefix::VerbatimDisk(drive) = prefix.kind() {
                let mut rendered = PathBuf::from(format!("{}:\\", char::from(drive)));
                for component in components {
                    match component {
                        Component::RootDir => {}
                        Component::Normal(part) => rendered.push(part),
                        _ => return path.to_string_lossy().into_owned(),
                    }
                }
                return rendered.to_string_lossy().into_owned();
            }
        }
    }
    path.to_string_lossy().into_owned()
}

fn source_mentions_worktree_path(source: &Path, worktree: &Path) -> Result<bool, &'static str> {
    let listing = managed_worktree_git_text(source, &["worktree", "list", "--porcelain"])?;
    Ok(listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|path| paths_equal(Path::new(path), worktree)))
}

fn managed_worktree_project_toml(
    id: &str,
    name: &str,
    worktree: &str,
    source: &str,
    base_ref: &str,
    base_sha: &str,
    operation_id: &str,
) -> String {
    let mut content = build_project_toml_with_registration_source(
        id,
        name,
        worktree,
        Some(AUTO_REGISTERED_REGISTRATION_SOURCE),
        &None,
        true,
    );
    content.push_str("managed_worktree = true\n");
    content.push_str(&format!("managed_source = {}\n", toml_basic_string(source)));
    content.push_str(&format!(
        "managed_base_ref = {}\n",
        toml_basic_string(base_ref)
    ));
    content.push_str(&format!(
        "managed_base_sha = {}\n",
        toml_basic_string(base_sha)
    ));
    content.push_str(&format!(
        "managed_operation_id = {}\n",
        toml_basic_string(operation_id)
    ));
    content
}

fn managed_worktree_success(
    start: Instant,
    client_id: &str,
    project: &RunnerProjectFile,
    worktree: &Path,
    base_ref: &str,
    base_sha: &str,
    source_dirty: bool,
    outcome: &'static str,
    registered: bool,
    changed: bool,
) -> CommandResult {
    ok_cmd(
        start,
        serde_json::json!({
            "id": format!("agent:{}:{}", client_id, project.id),
            "agent_project_id": project.id,
            "client_id": client_id,
            "name": project.name,
            "path": worktree.to_string_lossy(),
            "kind": project_wire_kind(project),
            "registration_source": effective_registration_source(project).as_str(),
            "description": project.description,
            "allow_patch": project.allow_patch,
            "disabled": project.disabled,
            "revision": project_revision(project),
            "source": "managed_worktree",
            "outcome": outcome,
            "registered": registered,
            "created_config": registered,
            "changed": changed,
            "recovered": outcome == "managed_worktree_recovered",
            "managed": true,
            "base_ref": base_ref,
            "base_sha": base_sha,
            "source_dirty": source_dirty,
        }),
    )
}

fn resume_managed_worktree(
    start: Instant,
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    client_id: &str,
    source_root: &Path,
    source_dirty: bool,
    requested_base_ref: Option<&str>,
    resume_project_id: &str,
) -> CommandResult {
    if validate_project_op_id(resume_project_id).is_err() {
        return managed_worktree_error(
            start,
            "invalid_request",
            false,
            requested_base_ref,
            None,
            Some(source_dirty),
        );
    }
    let projects = match load_project_files_for_path_resolution(project_registry_dir) {
        Ok(projects) => projects,
        Err(error) => {
            return managed_worktree_error(
                start,
                error,
                false,
                requested_base_ref,
                None,
                Some(source_dirty),
            )
        }
    };
    let Some(project) = projects
        .into_iter()
        .find(|project| project.id == resume_project_id)
    else {
        return managed_worktree_error(
            start,
            "managed_worktree_resume_mismatch",
            false,
            requested_base_ref,
            None,
            Some(source_dirty),
        );
    };
    let stored_base_sha = project.managed_base_sha.as_deref().filter(|sha| {
        matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    let stored_source = project
        .managed_source
        .as_deref()
        .and_then(|path| canonicalize_existing(Path::new(path)).ok());
    if project.disabled
        || !project.managed_worktree
        || stored_base_sha.is_none()
        || stored_source
            .as_ref()
            .is_none_or(|source| !paths_equal(source, source_root))
    {
        return managed_worktree_error(
            start,
            "managed_worktree_resume_mismatch",
            false,
            requested_base_ref,
            stored_base_sha,
            Some(source_dirty),
        );
    }
    let base_sha = stored_base_sha.expect("validated managed base SHA");
    if let Some(base_ref) = requested_base_ref {
        match exact_git_commit(source_root, base_ref) {
            Ok(resolved) if resolved == base_sha => {}
            Ok(_) => {
                return managed_worktree_error(
                    start,
                    "managed_worktree_resume_base_mismatch",
                    false,
                    Some(base_ref),
                    Some(base_sha),
                    Some(source_dirty),
                )
            }
            Err(error) => {
                return managed_worktree_error(
                    start,
                    error,
                    false,
                    Some(base_ref),
                    Some(base_sha),
                    Some(source_dirty),
                )
            }
        }
    }
    let worktree = match canonicalize_existing(Path::new(&project.path)) {
        Ok(worktree) => worktree,
        Err(_) => {
            return managed_worktree_error(
                start,
                "managed_worktree_recovery_conflict",
                false,
                project.managed_base_ref.as_deref(),
                Some(base_sha),
                Some(source_dirty),
            )
        }
    };
    if validate_windows_project_root(&worktree).is_err()
        || validate_project_path_policy(policy, &worktree).is_err()
    {
        return managed_worktree_error(
            start,
            "managed_worktree_recovery_conflict",
            false,
            project.managed_base_ref.as_deref(),
            Some(base_sha),
            Some(source_dirty),
        );
    }
    let head = match managed_worktree_git_text(&worktree, &["rev-parse", "HEAD"]) {
        Ok(head) => head,
        Err(error) => {
            return managed_worktree_error(
                start,
                error,
                false,
                project.managed_base_ref.as_deref(),
                Some(base_sha),
                Some(source_dirty),
            )
        }
    };
    let listed = match source_mentions_worktree_path(source_root, &worktree) {
        Ok(listed) => listed,
        Err(error) => {
            return managed_worktree_error(
                start,
                error,
                false,
                project.managed_base_ref.as_deref(),
                Some(base_sha),
                Some(source_dirty),
            )
        }
    };
    if head != base_sha || !listed {
        return managed_worktree_error(
            start,
            "managed_worktree_recovery_conflict",
            false,
            project.managed_base_ref.as_deref(),
            Some(base_sha),
            Some(source_dirty),
        );
    }
    let projected_base_ref = project
        .managed_base_ref
        .as_deref()
        .unwrap_or(requested_base_ref.unwrap_or("HEAD"));
    managed_worktree_success(
        start,
        client_id,
        &project,
        &worktree,
        projected_base_ref,
        base_sha,
        source_dirty,
        "managed_worktree_recovered",
        false,
        false,
    )
}

/// Internal Server↔Runner operation that owns Git/ref/path semantics for managed
/// worktrees. It is intentionally not model-visible; successful output is fed
/// back through the ordinary registered runtime Project authority path.
pub(crate) fn handle_prepare_managed_worktree_operation(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    client_id: &str,
    operation: &RunnerProjectOperation,
) -> CommandResult {
    let start = Instant::now();
    let _registry_guard = match project_registry_write_lock().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return managed_worktree_error(start, "operation_failed", false, None, None, None)
        }
    };
    let Some(payload) = serde_json::from_str::<serde_json::Value>(&operation.payload)
        .ok()
        .and_then(|payload| payload.as_object().cloned())
    else {
        return managed_worktree_error(start, "invalid_request", false, None, None, None);
    };
    if payload.len() != 4 {
        return managed_worktree_error(start, "invalid_request", false, None, None, None);
    }
    let Some(path) = payload
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.is_empty() && !path.contains('\0') && Path::new(path).is_absolute())
    else {
        return managed_worktree_error(start, "invalid_project_path", false, None, None, None);
    };
    let requested_base_ref = match payload.get("base_ref") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value))
            if !value.trim().is_empty() && value.len() <= 1024 && !value.contains('\0') =>
        {
            Some(value.clone())
        }
        _ => return managed_worktree_error(start, "invalid_base_ref", false, None, None, None),
    };
    let base_ref = requested_base_ref
        .clone()
        .unwrap_or_else(|| "HEAD".to_string());
    let resume_project_id = match payload.get("resume_project_id") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) if !value.trim().is_empty() => Some(value.as_str()),
        _ => {
            return managed_worktree_error(
                start,
                "invalid_request",
                false,
                Some(&base_ref),
                None,
                None,
            )
        }
    };
    let Some(operation_id) = payload
        .get("operation_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| uuid::Uuid::parse_str(value).is_ok())
    else {
        return managed_worktree_error(
            start,
            "invalid_request",
            false,
            Some(&base_ref),
            None,
            None,
        );
    };
    if validate_windows_project_root(Path::new(path)).is_err() {
        return managed_worktree_error(
            start,
            "invalid_project_path",
            false,
            Some(&base_ref),
            None,
            None,
        );
    }
    if let Err(error_kind) =
        validate_model_network_project_ingress_authority(policy, Path::new(path))
    {
        return managed_worktree_error(start, error_kind, false, Some(&base_ref), None, None);
    }
    let source = match canonicalize_existing(Path::new(path)) {
        Ok(source) if source.is_dir() && source.to_str().is_some() => source,
        _ => {
            return managed_worktree_error(
                start,
                "project_path_not_found",
                false,
                Some(&base_ref),
                None,
                None,
            )
        }
    };
    if validate_windows_project_root(&source).is_err()
        || validate_project_path_policy(policy, &source).is_err()
    {
        return managed_worktree_error(
            start,
            "path_outside_allowed_roots",
            false,
            Some(&base_ref),
            None,
            None,
        );
    }
    let source_root = match managed_worktree_git_text(&source, &["rev-parse", "--show-toplevel"])
        .ok()
        .and_then(|root| canonicalize_existing(Path::new(&root)).ok())
    {
        Some(root) if paths_equal(&root, &source) => root,
        _ => {
            return managed_worktree_error(
                start,
                "source_not_git_repository",
                false,
                Some(&base_ref),
                None,
                None,
            )
        }
    };
    let source_dirty = match managed_worktree_git_text(&source_root, &["status", "--porcelain"]) {
        Ok(status) => !status.is_empty(),
        Err(_) => {
            return managed_worktree_error(
                start,
                "source_git_status_failed",
                false,
                Some(&base_ref),
                None,
                None,
            )
        }
    };
    if let Some(resume_project_id) = resume_project_id {
        return resume_managed_worktree(
            start,
            policy,
            project_registry_dir,
            client_id,
            &source_root,
            source_dirty,
            requested_base_ref.as_deref(),
            resume_project_id,
        );
    }
    let base_sha = match exact_git_commit(&source_root, &base_ref) {
        Ok(sha) => sha,
        Err(error) => {
            return managed_worktree_error(
                start,
                error,
                false,
                Some(&base_ref),
                None,
                Some(source_dirty),
            )
        }
    };
    let managed_root = match choose_managed_worktree_root(policy, &source_root) {
        Ok(root) => root,
        Err(error) => {
            return managed_worktree_error(
                start,
                error,
                false,
                Some(&base_ref),
                Some(&base_sha),
                Some(source_dirty),
            )
        }
    };
    if std::fs::create_dir_all(&managed_root).is_err() {
        return managed_worktree_error(
            start,
            "worktree_creation_failed",
            false,
            Some(&base_ref),
            Some(&base_sha),
            Some(source_dirty),
        );
    }
    let managed_root = match canonicalize_existing(&managed_root) {
        Ok(root) => root,
        Err(_) => {
            return managed_worktree_error(
                start,
                "worktree_creation_failed",
                false,
                Some(&base_ref),
                Some(&base_sha),
                Some(source_dirty),
            )
        }
    };
    if validate_project_path_policy(policy, &managed_root).is_err() {
        return managed_worktree_error(
            start,
            "managed_worktree_root_unavailable",
            false,
            Some(&base_ref),
            Some(&base_sha),
            Some(source_dirty),
        );
    }
    let destination = managed_root.join(format!(
        "{}-{}",
        sanitized_project_basename(&source_root),
        operation_id
    ));
    let mut created_worktree = false;
    let canonical_worktree = if destination.exists() {
        let Ok(existing) = canonicalize_existing(&destination) else {
            return managed_worktree_error(
                start,
                "managed_worktree_recovery_conflict",
                true,
                Some(&base_ref),
                Some(&base_sha),
                Some(source_dirty),
            );
        };
        let head = match managed_worktree_git_text(&existing, &["rev-parse", "HEAD"]) {
            Ok(head) => head,
            Err(_) => {
                return managed_worktree_error(
                    start,
                    "operation_indeterminate",
                    true,
                    Some(&base_ref),
                    Some(&base_sha),
                    Some(source_dirty),
                )
            }
        };
        let listed = match source_mentions_worktree_path(&source_root, &existing) {
            Ok(listed) => listed,
            Err(_) => {
                return managed_worktree_error(
                    start,
                    "operation_indeterminate",
                    true,
                    Some(&base_ref),
                    Some(&base_sha),
                    Some(source_dirty),
                )
            }
        };
        if head != base_sha || !listed {
            return managed_worktree_error(
                start,
                "managed_worktree_recovery_conflict",
                true,
                Some(&base_ref),
                Some(&base_sha),
                Some(source_dirty),
            );
        }
        existing
    } else {
        match source_mentions_worktree_path(&source_root, &destination) {
            Ok(true) => {
                return managed_worktree_error(
                    start,
                    "managed_worktree_recovery_conflict",
                    true,
                    Some(&base_ref),
                    Some(&base_sha),
                    Some(source_dirty),
                )
            }
            Ok(false) => {}
            Err(_) => {
                return managed_worktree_error(
                    start,
                    "operation_indeterminate",
                    false,
                    Some(&base_ref),
                    Some(&base_sha),
                    Some(source_dirty),
                )
            }
        }
        let destination_string = managed_worktree_git_cli_path(&destination);
        let add_result = run_git_bounded(
            &source_root,
            &[
                "worktree",
                "add",
                "--detach",
                destination_string.as_str(),
                base_sha.as_str(),
            ],
            MANAGED_WORKTREE_GIT_TIMEOUT,
            None,
        );
        match &add_result {
            Ok(output) if output.status.success() => created_worktree = true,
            Ok(_) if !destination.exists() => {
                return match source_mentions_worktree_path(&source_root, &destination) {
                    Ok(false) => managed_worktree_error(
                        start,
                        "worktree_creation_failed",
                        false,
                        Some(&base_ref),
                        Some(&base_sha),
                        Some(source_dirty),
                    ),
                    Ok(true) => managed_worktree_error(
                        start,
                        "managed_worktree_recovery_conflict",
                        true,
                        Some(&base_ref),
                        Some(&base_sha),
                        Some(source_dirty),
                    ),
                    Err(_) => managed_worktree_error(
                        start,
                        "operation_indeterminate",
                        false,
                        Some(&base_ref),
                        Some(&base_sha),
                        Some(source_dirty),
                    ),
                };
            }
            Err(_) if !destination.exists() => {
                return match source_mentions_worktree_path(&source_root, &destination) {
                    Ok(true) => managed_worktree_error(
                        start,
                        "managed_worktree_recovery_conflict",
                        true,
                        Some(&base_ref),
                        Some(&base_sha),
                        Some(source_dirty),
                    ),
                    Ok(false) | Err(_) => managed_worktree_error(
                        start,
                        "operation_indeterminate",
                        false,
                        Some(&base_ref),
                        Some(&base_sha),
                        Some(source_dirty),
                    ),
                };
            }
            Ok(_) | Err(_) => {}
        }
        let worktree = match canonicalize_existing(&destination) {
            Ok(worktree) => worktree,
            Err(_) => {
                return managed_worktree_error(
                    start,
                    "operation_indeterminate",
                    true,
                    Some(&base_ref),
                    Some(&base_sha),
                    Some(source_dirty),
                )
            }
        };
        let head = match managed_worktree_git_text(&worktree, &["rev-parse", "HEAD"]) {
            Ok(head) => head,
            Err(_) => {
                return managed_worktree_error(
                    start,
                    "operation_indeterminate",
                    true,
                    Some(&base_ref),
                    Some(&base_sha),
                    Some(source_dirty),
                )
            }
        };
        let listed = match source_mentions_worktree_path(&source_root, &worktree) {
            Ok(listed) => listed,
            Err(_) => {
                return managed_worktree_error(
                    start,
                    "operation_indeterminate",
                    true,
                    Some(&base_ref),
                    Some(&base_sha),
                    Some(source_dirty),
                )
            }
        };
        if head != base_sha || !listed {
            return managed_worktree_error(
                start,
                "managed_worktree_recovery_conflict",
                true,
                Some(&base_ref),
                Some(&base_sha),
                Some(source_dirty),
            );
        }
        worktree
    };
    if validate_project_path_policy(policy, &canonical_worktree).is_err() {
        return managed_worktree_error(
            start,
            "managed_worktree_root_unavailable",
            true,
            Some(&base_ref),
            Some(&base_sha),
            Some(source_dirty),
        );
    }
    let projects = match load_project_files_for_path_resolution(project_registry_dir) {
        Ok(projects) => projects,
        Err(error) => {
            return managed_worktree_error(
                start,
                error,
                true,
                Some(&base_ref),
                Some(&base_sha),
                Some(source_dirty),
            )
        }
    };
    let matches = projects_matching_canonical_path(&projects, &canonical_worktree);
    if matches.len() > 1 {
        return managed_worktree_error(
            start,
            "ambiguous_project_path",
            true,
            Some(&base_ref),
            Some(&base_sha),
            Some(source_dirty),
        );
    }
    if let Some(project) = matches.into_iter().next() {
        let same_operation = project.managed_worktree
            && project.managed_operation_id.as_deref() == Some(operation_id)
            && project.managed_base_sha.as_deref() == Some(base_sha.as_str())
            && project.managed_source.as_deref() == source_root.to_str();
        if project.disabled || !same_operation {
            return managed_worktree_error(
                start,
                "managed_worktree_recovery_conflict",
                true,
                Some(&base_ref),
                Some(&base_sha),
                Some(source_dirty),
            );
        }
        return managed_worktree_success(
            start,
            client_id,
            &project,
            &canonical_worktree,
            &base_ref,
            &base_sha,
            source_dirty,
            "managed_worktree_recovered",
            false,
            false,
        );
    }
    let project_id =
        match choose_auto_project_id(project_registry_dir, &projects, &canonical_worktree) {
            Ok(id) => id,
            Err(error) => {
                return managed_worktree_error(
                    start,
                    error,
                    true,
                    Some(&base_ref),
                    Some(&base_sha),
                    Some(source_dirty),
                )
            }
        };
    let Some(worktree_string) = canonical_worktree.to_str() else {
        return managed_worktree_error(
            start,
            "invalid_project_path",
            true,
            Some(&base_ref),
            Some(&base_sha),
            Some(source_dirty),
        );
    };
    let source_string = source_root.to_str().expect("validated UTF-8 source path");
    let content = managed_worktree_project_toml(
        &project_id,
        &bounded_project_name(&canonical_worktree),
        worktree_string,
        source_string,
        &base_ref,
        &base_sha,
        operation_id,
    );
    match write_project_toml_atomic(project_registry_dir, &project_id, &content, false) {
        Ok(_) => {}
        Err(ProjectTomlWriteError::BeforeRename) => {
            return managed_worktree_error(
                start,
                "worktree_registration_failed",
                true,
                Some(&base_ref),
                Some(&base_sha),
                Some(source_dirty),
            )
        }
        Err(ProjectTomlWriteError::AfterRename) => {
            return managed_worktree_error(
                start,
                "operation_indeterminate",
                true,
                Some(&base_ref),
                Some(&base_sha),
                Some(source_dirty),
            )
        }
    }
    let project = match parse_runner_project_toml(&content) {
        Ok(project) => project,
        Err(_) => {
            return managed_worktree_error(
                start,
                "operation_indeterminate",
                true,
                Some(&base_ref),
                Some(&base_sha),
                Some(source_dirty),
            )
        }
    };
    managed_worktree_success(
        start,
        client_id,
        &project,
        &canonical_worktree,
        &base_ref,
        &base_sha,
        source_dirty,
        if created_worktree {
            "managed_worktree_created"
        } else {
            "managed_worktree_recovered"
        },
        true,
        true,
    )
}

fn lifecycle_config_path(project_registry_dir: &Path, id: &str) -> Result<PathBuf, String> {
    validate_project_op_id(id)?;
    let canonical_dir = canonicalize_existing(project_registry_dir)?;
    let path = canonical_dir.join(format!("{id}.toml"));
    if !path.starts_with(&canonical_dir) {
        return Err("project config path would escape project_registry_dir".to_string());
    }
    Ok(path)
}

fn write_existing_project_atomic(path: &Path, content: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "project config has no parent".to_string())?;
    let id = path
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("project");
    let temp = unique_registry_temp(dir, id, "toml.tmp");
    let result = (|| {
        let mut file = std::fs::File::create(&temp)
            .map_err(|e| format!("failed to create lifecycle temp file: {e}"))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("failed to write lifecycle temp file: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("failed to sync lifecycle temp file: {e}"))?;
        std::fs::rename(&temp, path)
            .map_err(|e| format!("failed to atomically replace project config: {e}"))?;
        sync_parent_dir(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn cleanup_unregister_tombstones(project_registry_dir: &Path, id: &str) -> Result<(), String> {
    let prefix = format!(".{id}.");
    let suffix = ".toml.unregistering";
    let mut changed = false;
    for entry in std::fs::read_dir(project_registry_dir)
        .map_err(|e| format!("failed to inspect project registry tombstones: {e}"))?
    {
        let entry = entry.map_err(|e| format!("failed to inspect project registry entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(suffix) {
            std::fs::remove_file(entry.path())
                .map_err(|e| format!("failed to remove stale unregister tombstone: {e}"))?;
            changed = true;
        }
    }
    if changed {
        sync_dir(project_registry_dir)?;
    }
    Ok(())
}

fn unregister_project_config(path: &Path) -> Result<(), ProjectUnregisterError> {
    let dir = path.parent().ok_or(ProjectUnregisterError::BeforeRename)?;
    let id = path
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("project");
    let tombstone = unique_registry_temp(dir, id, "toml.unregistering");
    std::fs::rename(path, &tombstone).map_err(|_| ProjectUnregisterError::BeforeRename)?;
    sync_project_parent_after_rename(path).map_err(|_| ProjectUnregisterError::AfterRename)?;
    std::fs::remove_file(&tombstone).map_err(|_| ProjectUnregisterError::AfterRename)?;
    sync_project_parent_after_rename(path).map_err(|_| ProjectUnregisterError::AfterRename)
}

/// Structured, non-shell project lifecycle mutation. Unregister only removes
/// the registry TOML and never touches the project path or Git data.
pub(crate) fn handle_project_lifecycle_operation(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    operation: &RunnerProjectOperation,
) -> CommandResult {
    let _registry_guard = match project_registry_write_lock().lock() {
        Ok(guard) => guard,
        Err(_) => return project_error_cmd(Instant::now(), "operation_failed"),
    };
    let start = Instant::now();
    let action = match operation.kind {
        RunnerProjectOperationKind::LifecycleEnable => "enable",
        RunnerProjectOperationKind::LifecycleDisable => "disable",
        RunnerProjectOperationKind::LifecycleUnregister => "unregister",
        _ => return project_error_cmd(start, "unsupported_runner_version"),
    };
    let payload: serde_json::Value = match serde_json::from_str(&operation.payload).ok() {
        Some(v) => v,
        None => return project_error_cmd(start, "invalid_request"),
    };
    let id = match payload.get("project_id").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return project_error_cmd(start, "invalid_request"),
    };
    let expected_revision = match payload.get("expected_revision").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return project_error_cmd(start, "invalid_request"),
    };
    let config_path = match lifecycle_config_path(project_registry_dir, id) {
        Ok(v) => v,
        Err(e) => return err_cmd(start, e),
    };
    if !config_path.exists() {
        if action == "unregister" {
            if cleanup_unregister_tombstones(project_registry_dir, id).is_err() {
                return project_error_cmd(start, "operation_failed");
            }
            return ok_cmd(
                start,
                serde_json::json!({
                    "operation": action, "agent_project_id": id,
                    "outcome": "already_unregistered", "changed": false,
                    "revision": serde_json::Value::Null
                }),
            );
        }
        return project_error_cmd(start, "project_not_found");
    }
    let content = match std::fs::read_to_string(&config_path) {
        Ok(v) => v,
        Err(_) => return project_error_cmd(start, "operation_failed"),
    };
    let mut project = match parse_runner_project_toml(&content) {
        Ok(v) => v,
        Err(_) => return project_error_cmd(start, "operation_failed"),
    };
    let current_revision = project_revision(&project);
    let desired_disabled = action == "disable";
    if action != "unregister" && project.disabled == desired_disabled {
        return ok_cmd(
            start,
            serde_json::json!({
                "operation": action, "agent_project_id": id,
                "outcome": if desired_disabled {"already_disabled"} else {"already_enabled"},
                "changed": false, "revision": current_revision,
                "disabled": project.disabled, "path": project.path,
                "name": project.name, "kind": project.kind,
                "registration_source": effective_registration_source(&project).as_str(),
                "description": project.description,
                "allow_patch": project.allow_patch
            }),
        );
    }
    if expected_revision != current_revision {
        return project_error_cmd(start, "revision_conflict");
    }
    if action == "unregister" {
        match unregister_project_config(&config_path) {
            Ok(()) => {}
            Err(ProjectUnregisterError::BeforeRename) => {
                return project_error_cmd(start, "operation_failed")
            }
            Err(ProjectUnregisterError::AfterRename) => {
                return structured_project_error_cmd(
                    start,
                    "operation_indeterminate",
                    true,
                    serde_json::json!({}),
                )
            }
        }
        return ok_cmd(
            start,
            serde_json::json!({
                "operation": action, "agent_project_id": id,
                "outcome": "unregistered", "changed": true,
                "revision": serde_json::Value::Null
            }),
        );
    }
    if !desired_disabled {
        let canonical = match canonicalize_existing(Path::new(&project.path)) {
            Ok(v) if v.is_dir() => v,
            _ => return project_error_cmd(start, "project_not_found"),
        };
        if let Err(error_kind) = validate_windows_project_root(&canonical) {
            return project_error_cmd(start, error_kind);
        }
        if validate_project_path_policy(policy, &canonical).is_err() {
            return project_error_cmd(start, "path_outside_allowed_roots");
        }
    }
    project.disabled = desired_disabled;
    let serialized = match toml::to_string_pretty(&project) {
        Ok(v) => v,
        Err(_) => return project_error_cmd(start, "operation_failed"),
    };
    if write_existing_project_atomic(&config_path, &serialized).is_err() {
        return project_error_cmd(start, "operation_failed");
    }
    let revision = project_revision(&project);
    ok_cmd(
        start,
        serde_json::json!({
            "operation": action, "agent_project_id": id,
            "outcome": if desired_disabled {"disabled"} else {"enabled"},
            "changed": true, "revision": revision,
            "disabled": project.disabled, "path": project.path,
            "name": project.name, "kind": project.kind,
            "registration_source": effective_registration_source(&project).as_str(),
            "description": project.description,
            "allow_patch": project.allow_patch
        }),
    )
}

fn matching_existing_project(
    project_registry_dir: &Path,
    id: &str,
    name: &str,
    path: &str,
    description: Option<&str>,
    allow_patch: bool,
) -> Result<Option<RunnerProjectFile>, &'static str> {
    let config_path = project_registry_dir.join(format!("{id}.toml"));
    if !config_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&config_path).map_err(|_| "operation_failed")?;
    let project = parse_runner_project_toml(&content).map_err(|_| "operation_failed")?;
    let matches = project.id == id
        && paths_equal(Path::new(&project.path), Path::new(path))
        && project.name.as_deref() == Some(name)
        && project.description.as_deref() == description
        && project.allow_patch == allow_patch
        && !project.disabled;
    if matches {
        Ok(Some(project))
    } else {
        Err("project_already_exists")
    }
}

fn validate_recovered_create_side_effects(
    path: &Path,
    template: &str,
    git_init: bool,
) -> Result<(), &'static str> {
    if !path.is_dir() {
        return Err("project_already_exists");
    }
    if git_init && !path.join(".git").is_dir() {
        return Err("project_already_exists");
    }
    if template == "basic"
        && (!path.join("README.md").is_file() || !path.join(".gitignore").is_file())
    {
        return Err("project_already_exists");
    }
    Ok(())
}

fn recovered_project_result(
    create: bool,
    runtime_id: &str,
    client_id: &str,
    project: &RunnerProjectFile,
    template: Option<&str>,
    git_init: bool,
) -> serde_json::Value {
    serde_json::json!({
        "id": runtime_id, "agent_project_id": project.id, "client_id": client_id,
        "name": project.name, "path": project.path, "kind": project.kind,
        "registration_source": effective_registration_source(project).as_str(),
        "description": project.description,
        "created_directory": false, "created_config": false, "overwritten": false,
        "allow_patch": project.allow_patch, "template": template,
        "git_initialized": git_init, "recovered": true, "changed": false,
        "operation": if create { "create" } else { "register" },
        "outcome": if create { "created" } else { "registered" },
        "revision": project_revision(project),
    })
}

/// Handle `register_project` / `create_project` agent requests. Parses the
/// JSON payload from `request.stdin`, validates fields and path against
/// policy, writes `project_registry_dir/<id>.toml` atomically (and for
/// `create_project` creates the directory / templates / optional git init),
/// and returns structured JSON in `CommandResult.stdout`.
pub(crate) fn handle_project_operation(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    client_id: &str,
    operation: &RunnerProjectOperation,
) -> CommandResult {
    let _registry_guard = match project_registry_write_lock().lock() {
        Ok(guard) => guard,
        Err(_) => return project_error_cmd(Instant::now(), "operation_failed"),
    };
    let start = Instant::now();
    let kind = operation.kind.wire_kind();
    let create = operation.kind == RunnerProjectOperationKind::Create;
    let payload = match operation.payload.as_str() {
        s if !s.is_empty() => s,
        _ => {
            return CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("{} request missing stdin payload", kind)),
            };
        }
    };
    let json: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            return CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("failed to parse {} payload: {}", kind, e)),
            };
        }
    };
    if json.get("managed_temporary_project").is_some() {
        return project_error_cmd(start, "managed_temporary_projects_retired");
    }
    let get_str = |key: &str| -> Result<String, String> {
        json.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("{} missing required field '{}'", kind, key))
    };
    let id = match get_str("id") {
        Ok(v) => v,
        Err(e) => return err_cmd(start, e),
    };
    let name = match get_str("name") {
        Ok(v) => v,
        Err(e) => return err_cmd(start, e),
    };
    let path = match get_str("path") {
        Ok(v) => v,
        Err(e) => return err_cmd(start, e),
    };
    let description = json
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let allow_patch = json
        .get("allow_patch")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let overwrite = json
        .get("overwrite")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Err(e) = validate_project_op_id(&id) {
        return err_cmd(start, e);
    }
    if let Err(e) = validate_project_op_name(&name) {
        return err_cmd(start, e);
    }
    if let Some(ref desc) = description {
        if let Err(e) = validate_project_op_description(desc) {
            return err_cmd(start, e);
        }
    }
    // `Path::is_absolute` is platform-correct: drive-letter and UNC paths
    // (`C:\foo`, `\\server\share`) are absolute on Windows; bare `foo` or
    // drive-relative `/foo` are not.
    if path.is_empty() || path.contains('\0') || !Path::new(&path).is_absolute() {
        return err_cmd(start, "path must be a non-empty absolute path".to_string());
    }
    // Existing project registration accepts local disks and network shares;
    // special Windows namespaces still fail before filesystem access.
    if let Err(error_kind) = validate_windows_project_root(Path::new(&path)) {
        return project_error_cmd(start, error_kind);
    }
    if !create {
        if let Err(error_kind) =
            validate_model_network_project_ingress_authority(policy, Path::new(&path))
        {
            return project_error_cmd(start, error_kind);
        }
    }
    #[cfg(windows)]
    if create && webcodex_runner_config::paths::is_windows_network_share_path(Path::new(&path)) {
        // Network project creation is deliberately outside this phase. Existing
        // network directories can be registered when RunnerPolicy already grants them.
        return project_error_cmd(start, "windows_project_path_unsupported");
    }

    let client_id = client_id.to_string();
    let runtime_id = format!("agent:{}:{}", client_id, id);

    let toml_content = build_project_toml(&id, &name, &path, &description, allow_patch);
    let template = json
        .get("template")
        .and_then(|v| v.as_str())
        .unwrap_or("empty")
        .to_string();
    let git_init = json
        .get("git_init")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let adopt_existing_empty = json
        .get("adopt_existing_empty")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if create && template != "empty" && template != "basic" {
        return project_error_cmd(start, "invalid_request");
    }

    if !create {
        // The directory must exist and be a directory.
        let path_buf = PathBuf::from(&path);
        let canonical = match path_buf.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                return err_cmd(
                    start,
                    format!(
                        "path does not exist or cannot be canonicalized: {}: {}",
                        path, e
                    ),
                );
            }
        };
        if !canonical.is_dir() {
            return err_cmd(start, format!("path {} is not a directory", path));
        }
        if let Err(error_kind) = validate_windows_project_root(&canonical) {
            return project_error_cmd(start, error_kind);
        }
        if validate_project_path_policy(policy, &canonical).is_err() {
            return project_error_cmd(start, "path_outside_allowed_roots");
        }
        if !overwrite {
            match matching_existing_project(
                project_registry_dir,
                &id,
                &name,
                &path,
                description.as_deref(),
                allow_patch,
            ) {
                Ok(Some(project)) => {
                    return ok_cmd(
                        start,
                        recovered_project_result(
                            create,
                            &runtime_id,
                            &client_id,
                            &project,
                            None,
                            false,
                        ),
                    )
                }
                Ok(None) => {}
                Err(code) => return project_error_cmd(start, code),
            }
        }
        let write_result =
            match write_project_toml_atomic(project_registry_dir, &id, &toml_content, overwrite) {
                Ok(p) => p,
                Err(ProjectTomlWriteError::BeforeRename) => {
                    return project_error_cmd(start, "operation_failed")
                }
                Err(ProjectTomlWriteError::AfterRename) => {
                    return project_error_cmd(start, "operation_indeterminate")
                }
            };
        let result = serde_json::json!({
            "id": runtime_id,
            "agent_project_id": id,
            "client_id": client_id,
            "name": name,
            "path": path,
            "description": description,
            "project_record_path": write_result.config_path.to_string_lossy(),
            "projects_config_path": write_result.config_path.to_string_lossy(),
            "created_config": write_result.created_config,
            "overwritten": write_result.overwritten,
            "allow_patch": allow_patch,
            "registration_source": EXPLICIT_REGISTRATION_SOURCE,
            "revision": project_revision(&parse_runner_project_toml(&toml_content).expect("generated project TOML must parse")),
            "operation": "register", "outcome": "registered", "changed": true, "recovered": false,
        });
        return ok_cmd(start, result);
    }

    // create_project
    let path_buf = PathBuf::from(&path);
    let mut created_directory = false;
    let mut created_paths = CreatedProjectPaths::default();

    // Determine the canonical parent for policy validation. If the path exists,
    // canonicalize it directly. If not, canonicalize the existing ancestor.
    let canonical_for_policy = if path_buf.exists() {
        match path_buf.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                return err_cmd(
                    start,
                    format!("path cannot be canonicalized: {}: {}", path, e),
                );
            }
        }
    } else {
        // Find the nearest existing ancestor and canonicalize it.
        let mut ancestor = path_buf.clone();
        while !ancestor.exists() {
            if let Some(parent) = ancestor.parent() {
                ancestor = parent.to_path_buf();
            } else {
                break;
            }
        }
        match ancestor.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                return err_cmd(
                    start,
                    format!(
                        "parent path cannot be canonicalized: {}: {}",
                        ancestor.display(),
                        e
                    ),
                );
            }
        }
    };
    if let Err(error_kind) = validate_windows_project_root(&canonical_for_policy) {
        return project_error_cmd(start, error_kind);
    }
    #[cfg(windows)]
    if webcodex_runner_config::paths::is_windows_network_share_path(&canonical_for_policy) {
        // A mapped drive may canonicalize to VerbatimUNC. Keep create_project
        // local-only even when the raw spelling looked like a drive letter.
        return project_error_cmd(start, "windows_project_path_unsupported");
    }
    if validate_project_path_policy(policy, &canonical_for_policy).is_err() {
        return project_error_cmd(start, "path_outside_allowed_roots");
    }
    if !overwrite {
        match matching_existing_project(
            project_registry_dir,
            &id,
            &name,
            &path,
            description.as_deref(),
            allow_patch,
        ) {
            Ok(Some(project)) => {
                if let Err(code) =
                    validate_recovered_create_side_effects(&path_buf, &template, git_init)
                {
                    return project_error_cmd(start, code);
                }
                return ok_cmd(
                    start,
                    recovered_project_result(
                        create,
                        &runtime_id,
                        &client_id,
                        &project,
                        Some(&template),
                        git_init,
                    ),
                );
            }
            Ok(None) => {}
            Err(code) => return project_error_cmd(start, code),
        }
    }

    // Handle existing vs new directory.
    if path_buf.exists() {
        let meta = match std::fs::metadata(&path_buf) {
            Ok(m) => m,
            Err(e) => return err_cmd(start, format!("failed to stat path {}: {}", path, e)),
        };
        if !meta.is_dir() {
            return err_cmd(
                start,
                format!("path {} exists but is not a directory", path),
            );
        }
        // Check if the directory is empty.
        let is_empty = match std::fs::read_dir(&path_buf) {
            Ok(mut it) => it.next().is_none(),
            Err(e) => {
                return err_cmd(start, format!("failed to read directory {}: {}", path, e));
            }
        };
        if !is_empty {
            return project_error_cmd(start, "path_not_empty");
        }
        if !adopt_existing_empty {
            return project_error_cmd(start, "path_exists");
        }
    } else {
        // Create the directory.
        if let Err(e) = std::fs::create_dir_all(&path_buf) {
            return err_cmd(start, format!("failed to create directory {}: {}", path, e));
        }
        created_directory = true;
        created_paths.mark_project_dir_created(path_buf.clone());
    }

    // Apply template.
    if template == "basic" {
        let readme = if let Some(ref desc) = description {
            format!("# {}\n\n{}\n", name, desc)
        } else {
            format!("# {}\n", name)
        };
        let readme_path = path_buf.join("README.md");
        if let Err(e) = write_created_file(&readme_path, readme.as_bytes(), &mut created_paths) {
            created_paths.cleanup();
            return err_cmd(start, format!("failed to write README.md: {}", e));
        }
        let gitignore = "target/\nnode_modules/\n.env\n*.log\n";
        let gitignore_path = path_buf.join(".gitignore");
        if let Err(e) =
            write_created_file(&gitignore_path, gitignore.as_bytes(), &mut created_paths)
        {
            created_paths.cleanup();
            return err_cmd(start, format!("failed to write .gitignore: {}", e));
        }
    }
    // `empty` itself generates no project files. Description stays registration
    // metadata; `git_init` remains a separate explicit filesystem side effect.

    // git init.
    let mut git_initialized = false;
    if git_init {
        match run_git_bounded(&path_buf, &["init"], Duration::from_secs(5), None) {
            Ok(output) if output.status.success() => {
                git_initialized = true;
                created_paths.track(path_buf.join(".git"));
            }
            Ok(output) => {
                created_paths.cleanup();
                let stderr = String::from_utf8_lossy(&output.stderr);
                let suffix = if output.stderr_capped {
                    " [stderr truncated]"
                } else {
                    ""
                };
                return err_cmd(
                    start,
                    format!("git init failed: {}{}", stderr.trim(), suffix),
                );
            }
            Err(e) => {
                created_paths.cleanup();
                return err_cmd(start, format!("git init failed (is git installed?): {}", e));
            }
        }
    }

    // Write project TOML.
    let write_result =
        match write_project_toml_atomic(project_registry_dir, &id, &toml_content, overwrite) {
            Ok(p) => p,
            Err(ProjectTomlWriteError::BeforeRename) => {
                created_paths.cleanup();
                return project_error_cmd(start, "operation_failed");
            }
            Err(ProjectTomlWriteError::AfterRename) => {
                return project_error_cmd(start, "operation_indeterminate");
            }
        };
    let result = serde_json::json!({
        "id": runtime_id,
        "agent_project_id": id,
        "client_id": client_id,
        "name": name,
        "path": path,
        "description": description,
        "project_record_path": write_result.config_path.to_string_lossy(),
        "projects_config_path": write_result.config_path.to_string_lossy(),
        "created_directory": created_directory,
        "created_config": write_result.created_config,
        "overwritten": write_result.overwritten,
        "allow_patch": allow_patch,
        "registration_source": EXPLICIT_REGISTRATION_SOURCE,
        "template": template,
        "revision": project_revision(&parse_runner_project_toml(&toml_content).expect("generated project TOML must parse")),
        "git_initialized": git_initialized,
        "operation": "create", "outcome": "created", "changed": true, "recovered": false,
    });
    ok_cmd(start, result)
}

#[cfg(test)]
fn test_project_operation(
    request: &RunnerRequest,
) -> Result<RunnerProjectOperation, CommandResult> {
    match request.decode_operation() {
        Ok(RunnerOperation::Project(operation)) => Ok(operation),
        _ => Err(project_error_cmd(
            Instant::now(),
            "unsupported_runner_version",
        )),
    }
}

#[cfg(test)]
pub(crate) fn handle_project_op(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    request: &RunnerRequest,
) -> CommandResult {
    let operation = match test_project_operation(request) {
        Ok(operation) => operation,
        Err(result) => return result,
    };
    handle_project_operation(policy, project_registry_dir, &request.client_id, &operation)
}

#[cfg(test)]
pub(crate) fn handle_resolve_or_register_project(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    request: &RunnerRequest,
) -> CommandResult {
    let operation = match test_project_operation(request) {
        Ok(operation) => operation,
        Err(result) => return result,
    };
    handle_resolve_or_register_project_operation(
        policy,
        project_registry_dir,
        &request.client_id,
        &operation,
    )
}

#[cfg(test)]
pub(crate) fn handle_prepare_managed_worktree(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    request: &RunnerRequest,
) -> CommandResult {
    let operation = match test_project_operation(request) {
        Ok(operation) => operation,
        Err(result) => return result,
    };
    handle_prepare_managed_worktree_operation(
        policy,
        project_registry_dir,
        &request.client_id,
        &operation,
    )
}

#[cfg(test)]
pub(crate) fn handle_project_lifecycle_op(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    request: &RunnerRequest,
) -> CommandResult {
    let operation = match test_project_operation(request) {
        Ok(operation) => operation,
        Err(result) => return result,
    };
    handle_project_lifecycle_operation(policy, project_registry_dir, &operation)
}

#[cfg(test)]
mod durability_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn managed_worktree_git_cli_path_normalizes_only_verbatim_local_disk_paths() {
        assert_eq!(
            managed_worktree_git_cli_path(Path::new(r"\\?\C:\workspace\managed")),
            r"C:\workspace\managed"
        );
        assert_eq!(
            managed_worktree_git_cli_path(Path::new(r"C:\workspace\managed")),
            r"C:\workspace\managed"
        );
        assert_eq!(
            managed_worktree_git_cli_path(Path::new(r"\\server\share\managed")),
            r"\\server\share\managed"
        );
    }

    /// Unix-only: verifies the POSIX directory-fsync contract (opening a
    /// directory as a file). On Windows directory sync is intentionally
    /// skipped because std cannot open directories with the required
    /// FILE_FLAG_BACKUP_SEMANTICS.
    #[cfg(unix)]
    #[test]
    fn registry_parent_sync_failures_are_not_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing").join("demo.toml");
        let error = sync_parent_dir(&missing).unwrap_err();
        assert!(error.contains("sync project registry directory"));
    }

    #[test]
    fn registry_loader_ignores_temp_and_unregister_tombstones() {
        let tmp = tempfile::tempdir().unwrap();
        let project_registry_dir = tmp.path().join("project-registry");
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&project_registry_dir).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        let content = build_project_toml("demo", "Demo", source.to_str().unwrap(), &None, true);
        std::fs::write(project_registry_dir.join("demo.toml"), &content).unwrap();
        std::fs::write(project_registry_dir.join(".demo.random.toml.tmp"), &content).unwrap();
        std::fs::write(
            project_registry_dir.join(".demo.random.toml.unregistering"),
            &content,
        )
        .unwrap();
        let projects = load_runner_project_summaries_from_dir(&project_registry_dir);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, "demo");
    }

    #[cfg(unix)]
    #[test]
    fn project_summary_reports_retargeted_symlinks_as_distinct_canonical_roots() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        let link = tmp.path().join("current");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        symlink(&first, &link).unwrap();
        let project = RunnerProjectFile {
            id: "demo".to_string(),
            path: link.to_string_lossy().to_string(),
            shell_profile: None,
            allow_patch: true,
            name: None,
            kind: None,
            registration_source: None,
            description: None,
            disabled: false,
            hooks: HashMap::new(),
            managed_worktree: false,
            managed_source: None,
            managed_base_ref: None,
            managed_base_sha: None,
            managed_operation_id: None,
        };

        let first_summary = runner_project_summary(&project, 1, false);
        assert_eq!(
            Path::new(&first_summary.path),
            first.canonicalize().unwrap()
        );
        std::fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();
        let second_summary = runner_project_summary(&project, 2, false);
        assert_eq!(
            Path::new(&second_summary.path),
            second.canonicalize().unwrap()
        );
        assert_ne!(first_summary.path, second_summary.path);
    }
}

#[cfg(test)]
mod git_lifecycle_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::SystemTime;

    // -----------------------------------------------------------------------
    // Git lifecycle regression coverage for the run_git_bounded ManagedChild
    // migration. The scenarios run the real `validation_tree_helper` fixture
    // (compiled at test time with rustc, exactly like the validation and job
    // tree tests) through the `run_git_bounded_with_program` seam, so the same
    // tests run on Windows and Unix without cmd, PowerShell, or bash. Each
    // test tracks the real parent/descendant pids written to marker files and
    // probes them with platform-native APIs, and every test reaps the tree it
    // starts before returning.
    // -----------------------------------------------------------------------

    /// Compiled copy of the `validation_tree_helper` fixture, kept alive for
    /// the whole test process so its binary path never disappears under a
    /// running descendant.
    struct GitTreeHelper {
        _temp: tempfile::TempDir,
        path: PathBuf,
    }

    static GIT_TREE_HELPER: OnceLock<Arc<GitTreeHelper>> = OnceLock::new();

    fn helper_binary() -> PathBuf {
        GIT_TREE_HELPER
            .get_or_init(|| {
                let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("src/webcodex_runner/validation/validation_tree_helper.rs");
                let temp = tempfile::tempdir().unwrap();
                let output = temp
                    .path()
                    .join(format!("git-tree-helper{}", std::env::consts::EXE_SUFFIX));
                let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
                let result = Command::new(rustc)
                    .arg("--edition=2021")
                    .arg("--crate-name=webcodex_git_tree_helper")
                    .arg(&source)
                    .arg("-o")
                    .arg(&output)
                    .output()
                    .expect("run rustc for git tree helper");
                assert!(
                    result.status.success(),
                    "git tree helper compilation failed: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                Arc::new(GitTreeHelper {
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
            "wc-project-git-{tag}-{}-{nanos}",
            std::process::id()
        ));
        CleanupPath(path)
    }

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
        // test helper. Non-Linux Unix test hosts reap orphaned descendants
        // promptly, so a successful probe represents a live process here.
        (unsafe { libc::kill(pid as i32, 0) }) == 0
    }

    /// Upper bound for the whole test body including cleanup; the fixture
    /// sleeps far longer (600s), so any run exceeding this is a cleanup hang,
    /// not a slow exit.
    const BOUNDEDNESS_LIMIT: Duration = Duration::from_secs(15);

    /// A. Normal completion: a short-lived process exits successfully, its
    /// stdout/stderr are collected, and no cleanup stall occurs.
    #[test]
    #[ignore = "runner real-process lane: spawns the Git ManagedChild process-tree fixture"]
    fn runner_real_process_git_normal_completion_collects_output_and_returns_bounded() {
        let cwd = tempfile::tempdir().unwrap();
        let program = helper_binary();
        let started = Instant::now();
        let output = run_git_bounded_with_program(
            &program.to_string_lossy(),
            cwd.path(),
            &["sleep", "0", "7"],
            Duration::from_secs(10),
            None,
        )
        .expect("normal completion must succeed");
        assert_eq!(output.status.code(), Some(7));
        assert!(!output.stdout_capped);
        assert!(!output.stderr_capped);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("VALIDATION_HELPER_STDOUT"), "{stdout}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("VALIDATION_HELPER_STDERR"), "{stderr}");
        assert!(
            started.elapsed() < BOUNDEDNESS_LIMIT,
            "normal completion was not bounded"
        );
    }

    /// B. Explicit timeout kills the whole tree: the direct Git process and
    /// its pipe-holding descendant must both die, with the timeout error
    /// unchanged.
    #[test]
    #[ignore = "runner real-process lane: spawns the Git ManagedChild process-tree fixture"]
    fn runner_real_process_git_timeout_terminates_whole_tree() {
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
        let result = thread::scope(|scope| {
            let handle = scope.spawn(|| {
                run_git_bounded_with_program(
                    &program.to_string_lossy(),
                    cwd.path(),
                    &args.iter().map(String::as_str).collect::<Vec<_>>(),
                    Duration::from_secs(2),
                    None,
                )
            });
            assert!(
                wait_until_file(&parent_marker, Duration::from_secs(5)),
                "parent marker never appeared"
            );
            assert!(
                wait_until_file(&alive_marker, Duration::from_secs(5)),
                "descendant marker never appeared"
            );
            let parent_pid = read_pid(&parent_marker, "PARENT_PID");
            let descendant_pid = read_pid(&parent_marker, "DESCENDANT_PID");
            // Both sleep 600s while the timeout is 2s, so both must still be
            // alive when the timeout fires.
            assert!(process_alive(parent_pid), "parent not alive before timeout");
            assert!(
                process_alive(descendant_pid),
                "descendant not alive before timeout"
            );
            handle.join().expect("run_git_bounded panicked")
        });
        let error = match result {
            Ok(_) => panic!("run_git_bounded must report a timeout, not success"),
            Err(error) => error,
        };
        assert_eq!(error, "git command timed out");
        assert!(
            started.elapsed() < BOUNDEDNESS_LIMIT,
            "timeout cleanup not bounded"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "PARENT_PID")),
            "Git parent survived timeout cleanup"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "DESCENDANT_PID")),
            "Git descendant survived timeout cleanup"
        );
    }

    /// C. Runner shutdown terminates the whole tree with the shutdown error
    /// unchanged. Works on Windows and Linux.
    #[test]
    #[ignore = "runner real-process lane: spawns the Git ManagedChild process-tree fixture"]
    fn runner_real_process_git_runner_shutdown_terminates_whole_tree() {
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
        let result = thread::scope(|scope| {
            let handle = scope.spawn(|| {
                run_git_bounded_with_program(
                    &program.to_string_lossy(),
                    cwd.path(),
                    &args.iter().map(String::as_str).collect::<Vec<_>>(),
                    Duration::from_secs(60),
                    Some(&shutdown),
                )
            });
            assert!(
                wait_until_file(&parent_marker, Duration::from_secs(5)),
                "parent marker never appeared"
            );
            assert!(
                wait_until_file(&alive_marker, Duration::from_secs(5)),
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
            handle.join().expect("run_git_bounded panicked")
        });
        let error = match result {
            Ok(_) => panic!("run_git_bounded must report shutdown, not success"),
            Err(error) => error,
        };
        assert_eq!(error, "git stopped during runner shutdown");
        assert!(
            started.elapsed() < BOUNDEDNESS_LIMIT,
            "shutdown cleanup not bounded"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "PARENT_PID")),
            "Git parent survived runner shutdown"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "DESCENDANT_PID")),
            "Git descendant survived runner shutdown"
        );
    }

    /// D. The direct Git process exits while its descendant survives and holds
    /// the captured pipes. Direct-child exit alone must not finish cleanup:
    /// the surviving tree is terminated, the readers reach EOF, and
    /// run_git_bounded returns without an indefinite reader wait.
    #[test]
    #[ignore = "runner real-process lane: spawns the Git ManagedChild process-tree fixture"]
    fn runner_real_process_git_parent_exit_alone_does_not_finish_cleanup() {
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
        let output = thread::scope(|scope| {
            let handle = scope.spawn(|| {
                run_git_bounded_with_program(
                    &program.to_string_lossy(),
                    cwd.path(),
                    &args.iter().map(String::as_str).collect::<Vec<_>>(),
                    Duration::from_secs(30),
                    None,
                )
            });
            // The direct child exits almost immediately after spawning its
            // descendant. The descendant's marker appears only if it actually
            // ran, so its existence proves the descendant was alive after the
            // direct child exited.
            assert!(
                wait_until_file(&alive_marker, Duration::from_secs(5)),
                "descendant marker never appeared"
            );
            handle.join().expect("run_git_bounded panicked")
        })
        .expect("direct-parent exit must not turn into an error");
        assert!(
            output.status.success(),
            "direct child exited 0; tree cleanup must not change its status"
        );
        // The captured stdout contains the helper's pid line only when the
        // reader hit EOF, which requires every descendant holding the pipe to
        // be gone. A cleanup that stops at the direct child leaves stdout
        // stuck at the un-flushed line or empty.
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("DESCENDANT_PID="),
            "stdout reader never reached EOF: {stdout}"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "DESCENDANT_PID")),
            "descendant survived cleanup after direct child exit"
        );
        assert!(
            started.elapsed() < BOUNDEDNESS_LIMIT,
            "parent-exit cleanup not bounded"
        );
    }

    /// E. A SIGTERM-resistant tree is escalated to force: the graceful request
    /// gets a short bounded grace, then the whole tree is killed. Never
    /// unbounded. (Windows has no generic graceful tree termination, so there
    /// is nothing to escalate from there.)
    #[cfg(unix)]
    #[test]
    #[ignore = "runner real-process lane: spawns the Git ManagedChild process-tree fixture"]
    fn runner_real_process_git_sigterm_resistant_tree_is_forcefully_escalated() {
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
        let result = thread::scope(|scope| {
            let handle = scope.spawn(|| {
                run_git_bounded_with_program(
                    &program.to_string_lossy(),
                    cwd.path(),
                    &args.iter().map(String::as_str).collect::<Vec<_>>(),
                    Duration::from_secs(2),
                    None,
                )
            });
            assert!(
                wait_until_file(&parent_marker, Duration::from_secs(5)),
                "parent marker never appeared"
            );
            assert!(
                wait_until_file(&alive_marker, Duration::from_secs(5)),
                "descendant marker never appeared"
            );
            let parent_pid = read_pid(&parent_marker, "PARENT_PID");
            let descendant_pid = read_pid(&parent_marker, "DESCENDANT_PID");
            assert!(process_alive(parent_pid), "parent not alive before timeout");
            assert!(
                process_alive(descendant_pid),
                "descendant not alive before timeout"
            );
            handle.join().expect("run_git_bounded panicked")
        });
        let error = match result {
            Ok(_) => panic!("run_git_bounded must report a timeout, not success"),
            Err(error) => error,
        };
        assert_eq!(error, "git command timed out");
        // Both processes ignore SIGTERM (inherited SIG_IGN), so only the
        // forceful escalation can have ended them.
        assert!(
            !process_alive(read_pid(&parent_marker, "PARENT_PID")),
            "SIGTERM-resistant parent survived escalation"
        );
        assert!(
            !process_alive(read_pid(&parent_marker, "DESCENDANT_PID")),
            "SIGTERM-resistant descendant survived escalation"
        );
        assert!(
            started.elapsed() < BOUNDEDNESS_LIMIT,
            "SIGTERM-resistant cleanup not bounded"
        );
    }

    /// F. Spawn failure keeps the standard direct-spawn failure semantics with
    /// the existing user-visible error prefix.
    #[test]
    fn spawn_failure_reports_spawn_error() {
        let cwd = tempfile::tempdir().unwrap();
        let error = match run_git_bounded_with_program(
            "webcodex-git-command-that-does-not-exist-xyz",
            cwd.path(),
            &["--version"],
            Duration::from_secs(5),
            None,
        ) {
            Ok(_) => panic!("spawn of a nonexistent executable must fail"),
            Err(error) => error,
        };
        assert!(
            error.starts_with("failed to spawn git"),
            "unexpected spawn error: {error}"
        );
    }

    /// Keep at least one real Git smoke path: production `run_git_bounded`
    /// with the hardcoded `"git"` program.
    #[test]
    fn real_git_smoke_runs_through_managed_spawn() {
        let cwd = tempfile::tempdir().unwrap();
        let output = run_git_bounded(cwd.path(), &["--version"], Duration::from_secs(5), None)
            .expect("real git must run through the managed spawn");
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("git version"),
            "unexpected git --version output"
        );
    }
}
