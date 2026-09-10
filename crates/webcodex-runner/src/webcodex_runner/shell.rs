use super::config::{
    dialect_for_program, platform_default_dialect, validate_shell_config, RunnerPolicy,
    ShellConfig, ShellDialect, ShellEnvironmentMode, ShellProfileConfig,
};
use super::output::{CommandResult, ShellCommandResult};
use super::output_text::{
    append_bounded_text, normalize_captured_output_text, normalize_output_text,
    CapturedOutputEncoding, FullStreamUtf8Validity, LeadingBom, OutputTextSource,
};
use super::projects::find_project_shell_context;
use crate::runner_protocol::{ShellProcessArgv, ShellScriptLanguage, ShellScriptPayload};
use std::collections::HashMap;
#[cfg(windows)]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use webcodex_process::{GracefulTermination, ManagedChild};

#[path = "process_command.rs"]
mod process_command;
pub(crate) use process_command::structured_process_command;

const SHELL_PROFILE_PREPARE_TIMEOUT_SECS: u64 = 30;
const PROCESS_GROUP_TERMINATION_GRACE: Duration = Duration::from_millis(50);
const PROCESS_TREE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const PROCESS_PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const PROFILE_PREPARE_PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const RAW_TAIL_CAPTURE_ALLOWANCE: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PreparedShellProfileKey {
    generation: u64,
    project_key: String,
    profile_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedShellProfile {
    pub(crate) profile_name: String,
    program: String,
    args: Vec<String>,
    dialect: ShellDialect,
    env_snapshot: HashMap<String, String>,
}

/// A native-process launch environment produced by the existing Runner shell
/// profile machinery. It contains only the prepared environment snapshot and
/// resolves the final executable through that snapshot's PATH; no shell layer
/// is inserted around the child process.
#[derive(Debug, Clone)]
pub(crate) struct PreparedExecutionEnvironment {
    env_snapshot: HashMap<String, String>,
}

/// Lazily prepared shell environment snapshots. Snapshots are keyed by
/// config generation, project/cwd, and profile name because inline init
/// scripts such as `. .venv/bin/activate` are intentionally resolved from the
/// project cwd. A successful hot reload retires older cached generations after
/// the new generation prepares its first snapshot.
#[derive(Debug, Clone, Default)]
pub(crate) struct PreparedShellProfileCache {
    profiles: Arc<Mutex<HashMap<PreparedShellProfileKey, Arc<PreparedShellProfile>>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedDetachedProcessLaunch {
    pub(crate) process: ShellProcessArgv,
    pub(crate) cwd: String,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) timeout_secs: u64,
}

/// POSIX sh single-quote escaping.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// PowerShell single-quote escaping (an embedded single quote is doubled).
/// PowerShell's single-quoted strings are literal, so spaces, backslashes,
/// double quotes, `$`, Unicode, and `C:\...` Windows paths need no further
/// escaping; only `'` does.
pub(crate) fn shell_quote_powershell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Resolve the effective shell dialect: an explicit config value wins, then a
/// known shell program basename, then the platform default. Profiles pass
/// `profile.dialect.or(shell.dialect)` as the explicit value so they inherit
/// the parent shell dialect unless designed otherwise.
fn resolve_dialect(program: &str, explicit: Option<ShellDialect>) -> ShellDialect {
    explicit
        .or_else(|| dialect_for_program(program))
        .unwrap_or_else(platform_default_dialect)
}

const SENSITIVE_ENV_KEYS: [&str; 4] = [
    "WEBCODEX_TOKEN",
    "WEBCODEX_AGENT_TOKEN",
    "WEBCODEX_USER_TOKEN",
    "AUTHORIZATION",
];

/// Compare environment variable names with the host platform's semantics.
/// Windows names are case-insensitive; Unix names remain case-sensitive.
pub(crate) fn env_keys_equal(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

/// Sensitive environment keys must never reach child processes. Windows
/// environment names are case-insensitive, so a mixed-case spelling such as
/// `WebCodex_Token` must be filtered too; Unix stays case-sensitive.
pub(crate) fn is_sensitive_env_key(key: &str) -> bool {
    SENSITIVE_ENV_KEYS
        .iter()
        .any(|sensitive| env_keys_equal(sensitive, key))
}

fn should_inherit_env_key(key: &str) -> bool {
    // Windows command processors may add drive-current-directory pseudo
    // entries such as `=E:=E:\\git\\webcodex` to the native environment
    // block. They are not ordinary environment variables and cannot be
    // reconstructed through `Command::env`; detached execution carries its
    // working directory explicitly, so dropping them preserves the intended
    // child environment without weakening launch-envelope validation.
    !is_sensitive_env_key(key) && !(cfg!(windows) && key.starts_with('='))
}

/// Case-insensitive lookup on Windows (where environment names are
/// case-insensitive), exact match on Unix.
fn env_lookup<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a String> {
    if cfg!(windows) {
        env.iter()
            .find(|(candidate, _)| env_keys_equal(candidate, key))
            .map(|(_, value)| value)
    } else {
        env.get(key)
    }
}

/// Insert `key=value`, replacing any existing entry that names the same
/// environment variable. On Windows the replacement is case-insensitive so a
/// snapshot never carries both `Path` and `PATH` (which would make the final
/// child environment depend on HashMap iteration order); on Unix it is exact.
fn env_insert(env: &mut HashMap<String, String>, key: &str, value: String) {
    if cfg!(windows) {
        env.retain(|candidate, _| !env_keys_equal(candidate, key));
    }
    env.insert(key.to_string(), value);
}

/// Remove every sensitive environment key from `env`, case-insensitively on
/// Windows (a profile could configure `webcodetoken = ...`).
fn remove_sensitive_env(env: &mut HashMap<String, String>) {
    let sensitive: Vec<String> = env
        .keys()
        .filter(|key| is_sensitive_env_key(key))
        .cloned()
        .collect();
    for key in sensitive {
        env.remove(&key);
    }
}

/// Deterministic UTF-8 setup for redirected PowerShell output. Bounded to the
/// child process: when stdout is redirected, .NET only caches these encodings
/// instead of calling SetConsoleOutputCP, so the parent Runner console state
/// is never mutated. PowerShell 5.1 otherwise writes through the console code
/// page (OEM), which would corrupt Unicode output and the env snapshot.
const POWERSHELL_UTF8_PREAMBLE: &str = concat!(
    "try { $OutputEncoding = [Console]::InputEncoding = ",
    "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false) } catch { }",
);

/// Wrap `command` so the PowerShell process exits with a meaningful status:
/// the shell's own `exit N` statements pass through, a failing trailing native
/// command is reported through `$LASTEXITCODE`, a failing PowerShell statement
/// returns 1, and a successful trailing PowerShell statement returns 0 even if
/// an earlier native command left a stale non-zero `$LASTEXITCODE` behind.
/// PowerShell 5.1 does not propagate these statuses consistently on its own,
/// so inspect `$?` immediately after the requested command and exit explicitly.
fn powershell_command_text(command: &str) -> String {
    format!(
        "{POWERSHELL_UTF8_PREAMBLE}\n\
         $LASTEXITCODE = 0\n\
         {command}\n\
         if (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}; exit 1 }}\n\
         exit 0"
    )
}

/// Dot-source a shell init script, then run the command only after the init
/// script reported success. Native-command failure inside the init script
/// blocks the command (like POSIX `. <path> && (...)`); a terminating
/// PowerShell error aborts the whole script, which also blocks the command.
///
/// `$?` is inspected immediately after dot-sourcing so a failed dot-source
/// operation (for example, an unavailable script) blocks the command. Native
/// failure status is preserved through `$LASTEXITCODE`; ordinary PowerShell
/// non-terminating errors retain PowerShell's own dot-source semantics.
fn powershell_init_command_text(init_script: &Path, command: &str) -> String {
    format!(
        "{POWERSHELL_UTF8_PREAMBLE}\n\
         . {}\n\
         if (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}; exit 1 }}\n\
         $LASTEXITCODE = 0\n\
         {command}\n\
         if (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}; exit 1 }}\n\
         exit 0",
        shell_quote_powershell(&init_script.to_string_lossy()),
    )
}

fn shell_command_text(shell: &ShellConfig, dialect: ShellDialect, command: &str) -> String {
    match (dialect, shell.init_script.as_ref()) {
        (ShellDialect::Posix, Some(path)) => format!(
            ". {} && (\n{}\n)",
            shell_quote(&path.to_string_lossy()),
            command
        ),
        (ShellDialect::Posix, None) => command.to_string(),
        (ShellDialect::PowerShell, Some(path)) => powershell_init_command_text(path, command),
        (ShellDialect::PowerShell, None) => powershell_command_text(command),
    }
}

/// Command text for an already-prepared profile execution. POSIX shells get
/// the raw command (the last statement's status is the shell status); the
/// PowerShell wrapper adds the explicit exit-status propagation.
fn prepared_shell_command_text(dialect: ShellDialect, command: &str) -> String {
    match dialect {
        ShellDialect::Posix => command.to_string(),
        ShellDialect::PowerShell => powershell_command_text(command),
    }
}

fn apply_shell_environment(cmd: &mut Command, shell: &ShellConfig) -> Result<(), String> {
    if shell.environment_mode == ShellEnvironmentMode::Isolated {
        let env = base_shell_env(shell, &ShellProfileConfig::default())?;
        apply_env_snapshot(cmd, &env);
        return Ok(());
    }
    // Rust's Windows env handling is case-insensitive (like the OS itself), so
    // removing the canonical spellings also removes mixed-case variants such
    // as `WebCodex_Token`.
    for key in SENSITIVE_ENV_KEYS {
        cmd.env_remove(key);
    }
    if !shell.path_prepend.is_empty() {
        let mut paths = shell.path_prepend.clone();
        if let Some(current) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&current));
        }
        let joined = std::env::join_paths(paths)
            .map_err(|e| format!("failed to build shell PATH from shell.path_prepend: {}", e))?;
        cmd.env("PATH", joined);
    }
    for (key, value) in &shell.env {
        if !is_sensitive_env_key(key) {
            cmd.env(key, value);
        }
    }
    Ok(())
}

fn apply_env_snapshot(cmd: &mut Command, env_snapshot: &HashMap<String, String>) {
    cmd.env_clear();
    for (key, value) in env_snapshot {
        cmd.env(key, value);
    }
}

