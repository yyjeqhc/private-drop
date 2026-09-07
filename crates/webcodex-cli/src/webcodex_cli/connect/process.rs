use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::super::system::discover_internal_binary;
use super::profile::{
    atomic_write, ensure_private_directory, protect_secret_file, sha256_hex,
    validate_existing_regular_file, ProfileLock,
};
#[cfg(windows)]
use webcodex_runner_config::paths::paths_equal;

const CONNECT_MARKER_FILE: &str = "hosted-connect";
const RUNNER_STATE_FILE: &str = "runner.toml";
const RUNNER_LOG_FILE: &str = "runner.log";
const RUNNER_LOG_ARCHIVES: usize = 2;
pub(crate) const RUNNER_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const RUNNER_LOG_READ_MAX_BYTES_PER_FILE: u64 = 1024 * 1024;
static OWNED_RUNNER_CHILDREN: OnceLock<Mutex<HashMap<u32, Child>>> = OnceLock::new();
static OWNED_LOG_WRITERS: OnceLock<Mutex<HashMap<u32, OwnedLogWriter>>> = OnceLock::new();

fn owned_runner_children() -> &'static Mutex<HashMap<u32, Child>> {
    OWNED_RUNNER_CHILDREN.get_or_init(|| Mutex::new(HashMap::new()))
}

fn owned_log_writers() -> &'static Mutex<HashMap<u32, OwnedLogWriter>> {
    OWNED_LOG_WRITERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn reap_owned_runner(pid: u32) {
    let child = owned_runner_children().lock().unwrap().remove(&pid);
    if let Some(mut child) = child {
        let _ = child.wait();
    }
}

enum OwnedLogWriter {
    #[cfg(not(test))]
    Process(Child),
    #[cfg(test)]
    Thread(std::thread::JoinHandle<Result<(), String>>),
}

fn reap_owned_log_writer(runner_pid: u32) {
    let writer = owned_log_writers().lock().unwrap().remove(&runner_pid);
    if let Some(writer) = writer {
        finish_owned_log_writer(writer);
    }
}

fn finish_owned_log_writer(writer: OwnedLogWriter) {
    match writer {
        #[cfg(not(test))]
        OwnedLogWriter::Process(mut child) => {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        #[cfg(test)]
        OwnedLogWriter::Thread(thread) => {
            let _ = thread.join();
        }
    }
}

fn owned_log_writer_exited(writer: &mut OwnedLogWriter) -> Result<bool, String> {
    match writer {
        #[cfg(not(test))]
        OwnedLogWriter::Process(child) => child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| format!("failed to inspect hosted Runner log writer: {error}")),
        #[cfg(test)]
        OwnedLogWriter::Thread(_) => Ok(false),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct LogWriterState {
    pid: u32,
    process_start: String,
    executable: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct RunnerState {
    pid: u32,
    process_start: String,
    executable: String,
    config: String,
    config_sha256: String,
    started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_writer: Option<LogWriterState>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalRunnerStateSummary {
    pub(crate) managed: bool,
    pub(crate) running: bool,
    pub(crate) pid: Option<u32>,
    pub(crate) log_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalRunnerServiceAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunnerStart {
    Started,
    Reused,
}

pub(crate) fn local_runner_profile_marker(state_dir: &Path) -> PathBuf {
    state_dir.join(CONNECT_MARKER_FILE)
}

fn local_runner_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RUNNER_STATE_FILE)
}

pub(crate) fn local_runner_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RUNNER_LOG_FILE)
}

pub(super) fn load_runner_state(state_dir: &Path) -> Result<Option<RunnerState>, String> {
    let path = local_runner_state_path(state_dir);
    if !path.exists() {
        return Ok(None);
    }
    validate_existing_regular_file(&path)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|error| format!("failed to read runner state {}: {error}", path.display()))?;
    toml::from_str(&content)
        .map(Some)
        .map_err(|error| format!("failed to parse runner state {}: {error}", path.display()))
}

#[cfg(target_os = "linux")]
fn linux_process_start(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let remaining = stat.get(close + 2..)?;
    let fields = remaining.split_whitespace().collect::<Vec<_>>();
    if fields.first().copied() == Some("Z") {
        return None;
    }
    fields.get(19).map(|value| (*value).to_string())
}

#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<String> {
    linux_process_start(pid)
}

#[cfg(windows)]
fn process_start(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: OpenProcess returns a handle or NULL; NULL means the pid no
    // longer exists (or is inaccessible, which also means it is not ours).
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut kernel = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut user = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: `handle` is valid and the four out-params are valid FILETIMEs.
    let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    // SAFETY: close the handle we opened.
    unsafe { CloseHandle(handle) };
    // The creation FILETIME (100ns ticks since 1601) is a stable per-process
    // identity: a reused pid has a different creation time, exactly like the
    // Linux starttime field.
    (ok != 0)
        .then(|| u64::from(creation.dwHighDateTime) << 32 | u64::from(creation.dwLowDateTime))
        .map(|value| value.to_string())
}