/// On Windows, resolve a bare shell program name through the platform rules
/// so an extensionless POSIX shim shadowing the real executable is never
/// selected (CreateProcess would fail with error 193). Path-qualified values
/// are used verbatim; an unresolvable bare name falls back to the configured
/// value so the spawn surfaces the real error.
fn resolved_shell_program(program: &str) -> String {
    #[cfg(windows)]
    {
        let path = Path::new(program);
        if path.components().count() <= 1 && !path.is_absolute() {
            if let Some(resolved) = super::util::resolve_program_in_path(
                program,
                std::env::var_os("PATH")
                    .as_deref()
                    .unwrap_or(OsStr::new("")),
            ) {
                return resolved.path().to_string_lossy().into_owned();
            }
        }
    }
    #[cfg(not(windows))]
    let _ = program;
    program.to_string()
}

fn configured_shell_command(shell: &ShellConfig, command: &str) -> Result<Command, String> {
    validate_shell_config(shell)?;
    let dialect = resolve_dialect(&shell.program, shell.dialect);
    let program = resolved_shell_program(&shell.program);
    let mut cmd = Command::new(program);
    for arg in &shell.args {
        cmd.arg(arg);
    }
    cmd.arg(shell_command_text(shell, dialect, command));
    // The shell execution path owns its process tree through ManagedChild; do
    // not add a process-group pre_exec here. ManagedChild creates the private
    // process group (Unix) / Job Object (Windows) at spawn time.
    apply_shell_environment(&mut cmd, shell)?;
    Ok(cmd)
}

fn configured_prepared_shell_command(
    profile: &PreparedShellProfile,
    command: &str,
) -> Result<Command, String> {
    let mut cmd = Command::new(&profile.program);
    for arg in &profile.args {
        cmd.arg(arg);
    }
    cmd.arg(prepared_shell_command_text(profile.dialect, command));
    // The shell execution path owns its process tree through ManagedChild; do
    // not add a process-group pre_exec here. ManagedChild creates the private
    // process group (Unix) / Job Object (Windows) at spawn time.
    apply_env_snapshot(&mut cmd, &profile.env_snapshot);
    Ok(cmd)
}

pub(crate) fn configured_shell_job_command(
    shell: &ShellConfig,
    command: &str,
) -> Result<Command, String> {
    validate_shell_config(shell)?;
    let dialect = resolve_dialect(&shell.program, shell.dialect);
    let program = resolved_shell_program(&shell.program);
    let mut cmd = Command::new(program);
    for arg in &shell.args {
        cmd.arg(arg);
    }
    cmd.arg(shell_command_text(shell, dialect, command));
    // JobManager owns this process tree through ManagedChild; do not add
    // the legacy setsid pre_exec here. ManagedChild creates the private group.
    apply_shell_environment(&mut cmd, shell)?;
    Ok(cmd)
}

pub(crate) fn configured_prepared_shell_job_command(
    profile: &PreparedShellProfile,
    command: &str,
) -> Result<Command, String> {
    let mut cmd = Command::new(&profile.program);
    for arg in &profile.args {
        cmd.arg(arg);
    }
    cmd.arg(prepared_shell_command_text(profile.dialect, command));
    // JobManager owns this process tree through ManagedChild; do not add
    // the legacy setsid pre_exec here. ManagedChild creates the private group.
    apply_env_snapshot(&mut cmd, &profile.env_snapshot);
    Ok(cmd)
}

pub(crate) fn configured_validation_job_command(
    shell: &ShellConfig,
    profile: Option<&PreparedShellProfile>,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<Command, String> {
    if profile.is_none() {
        validate_shell_config(shell)?;
    }
    configured_process_command(shell, profile, program, args, Some(cwd))
}

fn configured_process_command(
    shell: &ShellConfig,
    profile: Option<&PreparedShellProfile>,
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<Command, String> {
    let resolved_program = resolve_process_program(shell, profile, program, cwd)?;
    let mut cmd = structured_process_command(&resolved_program, args, cwd)?;
    // ManagedChild (or JobManager for structured validation) owns this process
    // tree. Native argv stays literal; batch conversion belongs to the helper.
    match profile {
        Some(profile) => apply_env_snapshot(&mut cmd, &profile.env_snapshot),
        None => apply_shell_environment(&mut cmd, shell)?,
    }
    Ok(cmd)
}

fn resolve_process_program(
    shell: &ShellConfig,
    profile: Option<&PreparedShellProfile>,
    program: &str,
    cwd: Option<&Path>,
) -> Result<OsString, String> {
    #[cfg(windows)]
    {
        let path = configured_process_path(shell, profile)?;
        let program_path = Path::new(program);
        let resolved_input = if !program_path.is_absolute() && program_path.components().count() > 1
        {
            cwd.map(|cwd| cwd.join(program_path))
                .unwrap_or_else(|| program_path.to_path_buf())
                .to_string_lossy()
                .into_owned()
        } else {
            program.to_string()
        };
        match super::util::resolve_program_in_path(&resolved_input, &path) {
            Some(super::util::ResolvedProgram::Native(path)) => Ok(path.into_os_string()),
            Some(super::util::ResolvedProgram::Batch(path)) => Ok(path.into_os_string()),
            None => Err(format!(
                "structured process executable is unavailable or has an unsupported Windows extension: {program}"
            )),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (shell, profile, cwd);
        Ok(OsString::from(program))
    }
}

fn configured_process_path(
    shell: &ShellConfig,
    profile: Option<&PreparedShellProfile>,
) -> Result<OsString, String> {
    if let Some(profile) = profile {
        return Ok(env_lookup(&profile.env_snapshot, "PATH")
            .map(OsString::from)
            .unwrap_or_default());
    }
    if shell.environment_mode == ShellEnvironmentMode::Isolated {
        let env = base_shell_env(shell, &ShellProfileConfig::default())?;
        return Ok(env_lookup(&env, "PATH")
            .map(OsString::from)
            .unwrap_or_default());
    }
    if let Some(configured) = env_lookup(&shell.env, "PATH") {
        return Ok(OsString::from(configured));
    }
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    if shell.path_prepend.is_empty() {
        return Ok(inherited);
    }
    let mut paths = shell.path_prepend.clone();
    paths.extend(std::env::split_paths(&inherited));
    std::env::join_paths(paths)
        .map_err(|error| format!("failed to build process PATH from shell.path_prepend: {error}"))
}

#[cfg(windows)]
fn is_windows_wsl_bash_launcher(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    normalized.ends_with("\\windows\\system32\\bash.exe")
        || normalized.ends_with("\\windows\\sysnative\\bash.exe")
        || normalized.ends_with("\\windows\\syswow64\\bash.exe")
        || normalized.ends_with("\\microsoft\\windowsapps\\bash.exe")
}

#[cfg(windows)]
fn resolve_windows_internal_posix_interpreter(
    shell: &ShellConfig,
    profile: Option<&PreparedShellProfile>,
) -> Result<OsString, String> {
    let path = configured_process_path(shell, profile)?;

    // Keep generated Git/workspace programs on the same native Windows toolchain
    // as the `git.exe` visible to the Runner. Git for Windows normally exposes
    // `cmd\\git.exe` on PATH while its Bash lives in `bin\\bash.exe`; MSYS/Cygwin
    // layouts commonly keep both executables in the same `bin` directory.
    if let Some(super::util::ResolvedProgram::Native(git)) =
        super::util::resolve_program_in_path("git", &path)
    {
        let mut candidates = Vec::new();
        if let Some(parent) = git.parent() {
            let parent_name = parent
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if parent_name.eq_ignore_ascii_case("cmd") {
                if let Some(root) = parent.parent() {
                    candidates.push(root.join("bin").join("bash.exe"));
                    candidates.push(root.join("usr").join("bin").join("bash.exe"));
                }
            } else if parent_name.eq_ignore_ascii_case("bin") {
                candidates.push(parent.join("bash.exe"));
                if let Some(root) = parent.parent() {
                    candidates.push(root.join("usr").join("bin").join("bash.exe"));
                }
            }
        }
        for candidate in candidates {
            if is_windows_wsl_bash_launcher(&candidate) {
                continue;
            }
            if let Some(super::util::ResolvedProgram::Native(resolved)) =
                super::util::resolve_program_in_path(&candidate.to_string_lossy(), OsStr::new(""))
            {
                return Ok(resolved.into_os_string());
            }
        }
    }

    // A standalone native Windows Bash remains valid for non-Git internal work,
    // but the Windows-provided `bash.exe` / app-execution alias is a WSL launcher.
    // Entering WSL changes cwd/Git/environment semantics and is therefore not a
    // valid runtime for Runner-generated programs over a native Windows project.
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join("bash.exe");
        if is_windows_wsl_bash_launcher(&candidate) {
            continue;
        }
        if let Some(super::util::ResolvedProgram::Native(resolved)) =
            super::util::resolve_program_in_path(&candidate.to_string_lossy(), OsStr::new(""))
        {
            return Ok(resolved.into_os_string());
        }
    }

    Err(
        "interpreter_unavailable: native Windows Bash interpreter is unavailable; WSL bash launchers are not valid for Runner-generated internal programs; command was not started"
            .to_string(),
    )
}

fn configured_script_interpreter(
    shell: &ShellConfig,
    profile: Option<&PreparedShellProfile>,
    language: ShellScriptLanguage,
) -> Result<OsString, String> {
    let configured_program = profile
        .map(|profile| profile.program.as_str())
        .unwrap_or(shell.program.as_str());
    let configured_basename = Path::new(configured_program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(configured_program)
        .to_ascii_lowercase();
    let configured_matches = match language {
        ShellScriptLanguage::Sh => matches!(configured_basename.as_str(), "sh" | "sh.exe"),
        ShellScriptLanguage::Bash => {
            matches!(configured_basename.as_str(), "bash" | "bash.exe")
        }
        ShellScriptLanguage::Powershell if cfg!(windows) => matches!(
            configured_basename.as_str(),
            "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
        ),
        ShellScriptLanguage::Powershell => {
            matches!(configured_basename.as_str(), "pwsh" | "pwsh.exe")
        }
        ShellScriptLanguage::Javascript => {
            matches!(configured_basename.as_str(), "node" | "node.exe")
        }
    };
    let mut candidates = Vec::new();
    if configured_matches {
        candidates.push(configured_program.to_string());
    }
    match language {
        ShellScriptLanguage::Sh => candidates.push("sh".to_string()),
        ShellScriptLanguage::Bash => candidates.push("bash".to_string()),
        ShellScriptLanguage::Powershell if cfg!(windows) => {
            candidates.push("pwsh".to_string());
            candidates.push("powershell".to_string());
        }
        ShellScriptLanguage::Powershell => candidates.push("pwsh".to_string()),
        ShellScriptLanguage::Javascript => candidates.push("node".to_string()),
    }
    candidates.dedup_by(|left, right| {
        if cfg!(windows) {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    });
    let path = configured_process_path(shell, profile)?;
    for candidate in candidates {
        if let Some(super::util::ResolvedProgram::Native(path)) =
            super::util::resolve_program_in_path(&candidate, &path)
        {
            return Ok(path.into_os_string());
        }
    }
    let interpreter_name = match language {
        ShellScriptLanguage::Javascript => "JavaScript/Node",
        _ => language.as_str(),
    };
    Err(format!(
        "interpreter_unavailable: {interpreter_name} interpreter is unavailable; command was not started"
    ))
}

fn build_script_command(
    interpreter: impl Into<OsString>,
    language: ShellScriptLanguage,
    script_path: &Path,
    args: &[String],
) -> Command {
    let mut command = Command::new(interpreter.into());
    match language {
        ShellScriptLanguage::Sh | ShellScriptLanguage::Bash | ShellScriptLanguage::Javascript => {
            command.arg(script_path);
        }
        ShellScriptLanguage::Powershell => {
            command.arg("-NoProfile").arg("-NonInteractive");
            if cfg!(windows) {
                // Match the Runner's existing Windows PowerShell policy: a
                // process-scoped bypass keeps Runner-owned temporary .ps1 files
                // executable under the stock Restricted machine policy.
                command.arg("-ExecutionPolicy").arg("Bypass");
            }
            command.arg("-File").arg(script_path);
        }
    }
    command.args(args);
    command
}

fn script_setup_error(action: &str, error: &std::io::Error) -> String {
    format!(
        "script_setup_failed: failed to {action} Runner-owned temporary script file ({:?}); command was not started",
        error.kind()
    )
}

fn create_temporary_script(
    payload: &ShellScriptPayload,
) -> Result<(tempfile::TempPath, PathBuf, PathBuf), String> {
    let mut builder = tempfile::Builder::new();
    builder
        .prefix("webcodex-script-")
        .suffix(payload.language.file_extension());
    let mut file = builder
        .tempfile()
        .map_err(|error| script_setup_error("create", &error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| script_setup_error("secure", &error))?;
    }
    if payload.language == ShellScriptLanguage::Powershell {
        // Windows PowerShell 5.1 needs a UTF-8 BOM to preserve arbitrary
        // Unicode in script files. pwsh also accepts it. The BOM is encoding
        // metadata, so a leading param(...) block remains the first script
        // construct and no behavioral preamble is injected.
        file.write_all(&[0xEF, 0xBB, 0xBF])
            .map_err(|error| script_setup_error("write", &error))?;
    }
    file.write_all(payload.script.as_bytes())
        .and_then(|_| file.flush())
        .map_err(|error| script_setup_error("write", &error))?;
    let original_path = file.path().to_path_buf();
    // Avoid `canonicalize` here: on Windows it commonly adds a `\\?\` prefix
    // that Windows PowerShell 5.1 does not reliably accept for `-File`.
    let absolute_path = if file.path().is_absolute() {
        file.path().to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(file.path()))
            .map_err(|error| script_setup_error("resolve", &error))?
    };
    Ok((file.into_temp_path(), original_path, absolute_path))
}

fn redact_temporary_script_path(result: &mut ShellCommandResult, paths: &[&Path]) {
    for value in [
        result.result.stdout.as_mut(),
        result.result.stderr.as_mut(),
        result.result.error.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        for path in paths {
            let rendered = path.to_string_lossy();
            if !rendered.is_empty() {
                *value = value.replace(rendered.as_ref(), "<temporary-script>");
                let alternate = if rendered.contains('\\') {
                    rendered.replace('\\', "/")
                } else {
                    rendered.replace('/', "\\")
                };
                if alternate != rendered {
                    *value = value.replace(&alternate, "<temporary-script>");
                }
            }
        }
    }
}

pub(crate) fn base_shell_env(
    shell: &ShellConfig,
    profile: &ShellProfileConfig,
) -> Result<HashMap<String, String>, String> {
    let mut env: HashMap<String, String> = match shell.environment_mode {
        ShellEnvironmentMode::Inherit => std::env::vars()
            .filter(|(key, _)| should_inherit_env_key(key))
            .collect(),
        ShellEnvironmentMode::Isolated => {
            let mut env = HashMap::new();
            #[cfg(not(windows))]
            env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
            #[cfg(windows)]
            if let Ok(root) = std::env::var("SystemRoot") {
                let path = Path::new(&root).join("System32");
                env.insert("PATH".to_string(), path.to_string_lossy().into_owned());
                env.insert("SystemRoot".to_string(), root);
            }
            env
        }
    };
    if !shell.path_prepend.is_empty() {
        let mut paths = shell.path_prepend.clone();
        // The inherited Windows PATH may be spelled `Path`; lookup must be
        // case-insensitive or the prepended entries would replace it instead
        // of extending it.
        if let Some(current) = env_lookup(&env, "PATH") {
            paths.extend(std::env::split_paths(current));
        }
        let joined = std::env::join_paths(paths)
            .map_err(|e| format!("failed to build shell PATH from shell.path_prepend: {}", e))?;
        env_insert(&mut env, "PATH", joined.to_string_lossy().to_string());
    }
    for (key, value) in &shell.env {
        env_insert(&mut env, key, value.clone());
    }
    for (key, value) in &profile.env {
        env_insert(&mut env, key, value.clone());
    }
    // A profile could configure a sensitive name in any case; Windows filters
    // case-insensitively.
    remove_sensitive_env(&mut env);
    Ok(env)
}

fn stderr_tail(bytes: &[u8]) -> String {
    const MAX_ERR: usize = 4096;
    normalize_output_text(bytes, false, MAX_ERR, OutputTextSource::LocalProcess)
}

struct ProfilePreparePipeReader {
    stream_name: &'static str,
    result_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
    handle: std::thread::JoinHandle<()>,
}

impl ProfilePreparePipeReader {
    fn finish_until(self, deadline: Instant) -> Result<Vec<u8>, String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.result_rx.recv_timeout(remaining) {
            Ok(result) => {
                join_profile_prepare_reader_until(self.handle, deadline, self.stream_name)?;
                result
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "profile prepare {} reader did not finish before the cleanup deadline",
                self.stream_name
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                join_profile_prepare_reader_until(self.handle, deadline, self.stream_name)?;
                Err(format!(
                    "profile prepare {} reader exited without a result",
                    self.stream_name
                ))
            }
        }
    }
}

fn join_profile_prepare_reader_until(
    handle: std::thread::JoinHandle<()>,
    deadline: Instant,
    stream_name: &'static str,
) -> Result<(), String> {
    while !handle.is_finished() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "profile prepare {stream_name} reader did not join before the cleanup deadline"
            ));
        }
        std::thread::sleep(Duration::from_millis(5).min(remaining));
    }
    handle
        .join()
        .map_err(|_| format!("profile prepare {stream_name} reader panicked"))
}

fn spawn_profile_prepare_pipe_reader(
    stream_name: &'static str,
    mut pipe: impl Read + Send + 'static,
) -> ProfilePreparePipeReader {
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let result = pipe
            .read_to_end(&mut buf)
            .map(|_| buf)
            .map_err(|e| format!("failed to read profile prepare {stream_name}: {e}"));
        let _ = result_tx.send(result);
    });
    ProfilePreparePipeReader {
        stream_name,
        result_rx,
        handle,
    }
}

fn collect_profile_prepare_output(
    stdout: ProfilePreparePipeReader,
    stderr: ProfilePreparePipeReader,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let deadline = Instant::now() + PROFILE_PREPARE_PIPE_DRAIN_TIMEOUT;
    let stdout = stdout.finish_until(deadline)?;
    let stderr = stderr.finish_until(deadline)?;
    Ok((stdout, stderr))
}

fn run_prepare_command(
    mut cmd: Command,
    timeout: Duration,
    stop_requested: Option<&AtomicBool>,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    if stop_requested.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err("profile prepare stopped during runner shutdown".to_string());
    }
    // ManagedChild owns the whole profile-prepare process tree: a private
    // process group on Unix, a kill-on-close Job Object on Windows.
    let mut child = ManagedChild::spawn(cmd.stdout(Stdio::piped()).stderr(Stdio::piped()))
        .map_err(|e| format!("failed to spawn profile prepare command: {}", e))?;
    let stdout = match child.child_mut().stdout.take() {
        Some(stdout) => stdout,
        None => {
            let cleanup = terminate_child_without_output(child).err();
            return Err(with_cleanup_error(
                "profile prepare stdout pipe missing",
                cleanup,
            ));
        }
    };
    let stderr = match child.child_mut().stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            let cleanup = terminate_child_without_output(child).err();
            return Err(with_cleanup_error(
                "profile prepare stderr pipe missing",
                cleanup,
            ));
        }
    };
    let stdout_reader = spawn_profile_prepare_pipe_reader("stdout", stdout);
    let stderr_reader = spawn_profile_prepare_pipe_reader("stderr", stderr);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if stop_requested.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                    let cleanup = terminate_child_process_tree(&mut child).err();
                    let output = collect_profile_prepare_output(stdout_reader, stderr_reader).err();
                    return Err(with_cleanup_error(
                        output.map_or_else(
                            || "profile prepare stopped during runner shutdown".to_string(),
                            |error| {
                                format!(
                                    "profile prepare stopped during runner shutdown; failed to collect output: {error}"
                                )
                            },
                        ),
                        cleanup,
                    ));
                }
                if start.elapsed() >= timeout {
                    let cleanup = terminate_child_process_tree(&mut child).err();
                    return match collect_profile_prepare_output(stdout_reader, stderr_reader) {
                        Ok((_stdout, stderr)) => Err(format!(
                            "profile prepare timed out after {} seconds; stderr tail: {}{}",
                            timeout.as_secs(),
                            stderr_tail(&stderr),
                            cleanup
                                .as_deref()
                                .map(|error| format!("; cleanup failed: {error}"))
                                .unwrap_or_default(),
                        )),
                        Err(error) => Err(with_cleanup_error(
                            format!(
                                "profile prepare timed out after {} seconds; failed to collect output: {}",
                                timeout.as_secs(),
                                error
                            ),
                            cleanup,
                        )),
                    };
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let cleanup = terminate_child_process_tree(&mut child).err();
                let output = collect_profile_prepare_output(stdout_reader, stderr_reader).err();
                let base = match output {
                    Some(error) => {
                        format!("failed to wait profile prepare command: {}; failed to collect output: {}", e, error)
                    }
                    None => format!("failed to wait profile prepare command: {}", e),
                };
                return Err(with_cleanup_error(base, cleanup));
            }
        }
    };
    // The direct child has already exited, but its managed process tree can
    // still contain background descendants that inherited these pipe handles.
    // Terminate the whole tree before waiting on the readers so they see EOF
    // promptly.
    let cleanup = terminate_child_process_tree(&mut child).err();
    let output = collect_profile_prepare_output(stdout_reader, stderr_reader);
    match (cleanup, output) {
        (None, Ok((stdout, stderr))) => Ok((status, stdout, stderr)),
        (Some(cleanup), Ok(_)) => Err(format!(
            "failed to clean up profile prepare command process group: {cleanup}"
        )),
        (None, Err(error)) => Err(format!("failed to collect profile prepare output: {error}")),
        (Some(cleanup), Err(error)) => Err(format!(
            "failed to clean up profile prepare command process group: {cleanup}; failed to collect output: {error}"
        )),
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_env_payload(
    payload: &[u8],
    profile_name: &str,
) -> Result<HashMap<String, String>, String> {
    let mut env = HashMap::new();
    for entry in payload.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let Some(eq) = entry.iter().position(|byte| *byte == b'=') else {
            return Err(format!(
                "failed to parse env snapshot for profile '{}': entry missing '='",
                profile_name
            ));
        };
        let key = std::str::from_utf8(&entry[..eq]).map_err(|_| {
            format!(
                "failed to parse env snapshot for profile '{}': key is not UTF-8",
                profile_name
            )
        })?;
        if key.is_empty() {
            return Err(format!(
                "failed to parse env snapshot for profile '{}': empty env key",
                profile_name
            ));
        }
        let value = std::str::from_utf8(&entry[eq + 1..]).map_err(|_| {
            format!(
                "failed to parse env snapshot for profile '{}': value is not UTF-8",
                profile_name
            )
        })?;
        if should_inherit_env_key(key) {
            env.insert(key.to_string(), value.to_string());
        }
    }
    Ok(env)
}