#[cfg(target_os = "linux")]
fn process_executable(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|path| path.to_string_lossy().to_string())
}

#[cfg(windows)]
fn process_executable(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: OpenProcess returns a handle or NULL; NULL means the pid no
    // longer exists (or is inaccessible, which also means it is not ours).
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    // QueryFullProcessImageNameW returns the image path in UTF-16; the size
    // parameter is both the buffer length and the written length.
    let mut buffer = [0u16; 32768];
    let mut size = buffer.len() as u32;
    // SAFETY: `handle` is valid; `buffer` outlives the call and `size` is a
    // valid in/out length for it.
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) };
    // SAFETY: close the handle we opened.
    unsafe { CloseHandle(handle) };
    (ok != 0)
        .then(|| String::from_utf16(&buffer[..size as usize]).ok())
        .flatten()
}

#[cfg(not(any(target_os = "linux", windows)))]
fn process_start(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn process_executable(_pid: u32) -> Option<String> {
    None
}

pub(super) fn process_matches(state: &RunnerState) -> bool {
    if state.pid <= 1 || process_start(state.pid).as_deref() != Some(&state.process_start) {
        return false;
    }
    process_image_matches(state.pid, &state.executable, &[&state.config])
}

/// Confirm the process with the recorded creation time still runs the
/// recorded image, without relying on POSIX `ps`.
///
/// - Linux: `/proc/<pid>/exe` readlink.
/// - Windows: `QueryFullProcessImageNameW`, compared under the platform path
///   identity rules (`\\?\` prefixes, case and separators) because the image
///   name form can differ from the stored canonical path.
/// - Other Unix: `ps -p <pid> -o command=` (no `/proc`); the extra needles
///   (config path / log-writer marker) pin down the command line.
#[cfg(windows)]
fn process_image_matches(pid: u32, executable: &str, _needles: &[&str]) -> bool {
    process_executable(pid)
        .is_some_and(|actual| paths_equal(Path::new(&actual), Path::new(executable)))
}

#[cfg(target_os = "linux")]
fn process_image_matches(pid: u32, executable: &str, _needles: &[&str]) -> bool {
    process_executable(pid).as_deref() == Some(executable)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn process_image_matches(pid: u32, executable: &str, needles: &[&str]) -> bool {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    output.is_ok_and(|output| {
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains(executable)
            && needles
                .iter()
                .all(|needle| String::from_utf8_lossy(&output.stdout).contains(needle))
    })
}

fn remove_stale_state(state_dir: &Path) -> Result<(), String> {
    let path = local_runner_state_path(state_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to remove stale runner state {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn local_runner_state_summary(
    state_dir: &Path,
) -> Result<LocalRunnerStateSummary, String> {
    let state = load_runner_state(state_dir)?;
    let running = state.as_ref().is_some_and(process_matches);
    Ok(LocalRunnerStateSummary {
        managed: local_runner_profile_marker(state_dir).is_file(),
        running,
        pid: running.then(|| state.as_ref().unwrap().pid),
        log_path: local_runner_log_path(state_dir),
    })
}

fn local_runner_log_archive_path(state_dir: &Path, generation: usize) -> PathBuf {
    state_dir.join(format!("{RUNNER_LOG_FILE}.{generation}"))
}

fn open_private_append(path: &Path) -> Result<File, String> {
    if path.exists() {
        validate_existing_regular_file(path)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("failed to open Runner log {}: {error}", path.display()))?;
    protect_secret_file(path)?;
    Ok(file)
}

fn rotate_runner_logs(state_dir: &Path) -> Result<File, String> {
    let current = local_runner_log_path(state_dir);
    let oldest = local_runner_log_archive_path(state_dir, RUNNER_LOG_ARCHIVES);
    if oldest.exists() {
        validate_existing_regular_file(&oldest)?;
        std::fs::remove_file(&oldest)
            .map_err(|error| format!("failed to remove {}: {error}", oldest.display()))?;
    }
    for generation in (1..RUNNER_LOG_ARCHIVES).rev() {
        let source = local_runner_log_archive_path(state_dir, generation);
        if !source.exists() {
            continue;
        }
        validate_existing_regular_file(&source)?;
        let target = local_runner_log_archive_path(state_dir, generation + 1);
        std::fs::rename(&source, &target).map_err(|error| {
            format!(
                "failed to rotate Runner log {} to {}: {error}",
                source.display(),
                target.display()
            )
        })?;
        protect_secret_file(&target)?;
    }
    if current.exists() {
        validate_existing_regular_file(&current)?;
        let target = local_runner_log_archive_path(state_dir, 1);
        std::fs::rename(&current, &target).map_err(|error| {
            format!(
                "failed to rotate Runner log {} to {}: {error}",
                current.display(),
                target.display()
            )
        })?;
        protect_secret_file(&target)?;
    }
    open_private_append(&current)
}

struct BoundedRunnerLog {
    state_dir: PathBuf,
    file: Option<File>,
    bytes: u64,
}

impl BoundedRunnerLog {
    fn open(state_dir: &Path) -> Result<Self, String> {
        let path = local_runner_log_path(state_dir);
        let mut file = open_private_append(&path)?;
        let mut bytes = file
            .metadata()
            .map_err(|error| format!("failed to inspect Runner log {}: {error}", path.display()))?
            .len();
        if bytes >= RUNNER_LOG_MAX_BYTES {
            file.flush()
                .map_err(|error| format!("failed to flush {}: {error}", path.display()))?;
            drop(file);
            file = rotate_runner_logs(state_dir)?;
            bytes = 0;
        }
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            file: Some(file),
            bytes,
        })
    }

    fn rotate(&mut self) -> Result<(), String> {
        if let Some(mut file) = self.file.take() {
            file.flush().map_err(|error| {
                format!(
                    "failed to flush {} before rotation: {error}",
                    local_runner_log_path(&self.state_dir).display()
                )
            })?;
        }
        match rotate_runner_logs(&self.state_dir) {
            Ok(file) => {
                self.file = Some(file);
                self.bytes = 0;
                Ok(())
            }
            Err(rotation_error) => {
                let path = local_runner_log_path(&self.state_dir);
                let mut options = OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let fallback = options.open(&path).and_then(|file| {
                    protect_secret_file(&path)
                        .map_err(std::io::Error::other)
                        .map(|()| file)
                });
                match fallback {
                    Ok(file) => {
                        self.file = Some(file);
                        self.bytes = 0;
                        Ok(())
                    }
                    Err(fallback_error) => Err(format!(
                        "{rotation_error}; failed to bound the current Runner log: {fallback_error}"
                    )),
                }
            }
        }
    }

    fn write_all_bounded(&mut self, mut content: &[u8]) -> Result<(), String> {
        while !content.is_empty() {
            if self.bytes >= RUNNER_LOG_MAX_BYTES {
                self.rotate()?;
            }
            let remaining = RUNNER_LOG_MAX_BYTES.saturating_sub(self.bytes) as usize;
            let take = remaining.min(content.len());
            let file = self
                .file
                .as_mut()
                .ok_or_else(|| "Runner log sink is unavailable".to_string())?;
            file.write_all(&content[..take]).map_err(|error| {
                format!(
                    "failed to write Runner log {}: {error}",
                    local_runner_log_path(&self.state_dir).display()
                )
            })?;
            self.bytes = self.bytes.saturating_add(take as u64);
            content = &content[take..];
        }
        Ok(())
    }
}

fn validate_log_writer_state_dir(state_dir: &Path) -> Result<PathBuf, String> {
    if !state_dir.is_absolute() {
        return Err("hosted log writer state directory must be absolute".to_string());
    }
    let metadata = std::fs::symlink_metadata(state_dir).map_err(|error| {
        format!(
            "failed to inspect hosted Runner state directory {}: {error}",
            state_dir.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("hosted log writer state directory must be a real directory".to_string());
    }
    let canonical = state_dir.canonicalize().map_err(|error| {
        format!(
            "failed to resolve hosted Runner state directory {}: {error}",
            state_dir.display()
        )
    })?;
    if canonical != state_dir {
        return Err("hosted log writer state directory must already be canonical".to_string());
    }
    validate_existing_regular_file(&local_runner_profile_marker(&canonical))?;
    Ok(canonical)
}

pub(crate) fn run_hosted_log_writer(state_dir: &Path, input: &mut impl Read) -> Result<(), String> {
    let state_dir = validate_log_writer_state_dir(state_dir)?;
    let mut sink = BoundedRunnerLog::open(&state_dir).ok();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| format!("failed to read hosted Runner log pipe: {error}"))?;
        if read == 0 {
            return Ok(());
        }
        if let Some(active) = sink.as_mut() {
            if active.write_all_bounded(&buffer[..read]).is_err() {
                // Keep draining the pipe so a local filesystem failure cannot
                // block or terminate an otherwise healthy Runner.
                sink = None;
            }
        }
    }
}

struct SpawnedLogWriter {
    stdout: os_pipe::PipeWriter,
    stderr: os_pipe::PipeWriter,
    owned: OwnedLogWriter,
    state: Option<LogWriterState>,
}

#[cfg(not(test))]
fn spawn_log_writer(state_dir: &Path) -> Result<SpawnedLogWriter, String> {
    let (reader, writer) =
        os_pipe::pipe().map_err(|error| format!("failed to create Runner log pipe: {error}"))?;
    let stderr = writer
        .try_clone()
        .map_err(|error| format!("failed to clone Runner log pipe: {error}"))?;
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("failed to resolve the webcodex executable: {error}"))?;
    let mut command = Command::new(&executable);
    command
        .arg("__hosted-log-writer")
        .arg(state_dir)
        .stdin(Stdio::from(reader))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start hosted Runner log writer: {error}"))?;
    let pid = child.id();
    let process_start = (0..20)
        .find_map(|_| {
            let marker = process_start(pid);
            if marker.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            marker
        })
        .unwrap_or_default();
    if process_start.is_empty() {
        let _ = child.kill();
        let _ = child.wait();
        return Err("failed to capture the hosted log writer process identity".to_string());
    }
    let process_executable =
        process_executable(pid).unwrap_or_else(|| executable.to_string_lossy().to_string());
    Ok(SpawnedLogWriter {
        stdout: writer,
        stderr,
        owned: OwnedLogWriter::Process(child),
        state: Some(LogWriterState {
            pid,
            process_start,
            executable: process_executable,
        }),
    })
}

#[cfg(test)]
fn spawn_log_writer(state_dir: &Path) -> Result<SpawnedLogWriter, String> {
    let (mut reader, writer) =
        os_pipe::pipe().map_err(|error| format!("failed to create Runner log pipe: {error}"))?;
    let stderr = writer
        .try_clone()
        .map_err(|error| format!("failed to clone Runner log pipe: {error}"))?;
    let state_dir = state_dir.to_path_buf();
    let thread = std::thread::spawn(move || run_hosted_log_writer(&state_dir, &mut reader));
    Ok(SpawnedLogWriter {
        stdout: writer,
        stderr,
        owned: OwnedLogWriter::Thread(thread),
        state: None,
    })
}

fn start_runner(
    runner_bin: &Path,
    config: &Path,
    state_dir: &Path,
    config_sha256: String,
) -> Result<(), String> {
    let executable = runner_bin.canonicalize().map_err(|error| {
        format!(
            "failed to resolve Runner binary {}: {error}",
            runner_bin.display()
        )
    })?;
    let config = config.canonicalize().map_err(|error| {
        format!(
            "failed to resolve Runner config {}: {error}",
            config.display()
        )
    })?;
    let mut log_writer = spawn_log_writer(state_dir)?;
    let mut command = Command::new(&executable);
    command
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_writer.stdout))
        .stderr(Stdio::from(log_writer.stderr))
        .env("RUST_LOG", "info");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            drop(command);
            finish_owned_log_writer(log_writer.owned);
            return Err(format!(
                "failed to start webcodex-runner {}: {error}",
                executable.display()
            ));
        }
    };
    std::thread::sleep(Duration::from_millis(150));
    let immediate_status = match child.try_wait() {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            drop(command);
            finish_owned_log_writer(log_writer.owned);
            return Err(format!("failed to inspect new Runner process: {error}"));
        }
    };
    if let Some(status) = immediate_status {
        drop(command);
        finish_owned_log_writer(log_writer.owned);
        return Err(format!("webcodex-runner exited immediately with {status}"));
    }
    match owned_log_writer_exited(&mut log_writer.owned) {
        Ok(false) => {}
        Ok(true) => {
            let _ = child.kill();
            let _ = child.wait();
            drop(command);
            finish_owned_log_writer(log_writer.owned);
            return Err("hosted Runner log writer exited immediately".to_string());
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            drop(command);
            finish_owned_log_writer(log_writer.owned);
            return Err(error);
        }
    }
    let pid = child.id();
    let process_start = (0..20)
        .find_map(|_| {
            let marker = process_start(pid);
            if marker.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            marker
        })
        .unwrap_or_default();
    if process_start.is_empty() {
        let _ = child.kill();
        let _ = child.wait();
        drop(command);
        finish_owned_log_writer(log_writer.owned);
        return Err("failed to capture the new Runner process identity".to_string());
    }
    let process_executable =
        process_executable(pid).unwrap_or_else(|| executable.to_string_lossy().to_string());
    let state = RunnerState {
        pid,
        process_start,
        executable: process_executable,
        config: config.to_string_lossy().to_string(),
        config_sha256,
        started_at: chrono::Utc::now().to_rfc3339(),
        log_writer: log_writer.state,
    };
    let content = match toml::to_string(&state) {
        Ok(content) => content,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            drop(command);
            finish_owned_log_writer(log_writer.owned);
            return Err(format!("failed to render Runner state: {error}"));
        }
    };
    if let Err(error) = atomic_write(
        &local_runner_state_path(state_dir),
        content.as_bytes(),
        true,
    ) {
        let _ = child.kill();
        let _ = child.wait();
        drop(command);
        finish_owned_log_writer(log_writer.owned);
        return Err(error);
    }
    owned_log_writers()
        .lock()
        .unwrap()
        .insert(pid, log_writer.owned);
    owned_runner_children().lock().unwrap().insert(pid, child);
    Ok(())
}