/// POSIX profile-prepare script: `set -e`, the configured init snippet, a
/// marker line, then a NUL-delimited `env -0` dump. Unchanged from the legacy
/// Unix behavior.
fn posix_profile_prepare_script(init_script: &str, marker: &str) -> String {
    format!(
        "set -e\n{}\nprintf '\\n{}\\n'\nenv -0\n",
        init_script, marker
    )
}

/// PowerShell profile-prepare script. The UTF-8 preamble makes the env dump
/// deterministic Unicode; `$ErrorActionPreference = 'Stop'` mirrors `set -e`
/// for cmdlet errors (a terminating error in the snippet aborts preparation
/// and reports a non-zero exit instead of producing a truncated snapshot); the
/// trailing `$LASTEXITCODE` truthiness check mirrors it for the last native
/// command (`$LASTEXITCODE` is `$null` after a pure-PowerShell snippet, which
/// is falsy). The marker is a host-formatted line, then each environment
/// entry is written as `NAME=VALUE\0` straight through `[Console]::Out` — no
/// human-formatted table, no line wrapping, values may contain `=`, spaces,
/// quotes, newlines, or any shell metacharacter, and entries are unambiguous
/// because the value can never contain NUL. `Get-ChildItem Env:` reflects the
/// process block after the init snippet ran.
fn powershell_profile_prepare_script(init_script: &str, marker: &str) -> String {
    format!(
        "{POWERSHELL_UTF8_PREAMBLE}\n\
         $ErrorActionPreference = 'Stop'\n\
         try {{\n\
         {init_script}\n\
         if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}\n\
         }} catch {{\n\
         [Console]::Error.WriteLine($_)\n\
         exit 1\n\
         }}\n\
         Write-Output '{marker}'\n\
         Get-ChildItem Env: | ForEach-Object {{ \
         [Console]::Out.Write($_.Name + '=' + $_.Value + [string][char]0) }}\n\
         [Console]::Out.Flush()"
    )
}

fn capture_profile_env_snapshot(
    profile_name: &str,
    profile: &ShellProfileConfig,
    program: &str,
    args: &[String],
    dialect: ShellDialect,
    prepare_cwd: &Path,
    initial_env: HashMap<String, String>,
    stop_requested: Option<&AtomicBool>,
) -> Result<HashMap<String, String>, String> {
    let Some(init_script) = profile.init_script.as_deref() else {
        return Ok(initial_env);
    };
    let marker = format!("__WEBCODEX_ENV_START_{}__", uuid::Uuid::new_v4().simple());
    let prepare_script = match dialect {
        ShellDialect::Posix => posix_profile_prepare_script(init_script, &marker),
        ShellDialect::PowerShell => powershell_profile_prepare_script(init_script, &marker),
    };
    let mut cmd = Command::new(program);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg(prepare_script).current_dir(prepare_cwd).env_clear();
    // `run_prepare_command` owns this process tree through ManagedChild; do
    // not add a process-group pre_exec here. ManagedChild creates the private
    // process group (Unix) / Job Object (Windows) at spawn time.
    for (key, value) in initial_env {
        cmd.env(key, value);
    }
    let (status, stdout, stderr) = run_prepare_command(
        cmd,
        Duration::from_secs(SHELL_PROFILE_PREPARE_TIMEOUT_SECS),
        stop_requested,
    )
    .map_err(|e| {
        format!(
            "failed to prepare shell profile '{}' at {}: {}",
            profile_name,
            prepare_cwd.display(),
            e
        )
    })?;
    if !status.success() {
        return Err(format!(
            "failed to prepare shell profile '{}' at {}: exit code {}; stderr tail: {}",
            profile_name,
            prepare_cwd.display(),
            status.code().unwrap_or(-1),
            stderr_tail(&stderr)
        ));
    }
    let marker_pos = find_bytes(&stdout, marker.as_bytes()).ok_or_else(|| {
        format!(
            "failed to prepare shell profile '{}' at {}: env marker not found",
            profile_name,
            prepare_cwd.display()
        )
    })?;
    let mut payload_start = marker_pos + marker.len();
    while stdout
        .get(payload_start)
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        payload_start += 1;
    }
    let mut snapshot = parse_env_payload(&stdout[payload_start..], profile_name)?;
    // The init snippet may have exported a sensitive variable in any case;
    // Windows filters case-insensitively.
    remove_sensitive_env(&mut snapshot);
    Ok(snapshot)
}

impl PreparedShellProfileCache {
    /// Number of currently prepared snapshots. Used only for the sanitized
    /// observability summary; never exposes snapshot contents.
    pub(crate) fn len(&self) -> usize {
        self.profiles.lock().unwrap().len()
    }

    fn get_or_prepare(
        &self,
        generation: u64,
        shell: &ShellConfig,
        profile_name: &str,
        project_key: String,
        prepare_cwd: &Path,
        stop_requested: Option<&AtomicBool>,
    ) -> Result<Arc<PreparedShellProfile>, String> {
        let key = PreparedShellProfileKey {
            generation,
            project_key,
            profile_name: profile_name.to_string(),
        };
        let profiles = self.profiles.lock().unwrap();
        if let Some(prepared) = profiles.get(&key).cloned() {
            return Ok(prepared);
        }
        drop(profiles);
        let profile = shell.profiles.get(profile_name).ok_or_else(|| {
            format!(
                "shell profile '{}' is not configured for project/cwd {}",
                profile_name,
                prepare_cwd.display()
            )
        })?;
        let program = resolved_shell_program(
            &profile
                .program
                .clone()
                .unwrap_or_else(|| shell.program.clone()),
        );
        let args = profile.args.clone().unwrap_or_else(|| shell.args.clone());
        // The profile inherits the parent shell dialect unless it (or the
        // parent) explicitly configures one; the prepare script and every
        // later command in this profile use the same resolved dialect.
        let dialect = resolve_dialect(&program, profile.dialect.or(shell.dialect));
        let initial_env = base_shell_env(shell, profile)?;
        let env_snapshot = capture_profile_env_snapshot(
            profile_name,
            profile,
            &program,
            &args,
            dialect,
            prepare_cwd,
            initial_env,
            stop_requested,
        )?;
        let prepared = Arc::new(PreparedShellProfile {
            profile_name: profile_name.to_string(),
            program,
            args,
            dialect,
            env_snapshot,
        });
        let mut profiles = self.profiles.lock().unwrap();
        if let Some(cached) = profiles.get(&key).cloned() {
            return Ok(cached);
        }
        if profiles.keys().any(|cached| cached.generation > generation) {
            return Ok(prepared);
        }
        profiles.retain(|cached, _| cached.generation == generation);
        profiles.insert(key, prepared.clone());
        Ok(prepared)
    }
}

impl PreparedExecutionEnvironment {
    pub(crate) fn prepare(
        generation: u64,
        shell: &ShellConfig,
        explicit_profile: Option<&str>,
        prepare_cwd: &Path,
        cache: &PreparedShellProfileCache,
        stop_requested: Option<&AtomicBool>,
    ) -> Result<Self, String> {
        let profile_name = explicit_profile.or(shell.default_profile.as_deref());
        let env_snapshot = match profile_name {
            Some(profile_name) => cache
                .get_or_prepare(
                    generation,
                    shell,
                    profile_name,
                    format!(
                        "plugin:{}",
                        prepare_cwd
                            .canonicalize()
                            .unwrap_or_else(|_| prepare_cwd.to_path_buf())
                            .to_string_lossy()
                    ),
                    prepare_cwd,
                    stop_requested,
                )?
                .env_snapshot
                .clone(),
            None => base_shell_env(shell, &ShellProfileConfig::default())?,
        };
        Ok(Self { env_snapshot })
    }

    pub(crate) fn native_command(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> Result<Command, String> {
        let requested = {
            let path = Path::new(program);
            if !path.is_absolute() && path.components().count() > 1 {
                cwd.join(path).to_string_lossy().into_owned()
            } else {
                program.to_string()
            }
        };
        let path = env_lookup(&self.env_snapshot, "PATH")
            .map(OsString::from)
            .unwrap_or_default();
        let resolved =
            super::util::resolve_program_in_path(&requested, &path).ok_or_else(|| {
                format!("plugin executable is unavailable in prepared PATH: {program}")
            })?;
        #[cfg(windows)]
        let native = match resolved {
            super::util::ResolvedProgram::Native(path) => path,
            super::util::ResolvedProgram::Batch(_) => {
                return Err(
                    "unsupported_executable_type: native Tool Plugins cannot launch Windows .cmd/.bat files; configure a native runtime executable instead"
                        .to_string(),
                )
            }
        };
        #[cfg(not(windows))]
        let native = match resolved {
            super::util::ResolvedProgram::Native(path) => path,
        };
        let mut command = Command::new(native);
        command.args(args);
        command.current_dir(cwd);
        apply_env_snapshot(&mut command, &self.env_snapshot);
        Ok(command)
    }
}

fn shell_profile_project_key(project_id: Option<&str>, path: &Path) -> String {
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string();
    match project_id {
        Some(id) => format!("project:{}:{}", id, path),
        None => format!("cwd:{}", path),
    }
}

pub(crate) fn resolve_prepared_shell_profile(
    generation: u64,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cwd_path: &Path,
    request_has_cwd: bool,
    cache: &PreparedShellProfileCache,
    stop_requested: Option<&AtomicBool>,
) -> Result<Option<Arc<PreparedShellProfile>>, String> {
    let project = request_has_cwd
        .then(|| find_project_shell_context(project_registry_dir, cwd_path))
        .flatten();
    let profile_name = project
        .as_ref()
        .and_then(|project| project.shell_profile.as_deref())
        .or(shell.default_profile.as_deref());
    let Some(profile_name) = profile_name else {
        return Ok(None);
    };
    let prepare_cwd = project
        .as_ref()
        .map(|project| PathBuf::from(&project.path))
        .unwrap_or_else(|| cwd_path.to_path_buf());
    if let Some(project) = &project {
        if project.shell_profile.as_deref() == Some(profile_name)
            && !shell.profiles.contains_key(profile_name)
        {
            return Err(format!(
                "project '{}' shell_profile '{}' does not match any shell.profiles entry",
                project.id, profile_name
            ));
        }
    }
    let project_key = shell_profile_project_key(
        project.as_ref().map(|project| project.id.as_str()),
        &prepare_cwd,
    );
    cache
        .get_or_prepare(
            generation,
            shell,
            profile_name,
            project_key,
            &prepare_cwd,
            stop_requested,
        )
        .map(Some)
}

pub(crate) fn canonicalize_existing(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|e| format!("failed to access {}: {}", path.display(), e))
}

pub(crate) fn cwd_allowed(policy: &RunnerPolicy, cwd: &Path) -> Result<(), String> {
    if policy.allow_cwd_anywhere {
        return Ok(());
    }
    let cwd = canonicalize_existing(cwd)?;
    for root in &policy.allowed_roots {
        if let Ok(root) = canonicalize_existing(root) {
            // Case-insensitive component-wise containment on Windows.
            if webcodex_runner_config::paths::path_is_within(&cwd, &root) {
                return Ok(());
            }
        }
    }
    Err(format!(
        "cwd {} is outside allowed_roots",
        cwd.to_string_lossy()
    ))
}

#[derive(Debug)]
struct BoundedPipeTail {
    bytes: Vec<u8>,
    raw_truncated: bool,
    encoding: CapturedOutputEncoding,
}

impl BoundedPipeTail {
    fn normalize(&self, max_output_bytes: usize) -> String {
        normalize_captured_output_text(
            &self.bytes,
            self.raw_truncated,
            max_output_bytes,
            OutputTextSource::LocalProcess,
            self.encoding,
        )
    }

    #[cfg(test)]
    fn normalize_as_windows_for_test(&self, max_output_bytes: usize) -> String {
        super::output_text::normalize_captured_output_text_as_windows_for_test(
            &self.bytes,
            self.raw_truncated,
            max_output_bytes,
            self.encoding,
        )
    }
}

#[derive(Debug)]
struct IncrementalUtf8Validator {
    valid_so_far: bool,
    pending: Vec<u8>,
}

fn incomplete_utf8_sequence_len(first: u8) -> Option<usize> {
    match first {
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

impl IncrementalUtf8Validator {
    fn new() -> Self {
        Self {
            valid_so_far: true,
            pending: Vec::with_capacity(3),
        }
    }

    fn push(&mut self, mut bytes: &[u8]) {
        if !self.valid_so_far {
            return;
        }
        if !self.pending.is_empty() {
            let sequence_len = incomplete_utf8_sequence_len(self.pending[0])
                .expect("pending UTF-8 starts with a valid multibyte lead byte");
            let needed = sequence_len - self.pending.len();
            let take = needed.min(bytes.len());
            if take < needed {
                self.pending.extend_from_slice(&bytes[..take]);
                if std::str::from_utf8(&self.pending)
                    .is_err_and(|error| error.error_len().is_some())
                {
                    self.pending.clear();
                    self.valid_so_far = false;
                    return;
                }
                debug_assert!(self.pending.len() <= 3);
                return;
            }
            let mut sequence = [0_u8; 4];
            sequence[..self.pending.len()].copy_from_slice(&self.pending);
            sequence[self.pending.len()..sequence_len].copy_from_slice(&bytes[..take]);
            self.pending.clear();
            if std::str::from_utf8(&sequence[..sequence_len]).is_err() {
                self.valid_so_far = false;
                return;
            }
            bytes = &bytes[take..];
        }
        match std::str::from_utf8(bytes) {
            Ok(_) => {}
            Err(error) if error.error_len().is_some() => {
                self.valid_so_far = false;
            }
            Err(error) => {
                self.pending
                    .extend_from_slice(&bytes[error.valid_up_to()..]);
                debug_assert!(self.pending.len() <= 3);
            }
        }
    }

    fn finish(&self) -> FullStreamUtf8Validity {
        if self.valid_so_far && self.pending.is_empty() {
            FullStreamUtf8Validity::Valid
        } else {
            FullStreamUtf8Validity::Invalid
        }
    }
}

fn with_cleanup_error(base: impl Into<String>, cleanup: Option<String>) -> String {
    match cleanup {
        Some(cleanup) => format!("{}; cleanup failed: {}", base.into(), cleanup),
        None => base.into(),
    }
}

/// Terminate a command and its entire managed process tree, then confirm the
/// whole tree exited and reap the direct child. Callers do this before waiting
/// for output pipes, so descendants cannot keep them open after a timeout,
/// stop, executor failure, or direct-child exit.
///
/// The whole tree is owned by the [`ManagedChild`]: a private process group on
/// Unix, a kill-on-close Job Object on Windows. No pid/pgid is handled here
/// directly.
fn terminate_child_process_tree(child: &mut ManagedChild) -> Result<(), String> {
    terminate_child_process_tree_until(child, Instant::now() + PROCESS_TREE_CLEANUP_TIMEOUT)
}

/// Terminate the managed command tree within one overall cleanup deadline.
///
/// Every phase recomputes its remaining budget from the single deadline, so an
/// expired graceful phase can never silently reuse a stale deadline for the
/// force-confirmation wait.
///
/// 1. Graceful request ([`GracefulTermination`]): on Unix this delivers SIGTERM
///    to the whole managed process group, which gets a bounded grace
///    ([`PROCESS_GROUP_TERMINATION_GRACE`]) to exit on its own; on Windows the
///    request reports `Unsupported` and the next phase escalates immediately.
/// 2. Force phase: `terminate_tree` for anything still alive.
/// 3. Whole-tree exit confirmation: `wait_tree_exit`, not just the direct
///    child (a direct-child exit never proves the tree is gone).
/// 4. Direct-child reap within the remaining budget.
fn terminate_child_process_tree_until(
    child: &mut ManagedChild,
    deadline: Instant,
) -> Result<(), String> {
    let mut errors = Vec::new();
    let mut tree_exited = false;
    let mut force_tree = false;
    match child.request_terminate_tree() {
        Ok(GracefulTermination::Requested) => {
            let grace_deadline = deadline.min(Instant::now() + PROCESS_GROUP_TERMINATION_GRACE);
            let grace = grace_deadline.saturating_duration_since(Instant::now());
            match child.wait_tree_exit(grace) {
                Ok(true) => tree_exited = true,
                Ok(false) => force_tree = true,
                Err(error) => {
                    errors.push(format!(
                        "failed to wait for command process tree graceful exit: {error}"
                    ));
                    force_tree = true;
                }
            }
        }
        // Once the backend reports that the tree is already gone, do not
        // probe the numeric Unix process-group id again. The direct child may
        // already have been reaped, allowing that id to be reused by an
        // unrelated process group between calls.
        Ok(GracefulTermination::AlreadyExited) => tree_exited = true,
        Ok(GracefulTermination::Unsupported) => force_tree = true,
        Err(error) => {
            errors.push(format!(
                "failed to request graceful command process tree termination: {error}"
            ));
            force_tree = true;
        }
    }
    if force_tree {
        if let Err(error) = child.terminate_tree() {
            errors.push(format!("failed to terminate command process tree: {error}"));
        }
    }
    if !tree_exited {
        // Confirm the complete tree exited, not just the direct child.
        // Forceful termination can complete asynchronously (notably Job Object
        // teardown on Windows), so use the remaining cleanup budget.
        let remaining = deadline.saturating_duration_since(Instant::now());
        match child.wait_tree_exit(remaining) {
            Ok(true) => {}
            Ok(false) => {
                errors.push("command process tree did not exit before deadline".to_string())
            }
            Err(error) => errors.push(format!(
                "failed to wait for command process tree exit: {error}"
            )),
        }
    }
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    errors.push("command child reap timed out".to_string());
                    break;
                }
                std::thread::sleep(Duration::from_millis(10).min(remaining));
            }
            Err(error) => {
                errors.push(format!("failed to reap command child: {error}"));
                break;
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn terminate_child_without_output(mut child: ManagedChild) -> Result<(), String> {
    let result = terminate_child_process_tree(&mut child);
    // The direct child has been reaped above. Closing the local pipe handles
    // is sufficient on error paths where the response intentionally has no
    // command output.
    drop(child.child_mut().stdout.take());
    drop(child.child_mut().stderr.take());
    result
}

struct ContinuousPipeDrain {
    stdout_rx: mpsc::Receiver<Result<BoundedPipeTail, String>>,
    stderr_rx: mpsc::Receiver<Result<BoundedPipeTail, String>>,
    stdout_handle: std::thread::JoinHandle<()>,
    stderr_handle: std::thread::JoinHandle<()>,
}

impl ContinuousPipeDrain {
    /// Take both child pipes and start independent bounded readers immediately.
    ///
    /// The readers own the OS pipe handles for the full child lifetime. They do
    /// not forward per-chunk data through another bounded queue, so Server/model
    /// observation cannot backpressure either stream. Each reader retains only
    /// the existing bounded tail plus UTF-8/BOM validation state.
    fn start(child: &mut ManagedChild, max_output_bytes: usize) -> Result<Self, String> {
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| "stdout pipe missing".to_string())?;
        let stderr = match child.child_mut().stderr.take() {
            Some(stderr) => stderr,
            None => {
                drop(stdout);
                return Err("stderr pipe missing".to_string());
            }
        };
        let (stdout_tx, stdout_rx) = mpsc::sync_channel(1);
        let stdout_handle = std::thread::spawn(move || {
            let _ = stdout_tx.send(read_bounded_pipe_tail(stdout, max_output_bytes, "stdout"));
        });
        let (stderr_tx, stderr_rx) = mpsc::sync_channel(1);
        let stderr_handle = std::thread::spawn(move || {
            let _ = stderr_tx.send(read_bounded_pipe_tail(stderr, max_output_bytes, "stderr"));
        });
        Ok(Self {
            stdout_rx,
            stderr_rx,
            stdout_handle,
            stderr_handle,
        })
    }

    fn finish_until(self, deadline: Instant) -> Result<(BoundedPipeTail, BoundedPipeTail), String> {
        let Self {
            stdout_rx,
            stderr_rx,
            stdout_handle,
            stderr_handle,
        } = self;
        let receive = |rx: mpsc::Receiver<Result<BoundedPipeTail, String>>,
                       stream_name: &'static str|
         -> Result<BoundedPipeTail, String> {
            let remaining = deadline.saturating_duration_since(Instant::now());
            rx.recv_timeout(remaining).map_err(|_| {
                format!("{stream_name} reader did not finish before cleanup deadline")
            })?
        };
        // Always give both already-running readers the same drain deadline.
        // A read error on one stream must not short-circuit collection of the
        // other stream and turn an otherwise bounded reader into a detached one.
        let stdout = receive(stdout_rx, "stdout");
        let stderr = receive(stderr_rx, "stderr");
        if stdout_handle.is_finished() {
            let _ = stdout_handle.join();
        }
        if stderr_handle.is_finished() {
            let _ = stderr_handle.join();
        }
        match (stdout, stderr) {
            (Ok(stdout), Ok(stderr)) => Ok((stdout, stderr)),
            (Err(stdout), Ok(_)) => Err(stdout),
            (Ok(_), Err(stderr)) => Err(stderr),
            (Err(stdout), Err(stderr)) => Err(format!("{stdout}; {stderr}")),
        }
    }
}

fn wait_child_until(
    child: &mut ManagedChild,
    deadline: Instant,
) -> Result<std::process::ExitStatus, String> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("command child wait timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(10).min(remaining));
            }
            Err(error) => return Err(format!("failed to wait command: {error}")),
        }
    }
}