#[cfg(unix)]
fn signal_process(pid: u32, signal: i32) -> Result<(), String> {
    if pid <= 1 {
        return Err("refusing to signal an invalid Runner pid".to_string());
    }
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(format!("failed to signal Runner pid {pid}: {error}"))
        }
    }
}

/// Native hard stop for the Runner on Windows.
///
/// `taskkill` without `/F` only works on GUI apps (it sends `WM_CLOSE`), so
/// console processes could never be stopped through it. `TerminateProcess`
/// terminates the Runner directly; its descendants live in Job Objects with
/// `KILL_ON_JOB_CLOSE` and die with it, so no tree walk is needed. A pid
/// that no longer exists is not an error (it already stopped).
#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    if pid <= 1 {
        return Err("refusing to terminate an invalid Runner pid".to_string());
    }
    // SAFETY: OpenProcess returns a handle or NULL; NULL means the pid no
    // longer exists (or is inaccessible), which means the stop already
    // happened.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Ok(());
    }
    // SAFETY: `handle` is valid; exit code 1 is arbitrary for a killed
    // process.
    let ok = unsafe { TerminateProcess(handle, 1) };
    // SAFETY: close the handle we opened.
    unsafe { CloseHandle(handle) };
    if ok != 0 {
        Ok(())
    } else {
        Err(format!(
            "failed to terminate Runner pid {pid}: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn log_writer_matches(state: &LogWriterState) -> bool {
    if state.pid <= 1 || process_start(state.pid).as_deref() != Some(&state.process_start) {
        return false;
    }
    process_image_matches(state.pid, &state.executable, &["__hosted-log-writer"])
}

#[cfg(not(test))]
fn runner_log_writer_active(state: &RunnerState) -> bool {
    state.log_writer.as_ref().is_some_and(log_writer_matches)
}

#[cfg(test)]
fn runner_log_writer_active(_state: &RunnerState) -> bool {
    true
}

fn stop_log_writer(state: &RunnerState) {
    if let Some(writer) = state.log_writer.as_ref() {
        let natural_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < natural_deadline && log_writer_matches(writer) {
            std::thread::sleep(Duration::from_millis(25));
        }
        if log_writer_matches(writer) {
            #[cfg(unix)]
            let _ = signal_process(writer.pid, libc::SIGTERM);
            #[cfg(windows)]
            let _ = terminate_process(writer.pid);
            let term_deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < term_deadline && log_writer_matches(writer) {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        if log_writer_matches(writer) {
            #[cfg(unix)]
            let _ = signal_process(writer.pid, libc::SIGKILL);
        }
    }
    reap_owned_log_writer(state.pid);
}

pub(super) fn stop_runner_unlocked(state_dir: &Path) -> Result<bool, String> {
    let Some(state) = load_runner_state(state_dir)? else {
        return Ok(false);
    };
    if !process_matches(&state) {
        stop_log_writer(&state);
        remove_stale_state(state_dir)?;
        return Ok(false);
    }
    #[cfg(unix)]
    signal_process(state.pid, libc::SIGTERM)?;
    #[cfg(windows)]
    terminate_process(state.pid)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !process_matches(&state) {
            reap_owned_runner(state.pid);
            stop_log_writer(&state);
            remove_stale_state(state_dir)?;
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(unix)]
    signal_process(state.pid, libc::SIGKILL)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && process_matches(&state) {
        std::thread::sleep(Duration::from_millis(25));
    }
    reap_owned_runner(state.pid);
    stop_log_writer(&state);
    remove_stale_state(state_dir)?;
    Ok(true)
}

pub(super) fn ensure_runner_unlocked(
    runner_bin: &Path,
    config: &Path,
    state_dir: &Path,
) -> Result<RunnerStart, String> {
    let config_bytes = std::fs::read(config)
        .map_err(|error| format!("failed to read Runner config {}: {error}", config.display()))?;
    let config_sha256 = sha256_hex(&config_bytes);
    if let Some(state) = load_runner_state(state_dir)? {
        if process_matches(&state)
            && state.config_sha256 == config_sha256
            && runner_log_writer_active(&state)
        {
            return Ok(RunnerStart::Reused);
        }
        if process_matches(&state) {
            stop_runner_unlocked(state_dir)?;
        } else {
            stop_log_writer(&state);
            remove_stale_state(state_dir)?;
        }
    }
    start_runner(runner_bin, config, state_dir, config_sha256)?;
    Ok(RunnerStart::Started)
}

pub(crate) fn run_local_runner_service(
    action: LocalRunnerServiceAction,
    config: &Path,
    state_dir: &Path,
    runner_bin: Option<&Path>,
) -> Result<String, String> {
    let state_dir = ensure_private_directory(state_dir)?;
    let _lock = ProfileLock::acquire(&state_dir)?;
    match action {
        LocalRunnerServiceAction::Stop => {
            let stopped = stop_runner_unlocked(&state_dir)?;
            Ok(if stopped {
                "Hosted Runner stopped.\n".to_string()
            } else {
                "Hosted Runner is not running.\n".to_string()
            })
        }
        LocalRunnerServiceAction::Start | LocalRunnerServiceAction::Restart => {
            if action == LocalRunnerServiceAction::Restart {
                stop_runner_unlocked(&state_dir)?;
            }
            let runner = runner_bin
                .map(Path::to_path_buf)
                .or_else(|| discover_internal_binary("webcodex-runner"))
                .ok_or_else(|| {
                    "webcodex-runner was not found beside webcodex or in an absolute PATH entry"
                        .to_string()
                })?;
            let started = ensure_runner_unlocked(&runner, config, &state_dir)?;
            Ok(format!(
                "Hosted Runner {}.\n  config: {}\n  logs:   {}\n",
                if started == RunnerStart::Started {
                    "started"
                } else {
                    "was already running"
                },
                config.display(),
                local_runner_log_path(&state_dir).display()
            ))
        }
    }
}

pub(crate) fn run_local_runner_logs(
    state_dir: &Path,
    lines: u32,
    since: Option<&str>,
    follow: bool,
) -> Result<String, String> {
    let path = local_runner_log_path(state_dir);
    if since.is_some() {
        return Err(format!(
            "--since is available for systemd journal logs only; local Runner logs are at {}",
            path.display()
        ));
    }
    if follow {
        return follow_log_tail(state_dir, lines);
    }
    read_local_runner_log_tail(state_dir, lines).map(|tail| tail.output)
}

/// Stream the local Runner log from `lines` back, then keep printing
/// appended bytes until the CLI is interrupted.
///
/// In-process so the CLI never depends on a POSIX `tail` binary (absent on
/// Windows). The file is re-opened on every poll, so rotation (rename to
/// `.1` plus recreate, which makes the length shrink) is followed the same
/// way `tail -F` follows the file across renames.
fn follow_log_tail(state_dir: &Path, lines: u32) -> Result<String, String> {
    let path = local_runner_log_path(state_dir);
    let tail = read_local_runner_log_tail(state_dir, lines)?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(tail.output.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("failed to write Runner log tail: {error}"))?;
    let mut last_len = std::fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let Ok(metadata) = std::fs::metadata(&path) else {
            // Log missing momentarily (rotation gap); keep waiting.
            continue;
        };
        let len = metadata.len();
        if len < last_len {
            // Rotated or truncated: a fresh file is being written. The new
            // file starts empty, so resetting the position prints nothing
            // until real content arrives (no duplicate lines).
            last_len = 0;
        }
        if len == last_len {
            continue;
        }
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(_) => continue,
        };
        if file.seek(SeekFrom::Start(last_len)).is_err() {
            continue;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            continue;
        }
        last_len = last_len.saturating_add(bytes.len() as u64);
        stdout
            .write_all(&bytes)
            .and_then(|()| stdout.flush())
            .map_err(|error| format!("failed to write Runner log stream: {error}"))?;
    }
}

struct LogTail {
    output: String,
    #[cfg(test)]
    bytes_read: u64,
}

fn read_log_tail_chunk(path: &Path) -> Result<Option<(String, bool, u64)>, String> {
    if !path.exists() {
        return Ok(None);
    }
    validate_existing_regular_file(path)?;
    let mut file = File::open(path)
        .map_err(|error| format!("failed to read Runner log {}: {error}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|error| format!("failed to inspect Runner log {}: {error}", path.display()))?
        .len();
    let start = len.saturating_sub(RUNNER_LOG_READ_MAX_BYTES_PER_FILE);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("failed to seek Runner log {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity((len - start) as usize);
    file.take(RUNNER_LOG_READ_MAX_BYTES_PER_FILE)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read Runner log {}: {error}", path.display()))?;
    let bytes_read = bytes.len() as u64;
    Ok(Some((
        String::from_utf8_lossy(&bytes).into_owned(),
        start == 0,
        bytes_read,
    )))
}

fn logical_line_count(value: &str) -> usize {
    if value.is_empty() {
        0
    } else {
        value
            .as_bytes()
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            + usize::from(!value.ends_with('\n'))
    }
}

fn select_last_lines(value: &str, lines: usize) -> String {
    if value.is_empty() || lines == 0 {
        return String::new();
    }
    let bytes = value.as_bytes();
    let end = if bytes.last() == Some(&b'\n') {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    let mut start = end;
    let mut selected = 0;
    while start > 0 && selected < lines {
        match bytes[..start].iter().rposition(|byte| *byte == b'\n') {
            Some(newline) => {
                selected += 1;
                if selected == lines {
                    start = newline + 1;
                    break;
                }
                start = newline;
            }
            None => {
                start = 0;
                break;
            }
        }
    }
    let mut output = value[start..end].to_string();
    output.push('\n');
    output
}

fn read_local_runner_log_tail(state_dir: &Path, lines: u32) -> Result<LogTail, String> {
    let paths = [
        local_runner_log_path(state_dir),
        local_runner_log_archive_path(state_dir, 1),
        local_runner_log_archive_path(state_dir, 2),
    ];
    let mut newest_first = Vec::new();
    let mut any_file = false;
    let mut bytes_read = 0_u64;
    for path in paths {
        let Some((chunk, reached_start, read)) = read_log_tail_chunk(&path)? else {
            continue;
        };
        any_file = true;
        bytes_read = bytes_read.saturating_add(read);
        newest_first.push(chunk);
        let combined = newest_first
            .iter()
            .rev()
            .map(String::as_str)
            .collect::<String>();
        if logical_line_count(&combined) >= lines as usize || !reached_start {
            break;
        }
    }
    if !any_file {
        return Err(format!(
            "failed to read Runner log {}: file does not exist",
            local_runner_log_path(state_dir).display()
        ));
    }
    let combined = newest_first
        .iter()
        .rev()
        .map(String::as_str)
        .collect::<String>();
    Ok(LogTail {
        output: select_last_lines(&combined, lines as usize),
        #[cfg(test)]
        bytes_read,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webcodex_cli::test_support::canonical_test_tempdir;

    fn write_lines(path: &Path, start: usize, end: usize, trailing_newline: bool) {
        let mut content = (start..end)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        if trailing_newline {
            content.push('\n');
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn local_runner_tail_reads_archives_in_order_and_bounds_large_files() {
        let tmp = canonical_test_tempdir();
        let state = tmp.path();
        write_lines(&local_runner_log_archive_path(state, 2), 0, 50, true);
        write_lines(&local_runner_log_archive_path(state, 1), 50, 100, true);
        write_lines(&local_runner_log_path(state), 100, 120, false);

        let tail = read_local_runner_log_tail(state, 100).unwrap();
        let expected = (20..120)
            .map(|line| format!("line-{line}\n"))
            .collect::<String>();
        assert_eq!(tail.output, expected);

        let bounded = tmp.path().join("bounded");
        std::fs::create_dir(&bounded).unwrap();
        write_lines(&local_runner_log_path(&bounded), 100, 120, true);
        let archive = local_runner_log_archive_path(&bounded, 1);
        // The archive content is replaced wholesale below (set_len to 2x the
        // read cap, then a fresh log), so truncation is the intended contract.
        let mut large = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&archive)
            .unwrap();
        large
            .set_len(RUNNER_LOG_READ_MAX_BYTES_PER_FILE * 2)
            .unwrap();
        large.seek(SeekFrom::End(0)).unwrap();
        large.write_all(b"\n").unwrap();
        for line in 20..100 {
            writeln!(large, "line-{line}").unwrap();
        }
        drop(large);
        std::fs::write(
            local_runner_log_archive_path(&bounded, 2),
            "must-not-be-read\n",
        )
        .unwrap();

        let tail = read_local_runner_log_tail(&bounded, 100).unwrap();
        let expected = (20..120)
            .map(|line| format!("line-{line}\n"))
            .collect::<String>();
        assert_eq!(tail.output, expected);
        assert!(
            tail.bytes_read
                <= RUNNER_LOG_READ_MAX_BYTES_PER_FILE
                    + std::fs::metadata(local_runner_log_path(&bounded))
                        .unwrap()
                        .len()
        );
        assert!(
            tail.bytes_read < std::fs::metadata(&archive).unwrap().len(),
            "the archived log must be read from the tail instead of loaded in full"
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_runner_reuses_process_recovers_stale_pid_and_stops() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = canonical_test_tempdir();
        let runner = tmp.path().join("webcodex-runner");
        std::fs::write(
            &runner,
            "#!/bin/sh\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = tmp.path().join("runner.toml");
        std::fs::write(&config, "server_url='http://example.test'\n").unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(
            local_runner_profile_marker(&state),
            "profile = \"lifecycle-test\"\n",
        )
        .unwrap();

        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Started
        );
        let first = load_runner_state(&state).unwrap().unwrap();
        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Reused
        );
        assert_eq!(load_runner_state(&state).unwrap().unwrap().pid, first.pid);

        std::fs::write(
            &config,
            "server_url='http://example.test'\ntransport='websocket'\n",
        )
        .unwrap();
        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Started
        );
        let restarted = load_runner_state(&state).unwrap().unwrap();
        assert_ne!(restarted.pid, first.pid);
        assert!(stop_runner_unlocked(&state).unwrap());
        let mut stale = restarted;
        stale.pid = std::process::id();
        stale.process_start = "not-this-process".to_string();
        atomic_write(
            &local_runner_state_path(&state),
            toml::to_string(&stale).unwrap().as_bytes(),
            true,
        )
        .unwrap();
        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Started
        );
        assert_ne!(load_runner_state(&state).unwrap().unwrap().pid, stale.pid);
        assert!(stop_runner_unlocked(&state).unwrap());
        assert!(!local_runner_state_summary(&state).unwrap().running);
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_identity_is_stable_for_current_process() {
        let pid = std::process::id();
        let start = process_start(pid).expect("current process creation time");
        assert_eq!(process_start(pid).as_deref(), Some(start.as_str()));
        let executable = process_executable(pid).expect("current process image name");
        let current = std::env::current_exe().unwrap();
        assert!(
            paths_equal(Path::new(&executable), &current),
            "image name {executable} must identity-match current_exe {}",
            current.display()
        );
        // A pid that cannot exist has no identity.
        assert_eq!(process_start(u32::MAX), None);
    }

    #[cfg(windows)]
    #[test]
    fn local_runner_reuses_process_recovers_stale_pid_and_stops_windows() {
        let tmp = canonical_test_tempdir();
        // A real, native long-lived "Runner": a cmd batch that loops forever.
        // ensure_runner_unlocked drives it through the same Win32 identity
        // capture and taskkill stop path the production binary uses.
        let runner = tmp.path().join("webcodex-runner.cmd");
        std::fs::write(
            &runner,
            "@echo off\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n",
        )
        .unwrap();
        let config = tmp.path().join("runner.toml");
        std::fs::write(&config, "server_url='http://example.test'\n").unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(
            local_runner_profile_marker(&state),
            "profile = \"lifecycle-test\"\n",
        )
        .unwrap();

        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Started
        );
        let first = load_runner_state(&state).unwrap().unwrap();
        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Reused
        );
        assert_eq!(load_runner_state(&state).unwrap().unwrap().pid, first.pid);
        assert!(
            process_matches(&first),
            "the Win32 identity must recognize the running batch process"
        );

        std::fs::write(
            &config,
            "server_url='http://example.test'\ntransport='websocket'\n",
        )
        .unwrap();
        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Started
        );
        let restarted = load_runner_state(&state).unwrap().unwrap();
        assert_ne!(restarted.pid, first.pid);
        assert!(stop_runner_unlocked(&state).unwrap());
        let mut stale = restarted;
        stale.pid = std::process::id();
        stale.process_start = "not-this-process".to_string();
        atomic_write(
            &local_runner_state_path(&state),
            toml::to_string(&stale).unwrap().as_bytes(),
            true,
        )
        .unwrap();
        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Started
        );
        assert_ne!(load_runner_state(&state).unwrap().unwrap().pid, stale.pid);
        assert!(stop_runner_unlocked(&state).unwrap());
        assert!(!local_runner_state_summary(&state).unwrap().running);
    }

    #[cfg(unix)]
    #[test]
    fn local_runner_rotates_logs_while_alive_and_stops_its_writer() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = canonical_test_tempdir();
        let runner = tmp.path().join("webcodex-runner");
        std::fs::write(
            &runner,
            "#!/bin/sh\n\
             trap 'exit 0' TERM INT\n\
             dd if=/dev/zero bs=1048576 count=32 2>/dev/null\n\
             printf '\\nrotation-complete\\n'\n\
             while :; do printf 'runner-alive\\n'; sleep 0.05; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = tmp.path().join("runner.toml");
        std::fs::write(&config, "server_url='http://example.test'\n").unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(
            local_runner_profile_marker(&state),
            "profile = \"rotation-test\"\n",
        )
        .unwrap();

        assert_eq!(
            ensure_runner_unlocked(&runner, &config, &state).unwrap(),
            RunnerStart::Started
        );
        let runner_state = load_runner_state(&state).unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline
            && (!local_runner_log_archive_path(&state, 2).is_file()
                || !read_local_runner_log_tail(&state, 10)
                    .is_ok_and(|tail| tail.output.contains("runner-alive")))
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            process_matches(&runner_state),
            "the Runner must remain alive while logs rotate"
        );
        let paths = [
            local_runner_log_path(&state),
            local_runner_log_archive_path(&state, 1),
            local_runner_log_archive_path(&state, 2),
        ];
        assert!(paths.iter().all(|path| path.is_file()));
        let total = paths
            .iter()
            .map(|path| std::fs::metadata(path).unwrap().len())
            .sum::<u64>();
        assert!(total <= RUNNER_LOG_MAX_BYTES * 3);
        for path in &paths {
            let metadata = std::fs::metadata(path).unwrap();
            assert!(metadata.len() <= RUNNER_LOG_MAX_BYTES);
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }

        assert!(stop_runner_unlocked(&state).unwrap());
        assert!(!process_matches(&runner_state));
        assert!(!owned_log_writers()
            .lock()
            .unwrap()
            .contains_key(&runner_state.pid));
    }

    #[cfg(unix)]
    #[test]
    fn immediate_runner_failure_does_not_leave_active_state() {
        let tmp = canonical_test_tempdir();
        let runner = std::path::PathBuf::from("/usr/bin/false");
        let config = tmp.path().join("runner.toml");
        std::fs::write(&config, "server_url='http://example.test'\n").unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(
            local_runner_profile_marker(&state),
            "profile = \"failure-test\"\n",
        )
        .unwrap();

        let error = ensure_runner_unlocked(&runner, &config, &state).unwrap_err();
        assert!(error.contains("exited immediately"), "{error}");
        assert!(!local_runner_state_path(&state).exists());
    }
}