/// Finish one command whose stdout/stderr readers have already been draining
/// continuously since immediately after spawn. Tree termination still happens
/// before waiting for reader EOF so a descendant that inherited a pipe cannot
/// keep the readers alive after direct-child exit, stop, or timeout.
///
/// Tree cleanup/reaping and pipe drain are separate bounded phases. A saturated
/// CI/runtime scheduler can consume the tree-cleanup budget after the child has
/// already exited; reusing that expired deadline for readers would incorrectly
/// turn a known terminal status into `OutcomeUnknown`. stdout/stderr still share
/// one drain deadline, so neither stream receives an independently reset budget.
fn terminate_and_collect_pipes(
    mut child: ManagedChild,
    drains: ContinuousPipeDrain,
) -> Result<(std::process::ExitStatus, BoundedPipeTail, BoundedPipeTail), String> {
    let cleanup_deadline = Instant::now() + PROCESS_TREE_CLEANUP_TIMEOUT;
    let cleanup = terminate_child_process_tree_until(&mut child, cleanup_deadline).err();
    let status = wait_child_until(&mut child, cleanup_deadline);
    let drain_deadline = Instant::now() + PROCESS_PIPE_DRAIN_TIMEOUT;
    let output = drains.finish_until(drain_deadline);
    let mut errors = Vec::new();
    if let Some(cleanup) = cleanup {
        errors.push(format!(
            "failed to terminate command process tree: {cleanup}"
        ));
    }
    let status = match status {
        Ok(status) => Some(status),
        Err(error) => {
            errors.push(error);
            None
        }
    };
    let output = match output {
        Ok(output) => Some(output),
        Err(error) => {
            errors.push(format!("failed to collect output: {error}"));
            None
        }
    };
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    let (stdout, stderr) = output.expect("output exists when no collection error was recorded");
    Ok((
        status.expect("status exists when no wait error was recorded"),
        stdout,
        stderr,
    ))
}

/// Test/error-path convenience wrapper. Production structured execution starts
/// the drain immediately after spawn rather than waiting until termination.
#[cfg(test)]
fn terminate_and_read_pipes(
    mut child: ManagedChild,
    max_output_bytes: usize,
) -> Result<(std::process::ExitStatus, BoundedPipeTail, BoundedPipeTail), String> {
    let drains = match ContinuousPipeDrain::start(&mut child, max_output_bytes) {
        Ok(drains) => drains,
        Err(error) => {
            let cleanup = terminate_child_process_tree(&mut child).err();
            return Err(with_cleanup_error(error, cleanup));
        }
    };
    terminate_and_collect_pipes(child, drains)
}

fn read_bounded_pipe_tail(
    mut pipe: impl Read,
    max_bytes: usize,
    stream_name: &'static str,
) -> Result<BoundedPipeTail, String> {
    // Four extra raw bytes retain truncation evidence plus enough alignment
    // room for the largest UTF-8 scalar. The decoded result is independently
    // bounded after transcoding.
    let retained_limit = max_bytes
        .saturating_add(RAW_TAIL_CAPTURE_ALLOWANCE)
        .max(RAW_TAIL_CAPTURE_ALLOWANCE);
    let mut output = Vec::with_capacity(retained_limit.min(64 * 1024));
    let mut prefix = Vec::with_capacity(3);
    let mut utf8_validator = IncrementalUtf8Validator::new();
    let mut total_bytes = 0usize;
    let mut raw_truncated = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let read = pipe
            .read(&mut chunk)
            .map_err(|error| format!("failed to read {stream_name}: {error}"))?;
        if read == 0 {
            let encoding = CapturedOutputEncoding {
                full_stream_utf8: utf8_validator.finish(),
                leading_bom: leading_bom(&prefix),
            };
            if raw_truncated {
                align_and_restore_bom(encoding, total_bytes, &mut output);
            }
            return Ok(BoundedPipeTail {
                bytes: output,
                raw_truncated,
                encoding,
            });
        }
        if prefix.len() < 3 {
            let prefix_bytes = (3 - prefix.len()).min(read);
            prefix.extend_from_slice(&chunk[..prefix_bytes]);
        }
        utf8_validator.push(&chunk[..read]);
        total_bytes = total_bytes.saturating_add(read);
        output.extend_from_slice(&chunk[..read]);
        if output.len() > retained_limit {
            let discard = output.len() - retained_limit;
            output.drain(..discard);
            raw_truncated = true;
        }
    }
}

fn leading_bom(prefix: &[u8]) -> LeadingBom {
    if prefix.starts_with(&[0xEF, 0xBB, 0xBF]) {
        LeadingBom::Utf8
    } else if prefix.starts_with(&[0xFF, 0xFE]) {
        LeadingBom::Utf16Le
    } else if prefix.starts_with(&[0xFE, 0xFF]) {
        LeadingBom::Utf16Be
    } else {
        LeadingBom::None
    }
}

fn align_and_restore_bom(encoding: CapturedOutputEncoding, total_bytes: usize, tail: &mut Vec<u8>) {
    match encoding.leading_bom {
        LeadingBom::Utf8 => restore_utf8_bom(tail),
        LeadingBom::Utf16Le => restore_utf16_bom(tail, total_bytes, true),
        LeadingBom::Utf16Be => restore_utf16_bom(tail, total_bytes, false),
        LeadingBom::None if encoding.full_stream_utf8 == FullStreamUtf8Validity::Valid => {
            align_valid_utf8_tail(tail);
        }
        LeadingBom::None => {}
    }
}

fn align_valid_utf8_tail(tail: &mut Vec<u8>) {
    let discard = tail
        .iter()
        .take(3)
        .take_while(|byte| **byte & 0b1100_0000 == 0b1000_0000)
        .count();
    tail.drain(..discard);
}

fn restore_utf8_bom(tail: &mut Vec<u8>) {
    let replace = 3.min(tail.len());
    tail.drain(..replace);
    align_valid_utf8_tail(tail);
    let mut with_bom = Vec::with_capacity(3 + tail.len());
    with_bom.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    with_bom.extend_from_slice(tail);
    *tail = with_bom;
}

fn restore_utf16_bom(tail: &mut Vec<u8>, total_bytes: usize, little_endian: bool) {
    let mut tail_start = total_bytes.saturating_sub(tail.len());
    let alignment_discard = if tail_start < 2 {
        2 - tail_start
    } else {
        (tail_start - 2) % 2
    }
    .min(tail.len());
    if alignment_discard > 0 {
        tail.drain(..alignment_discard);
        tail_start = tail_start.saturating_add(alignment_discard);
    }
    debug_assert!(tail_start >= 2 || tail.is_empty());
    let replace = 2.min(tail.len());
    tail.drain(..replace);
    if tail.len() >= 2 {
        let first = if little_endian {
            u16::from_le_bytes([tail[0], tail[1]])
        } else {
            u16::from_be_bytes([tail[0], tail[1]])
        };
        if (0xDC00..=0xDFFF).contains(&first) {
            tail.drain(..2);
        }
    }
    let bom = if little_endian {
        [0xFF, 0xFE]
    } else {
        [0xFE, 0xFF]
    };
    let mut with_bom = Vec::with_capacity(2 + tail.len());
    with_bom.extend_from_slice(&bom);
    with_bom.extend_from_slice(tail);
    *tail = with_bom;
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_process_with_profiles_and_execution_state(
    generation: u64,
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    executable: &str,
    args: &[String],
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> ShellCommandResult {
    run_process_with_profiles_and_execution_state_with_start_hook(
        generation,
        policy,
        shell,
        project_registry_dir,
        cache,
        cwd,
        executable,
        args,
        stdin,
        timeout_secs,
        stop_requested,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_detached_process_launch(
    generation: u64,
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    executable: &str,
    args: &[String],
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> Result<PreparedDetachedProcessLaunch, String> {
    // Detached structured execution has the same local policy boundary as the
    // ordinary native-argv path. This helper performs preparation only; it never
    // spawns the requested payload.
    if !policy.allow_raw_shell {
        return Err("structured process execution is disabled by local Runner policy".to_string());
    }
    let cwd_path = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    cwd_allowed(policy, &cwd_path)?;
    let timeout_secs = timeout_secs.min(policy.max_timeout_secs).max(1);
    let profile = resolve_prepared_shell_profile(
        generation,
        shell,
        project_registry_dir,
        &cwd_path,
        cwd.is_some(),
        cache,
        stop_requested,
    )?;
    let resolved_program =
        resolve_process_program(shell, profile.as_deref(), executable, Some(&cwd_path))?;
    // Validate the same batch argv contract before a detached Job is accepted.
    let _ = structured_process_command(&resolved_program, args, Some(&cwd_path))?;
    let resolved_program = resolved_program.into_string().map_err(|_| {
        "structured process executable resolved to a non-UTF-8 native path".to_string()
    })?;
    let cwd = cwd_path
        .to_str()
        .ok_or_else(|| "structured process cwd resolved to a non-UTF-8 native path".to_string())?
        .to_string();
    let env = match profile.as_deref() {
        Some(profile) => profile.env_snapshot.clone(),
        None => base_shell_env(shell, &ShellProfileConfig::default())?,
    };
    let mut env = env.into_iter().collect::<Vec<_>>();
    env.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(PreparedDetachedProcessLaunch {
        process: ShellProcessArgv {
            executable: resolved_program,
            args: args.to_vec(),
        },
        cwd,
        env,
        timeout_secs,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_process_with_profiles_and_execution_state_with_start_hook(
    generation: u64,
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    executable: &str,
    args: &[String],
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
    on_started: Option<&dyn Fn()>,
) -> ShellCommandResult {
    // Structured execution intentionally receives the same policy treatment
    // as run_shell. Absence of shell syntax is not a permission bypass.
    if !policy.allow_raw_shell {
        return ShellCommandResult::not_started(CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(0),
            error: Some("structured process execution is disabled by local Runner policy".into()),
        });
    }
    let cwd_path = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    if let Err(error) = cwd_allowed(policy, &cwd_path) {
        return ShellCommandResult::not_started(CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(0),
            error: Some(error),
        });
    }
    let timeout_secs = timeout_secs.min(policy.max_timeout_secs).max(1);
    let start = Instant::now();
    let profile = match resolve_prepared_shell_profile(
        generation,
        shell,
        project_registry_dir,
        &cwd_path,
        cwd.is_some(),
        cache,
        stop_requested,
    ) {
        Ok(profile) => profile,
        Err(error) => {
            return ShellCommandResult::not_started(CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(error),
            })
        }
    };
    let cmd = match configured_process_command(
        shell,
        profile.as_deref(),
        executable,
        args,
        Some(&cwd_path),
    ) {
        Ok(cmd) => cmd,
        Err(error) => {
            return ShellCommandResult::not_started(CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(error),
            })
        }
    };
    execute_configured_command(
        policy,
        cmd,
        &cwd_path,
        stdin,
        timeout_secs,
        stop_requested,
        start,
        "failed to spawn structured process",
        on_started,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_internal_search_script_with_profiles_and_execution_state(
    generation: u64,
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    script: &str,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> ShellCommandResult {
    run_internal_posix_script_impl(
        generation,
        policy,
        shell,
        project_registry_dir,
        cache,
        cwd,
        script,
        timeout_secs,
        stop_requested,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_internal_posix_script_with_profiles_and_execution_state(
    generation: u64,
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    script: &str,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> ShellCommandResult {
    run_internal_posix_script_impl(
        generation,
        policy,
        shell,
        project_registry_dir,
        cache,
        cwd,
        script,
        timeout_secs,
        stop_requested,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_internal_posix_script_impl(
    generation: u64,
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    script: &str,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
    search_compat: bool,
) -> ShellCommandResult {
    #[cfg(not(windows))]
    let _ = search_compat;
    #[cfg(not(windows))]
    {
        let payload = ShellScriptPayload {
            language: ShellScriptLanguage::Sh,
            script: script.to_string(),
            args: Vec::new(),
        };
        return run_script_with_profiles_and_execution_state(
            generation,
            policy,
            shell,
            project_registry_dir,
            cache,
            cwd,
            &payload,
            None,
            timeout_secs,
            stop_requested,
        );
    }

    #[cfg(windows)]
    {
        // WebCodex-generated POSIX programs never inherit the configured shell
        // parser. Native Windows Bash launchers map the process cwd; `bash -s`
        // consumes the generated bytes directly from stdin without asking Bash
        // to interpret a Windows temporary-script path.
        if !policy.allow_raw_shell {
            let subject = if search_compat {
                "internal search script"
            } else {
                "internal POSIX script"
            };
            return ShellCommandResult::not_started(CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(0),
                error: Some(format!(
                    "{subject} execution is disabled by local Runner policy"
                )),
            });
        }
        if script.is_empty() {
            let subject = if search_compat {
                "internal search script"
            } else {
                "internal POSIX script"
            };
            return ShellCommandResult::not_started(CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(0),
                error: Some(format!("{subject} is empty; command was not started")),
            });
        }
        let cwd_path = cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        if let Err(error) = cwd_allowed(policy, &cwd_path) {
            return ShellCommandResult::not_started(CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(0),
                error: Some(error),
            });
        }
        let timeout_secs = timeout_secs.min(policy.max_timeout_secs).max(1);
        let start = Instant::now();
        let profile = match resolve_prepared_shell_profile(
            generation,
            shell,
            project_registry_dir,
            &cwd_path,
            cwd.is_some(),
            cache,
            stop_requested,
        ) {
            Ok(profile) => profile,
            Err(error) => {
                return ShellCommandResult::not_started(CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(error),
                })
            }
        };
        let interpreter =
            match resolve_windows_internal_posix_interpreter(shell, profile.as_deref()) {
                Ok(interpreter) => interpreter,
                Err(error) => {
                    return ShellCommandResult::not_started(CommandResult {
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: Some(start.elapsed().as_millis() as u64),
                        error: Some(error),
                    })
                }
            };
        let mut command = Command::new(interpreter);
        command.arg("-s");
        match profile.as_deref() {
            Some(profile) => apply_env_snapshot(&mut command, &profile.env_snapshot),
            None => {
                if let Err(error) = apply_shell_environment(&mut command, shell) {
                    return ShellCommandResult::not_started(CommandResult {
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: Some(start.elapsed().as_millis() as u64),
                        error: Some(error),
                    });
                }
            }
        }
        let script = if search_compat {
            // WSL/Git-Bash interop may expose a host ripgrep binary as `rg.exe`
            // rather than `rg`. Keep the existing search script and its rg-first
            // semantics unchanged by providing a process-local function only when
            // no native `rg` command is already visible. Relative project paths are
            // preserved across the Windows/Bash cwd mapping.
            format!(
                "if ! command -v rg >/dev/null 2>&1 && command -v rg.exe >/dev/null 2>&1; then rg() {{ command rg.exe --path-separator / \"$@\"; }}; fi\n{script}"
            )
        } else {
            script.to_string()
        };
        let spawn_error = if search_compat {
            "failed to spawn internal search interpreter"
        } else {
            "failed to spawn internal POSIX interpreter"
        };

        execute_configured_command(
            policy,
            command,
            &cwd_path,
            Some(&script),
            timeout_secs,
            stop_requested,
            start,
            spawn_error,
            None,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_script_with_profiles_and_execution_state(
    generation: u64,
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    payload: &ShellScriptPayload,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> ShellCommandResult {
    run_script_with_profiles_and_execution_state_with_start_hook(
        generation,
        policy,
        shell,
        project_registry_dir,
        cache,
        cwd,
        payload,
        stdin,
        timeout_secs,
        stop_requested,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_script_with_profiles_and_execution_state_with_start_hook(
    generation: u64,
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    payload: &ShellScriptPayload,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
    on_started: Option<&dyn Fn()>,
) -> ShellCommandResult {
    // Typed script execution is consequential and receives the same Runner
    // policy treatment as raw shell and structured native processes.
    if !policy.allow_raw_shell {
        return ShellCommandResult::not_started(CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(0),
            error: Some("structured script execution is disabled by local Runner policy".into()),
        });
    }
    let cwd_path = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    if let Err(error) = cwd_allowed(policy, &cwd_path) {
        return ShellCommandResult::not_started(CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(0),
            error: Some(error),
        });
    }
    let timeout_secs = timeout_secs.min(policy.max_timeout_secs).max(1);
    let start = Instant::now();
    let profile = match resolve_prepared_shell_profile(
        generation,
        shell,
        project_registry_dir,
        &cwd_path,
        cwd.is_some(),
        cache,
        stop_requested,
    ) {
        Ok(profile) => profile,
        Err(error) => {
            return ShellCommandResult::not_started(CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(error),
            })
        }
    };
    // Resolve the semantic interpreter before creating the payload file. A
    // missing interpreter is therefore a definite pre-start rejection with
    // no script side effect and no fallback to the configured shell parser.
    let interpreter =
        match configured_script_interpreter(shell, profile.as_deref(), payload.language) {
            Ok(interpreter) => interpreter,
            Err(error) => {
                return ShellCommandResult::not_started(CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(error),
                })
            }
        };
    let (temporary_path, original_path, absolute_path) = match create_temporary_script(payload) {
        Ok(temporary) => temporary,
        Err(error) => {
            return ShellCommandResult::not_started(CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(error),
            })
        }
    };
    let mut command =
        build_script_command(interpreter, payload.language, &absolute_path, &payload.args);
    match profile.as_deref() {
        Some(profile) => apply_env_snapshot(&mut command, &profile.env_snapshot),
        None => {
            if let Err(error) = apply_shell_environment(&mut command, shell) {
                return ShellCommandResult::not_started(CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(error),
                });
            }
        }
    }
    let mut result = execute_configured_command(
        policy,
        command,
        &cwd_path,
        stdin,
        timeout_secs,
        stop_requested,
        start,
        "failed to spawn script interpreter",
        on_started,
    );
    redact_temporary_script_path(&mut result, &[original_path.as_path(), &absolute_path]);
    if let Err(error) = temporary_path.close() {
        // Cleanup is infrastructure after the child result is already known.
        // Never rewrite completed/timed_out/outcome_unknown lifecycle truth.
        tracing::warn!(
            language = payload.language.as_str(),
            error_kind = ?error.kind(),
            "failed to remove Runner-owned temporary script file"
        );
    }
    result
}

// Test-only wrapper for callers that do not need prepared shell profiles; the
// production request path uses `run_shell_with_profiles` directly.
#[cfg(test)]
pub(crate) fn run_shell(
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> CommandResult {
    run_shell_impl(
        policy,
        shell,
        None,
        cwd,
        command,
        stdin,
        timeout_secs,
        stop_requested,
    )
    .result
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_shell_with_profiles(
    generation: u64,
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> CommandResult {
    run_shell_with_profiles_and_execution_state(
        generation,
        policy,
        shell,
        project_registry_dir,
        cache,
        cwd,
        command,
        stdin,
        timeout_secs,
        stop_requested,
    )
    .result
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_shell_with_profiles_and_execution_state(
    generation: u64,
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    project_registry_dir: &Path,
    cache: &PreparedShellProfileCache,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> ShellCommandResult {
    run_shell_impl(
        policy,
        shell,
        Some((generation, project_registry_dir, cache)),
        cwd,
        command,
        stdin,
        timeout_secs,
        stop_requested,
    )
}

fn run_shell_impl(
    policy: &RunnerPolicy,
    shell: &ShellConfig,
    profiles: Option<(u64, &Path, &PreparedShellProfileCache)>,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
) -> ShellCommandResult {
    if !policy.allow_raw_shell {
        return ShellCommandResult::not_started(CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(0),
            error: Some("raw shell is disabled by local Runner policy".to_string()),
        });
    }
    let cwd_path = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    if let Err(e) = cwd_allowed(policy, &cwd_path) {
        return ShellCommandResult::not_started(CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(0),
            error: Some(e),
        });
    }
    let timeout_secs = timeout_secs.min(policy.max_timeout_secs).max(1);
    let start = Instant::now();
    let mut prepared_profile_name = None;
    let cmd = match profiles {
        Some((generation, project_registry_dir, cache)) => match resolve_prepared_shell_profile(
            generation,
            shell,
            project_registry_dir,
            &cwd_path,
            cwd.is_some(),
            cache,
            stop_requested,
        ) {
            Ok(Some(profile)) => match configured_prepared_shell_command(&profile, command) {
                Ok(cmd) => {
                    prepared_profile_name = Some(profile.profile_name.clone());
                    cmd
                }
                Err(e) => {
                    return ShellCommandResult::not_started(CommandResult {
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: Some(start.elapsed().as_millis() as u64),
                        error: Some(format!(
                            "failed to configure shell profile '{}': {}",
                            profile.profile_name, e
                        )),
                    })
                }
            },
            Ok(None) => match configured_shell_command(shell, command) {
                Ok(cmd) => cmd,
                Err(e) => {
                    return ShellCommandResult::not_started(CommandResult {
                        exit_code: None,
                        stdout: None,
                        stderr: None,
                        duration_ms: Some(start.elapsed().as_millis() as u64),
                        error: Some(e),
                    })
                }
            },
            Err(e) => {
                return ShellCommandResult::not_started(CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(e),
                })
            }
        },
        None => match configured_shell_command(shell, command) {
            Ok(cmd) => cmd,
            Err(e) => {
                return ShellCommandResult::not_started(CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(e),
                })
            }
        },
    };
    let spawn_error_prefix = prepared_profile_name
        .as_deref()
        .map(|profile_name| format!("failed to spawn shell profile '{profile_name}'"))
        .unwrap_or_else(|| "failed to spawn command".to_string());
    execute_configured_command(
        policy,
        cmd,
        &cwd_path,
        stdin,
        timeout_secs,
        stop_requested,
        start,
        &spawn_error_prefix,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_configured_command(
    policy: &RunnerPolicy,
    mut cmd: Command,
    cwd_path: &Path,
    stdin: Option<&str>,
    timeout_secs: u64,
    stop_requested: Option<&AtomicBool>,
    start: Instant,
    spawn_error_prefix: &str,
    on_started: Option<&dyn Fn()>,
) -> ShellCommandResult {
    cmd.current_dir(cwd_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        // Never leak the Runner's own stdin into user subprocesses. Desktop
        // deliberately keeps the Runner stdin pipe open as its parent-liveness
        // lease; inheriting that handle lets grandchildren retain the lease and
        // also gives ordinary no-input commands a long-lived parent pipe instead
        // of an explicit EOF source. Structured commands with no stdin contract
        // receive a closed/null input handle instead.
        cmd.stdin(Stdio::null());
    }
    // ManagedChild owns the whole process tree: a private process group on
    // Unix, a kill-on-close Job Object on Windows. `child_mut()` below only
    // accesses pipe handles; every termination still uses the managed tree.
    let spawn = ManagedChild::spawn(&mut cmd);
    let mut child = match spawn {
        Ok(child) => child,
        Err(error) => {
            return ShellCommandResult::not_started(CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("{spawn_error_prefix}: {error}")),
            });
        }
    };
    // Structured execution must drain both OS pipes for the entire child
    // lifetime. Starting independent bounded readers before any lifecycle wait
    // prevents either stdout or stderr pipe capacity from throttling the child,
    // even when no caller observes Job logs until terminal completion.
    let drains = match ContinuousPipeDrain::start(&mut child, policy.max_output_bytes) {
        Ok(drains) => drains,
        Err(error) => {
            let cleanup = terminate_child_process_tree(&mut child).err();
            return ShellCommandResult::outcome_unknown(CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(with_cleanup_error(error, cleanup)),
            });
        }
    };
    if let Some(on_started) = on_started {
        on_started();
    }
    let mut stdin_writer = match stdin {
        Some(input) => match child.child_mut().stdin.take() {
            Some(mut child_stdin) => {
                let input = input.as_bytes().to_vec();
                let (tx, rx) = mpsc::sync_channel(1);
                std::thread::spawn(move || {
                    let _ = tx.send(child_stdin.write_all(&input));
                });
                Some(rx)
            }
            None => {
                let cleanup = terminate_and_collect_pipes(child, drains).err();
                return ShellCommandResult::outcome_unknown(CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(with_cleanup_error("stdin pipe missing", cleanup)),
                });
            }
        },
        None => None,
    };
    loop {
        if let Err(error) = poll_stdin_writer(&mut stdin_writer) {
            let cleanup = terminate_and_collect_pipes(child, drains).err();
            return ShellCommandResult::outcome_unknown(CommandResult {
                exit_code: None,
                stdout: None,
                stderr: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(with_cleanup_error(
                    format!("failed to write command stdin: {error}"),
                    cleanup,
                )),
            });
        }
        if stop_requested
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
        {
            let duration_ms = start.elapsed().as_millis() as u64;
            return match terminate_and_collect_pipes(child, drains) {
                Ok((_status, stdout, stderr)) => {
                    let mut stderr = stderr.normalize(policy.max_output_bytes);
                    append_bounded_text(
                        &mut stderr,
                        "job stopped by request",
                        policy.max_output_bytes,
                    );
                    ShellCommandResult::completed(CommandResult {
                        exit_code: Some(-1),
                        stdout: Some(stdout.normalize(policy.max_output_bytes)),
                        stderr: Some(stderr),
                        duration_ms: Some(duration_ms),
                        error: Some("job stopped".to_string()),
                    })
                }
                Err(e) => ShellCommandResult::outcome_unknown(CommandResult {
                    exit_code: Some(-1),
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(duration_ms),
                    error: Some(format!("job stopped; failed to collect output: {}", e)),
                }),
            };
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= Duration::from_secs(timeout_secs) {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    return match terminate_and_collect_pipes(child, drains) {
                        Ok((_status, stdout, stderr)) => {
                            let mut stderr = stderr.normalize(policy.max_output_bytes);
                            append_bounded_text(
                                &mut stderr,
                                &format!("command timed out after {} seconds", timeout_secs),
                                policy.max_output_bytes,
                            );
                            ShellCommandResult::timed_out(CommandResult {
                                exit_code: Some(-1),
                                stdout: Some(stdout.normalize(policy.max_output_bytes)),
                                stderr: Some(stderr),
                                duration_ms: Some(duration_ms),
                                error: Some("command timed out".to_string()),
                            })
                        }
                        Err(e) => ShellCommandResult::outcome_unknown(CommandResult {
                            exit_code: Some(-1),
                            stdout: None,
                            stderr: None,
                            duration_ms: Some(duration_ms),
                            error: Some(format!(
                                "command timed out; failed to collect output: {}",
                                e
                            )),
                        }),
                    };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let cleanup = terminate_and_collect_pipes(child, drains).err();
                return ShellCommandResult::outcome_unknown(CommandResult {
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some(with_cleanup_error(
                        format!("failed to wait command: {}", e),
                        cleanup,
                    )),
                });
            }
        }
    }
    if let Err(error) = finish_stdin_writer(stdin_writer) {
        let cleanup = terminate_and_collect_pipes(child, drains).err();
        return ShellCommandResult::outcome_unknown(CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(with_cleanup_error(
                format!("failed to write command stdin: {error}"),
                cleanup,
            )),
        });
    }
    match terminate_and_collect_pipes(child, drains) {
        Ok((status, stdout, stderr)) => ShellCommandResult::completed(CommandResult {
            exit_code: Some(status.code().unwrap_or(-1)),
            stdout: Some(stdout.normalize(policy.max_output_bytes)),
            stderr: Some(stderr.normalize(policy.max_output_bytes)),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        }),
        Err(e) => spawned_output_failure(start, e),
    }
}

fn poll_stdin_writer(
    receiver: &mut Option<mpsc::Receiver<std::io::Result<()>>>,
) -> Result<(), String> {
    let Some(active) = receiver.as_ref() else {
        return Ok(());
    };
    match active.try_recv() {
        Ok(Ok(())) => {
            *receiver = None;
            Ok(())
        }
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            // The child may deliberately close stdin before exiting. Its
            // terminal status and output remain the source of truth.
            *receiver = None;
            Ok(())
        }
        Ok(Err(error)) => {
            *receiver = None;
            Err(error.to_string())
        }
        Err(mpsc::TryRecvError::Empty) => Ok(()),
        Err(mpsc::TryRecvError::Disconnected) => {
            *receiver = None;
            Err("stdin writer ended without a result".to_string())
        }
    }
}

fn finish_stdin_writer(
    receiver: Option<mpsc::Receiver<std::io::Result<()>>>,
) -> Result<(), String> {
    let Some(receiver) = receiver else {
        return Ok(());
    };
    match receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err("stdin writer did not finish after process exit".to_string())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("stdin writer ended without a result".to_string())
        }
    }
}

fn spawned_output_failure(start: Instant, error: String) -> ShellCommandResult {
    ShellCommandResult::outcome_unknown(CommandResult {
        exit_code: None,
        stdout: None,
        stderr: None,
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: Some(error),
    })
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod runner_lifecycle_tests;
